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
import pathlib
import struct

import pytest

import claude_worker.bin15_accrue
import claude_worker.bin15_ledger
import claude_worker.pmlr


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
    with pytest.raises(ValueError, match="want 12 or 14 columns"):
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


# --- BIN15 S4: the venue's label, the stores corrected in place ---------


def test_a_venue_entry_round_trips_fourteen_wide_beside_an_engine_one(tmp_path) -> None:
    p = tmp_path / "entries.tsv"
    rows = [
        _e(7)._replace(y=0, y_engine=1_000_000, y_next_strike=0, venue=True),
        _e(9, ts=2_000),
    ]
    claude_worker.bin15_accrue.write_entries(p, rows)
    widths = [
        len(line.split("\t"))
        for line in p.read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
    assert widths == [14, 12]
    assert claude_worker.bin15_accrue.read_entries(p) == rows


def test_a_venue_entry_beats_an_engine_one_settled_or_not() -> None:
    engine = _e(7, y=1_000_000)
    venue_unknown = _e(7, y=claude_worker.bin15_accrue.Y_UNKNOWN)._replace(venue=True)
    merged, added = claude_worker.bin15_accrue.merge_entries([engine], [venue_unknown])
    assert added == 0 and merged == [venue_unknown], "the law is corrected"
    merged, _ = claude_worker.bin15_accrue.merge_entries([venue_unknown], [engine])
    assert merged == [venue_unknown], "and never reverted to the engine's label"


def test_the_report_puts_the_venues_label_first_and_never_mixes_the_two() -> None:
    relabelled = _e(7, y=0)._replace(y_engine=1_000_000, venue=True)
    accrued_venue = _e(8, ts=2_000, y=1_000_000)._replace(venue=True)
    engine_only = _e(9, ts=3_000, y=1_000_000)
    rows = [relabelled, accrued_venue, engine_only]
    venue, missing = claude_worker.bin15_accrue.on_label(rows, "venue")
    assert [e.outcome for e in venue] == [7, 8] and missing == 1
    engine, missing = claude_worker.bin15_accrue.on_label(rows, "engine")
    assert [(e.outcome, e.y) for e in engine] == [(7, 1_000_000), (9, 1_000_000)]
    assert missing == 1, "an entry accrued on the venue law has no engine label"
    text = "\n".join(claude_worker.bin15_accrue.render(venue, fee_bps=0))
    assert "EV per $ staked (p/a - 1, stake-weighted)" in text


# A HIP-4 capture, by hand: a `part-<epoch>` pull with its anchor, a v3
# Hyperliquid tick file whose venue clock runs 25 s AHEAD of the anchor
# law (the pre-S2 harness's error), the BTC marks and two rolls.
_S = 1_000_000_000
_EPOCH = 1_790_000_000 * _S
_ANCHOR = 5_000 * _S
_LAG = 25 * _S
_OFF = _EPOCH - _ANCHOR + _LAG
_K = 77_000_000_000
_PERP = 3
_YES = 4_096


def _venue(mono: int) -> int:
    return mono + _OFF


def _capture(root: pathlib.Path) -> pathlib.Path:
    run = root / f"part-{_EPOCH}"
    run.mkdir(parents=True)
    (run / "anchor.json").write_text(
        json.dumps({"run": f"run-{_EPOCH}", "epoch_ns": _EPOCH, "anchor_ns": _ANCHOR}),
        encoding="utf-8",
    )
    (run / "instrument-manifest.tsv").write_text(
        f"{_PERP}\thyperliquid:BTC\n{_YES}\thyperliquid:out:BTC:15m[yes]\n"
        f"{_YES + 1}\thyperliquid:out:BTC:15m[no]\n",
        encoding="utf-8",
    )
    head = claude_worker.pmlr._HEADER.pack(
        claude_worker.pmlr.MAGIC, 3, claude_worker.pmlr.SLOT_KIND_TICK, _EPOCH
    ).ljust(claude_worker.pmlr.HEADER_SIZE, b"\x00")
    blob = bytearray(head)
    for i in range(1_500):
        ts = _ANCHOR + i * _S
        slot = claude_worker.pmlr._TICK.pack(
            ts, _PERP, i + 1, 1, 1, 2, 1, 4, 0, _venue(ts) // 1_000_000
        )
        blob.extend(slot.ljust(claude_worker.pmlr.SLOT_SIZE, b"\x00"))
    (run / "hl-ticks.pmlr").write_bytes(bytes(blob))
    expiry = _venue(_ANCHOR + 900 * _S)
    events: list[tuple[int, int, int, int, int, int]] = []
    # The mark sits $100 OVER the strike through the minute BEFORE the
    # expiry and $100 UNDER it for the minute after: the venue pays Yes on
    # TWAP[T-60, T]; the engine's old minute [T, T+60] would have said No.
    for k in range(700):
        mono = _ANCHOR + k * 2 * _S
        px = _K + 100_000_000 if mono <= _ANCHOR + 900 * _S else _K - 100_000_000
        events.append((mono, _PERP, 2, 0, px, 0))
    # A zero mark inside the window is not a price: the harness drops it,
    # and so must the tape (it would drag the average under the strike).
    events.append((_ANCHOR + 871 * _S, _PERP, 2, 0, 0, 0))
    seq = 100 | (60 << 32)
    events.append((_ANCHOR + 10 * _S, _YES, 13, seq, _K, expiry))
    # The successor, created 9 s after the expiry at the venue's price.
    seq2 = 101 | (60 << 32)
    events.append((_ANCHOR + 909 * _S, _YES, 13, seq2, _K + 100_000_000, expiry + 900 * _S))
    events.sort()
    ev = bytearray(
        struct.pack("<4sHBxQ", claude_worker.pmlr.MAGIC, 3,
                    claude_worker.pmlr.SLOT_KIND_CHANNEL_EVENT, _EPOCH)
        .ljust(claude_worker.pmlr.HEADER_SIZE, b"\x00")
    )
    for ts, sym, channel, sq, v0, v1 in events:
        ev.extend(struct.pack("<QIBB2xQQqq16x", ts, sym, 4, channel, sq, 0, v0, v1))
    (run / "hl-events.pmlr").write_bytes(bytes(ev))
    return run


def test_relabel_corrects_both_stores_in_place_and_a_rerun_converts_nothing(tmp_path) -> None:
    pull = tmp_path / "pull"
    _capture(pull)
    expiry = _venue(_ANCHOR + 900 * _S)
    old = lambda mono: _EPOCH + (mono - _ANCHOR)  # noqa: E731 — the pre-S2 anchor law
    ledger = tmp_path / "ledger.tsv"
    claude_worker.bin15_ledger.write_ledger(
        ledger,
        [
            _row_l(old(_ANCHOR + 100 * _S), 100, y=0),
            _row_l(old(_ANCHOR + 130 * _S), 100, y=0),
            _row_l(_EPOCH - 3_600 * _S, 99, y=1_000_000),  # before any capture
        ],
    )
    entries = tmp_path / "entries.tsv"
    claude_worker.bin15_accrue.write_entries(
        entries,
        [
            claude_worker.bin15_accrue.Entry(
                ts_ns=old(_ANCHOR + 50 * _S), outcome=100, family=0,
                start_ns=expiry - claude_worker.bin15_accrue.TAU_15M_NS, expiry_ns=expiry,
                offset_s=25, is_yes=1, px_1e6=600_000, qty_1e6=10_000_000,
                p_hat_1e6=700_000, y=0, origin=claude_worker.bin15_accrue.ORIGIN_PAPER,
            )
        ],
    )
    lines: list[str] = []
    dirs = claude_worker.bin15_accrue.tape_dirs([str(pull)], [])
    cl, ce = claude_worker.bin15_accrue.relabel(ledger, entries, dirs, lines.append)
    assert (cl.converted, cl.uncovered, ce.converted) == (2, 1, 1), lines
    rows = claude_worker.bin15_ledger.read_ledger(ledger)
    moved = [r for r in rows if r.venue]
    assert [r.ts_ns for r in moved] == [
        _venue(_ANCHOR + 100 * _S), _venue(_ANCHOR + 130 * _S)
    ], "onto the venue clock: +25 s"
    assert all((r.y, r.y_engine, r.y_next_strike) == (1_000_000, 0, 1_000_000) for r in moved)
    assert next(r for r in rows if not r.venue).y == 1_000_000, "uncovered: kept, untouched"
    e = claude_worker.bin15_accrue.read_entries(entries)[0]
    assert e.venue and (e.y, e.y_engine, e.y_next_strike) == (1_000_000, 0, 1_000_000)
    assert e.ts_ns == _venue(_ANCHOR + 50 * _S) and e.offset_s == 50
    assert any("by expiry day" in line and "1/1" in line for line in lines), lines
    backups = sorted(p.name for p in tmp_path.glob("*.bak-relabel-*"))
    assert len(backups) == 2, "each changed store is backed up first"
    # Idempotent: +0 / +0, and nothing rewritten.
    before = (ledger.read_bytes(), entries.read_bytes())
    cl2, ce2 = claude_worker.bin15_accrue.relabel(ledger, entries, dirs, lines.append)
    assert (cl2.converted, ce2.converted, cl2.filled, ce2.filled) == (0, 0, 0, 0)
    assert (ledger.read_bytes(), entries.read_bytes()) == before
    assert any("ran early by 25.0" in line for line in lines), lines


def test_relabel_refuses_to_replace_a_store_that_changed_under_it(tmp_path, monkeypatch) -> None:
    """An accrual that lands while the relabel reads the captures would be
    lost by a whole-file replace; the lane checks the store again first."""
    pull = tmp_path / "pull"
    _capture(pull)
    ledger = tmp_path / "ledger.tsv"
    claude_worker.bin15_ledger.write_ledger(ledger, [_row_l(_EPOCH + 100 * _S, 100, y=0)])
    entries = tmp_path / "entries.tsv"
    real = claude_worker.bin15_accrue.relabel_ledger

    def accrual_lands_meanwhile(rows, tapes, labels):
        with ledger.open("a", encoding="utf-8") as f:
            f.write(_row_l(_EPOCH + 200 * _S, 100, y=0).tsv())
        return real(rows, tapes, labels)

    monkeypatch.setattr(claude_worker.bin15_accrue, "relabel_ledger", accrual_lands_meanwhile)
    dirs = claude_worker.bin15_accrue.tape_dirs([str(pull)], [])
    with pytest.raises(RuntimeError, match="changed while the relabel"):
        claude_worker.bin15_accrue.relabel(ledger, entries, dirs, lambda s: None)
    assert not list(tmp_path.glob("*.bak-relabel-*")), "nothing written, not even a backup"


def _row_l(ts: int, outcome: int, y: int):
    return claude_worker.bin15_ledger.Row(
        ts_ns=ts, family=0, outcome=outcome, tau_ns=300_000_000_000,
        p_hat_1e6=600_000, p_raw_1e6=600_000, arm=0, y=y, entered=1, mid_1e6=-1,
    )


# --- BIN15 S5: the counterfactual first fires (ruling O-4) + S5b ----------

_VENUE: dict = {"runs": [{"epoch_ns": 1, "lanes": {}, "wall": "venue"}]}

#: A persistence law for the tests -- not a research setting.
_LAW: dict = {"persist_polls": 3, "elapsed_max_ns": 240 * _S}


def _ff(outcome: int, ts: int = 1_000, px: int = 600_000, y: int = 1_000_000,
        is_yes: int = 1, entered: int = 0, law: tuple[int, int] = (3, 240 * _S),
        ctl: tuple[int, int, int] | None = None, fired: bool = True):
    """A first-fire row. ``ctl``: the control's ``(ts, px, is_yes)`` --
    ``None``, none recorded; ``fired=False``: the artifact's test never held."""
    expiry = ts + 800_000_000_000
    start = expiry - claude_worker.bin15_accrue.TAU_15M_NS
    c = {} if ctl is None else {
        "ctl_ts_ns": ctl[0], "ctl_offset_s": max(ctl[0] - start, 0) // 1_000_000_000,
        "ctl_is_yes": ctl[2], "ctl_px_1e6": ctl[1],
    }
    f = claude_worker.bin15_accrue.FirstFire(
        ts_ns=ts, outcome=outcome, family=0, start_ns=start, expiry_ns=expiry,
        offset_s=(ts - start) // 1_000_000_000, is_yes=is_yes, px_1e6=px, y=y,
        y_next_strike=y, entered=entered, persist_polls=law[0], elapsed_max_ns=law[1], **c,
    )
    return f if fired else f._replace(ts_ns=0, offset_s=0, is_yes=-1, px_1e6=0)


def test_first_fires_come_from_the_entry_columns_and_the_block() -> None:
    """Every instance either test held on, once: an entered one from its
    entry row's `first_fire_*` / `ctl_fire_*` columns, the rest from the
    `bin15_first_fires` block (a null test is one that never held), all
    stamped with the sidecar's entry law. A pre-S5 sidecar yields nothing, a
    pre-S5b one rows without a control; one off the venue clock yields
    nothing and counts what it dropped."""
    entry = {
        "ts_ns": 130 * _S, "outcome": 7, "family": 0, "start_ns": 0,
        "expiry_ns": 900 * _S, "offset_s": 130, "is_yes": 1, "px_1e6": 650_000,
        "qty_1e6": 76_000_000, "p_hat_1e6": 700_000, "origin": 1, "y": 1_000_000,
        "y_next_strike": 1_000_000, "settle_px_1e6": 77_000_000_000,
        "first_fire_ts_ns": 95 * _S, "first_fire_px_1e6": 640_000, "first_fire_is_yes": 1,
        "ctl_fire_ts_ns": 60 * _S, "ctl_fire_px_1e6": 660_000, "ctl_fire_is_yes": 1,
    }
    declined = {
        "ts_ns": 40 * _S, "family": 1, "outcome": 9, "start_ns": 0, "expiry_ns": 900 * _S,
        "offset_s": 40, "is_yes": 0, "px_1e6": 410_000, "ctl_ts_ns": 30 * _S,
        "ctl_offset_s": 30, "ctl_is_yes": 0, "ctl_px_1e6": 430_000, "y": None,
        "y_next_strike": -1, "settle_px_1e6": None,
    }
    control_only = {
        "ts_ns": None, "family": 2, "outcome": 10, "start_ns": 0, "expiry_ns": 900 * _S,
        "offset_s": None, "is_yes": None, "px_1e6": None, "ctl_ts_ns": 70 * _S,
        "ctl_offset_s": 70, "ctl_is_yes": 1, "ctl_px_1e6": 450_000, "y": 1_000_000,
        "y_next_strike": 1_000_000, "settle_px_1e6": 77_000_000_000,
    }
    blocks = {
        "bin15_entries": [entry],
        "bin15_first_fires": [declined, control_only],
        "bin15_entry_law": dict(_LAW, control_e_entry_1e6=20_000),
    }
    fires, dropped = claude_worker.bin15_accrue.first_fires_from_sidecar(
        json.dumps({"stale": _VENUE, **blocks})
    )
    assert dropped == 0
    assert [(f.outcome, f.ts_ns, f.px_1e6, f.is_yes, f.offset_s, f.entered) for f in fires] == [
        (7, 95 * _S, 640_000, 1, 95, 1),
        (9, 40 * _S, 410_000, 0, 40, 0),
        (10, 0, 0, -1, 0, 0),
    ]
    assert [(f.ctl_ts_ns, f.ctl_px_1e6, f.ctl_is_yes, f.ctl_offset_s) for f in fires] == [
        (60 * _S, 660_000, 1, 60),
        (30 * _S, 430_000, 0, 30),
        (70 * _S, 450_000, 1, 70),
    ]
    assert {f.law for f in fires} == {(3, 240 * _S)}
    assert fires[0].won and not fires[1].settled
    assert fires[2].ctl_fired and not fires[2].fired and fires[2].control().won
    lost = dict(
        entry, first_fire_ts_ns=None, first_fire_px_1e6=None, first_fire_is_yes=None,
        ctl_fire_ts_ns=None, ctl_fire_px_1e6=None, ctl_fire_is_yes=None,
    )
    assert claude_worker.bin15_accrue.first_fires_from_sidecar(
        json.dumps({"stale": _VENUE, "bin15_entries": [lost]})
    ) == ([], 0), "a lost first fire is not guessed"
    pre_s5 = {k: v for k, v in entry.items() if not k.startswith(("first_fire_", "ctl_fire_"))}
    assert claude_worker.bin15_accrue.first_fires_from_sidecar(
        json.dumps({"stale": _VENUE, "bin15_entries": [pre_s5]})
    ) == ([], 0)
    pre_s5b = {k: v for k, v in entry.items() if not k.startswith("ctl_fire_")}
    (old,), _ = claude_worker.bin15_accrue.first_fires_from_sidecar(
        json.dumps({"stale": _VENUE, "bin15_entries": [pre_s5b]})
    )
    assert old.fired and not old.ctl_fired and old.ctl_is_yes == -1
    anchor = {"runs": [{"epoch_ns": 1, "lanes": {}, "wall": "anchor"}]}
    assert claude_worker.bin15_accrue.first_fires_from_sidecar(
        json.dumps({"stale": anchor, **blocks})
    ) == ([], 3)


def test_the_first_fire_store_keeps_the_earliest_fire_and_never_mixes_two_laws(
    tmp_path,
) -> None:
    """A window cut replays a straddling instance twice: the EARLIEST of
    each counterfactual is the law's -- each on its own -- the label comes
    from whichever row has it, `entered` from either. A row accrued under
    another entry law only lends the label. A second merge changes nothing;
    the file round-trips, and a pre-S5b row (13 columns) reads with no
    control."""
    unknown = claude_worker.bin15_accrue.Y_UNKNOWN
    early = _ff(7, ts=10 * _S, px=600_000, y=unknown)
    late = _ff(7, ts=20 * _S, px=700_000, y=0, entered=1)
    merged, added = claude_worker.bin15_accrue.merge_first_fires([early], [late])
    assert added == 0 and len(merged) == 1
    m = merged[0]
    assert (m.ts_ns, m.px_1e6, m.y, m.y_next_strike, m.entered) == (10 * _S, 600_000, 0, 0, 1)
    again, added = claude_worker.bin15_accrue.merge_first_fires(merged, [late, early])
    assert added == 0 and again == merged
    other_law = _ff(8, ts=5 * _S, y=0, entered=1, law=claude_worker.bin15_accrue.OLD_LAW)
    stored = _ff(8, ts=30 * _S, px=650_000, y=unknown)
    (mix,), _ = claude_worker.bin15_accrue.merge_first_fires([stored], [other_law])
    assert (mix.ts_ns, mix.px_1e6, mix.entered, mix.law) == (30 * _S, 650_000, 0, (3, 240 * _S))
    assert mix.y == 0, "the label is a fact about the instance, whatever the law"
    # S5b: the control can come from one window and the artifact's test
    # from the other; a test that never held is later than any that did.
    a = _ff(30, ts=40 * _S, ctl=(35 * _S, 610_000, 1))
    b = _ff(30, ts=30 * _S, ctl=(50 * _S, 620_000, 0))
    (m2,), _ = claude_worker.bin15_accrue.merge_first_fires([a], [b])
    assert (m2.ts_ns, m2.ctl_ts_ns, m2.ctl_px_1e6, m2.ctl_is_yes) == (
        30 * _S, 35 * _S, 610_000, 1
    )
    only_ctl = _ff(31, ts=30 * _S, fired=False, ctl=(20 * _S, 600_000, 1))
    (m3,), _ = claude_worker.bin15_accrue.merge_first_fires([only_ctl], [_ff(31, ts=45 * _S)])
    assert (m3.ts_ns, m3.ctl_ts_ns) == (45 * _S, 20 * _S)
    p = tmp_path / "first_fires.tsv"
    rows = [*again, _ff(9, ts=30 * _S), m2, m3]
    claude_worker.bin15_accrue.write_first_fires(p, rows)
    assert claude_worker.bin15_accrue.read_first_fires(p) == rows
    legacy = tmp_path / "legacy.tsv"
    legacy.write_text("\t".join(str(v) for v in tuple(_ff(40))[:13]) + "\n", encoding="utf-8")
    assert claude_worker.bin15_accrue.read_first_fires(legacy) == [_ff(40)]
    p.write_text("1\t2\t3\n", encoding="utf-8")
    with pytest.raises(ValueError, match="want 17 columns"):
        claude_worker.bin15_accrue.read_first_fires(p)


def test_the_counterfactuals_are_scored_per_law_on_the_same_instances_and_the_declined() -> None:
    """Doc 27 §5's "over today's law on the SAME instances": each
    counterfactual paired with the entries by outcome, each side at an equal
    dollar, one block per entry law -- the old law itself only counted --
    plus each on the instances the member declined. An unsettled fire is
    counted, never scored; an entry today's law never bought is not a pair;
    the fill bar is named, not scored."""
    unknown = claude_worker.bin15_accrue.Y_UNKNOWN
    entries = [
        _e(7, px=650_000, y=1_000_000)._replace(venue=True),
        _e(8, ts=2_000, px=700_000, y=0)._replace(venue=True),
        _e(11, ts=5_000, px=500_000, y=0)._replace(venue=True),
        _e(12, ts=6_000, px=500_000, y=0)._replace(venue=True),
        _e(13, ts=7_000, px=500_000, y=0)._replace(venue=True),
        _e(15, ts=9_000, px=660_000, y=1_000_000)._replace(venue=True),
    ]
    # 9 is declined: an entry for it (a day re-accrued under another law)
    # is not a pair, and 9 is scored once, as declined.
    entries.append(_e(9, ts=3_000, px=500_000, y=0)._replace(venue=True))
    old = claude_worker.bin15_accrue.OLD_LAW
    fires = [
        _ff(7, px=600_000, y=1_000_000, entered=1, ctl=(500, 620_000, 1)),
        _ff(8, ts=2_000, px=600_000, y=0, is_yes=0, entered=1, ctl=(1_500, 580_000, 0)),
        _ff(9, ts=3_000, px=400_000, y=0, is_yes=1, ctl=(2_500, 420_000, 1)),
        _ff(10, ts=4_000, y=unknown),
        _ff(11, ts=5_000, y=unknown, entered=1, ctl=(4_000, 610_000, 1)),
        _ff(12, ts=6_000, px=500_000, y=0, entered=1, law=old, ctl=(6_000, 500_000, 1)),
        _ff(14, ts=8_000, fired=False, ctl=(8_000, 450_000, 1)),
        _ff(15, ts=9_000, px=640_000, y=1_000_000, entered=1),
    ]
    lines = claude_worker.bin15_accrue.render_counterfactual(entries, fires)
    assert lines == [
        "counterfactuals (the first passing reprice at persist 1, no ceiling -- logged, not "
        "traded): 8 instance(s), 6 settled; 5 the member entered, 3 it declined; the "
        "artifact's test held on 7, today's law (the control) on 6",
        "   1 entr(ies) with no first fire (accrued before S5) -- not paired",
        "   -- entry law persist 1, no ceiling: the old law itself, 1 instance(s) -- its "
        "entries ARE its unpersisted trigger, not a comparison",
        "      today's law (the control): the artifact's own test on all 1 instance(s) (its "
        "bound is today's) -- the same fires, not a second comparison",
        "   -- entry law persist 3, ceiling 240 s: 7 instance(s)",
        "      1 settled entr(ies) whose counterfactual is unsettled (run `relabel`) -- left out",
        "      vs the artifact's unpersisted trigger: the SAME 3 instance(s): hit 66.7 % vs "
        "100.0 % (-33.3 pts); EV per $1 +0.0179 vs +0.6319 (-61.4 pts); mean price 0.6700 "
        "vs 0.6133",
        "      the 1 instance(s) it declined that the artifact's unpersisted trigger would "
        "have bought: hit 0.0 % at a mean 0.4000, EV per $1 -1.0000",
        "      1 settled entr(ies) today's law (the control) never bought (or accrued before "
        "S5b) -- not paired",
        "      1 settled entr(ies) whose counterfactual is unsettled (run `relabel`) -- left out",
        "      vs today's law (the control): the SAME 2 instance(s): hit 50.0 % vs 100.0 % "
        "(-50.0 pts); EV per $1 -0.2308 vs +0.6685 (-89.9 pts); mean price 0.6750 vs 0.6000",
        "      the 2 instance(s) it declined that today's law (the control) would have "
        "bought: hit 50.0 % at a mean 0.4350, EV per $1 +0.1111",
        "   IoC fills (doc 27 §5's fill bar): not measurable in the paper model -- the "
        "harness models every fill; gated at the first live step (ruling 2026-09-24)",
    ]


def test_the_control_is_counted_where_it_is_the_artifacts_own_test_or_was_never_recorded() -> None:
    """BIN15 S5b: where the artifact's bound IS today's, the control fires
    are the artifact's own and are counted, not compared a second time; a
    law whose rows predate S5b recorded no control and says so."""
    old = claude_worker.bin15_accrue.OLD_LAW
    entries = [
        _e(20, px=600_000, y=1_000_000)._replace(venue=True),
        _e(21, ts=2_000, px=650_000, y=1_000_000)._replace(venue=True),
    ]
    fires = [
        _ff(20, entered=1, law=old),
        _ff(21, ts=2_000, entered=1, ctl=(2_000, 600_000, 1)),
    ]
    assert claude_worker.bin15_accrue.render_counterfactual(entries, fires) == [
        "counterfactuals (the first passing reprice at persist 1, no ceiling -- logged, not "
        "traded): 2 instance(s), 2 settled; 2 the member entered, 0 it declined; the "
        "artifact's test held on 2, today's law (the control) on 1",
        "   -- entry law persist 1, no ceiling: the old law itself, 1 instance(s) -- its "
        "entries ARE its unpersisted trigger, not a comparison",
        "      today's law (the control): no fire recorded (accrued before S5b, or it never "
        "held) -- not paired",
        "   -- entry law persist 3, ceiling 240 s: 1 instance(s)",
        "      vs the artifact's unpersisted trigger: the SAME 1 instance(s): hit 100.0 % vs "
        "100.0 % (+0.0 pts); EV per $1 +0.5385 vs +0.6667 (-12.8 pts); mean price 0.6500 "
        "vs 0.6000",
        "      today's law (the control): the artifact's own test on all 1 instance(s) (its "
        "bound is today's) -- the same fires, not a second comparison",
        "   IoC fills (doc 27 §5's fill bar): not measurable in the paper model -- the "
        "harness models every fill; gated at the first live step (ruling 2026-09-24)",
    ]


def test_an_entry_of_an_instance_stored_under_another_law_is_not_merged() -> None:
    """A day re-accrued after the artifact changed: an instance whose first
    fire is stored under another entry law keeps its stored entry; one with
    none stored (or under the same law) merges."""
    other = claude_worker.bin15_accrue.OLD_LAW
    law = (3, 240 * _S)
    stored = [_ff(7, law=other), _ff(8, law=law)]
    incoming = [(_e(7), law), (_e(8), law), (_e(9), law)]
    kept = claude_worker.bin15_accrue.same_law_entries(incoming, stored)
    assert [e.outcome for e in kept] == [8, 9]


def test_relabel_fills_an_unknown_first_fire_label_and_nothing_else(tmp_path) -> None:
    """BIN15 S5: a first fire whose window could not settle it takes the
    captures' LAW E-11 label, exactly as a venue entry does; a known label
    is never touched, and a rerun fills nothing."""
    pull = tmp_path / "pull"
    _capture(pull)
    ledger = tmp_path / "ledger.tsv"
    entries = tmp_path / "entries.tsv"
    fires = tmp_path / "first_fires.tsv"
    unknown = claude_worker.bin15_accrue.Y_UNKNOWN
    claude_worker.bin15_accrue.write_first_fires(
        fires, [_ff(100, ts=_EPOCH, y=unknown), _ff(7, ts=_EPOCH + _S, y=0)]
    )
    lines: list[str] = []
    dirs = claude_worker.bin15_accrue.tape_dirs([str(pull)], [])
    claude_worker.bin15_accrue.relabel(ledger, entries, dirs, lines.append, first_fires=fires)
    got = {f.outcome: (f.y, f.y_next_strike) for f in
           claude_worker.bin15_accrue.read_first_fires(fires)}
    assert got == {100: (1_000_000, 1_000_000), 7: (0, 0)}, lines
    assert any("first fires: rows 2: unknown labels filled 1" in line for line in lines), lines
    assert len(list(tmp_path.glob("first_fires.tsv.bak-relabel-*"))) == 1
    before = fires.read_bytes()
    claude_worker.bin15_accrue.relabel(ledger, entries, dirs, lines.append, first_fires=fires)
    assert fires.read_bytes() == before
