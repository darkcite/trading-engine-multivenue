# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_accrue — the entry store and the report's error bar.

Two properties make an accrual store trustworthy: merging is idempotent
(a re-driven day adds nothing, and can never UN-settle a row that
already has its payout), and the report refuses to call a result before
the sample can carry one. Both are pinned here.

The drive itself (cutting windows, seeding, spawning the harness) is
covered by the engine's own tests and by the harness contract; what is
worth pinning in Python is the arithmetic and the merge law.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json

import pytest

import claude_worker.bin15_accrue


def _e(outcome: int, ts: int = 1_000, px: int = 600_000, y: int = 1_000_000,
       is_yes: int = 1, qty: int = 80_000_000, offset: int = 30,
       origin: int = claude_worker.bin15_accrue.ORIGIN_PAPER):
    expiry = ts + 800_000_000_000
    return claude_worker.bin15_accrue.Entry(
        ts_ns=ts, outcome=outcome, family=0,
        start_ns=expiry - claude_worker.bin15_accrue.TAU_15M_NS,
        expiry_ns=expiry, offset_s=offset, is_yes=is_yes,
        px_1e6=px, qty_1e6=qty, p_hat_1e6=700_000, y=y, origin=origin,
    )


def test_an_instance_is_entered_once_so_a_redrive_adds_nothing() -> None:
    rows = [_e(7), _e(9, ts=2_000)]
    merged, added = claude_worker.bin15_accrue.merge_entries([], rows)
    assert added == 2 and len(merged) == 2
    again, added2 = claude_worker.bin15_accrue.merge_entries(merged, rows)
    assert added2 == 0, "re-driving a day must add nothing"
    assert len(again) == 2


def test_a_settled_row_upgrades_an_unsettled_one_but_never_the_reverse() -> None:
    """A window re-cut later can carry the same entry with its payout now
    derivable. That is an upgrade. The reverse — a later cut that has
    LOST the settlement because its expiry now falls outside — must not
    un-settle the store."""
    unk = _e(7, y=claude_worker.bin15_accrue.Y_UNKNOWN)
    known = _e(7, y=0)
    merged, _ = claude_worker.bin15_accrue.merge_entries([unk], [known])
    assert merged[0].y == 0, "unsettled -> settled is an upgrade"
    back, added = claude_worker.bin15_accrue.merge_entries([known], [unk])
    assert added == 0
    assert back[0].y == 0, "settled -> unsettled must never happen"


def test_won_reads_the_side_the_member_bought_not_the_yes_side() -> None:
    assert _e(1, is_yes=1, y=1_000_000).won
    assert not _e(1, is_yes=1, y=0).won
    # Bought NO and NO settled: a win, even though y is 0.
    assert _e(1, is_yes=0, y=0).won
    assert not _e(1, is_yes=0, y=1_000_000).won


def test_the_store_round_trips_through_the_file(tmp_path) -> None:
    p = tmp_path / "entries.tsv"
    rows = [_e(7), _e(9, ts=2_000, is_yes=0, y=0)]
    claude_worker.bin15_accrue.write_entries(p, rows)
    back = claude_worker.bin15_accrue.read_entries(p)
    assert back == rows
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"


def test_a_wrong_width_row_is_refused_rather_than_guessed(tmp_path) -> None:
    p = tmp_path / "entries.tsv"
    p.write_text("# head\n1\t2\t3\n", encoding="utf-8")
    with pytest.raises(ValueError, match="want 12 columns"):
        claude_worker.bin15_accrue.read_entries(p)


