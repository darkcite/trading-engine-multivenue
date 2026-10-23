# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""BIN15 H90 -- the late-window entry's stores and its report (vault doc 33
§L3, §3): the sidecar blocks read on the venue's clock only, the stores
round-trip and merge per law, and the bars decide KILL / PASS / ACCRUING."""

import json
import math
import pathlib
import typing

import pytest

import claude_worker.bin15_accrue
import claude_worker.bin15_late

L = claude_worker.bin15_late
PAPER = 1
DAY_NS = 86_400_000_000_000
T0 = 1_791_849_600_000_000_000  # 2026-10-13T00:00:00Z
LAW_BLOCK = {
    "enabled": 1,
    "tau_min_ns": 2_000_000_000,
    "tau_max_ns": 30_000_000_000,
    "p_min_1e6": 900_000,
    "e_1e6": 20_000,
    "persist_polls": 2,
    "usd_1e6": 100_000_000,
    "sigma_k_1e3": 1_550,
}
LAW = "tau2-30s_p900000_e20000_n2_k1550"


def sidecar(venue: bool = True, law: dict | None = None, **blocks) -> str:
    obj = {"stale": {"runs": [{"wall": "venue" if venue else "anchor"}]}}
    obj["bin15_late_law"] = LAW_BLOCK if law is None else law
    obj.update(blocks)
    return json.dumps(obj)


def entry_row(outcome: int = 9001, y: int | None = 1_000_000, px: int = 950_000, **kw) -> dict:
    r = {
        "ts_ns": T0 + 880_000_000_000,
        "family": 0,
        "outcome": outcome,
        "expiry_ns": T0 + 900_000_000_000,
        "tau_ms": 20_000,
        "is_yes": 1,
        "px_1e6": px,
        "qty_1e6": 100_000_000,
        "p_late_1e6": 990_000,
        "sigma_1e9": 120_000,
        "ctl_p_1e6": None,
        "origin": PAPER,
        "y": y,
        "y_next_strike": None,
        "settle_px_1e6": None,
    }
    r.update(kw)
    return r


def entry(ts_ns: int, won: bool, **kw: typing.Any) -> L.LateEntry:
    """A Yes entry of 10 contracts at ``px``, settled by ``won`` unless ``y`` says."""
    tau_ms = kw.get("tau_ms", 20_000)
    y = kw.get("y")
    return L.LateEntry(
        ts_ns=ts_ns, outcome=kw.get("outcome", 1), family=0,
        expiry_ns=ts_ns + tau_ms * 1_000_000, tau_ms=tau_ms, is_yes=1,
        px_1e6=kw.get("px", 950_000), qty_1e6=10_000_000, p_late_1e6=990_000, sigma_1e9=100_000,
        ctl_p_1e6=-1, y=(1_000_000 if won else 0) if y is None else y, y_next_strike=-1,
        origin=PAPER, law=kw.get("law", LAW),
    )


def test_the_law_is_one_key_and_absent_or_off_is_none() -> None:
    assert L.law_of(LAW_BLOCK) == LAW
    assert L.law_of(None) is None
    assert L.law_of({**LAW_BLOCK, "enabled": 0}) is None


def test_the_fills_keys_extend_the_law_only_when_stated() -> None:
    zero = {**LAW_BLOCK, "refire_fresh": 0, "limit_bound": 0, "max_quote_age_ns": 0}
    assert L.law_of(zero) == LAW, "stated at 0 is the law as it was"
    full = {**LAW_BLOCK, "refire_fresh": 1, "limit_bound": 1, "max_quote_age_ns": 1_500_000_000}
    assert L.law_of(full) == LAW + "_rf_age1500ms_bound"


def test_the_blocks_are_read_and_nulls_are_unknown() -> None:
    fires = [
        {"family": 0, "outcome": 9001, "expiry_ns": T0, "ff_ts_ns": T0 - 25_000_000_000,
         "ff_is_yes": 1, "ff_px_1e6": 940_000, "ctl_ts_ns": None, "ctl_is_yes": None,
         "ctl_px_1e6": None, "entered": 1, "y": 1_000_000, "y_next_strike": 1_000_000},
        # Neither test held: not a row.
        {"family": 0, "outcome": 9002, "expiry_ns": T0, "ff_ts_ns": None, "ff_is_yes": None,
         "ff_px_1e6": None, "ctl_ts_ns": None, "ctl_is_yes": None, "ctl_px_1e6": None,
         "entered": 0, "y": None, "y_next_strike": None},
    ]
    e, f, dropped = L.late_from_sidecar(
        sidecar(bin15_late_entries=[entry_row(y=None)], bin15_late_fires=fires)
    )
    assert dropped == 0
    assert len(e) == 1 and e[0].law == LAW and e[0].y == L.Y_UNKNOWN and e[0].ctl_p_1e6 == -1
    assert len(f) == 1 and f[0].view("ff") == (T0 - 25_000_000_000, 1, 940_000)
    assert f[0].view("ctl") == (0, -1, 0)


def test_a_run_off_the_venue_clock_is_dropped_and_counted_and_no_law_is_nothing() -> None:
    e, f, dropped = L.late_from_sidecar(sidecar(venue=False, bin15_late_entries=[entry_row()]))
    assert (e, f, dropped) == ([], [], 1)
    assert L.late_from_sidecar(json.dumps({"bin15_late_entries": [entry_row()]})) == ([], [], 0)


def test_an_entry_without_an_origin_is_refused_not_defaulted() -> None:
    r = entry_row()
    del r["origin"]
    with pytest.raises(ValueError, match="origin"):
        L.late_from_sidecar(sidecar(bin15_late_entries=[r]))


def test_the_stores_round_trip(tmp_path: pathlib.Path) -> None:
    e, f, _ = L.late_from_sidecar(
        sidecar(
            bin15_late_entries=[entry_row()],
            bin15_late_fires=[{"family": 1, "outcome": 7, "expiry_ns": T0, "ff_ts_ns": T0 - 1,
                               "ff_is_yes": 0, "ff_px_1e6": 930_000, "entered": 0}],
        )
    )
    pe, pf = tmp_path / "le.tsv", tmp_path / "lf.tsv"
    L.write_late_entries(pe, e)
    L.write_late_fires(pf, f)
    assert L.read_late_entries(pe) == e
    assert L.read_late_fires(pf) == f
    pe.write_text(pe.read_text() + "1\t2\n")
    with pytest.raises(ValueError, match="15 columns"):
        L.read_late_entries(pe)


def test_a_settled_row_upgrades_within_its_law_and_another_law_never_replaces_it() -> None:
    unsettled = entry(T0, True, y=L.Y_UNKNOWN)
    settled = entry(T0, True)
    merged, added = L.merge_late_entries([unsettled], [settled])
    assert (added, merged) == (0, [settled])
    merged, added = L.merge_late_entries([settled], [unsettled])
    assert merged == [settled], "a re-cut never un-settles"
    other = entry(T0, False, law="tau8-30s_p900000_e20000_n2_k1550")
    merged, _ = L.merge_late_entries([unsettled], [other])
    assert merged == [unsettled], "a day re-accrued under another law keeps the stored row"


def test_the_fire_store_keeps_each_earliest_and_takes_a_label_it_lacked() -> None:
    a = L.LateFire(5, 0, T0, T0 - 20, 1, 950_000, 0, -1, 0, 0, L.Y_UNKNOWN, L.Y_UNKNOWN, LAW)
    b = L.LateFire(5, 0, T0, T0 - 25, 1, 940_000, T0 - 10, 1, 960_000, 1, 1_000_000, 1_000_000, LAW)
    merged, added = L.merge_late_fires([a], [b])
    m = merged[0]
    assert added == 0
    assert m.view("ff") == (T0 - 25, 1, 940_000), "the earlier P1b fire"
    assert m.view("ctl") == (T0 - 10, 1, 960_000)
    assert (m.entered, m.y, m.y_next_strike) == (1, 1_000_000, 1_000_000)
    c = b._replace(law="other", ff_ts_ns=T0 - 99)
    merged, _ = L.merge_late_fires([a], [c])
    assert merged[0].view("ff") == a.view("ff") and merged[0].y == 1_000_000


def test_the_bars_kill_pass_and_accrue() -> None:
    # 100 settled at 0.95 winning 90 %: EV/$ = 0.9 * 0.9986 / 0.95 - 1 < 0 -> KILL.
    kill = [entry(T0 + i * 60_000_000_000, i % 10 != 0) for i in range(100)]
    assert L.verdict(kill).startswith("KILL")
    # 200 settled at 0.90 winning 97 % across four days -> PASS.
    pas = [
        entry(T0 + (i % 4) * DAY_NS + i * 1_000_000_000, i % 34 != 0, px=900_000, outcome=i)
        for i in range(200)
    ]
    m = L.money(pas)
    assert m.n == 200 and m.hit >= 0.95 and m.ev_net > 0
    t, days = L.day_t(pas)
    assert days == 4 and t >= 2.0
    assert L.verdict(pas).startswith("PASS"), L.verdict(pas)
    # The same edge on 50 entries is still accruing.
    assert L.verdict(pas[:50]).startswith("ACCRUING")


def test_day_t_needs_two_days() -> None:
    one = [entry(T0 + i, True) for i in range(5)]
    t, days = L.day_t(one)
    assert days == 1 and math.isnan(t)


def test_the_report_splits_and_never_mixes_two_laws() -> None:
    rows = [
        entry(T0 + i * 1_000_000_000, True, px=980_000 if i % 2 else 930_000,
              tau_ms=5_000 + 2_000 * i, outcome=i)
        for i in range(10)
    ]
    rows += [entry(T0, False, outcome=99, law="tau8-30s_p900000_e20000_n2_k1550")]
    fires = [L.LateFire(i, 0, T0, T0 - 1, 1, 940_000, T0 - 1, 1, 950_000, 1, 1_000_000, -1, LAW)
             for i in range(3)]
    out = "\n".join(L.render_late(rows, fires))
    assert out.count("== LATE law") == 2, out
    assert "ask >= 0.97" in out and "ask <  0.97" in out
    assert "tau [0, 8) s" in out and "family 0" in out
    assert "P1b at persistence 1" in out and "the production p-hat" in out
    assert L.render_late([], []) == ["late law: no late entry or fire accrued yet"]


def test_the_report_lane_prints_the_late_verdict_from_its_stores(tmp_path, capsys) -> None:
    le, lf = tmp_path / "le.tsv", tmp_path / "lf.tsv"
    L.write_late_entries(le, [entry(T0, True)])
    rc = claude_worker.bin15_accrue.main([
        "report", "--entries", str(tmp_path / "e.tsv"), "--first-fires", str(tmp_path / "f.tsv"),
        "--late-entries", str(le), "--late-fires", str(lf),
    ])
    out = capsys.readouterr().out
    assert rc == 0
    assert "VERDICT (doc 33 §3): ACCRUING" in out
    rc = claude_worker.bin15_accrue.main([
        "status", "--entries", str(tmp_path / "e.tsv"), "--first-fires", str(tmp_path / "f.tsv"),
        "--late-entries", str(le), "--late-fires", str(lf),
    ])
    assert "LATE (H90): entries=1 settled=1" in capsys.readouterr().out


def test_the_accrual_writes_no_late_store_without_a_row(tmp_path) -> None:
    le, lf = tmp_path / "le.tsv", tmp_path / "lf.tsv"
    said: list[str] = []
    claude_worker.bin15_accrue._merge_late((le, lf), [], [], 0, said.append)
    assert not le.exists() and not lf.exists() and said == []
    claude_worker.bin15_accrue._merge_late((le, lf), [entry(T0, True)], [], 2, said.append)
    assert L.read_late_entries(le) == [entry(T0, True)]
    assert any("2 late row(s) dropped" in s for s in said)


def test_relabel_fills_an_unknown_late_label(tmp_path) -> None:
    class Lab:
        def __init__(self, y: int, nxt: int) -> None:
            self.y, self.y_next_strike = y, nxt

    rows = [entry(T0, True, outcome=4, y=L.Y_UNKNOWN), entry(T0, True, outcome=5)]
    out, filled = L.fill_labels(rows, {4: Lab(0, 0), 5: Lab(0, 0)})
    assert filled == 1
    assert out[0].y == 0 and out[1].y == 1_000_000, "a known label is never touched"