def test_a_pre_split_row_reads_as_paper_and_a_split_row_keeps_its_origin(
    tmp_path,
) -> None:
    """§6.4 moved the store from 11 columns to 12. An 11-column row is
    PAPER — not as a default, but because the harness models every fill
    and no live path had written here when those rows were produced.
    Guessing between the two is the mislabelling the split exists to
    prevent, so 11 and 12 are the ONLY widths accepted."""
    p = tmp_path / "entries.tsv"
    legacy = "\t".join(str(v) for v in (1, 7, 0, 0, 900_000_000_000, 12, 1, 640_000, 78_000_000, 700_000, 1_000_000))
    live = "\t".join(str(v) for v in (2, 8, 0, 0, 900_000_000_000, 12, 1, 640_000, 78_000_000, 700_000, 1_000_000, 0))
    p.write_text(f"# head\n{legacy}\n{live}\n", encoding="utf-8")

    rows = claude_worker.bin15_accrue.read_entries(p)
    assert [r.origin for r in rows] == [
        claude_worker.bin15_accrue.ORIGIN_PAPER,
        claude_worker.bin15_accrue.ORIGIN_VENUE,
    ]
    assert [r.accounting for r in rows] == ["PAPER", "VENUE"]

    # And a round trip through write/read is stable at the new width.
    out = tmp_path / "again.tsv"
    claude_worker.bin15_accrue.write_entries(out, rows)
    assert claude_worker.bin15_accrue.read_entries(out) == rows


def test_one_instance_can_carry_both_accountings_without_either_winning() -> None:
    """THE reason the identity is (outcome, origin) and not outcome.

    The same market gets a PAPER entry — what the model would have done,
    from a replay of that day — and a VENUE entry, what the account
    actually did. Two different facts. Keyed on the outcome alone,
    whichever merged second would silently replace the other, and the
    store would then hold one number built out of two honest halves."""
    paper = _e(outcome=7, origin=claude_worker.bin15_accrue.ORIGIN_PAPER)
    venue = _e(outcome=7, origin=claude_worker.bin15_accrue.ORIGIN_VENUE)

    merged, added = claude_worker.bin15_accrue.merge_entries([paper], [venue])
    assert added == 1, "a different accounting is a new row, not a duplicate"
    assert len(merged) == 2
    assert {e.origin for e in merged} == {0, 1}

    # Idempotent within one accounting, exactly as before.
    again, added2 = claude_worker.bin15_accrue.merge_entries(merged, [venue])
    assert added2 == 0
    assert len(again) == 2


def test_a_mixed_set_is_refused_rather_than_averaged() -> None:
    """§6.4's law, enforced in the function rather than asked of the
    caller. Every figure `render` produces is a sum over what it was
    handed; a modelled fill averaged with a real one produces a number
    that looks exactly like the ones that are true."""
    mixed = [
        _e(outcome=7, origin=claude_worker.bin15_accrue.ORIGIN_PAPER),
        _e(outcome=8, origin=claude_worker.bin15_accrue.ORIGIN_VENUE),
    ]
    with pytest.raises(ValueError, match="MIXED"):
        claude_worker.bin15_accrue.render(mixed, 0)

    # Each half on its own renders fine.
    for _, group in claude_worker.bin15_accrue.by_origin(mixed):
        assert claude_worker.bin15_accrue.render(group, 0)


def test_by_origin_puts_the_account_first_and_omits_what_has_none() -> None:
    """VENUE leads because it is what actually happened. An accounting
    with no entries is ABSENT rather than a row of zeroes — "no live
    entries yet" and "live entries that netted nothing" are different
    answers and must not render the same."""
    paper_only = [_e(outcome=7, origin=claude_worker.bin15_accrue.ORIGIN_PAPER)]
    assert [o for o, _ in claude_worker.bin15_accrue.by_origin(paper_only)] == [
        claude_worker.bin15_accrue.ORIGIN_PAPER
    ]
    assert claude_worker.bin15_accrue.by_origin([]) == []

    both = paper_only + [_e(outcome=8, origin=claude_worker.bin15_accrue.ORIGIN_VENUE)]
    assert [o for o, _ in claude_worker.bin15_accrue.by_origin(both)] == [
        claude_worker.bin15_accrue.ORIGIN_VENUE,
        claude_worker.bin15_accrue.ORIGIN_PAPER,
    ]


def test_the_sidecar_block_is_read_and_a_null_payout_is_unknown() -> None:
    text = json.dumps({"bin15_entries": [
        {"ts_ns": 1, "outcome": 7, "family": 0, "start_ns": 0, "expiry_ns": 900_000_000_000,
         "offset_s": 12, "is_yes": 1, "px_1e6": 640_000, "qty_1e6": 78_000_000,
         "p_hat_1e6": 700_000, "origin": 1, "y": None},
    ]})
    rows = claude_worker.bin15_accrue.entries_from_sidecar(text)
    assert len(rows) == 1
    assert rows[0].y == claude_worker.bin15_accrue.Y_UNKNOWN
    assert not rows[0].settled
    assert rows[0].origin == claude_worker.bin15_accrue.ORIGIN_PAPER
    # A sidecar from any other member carries no block at all.
    assert claude_worker.bin15_accrue.entries_from_sidecar("{}") == []


def test_a_sidecar_without_an_origin_is_refused_not_defaulted() -> None:
    """The harness stamps `origin` (`Bin15EntryRow::origin`). A sidecar
    without it came from a binary predating §6.4 — and defaulting it
    here would make the FIRST live day inherit "paper" in silence,
    which is precisely the failure the split exists to prevent. The
    answer is to rebuild the harness, not to guess."""
    text = json.dumps({"bin15_entries": [
        {"ts_ns": 1, "outcome": 7, "family": 0, "start_ns": 0,
         "expiry_ns": 900_000_000_000, "offset_s": 12, "is_yes": 1,
         "px_1e6": 640_000, "qty_1e6": 78_000_000, "p_hat_1e6": 700_000,
         "y": None},
    ]})
    with pytest.raises(ValueError, match="predates"):
        claude_worker.bin15_accrue.entries_from_sidecar(text)


def test_the_wilson_interval_is_honest_at_the_sample_sizes_this_lane_has() -> None:
    """22/32 is the 2026-09-13 replay. Its interval has to be wide
    enough to contain the break-even it was measured against, or the
    report would be claiming a result the tape cannot support."""
    lo, hi = claude_worker.bin15_accrue.wilson(22, 32)
    assert 0.50 < lo < 0.53 and 0.81 < hi < 0.83, (lo, hi)
    assert lo <= 0.6788 <= hi, "break-even must sit inside — nothing is settled yet"
    # And it stays inside [0, 1] where a normal interval would not.
    assert claude_worker.bin15_accrue.wilson(0, 3)[0] == 0.0
    assert claude_worker.bin15_accrue.wilson(3, 3)[1] == 1.0
    assert claude_worker.bin15_accrue.wilson(0, 0) == (0.0, 1.0)


def test_the_sample_size_the_report_demands_scales_as_one_over_edge_squared() -> None:
    p = 0.6875
    n5 = claude_worker.bin15_accrue.needed_n(p, 0.05)
    n10 = claude_worker.bin15_accrue.needed_n(p, 0.10)
    assert n5 == 344 and n10 == 86, (n5, n10)
    assert abs(n5 / n10 - 4) < 0.05, "halving the edge quadruples the sample"


def test_the_verdict_is_dollars_and_it_can_disagree_with_the_hit_rate() -> None:
    """The trap this report exists to avoid. Two entries: one wins at a
    cheap price, one loses at a dear one. The hit rate is 50 % against a
    mean price of 0.50 — apparently break-even — while the account is
    down, because the stake sizes differ."""
    rows = [
        _e(1, px=200_000, qty=250_000_000, y=1_000_000, is_yes=1),   # win, $50 staked
        _e(2, px=800_000, qty=62_000_000, y=0, is_yes=1, ts=2_000),  # loss, $49.60
    ]
    text = "\n".join(claude_worker.bin15_accrue.render(rows, fee_bps=0))
    assert "1/2 = 50.0 %" in text
    assert "break-even hit rate 50.0 %" in text
    assert "AS TRADED" in text
    # cost 50.00 + 49.60 = 99.60; payout 250 contracts = $250 -> +$150.40
    assert "+$150.40" in text, text
    assert "INSIDE (nothing is settled yet)" in text


def test_an_empty_store_reports_nothing_to_score_rather_than_zero() -> None:
    text = "\n".join(claude_worker.bin15_accrue.render([], fee_bps=5))
    assert "no settled entry yet" in text
