# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""pm_paper_pnl (the PMJ P&L lane, J9) -- slots 9 and 11's paper book since start.

What is pinned: a fill is named from its client order id and the run's
manifest, and every naming source must agree (sym, descriptor, order id,
instance, the window's pair); it is priced exactly as its member prices it
(integer micro-dollars, the paper fee 0.07 / 0.14 · p(1-p)) -- including two
vectors the engine itself produced; only PAPER fills are booked; it settles
on the capture's settled roll row first and on Gamma's resolved market only
when the capture never saw the window resolve, Gamma's reads going to those
windows before the re-checks; a disagreement is counted, never absorbed; a
window still trading or unlabelled stays open and outlives its run
directory; an unreadable capture is reported, never fatal; a pass is
idempotent and serialized; the summary never sums across fill-law segments;
a root that runs neither member is refused before anything is written; the
dashboard serves the summary and its two blocks read it; the launchd job
names no worker.

Convention: full ``import x`` only. No ``from x import y``.
"""

import fcntl
import json
import os
import pathlib
import shutil
import typing

import httpx
import pytest

import claude_worker.dashboard
import claude_worker.pm_paper_pnl
import claude_worker.pmlr
import tests.craft

M = claude_worker.pm_paper_pnl
REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
_S = 1_000_000_000
_EPOCH = 1_792_368_000 * _S  # a run before J8
_START = {s[0]: s[1] for s in M.SEGMENTS}
_EPOCH_J8 = _START["j8"] + 60 * _S
_FROM = _EPOCH - 3_600 * _S
#: SOL 15m (family 2): window 1 991 521 rides pair 1. BTC 5m (family 0):
#: window 5 974 564 rides pair 1 (w % 3).
_OPEN15 = 1_792_368_900
_OPEN5 = 1_792_369_200
_END15 = (_OPEN15 + 900) * _S
_END5 = (_OPEN5 + 300) * _S
_SOL15 = {(0, "up"): 4106, (0, "down"): 4107, (1, "up"): 4108, (1, "down"): 4109}
_BTC5 = {(0, "up"): 4160, (0, "down"): 4161, (1, "up"): 4162, (1, "down"): 4163}
_HYPE5_UP2 = 4160 + 4 * 7 + 4
_MANIFEST = "".join(
    [f"{s}\tpolymarket-rolling:updown:sol:15m[{leg}{pair}]\n" for (pair, leg), s in _SOL15.items()]
    + [f"{s}\tpolymarket-rolling:updown:btc:5m[{leg}{pair}]\n" for (pair, leg), s in _BTC5.items()]
    + [f"{_HYPE5_UP2}\tpolymarket-rolling:updown:hype:5m[up2]\n", "7\tbinance:BTCUSDT\n",
       "4110\tpolymarket-rolling:updown:sol:15m[mark]\n"]
)
_SOL15_SLUG = f"sol-updown-15m-{_OPEN15}"
_BTC5_SLUG = f"btc-updown-5m-{_OPEN5}"

Ev = tuple[int, int, int, int, int, int, int, int]
Fl = tuple[int, int, int, int, int, int]


def _inst15(open_s: int, fam: int) -> int:
    return (open_s // 900) * 8 + fam


def _inst5(open_s: int, fam: int) -> int:
    return M.PM5_INSTANCE_FLAG | ((open_s // 300) * 8 + fam)


def _oid(inst: int, fam: int, leg: int, seq: int = 1) -> int:
    return (seq << 49) | (leg << 48) | (fam << 32) | inst


def _settled(sym: int, inst: int, v0: int, kind: int = 1) -> Ev:
    seq = inst | (60 << 32) | (kind << 56)
    return (10 * _S, sym, 0, claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL, seq, 1, v0, 0)


_I15 = _inst15(_OPEN15, 2)
_I5 = _inst5(_OPEN5, 0)
#: (ts, sym, strategy_id, px, qty, order id): slot 9 buys SOL 15m Up, slot 11 BTC 5m Down.
_FILLS: list[Fl] = [
    (5 * _S, _SOL15[1, "up"], 9, 970_000, 10_000_000, _oid(_I15, 2, 0)),
    (6 * _S, _BTC5[1, "down"], 11, 940_000, 5_210_000, _oid(_I5, 0, 1)),
]


def _root(tmp: pathlib.Path, strategy: str = "ai+bin15+pm15+pmj") -> pathlib.Path:
    root = tmp / "multivenue"
    (root / "logs").mkdir(parents=True)
    (root / "strategy.conf").write_text(f"# test\nSTRATEGY={strategy}\n", encoding="utf-8")
    return root


def _run(
    root: pathlib.Path,
    epoch: int = _EPOCH,
    fills: list[Fl] | None = None,
    ev15: list[Ev] | None = None,
    ev5: list[Ev] | None = None,
    origin: int = claude_worker.pmlr.FILL_ORIGIN_PAPER,
) -> pathlib.Path:
    run = root / "logs" / f"run-{epoch}"
    run.mkdir(parents=True)
    (run / "instrument-manifest.tsv").write_text(_MANIFEST, encoding="utf-8")
    rows = _FILLS if fills is None else fills
    tests.craft.write_fills(
        run / "engine-fills.pmlr",
        [(ts, sym, 0, sid, origin, 0, px, qty, oid) for ts, sym, sid, px, qty, oid in rows]
        # a bin15 fill on the same tape is not ours
        + [(7 * _S, 7, 0, 3, claude_worker.pmlr.FILL_ORIGIN_PAPER, 0, 500_000, 1_000_000, 9)],
        epoch,
    )
    if ev15 is None:
        ev15 = [_settled(_SOL15[1, "up"], _I15, M.SETTLED_UP)]
    if ev5 is None:
        ev5 = [_settled(_BTC5[1, "up"], _I5, M.SETTLED_UP)]
    tests.craft.write_events(run / "pm15-events.pmlr", ev15, epoch)
    tests.craft.write_events(run / "pm5-events.pmlr", ev5, epoch)
    return run


def _gamma(results: dict[str, str]) -> tuple[M.Fetch, list[str]]:
    calls: list[str] = []

    def fetch(slug: str) -> object:
        calls.append(slug)
        got = results.get(slug)
        if got is None:
            return []
        prices = '["1", "0"]' if got == "up" else '["0", "1"]'
        return [{"slug": slug, "closed": True, "umaResolutionStatus": "resolved",
                 "outcomes": '["Up", "Down"]', "outcomePrices": prices}]

    return fetch, calls


def _down(slug: str) -> object:
    raise httpx.ConnectError("no route")


_AGREE = {_SOL15_SLUG: "up", _BTC5_SLUG: "up"}


def _summary(root: pathlib.Path) -> dict[str, typing.Any]:
    out: dict[str, typing.Any] = json.loads(
        (root / "worker" / M.SUMMARY_FILE).read_text(encoding="utf-8")
    )
    return out


def _now(after_end_s: int = 120) -> int:
    return _END15 + after_end_s * _S


def _fillrec(sym: int, sid: int, px: int, qty: int, oid: int) -> claude_worker.pmlr.FillRec:
    return claude_worker.pmlr.FillRec(
        ts_ns=1, sym=sym, side=0, strategy_id=sid, origin=claude_worker.pmlr.FILL_ORIGIN_PAPER,
        flags=0, px=px, qty=qty, order_id=oid,
    )


# ---- the law -------------------------------------------------------------------------


def test_price_is_the_members_own_arithmetic() -> None:
    pm15, pmj = M.SLOTS[9], M.SLOTS[11]
    assert M.price(pm15, 970_000, 10_000_000, True) == (9_700_000, 10_000_000, 20_370, 279_630)
    assert M.price(pmj, 940_000, 5_210_000, False) == (4_897_400, 0, 41_138, -4_938_538)
    # the J8 sweep's size-weighted take, truncated as the member's i128 divisions truncate
    assert M.price(pm15, 825_121, 239_760_000, False) == (197_831_010, 0, 2_421_754, -200_252_764)
    assert (pm15.fee_rate_1e6, pmj.fee_rate_1e6) == (70_000, 140_000)


def test_price_reproduces_the_engines_own_booking() -> None:
    """A vector the engine's member booked itself (a winning slot 11 leg at a
    swept, size-weighted price); its provenance is in the session record,
    outside git."""
    cost, payout, fee, pnl = M.price(M.SLOTS[11], 928_836, 257_730_000, True)
    assert (cost, payout, fee, pnl) == (239_388_902, 257_730_000, 2_385_022, 15_956_076)


def test_names_two_fills_the_engine_wrote() -> None:
    """Order ids, instances and syms from a real capture (the 00:11Z run):
    the members' layout, not this module's formula, produced them."""
    manifest = {
        4189: "polymarket-rolling:updown:hype:5m[down0]",
        4164: "polymarket-rolling:updown:btc:5m[up2]",
    }
    hype = M.name_fill("run-1792368684525363000", 888,
                       _fillrec(4189, 11, 950_000, 5_000_000, 844_444_305_281_428), manifest)
    assert isinstance(hype, M.Fill)
    assert (hype.instance, hype.leg, hype.asset) == (2_195_280_276, "down", "hype")
    assert (hype.window_end_ns, hype.slug) == (1_792_373_700 * _S, "hype-updown-5m-1792373400")
    btc = M.name_fill("run-1792368684525363000", 1232,
                      _fillrec(4164, 11, 950_000, 5_000_000, 1_125_902_102_122_960), manifest)
    assert isinstance(btc, M.Fill)
    assert (btc.instance, btc.leg, btc.slug) == (2_195_280_336, "up", "btc-updown-5m-1792375800")


def test_descriptors_and_segments() -> None:
    assert M.parse_descriptor("polymarket-rolling:updown:btc:15m[up0]") == ("btc", "15m", "up", 0)
    assert M.parse_descriptor("polymarket-rolling:updown:hype:5m[down2]") == (
        "hype", "5m", "down", 2)
    for bad in ("polymarket-rolling:updown:btc:15m[mark]", "binance:BTCUSDT", "",
                "polymarket-rolling:updown:btc:15m[up7]", "polymarket-rolling:updown::5m[up0]"):
        assert M.parse_descriptor(bad) is None, bad
    for sid in (9, 11):
        assert M.segment_of(_EPOCH, sid) == "touch"
        assert M.segment_of(_START["j8"], sid) == "j8"
        assert M.segment_of(_START["j8"] - 1, sid) == "touch"
    # J11 moves slot 11 only (R-PMJ-S47 / S55): slot 9 stays on j8
    assert M.segment_of(_START["j11"], 11) == "j11"
    assert M.segment_of(_START["j11"] - 1, 11) == "j8"
    assert M.segment_of(_START["j11"], 9) == "j8"
    # J12 moves slot 11 only until slot 9's own flip (R-PMJ-S61 / S63)
    assert M.segment_of(_START["j12"], 11) == "j12"
    assert M.segment_of(_START["j12"] - 1, 11) == "j11"
    assert M.segment_of(_START["j12"], 9) == "j8"
    assert _START["j12"] == 1_792_771_200 * _S  # 2026-10-23 16:00Z
    assert [s[0] for s in M.SEGMENTS] == ["touch", "j8", "j11", "j12"]


def test_every_naming_source_must_agree() -> None:
    m = {int(k): v for k, v in (line.split("\t") for line in _MANIFEST.strip().splitlines())}
    ok = _fillrec(_SOL15[1, "up"], 9, 970_000, 10_000_000, _oid(_I15, 2, 0))
    assert isinstance(M.name_fill("run-1", 0, ok, m), M.Fill)
    bad = {
        "the oid's family bits": _oid(_I15, 3, 0),
        "the instance's family": _oid(_inst15(_OPEN15, 3), 2, 0),
        "the leg bit": _oid(_I15, 2, 1),
        "the window's pair": _oid(_inst15(_OPEN15 + 900, 2), 2, 0),
        "a 5m instance": _oid(_inst5(_OPEN5, 2), 2, 0),
    }
    for why, oid in bad.items():
        got = M.name_fill("run-1", 0, ok._replace(order_id=oid), m)
        assert isinstance(got, str), why
    for sym in (4110, 9_999, _BTC5[1, "up"]):  # the mark, not in the manifest, the other pool
        assert isinstance(M.name_fill("run-1", 0, ok._replace(sym=sym), m), str), sym
    for px, qty in ((1_000_000, 10_000_000), (0, 10_000_000), (970_000, 0)):
        assert isinstance(M.name_fill("run-1", 0, ok._replace(px=px, qty=qty), m), str)
    assert isinstance(M.name_fill("run-1", 0, ok._replace(side=1), m), str)
    # the descriptor names another leg than the sym's ordinal does
    assert isinstance(M.name_fill("run-1", 0, ok, {**m, ok.sym: m[_SOL15[1, "down"]]}), str)
    # a 5-minute leg on pair 2 of family 4
    w = 1_792_369_500
    inst = _inst5(w, 4)
    hype = _fillrec(_HYPE5_UP2, 11, 960_000, 5_000_000, _oid(inst, 4, 0))
    got = M.name_fill("run-1", 0, hype, m)
    assert isinstance(got, M.Fill) and got.slug == f"hype-updown-5m-{w}"


def test_parse_gamma_takes_only_a_resolved_market() -> None:
    ok = {"slug": "s", "closed": True, "umaResolutionStatus": "resolved",
          "outcomes": '["Up", "Down"]', "outcomePrices": '["0", "1"]'}
    assert M.parse_gamma("s", [ok]) == "down"
    assert M.parse_gamma("s", [dict(ok, outcomePrices=["1", "0"], outcomes=["Up", "Down"])]) == "up"
    assert M.parse_gamma("t", [ok]) is None
    assert M.parse_gamma("s", [dict(ok, closed=False)]) is None
    assert M.parse_gamma("s", [dict(ok, umaResolutionStatus="proposed")]) is None
    assert M.parse_gamma("s", [dict(ok, outcomePrices='["0.5", "0.5"]')]) is None
    assert M.parse_gamma("s", [dict(ok, outcomes='["Yes", "No"]')]) is None
    assert M.parse_gamma("s", []) is None
    assert M.parse_gamma("s", {"error": "x"}) is None


# ---- a pass --------------------------------------------------------------------------


def test_a_pass_settles_on_the_capture_and_is_idempotent(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    fetch, calls = _gamma(_AGREE)
    res = M.run_pass(root, _FROM, _now(), fetch)
    assert (res.settled_new, res.ledger_rows, res.open, res.unmapped) == (2, 2, 0, 0)
    assert res.net_usd_1e6 == {9: {"touch": 279_630}, 11: {"touch": -4_938_538}}
    assert sorted(calls) == sorted([_SOL15_SLUG, _BTC5_SLUG])  # the one re-check each
    s = _summary(root)
    pm15, pmj = s["slots"]["9"], s["slots"]["11"]
    touch = pm15["segments"][0]
    assert (touch["segment"], touch["settled"], touch["won"], touch["lost"]) == ("touch", 1, 1, 0)
    assert (touch["stake_usd"], touch["fees_usd"], touch["net_usd"]) == (9.7, 0.02037, 0.27963)
    assert pm15["recent"][0] == {
        "window_end_utc": M.utc_iso(_END15), "family": "SOL 15m", "leg": "up", "px": 0.97,
        "qty": 10.0, "won": True, "label": "capture", "fee_usd": 0.02037, "pnl_usd": 0.27963,
        "segment": "touch",
    }
    assert pmj["segments"][0]["net_usd"] == -4.938538 and pmj["recent"][0]["won"] is False
    assert pm15["counts"]["label_capture"] == 1 and pm15["counts"]["gamma_disagree"] == 0
    assert s["unreadable"] == [] and pm15["counts"]["venue_fills"] == 0
    ledger = (root / "worker" / M.LEDGER_FILE).read_text(encoding="utf-8")
    assert ledger.splitlines()[0].split("\t") == list(M.LEDGER_COLUMNS)
    assert len(ledger.splitlines()) == 3
    again = M.run_pass(root, _FROM, _now(600), fetch)
    assert (again.settled_new, again.ledger_rows) == (0, 2)
    assert (root / "worker" / M.LEDGER_FILE).read_text(encoding="utf-8") == ledger
    assert len(calls) == 2, "a settled fill is never read again"


def test_a_venue_fill_is_counted_never_booked(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root, origin=claude_worker.pmlr.FILL_ORIGIN_VENUE)
    res = M.run_pass(root, _FROM, _now(), None)
    assert (res.settled_new, res.open, res.unmapped) == (0, 0, 0)
    s = _summary(root)
    counts = (s["slots"]["9"]["counts"], s["slots"]["11"]["counts"])
    assert (counts[0]["venue_fills"], counts[1]["venue_fills"]) == (1, 1)
    assert (root / "worker" / M.OPEN_FILE).read_text(encoding="utf-8").splitlines() == [
        "\t".join(M.OPEN_COLUMNS)
    ]


def test_an_unsettled_kind_byte_is_no_label(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    # kind 3 has the settled bit set but is no kind the packer writes
    _run(root, ev15=[_settled(_SOL15[1, "up"], _I15, M.SETTLED_UP, kind=3)])
    res = M.run_pass(root, _FROM, _now(), None)
    assert (res.settled_new, res.open) == (1, 1)
    assert _summary(root)["slots"]["9"]["open"][0]["status"] == "awaiting label"


def test_a_window_still_trading_stays_open(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    res = M.run_pass(root, _FROM, _END5 - 30 * _S, None)
    assert (res.settled_new, res.open) == (0, 2)
    s = _summary(root)
    assert [o["status"] for o in s["slots"]["9"]["open"]] == ["open"]
    assert s["slots"]["11"]["segments"][-1]["open"] == 0, "the open fill is in its own segment"
    assert s["slots"]["11"]["segments"][0]["open_cost_usd"] == 4.8974


def test_a_window_the_capture_never_saw_resolve_settles_on_gamma(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    # slot 9's window was retired unresolved (v0 0); slot 11's never reached the capture.
    _run(root, ev15=[_settled(_SOL15[1, "up"], _I15, 0)], ev5=[])
    res = M.run_pass(root, _FROM, _now(), None)
    assert (res.settled_new, res.open) == (0, 2)
    assert _summary(root)["slots"]["9"]["counts"]["awaiting"] == 1
    late = M.run_pass(root, _FROM, _END15 + 2 * 3_600 * _S, None)
    assert late.open == 2 and _summary(root)["slots"]["9"]["counts"]["unlabelled"] == 1
    fetch, _calls = _gamma({_SOL15_SLUG: "down", _BTC5_SLUG: "down"})
    res = M.run_pass(root, _FROM, _END15 + 2 * 3_600 * _S, fetch)
    assert (res.settled_new, res.open) == (2, 0)
    s = _summary(root)
    assert s["slots"]["9"]["recent"][0]["label"] == "gamma"
    assert s["slots"]["9"]["recent"][0]["won"] is False  # bought Up, the venue said Down
    assert s["slots"]["11"]["recent"][0]["won"] is True
    assert s["slots"]["11"]["counts"]["label_gamma"] == 1


def test_an_open_fill_outlives_its_run_directory(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    run = _run(root, ev15=[], ev5=[])
    M.run_pass(root, _FROM, _now(), None)
    carried = (root / "worker" / M.OPEN_FILE).read_text(encoding="utf-8").splitlines()
    assert len(carried) == 3 and carried[0].split("\t") == list(M.OPEN_COLUMNS)
    shutil.rmtree(run)  # retention archived it
    fetch, _calls = _gamma(_AGREE)
    res = M.run_pass(root, _FROM, _now(), fetch)
    assert (res.settled_new, res.ledger_rows, res.open) == (2, 2, 0)
    assert {r.fill.run for r in M.read_ledger(root / "worker" / M.LEDGER_FILE).values()} == {
        run.name
    }
    assert (root / "worker" / M.OPEN_FILE).read_text(encoding="utf-8").splitlines() == [
        "\t".join(M.OPEN_COLUMNS)
    ]


def test_carried_fills_before_the_start_are_dropped(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    run = _run(root, ev15=[], ev5=[])
    M.run_pass(root, _FROM, _now(), None)
    shutil.rmtree(run)
    fetch, calls = _gamma(_AGREE)
    res = M.run_pass(root, _EPOCH + 1, _now(), fetch)
    assert (res.settled_new, res.open, calls) == (0, 0, [])


def test_a_disagreement_is_counted_never_absorbed(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    fetch, _calls = _gamma({_SOL15_SLUG: "down", _BTC5_SLUG: "up"})
    M.run_pass(root, _FROM, _now(), fetch)
    s = _summary(root)
    assert s["slots"]["9"]["recent"][0]["won"] is True, "the capture's label stands"
    assert s["slots"]["9"]["counts"]["gamma_disagree"] == 1
    assert s["slots"]["11"]["counts"]["gamma_disagree"] == 0


def test_a_failed_gamma_read_is_retried_at_the_next_pass(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root, ev15=[], ev5=[])
    res = M.run_pass(root, _FROM, _now(), _down)
    assert (res.settled_new, res.gamma_errors, res.open) == (0, 2, 2)
    fetch, _calls = _gamma(_AGREE)
    assert M.run_pass(root, _FROM, _now(), fetch).settled_new == 2


def test_gamma_stops_after_failures_in_a_row_and_labels_before_rechecks(
    tmp_path: pathlib.Path,
) -> None:
    g = M.Gamma(_down)
    for k in range(6):
        assert g.outcome(f"s{k}") is None
    assert (g.lookups, g.errors) == (M.GAMMA_MAX_ERRORS, M.GAMMA_MAX_ERRORS)
    # One read left. BTC's window has its capture label and comes first; SOL's
    # never reached the capture: SOL gets the read, BTC's re-check goes without.
    (tmp_path / "run-1").mkdir()
    tests.craft.write_events(
        tmp_path / "run-1" / "pm5-events.pmlr",
        [_settled(_BTC5[1, "up"], _I5, M.SETTLED_UP)], _EPOCH,
    )
    fetch, calls = _gamma(_AGREE)
    btc = M.Fill("run-1", 1, 2, 11, _BTC5[1, "down"], "btc", "5m", "down", 0, 940_000, 5_210_000,
                 _oid(_I5, 0, 1), _I5, _END5, _BTC5_SLUG, "touch")
    sol = M.Fill("run-1", 0, 1, 9, _SOL15[1, "up"], "sol", "15m", "up", 0, 970_000, 10_000_000,
                 _oid(_I15, 2, 0), _I15, _END15, _SOL15_SLUG, "touch")
    rows, opens = M.settle([btc, sol], tmp_path, M.Gamma(fetch, budget=1), _now(), M.Seen())
    assert calls == [_SOL15_SLUG] and opens == []
    by = {r.fill.asset: r for r in rows}
    assert (by["sol"].label_src, by["btc"].label_src, by["btc"].gamma) == ("gamma", "capture", "-")


def test_an_unreadable_capture_is_reported_not_fatal(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    bad = _run(root)
    (bad / "engine-fills.pmlr").write_bytes(b"")  # an ENOSPC-wedged writer
    good = _run(root, epoch=_EPOCH + 60 * _S)
    (good / "pm15-events.pmlr").write_bytes(b"PMLR")  # torn before its header
    fetch, _calls = _gamma(_AGREE)
    res = M.run_pass(root, _FROM, _now(), fetch)
    assert (res.settled_new, res.unreadable) == (2, 2)
    s = _summary(root)
    assert {(u["run"], u["file"]) for u in s["unreadable"]} == {
        (bad.name, "engine-fills"), (good.name, "pm15-events")
    }
    assert s["slots"]["9"]["recent"][0]["label"] == "gamma"  # its events file was unreadable


def test_segments_are_never_summed(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    o15 = o5 = 1_792_400_400  # 15m pair 0, 5m pair 0
    i15, i5 = _inst15(o15, 2), _inst5(o5, 0)
    _run(
        root, epoch=_EPOCH_J8,
        fills=[(5 * _S, _SOL15[0, "up"], 9, 950_000, 20_000_000, _oid(i15, 2, 0)),
               (6 * _S, _BTC5[0, "up"], 11, 960_000, 10_000_000, _oid(i5, 0, 0))],
        ev15=[_settled(_SOL15[0, "up"], i15, M.SETTLED_DOWN)],
        ev5=[_settled(_BTC5[0, "up"], i5, M.SETTLED_UP)],
    )
    res = M.run_pass(root, _FROM, (o15 + 1_000) * _S, None)
    assert res.settled_new == 4
    slot = _summary(root)["slots"]["9"]
    segs = slot["segments"]
    assert [g["segment"] for g in segs] == ["touch", "j8"]
    assert (segs[0]["net_usd"], segs[1]["settled"], segs[1]["lost"]) == (0.27963, 1, 1)
    assert segs[1]["from_utc"] == M.utc_iso(_START["j8"])
    assert not {"net_usd", "settled", "stake_usd"} & set(slot), "no slot-wide total"
    assert set(res.net_usd_1e6[9]) == {"touch", "j8"}


def test_fills_below_the_floor_are_booked_apart(tmp_path: pathlib.Path) -> None:
    """R-PMJ-S62: a segment books its fills under the 0.90 entry floor apart."""
    root = _root(tmp_path)
    o15 = 1_792_400_400  # 15m pair 0
    i15 = _inst15(o15, 2)
    _run(
        root,
        epoch=_EPOCH_J8,
        fills=[
            (5 * _S, _SOL15[0, "up"], 9, 950_000, 20_000_000, _oid(i15, 2, 0)),
            (6 * _S, _SOL15[0, "down"], 9, 580_000, 10_000_000, _oid(i15, 2, 1)),
        ],
        ev15=[_settled(_SOL15[0, "up"], i15, M.SETTLED_UP)],
        ev5=[],
    )
    res = M.run_pass(root, _FROM, (o15 + 1_000) * _S, None)
    assert res.settled_new == 2
    (j8,) = [g for g in _summary(root)["slots"]["9"]["segments"] if g["segment"] == "j8"]
    above, below = j8["at_or_above"], j8["below_floor"]
    assert (above["settled"], above["won"], above["lost"]) == (1, 1, 0)
    assert (below["settled"], below["won"], below["lost"]) == (1, 0, 1)
    assert below["stake_usd"] == 5.8 and below["net_usd"] < -5.8
    assert round(above["net_usd"] + below["net_usd"], 6) == j8["net_usd"]
    assert M.PRICE_FLOOR_1E6 == 900_000


def test_the_current_segment_shows_even_when_empty(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    M.run_pass(root, _FROM, _now(), None)
    segs = _summary(root)["slots"]["9"]["segments"]
    assert [g["segment"] for g in segs] == ["touch", "j8"] and segs[1]["settled"] == 0
    segs = _summary(root)["slots"]["11"]["segments"]
    assert [g["segment"] for g in segs] == ["touch", "j12"] and segs[1]["settled"] == 0


def test_j11_splits_slot_11_only(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    o15 = o5 = 1_792_427_400  # 15m pair 0, 5m pair 0, after J11's first boot
    i15, i5 = _inst15(o15, 2), _inst5(o5, 0)
    _run(
        root, epoch=_START["j11"] + 300 * _S,
        fills=[(5 * _S, _SOL15[0, "up"], 9, 950_000, 20_000_000, _oid(i15, 2, 0)),
               (6 * _S, _BTC5[0, "up"], 11, 960_000, 10_000_000, _oid(i5, 0, 0))],
        ev15=[_settled(_SOL15[0, "up"], i15, M.SETTLED_UP)],
        ev5=[_settled(_BTC5[0, "up"], i5, M.SETTLED_UP)],
    )
    res = M.run_pass(root, _FROM, (o15 + 1_000) * _S, None)
    assert res.settled_new == 2
    assert set(res.net_usd_1e6[9]) == {"j8"} and set(res.net_usd_1e6[11]) == {"j11"}
    slots = _summary(root)["slots"]
    assert [g["segment"] for g in slots["9"]["segments"]] == ["j8"]
    s11 = slots["11"]["segments"]
    # j12, the current segment of slot 11, shows even while empty
    assert [g["segment"] for g in s11] == ["j11", "j12"] and s11[0]["settled"] == 1
    assert s11[0]["from_utc"] == M.utc_iso(_START["j11"]) and s11[1]["settled"] == 0


def test_j12_splits_slot_11_only(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    o15 = o5 = 1_792_773_000  # 15m pair 0, 5m pair 0, after J12's first boot
    i15, i5 = _inst15(o15, 2), _inst5(o5, 0)
    _run(
        root, epoch=_START["j12"] + 300 * _S,
        fills=[(5 * _S, _SOL15[0, "up"], 9, 950_000, 20_000_000, _oid(i15, 2, 0)),
               (6 * _S, _BTC5[0, "up"], 11, 960_000, 10_000_000, _oid(i5, 0, 0))],
        ev15=[_settled(_SOL15[0, "up"], i15, M.SETTLED_UP)],
        ev5=[_settled(_BTC5[0, "up"], i5, M.SETTLED_UP)],
    )
    res = M.run_pass(root, _FROM, (o15 + 1_000) * _S, None)
    assert res.settled_new == 2
    assert set(res.net_usd_1e6[9]) == {"j8"} and set(res.net_usd_1e6[11]) == {"j12"}
    slots = _summary(root)["slots"]
    assert [g["segment"] for g in slots["9"]["segments"]] == ["j8"]
    s11 = slots["11"]["segments"]
    assert [g["segment"] for g in s11] == ["j12"] and s11[0]["settled"] == 1
    assert s11[0]["from_utc"] == M.utc_iso(_START["j12"])


def test_a_fill_that_cannot_be_named_is_reported_not_booked(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root, fills=[
        (5 * _S, 9_999, 9, 970_000, 10_000_000, _oid(_I15, 2, 0)),             # not in the manifest
        (5 * _S, _SOL15[1, "up"], 9, 970_000, 10_000_000, _oid(_I15, 2, 1)),   # the other leg's id
        (5 * _S, _SOL15[1, "up"], 11, 970_000, 10_000_000, _oid(_I15, 2, 0)),  # 15m leg on slot 11
        (5 * _S, _BTC5[1, "up"], 11, 970_000, 10_000_000, _oid(_I15, 0, 0)),   # 15m id, 5m leg
    ])
    res = M.run_pass(root, _FROM, _now(), None)
    assert (res.settled_new, res.unmapped) == (0, 4)
    s = _summary(root)
    assert (s["slots"]["9"]["counts"]["unmapped"], s["slots"]["11"]["counts"]["unmapped"]) == (2, 2)
    assert not (root / "worker" / M.LEDGER_FILE).exists()


def test_runs_before_the_start_are_not_counted(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    res = M.run_pass(root, _EPOCH + 1, _now(), None)
    assert (res.settled_new, res.ledger_rows, res.open) == (0, 0, 0)


def test_an_overlapping_pass_skips(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    lock = root / "worker" / M.LOCK_FILE
    lock.parent.mkdir(parents=True)
    with lock.open("a+") as fh:
        fcntl.flock(fh.fileno(), fcntl.LOCK_EX)
        res = M.run_pass(root, _FROM, _now(), None)
    assert (res.settled_new, res.ledger_rows) == (0, 0)
    assert not (root / "worker" / M.SUMMARY_FILE).exists()


def test_a_root_without_either_member_is_refused(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path, strategy="bin15")
    _run(root)
    with pytest.raises(M.RootRefused):
        M.run_pass(root, _FROM, _now(), None)
    assert not (root / "worker").exists()
    assert M.main(["--root", str(root), "--no-gamma"]) == 78


def test_a_malformed_or_doubled_ledger_is_refused_not_skipped(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    _run(root)
    M.run_pass(root, _FROM, _now(), None)
    path = root / "worker" / M.LEDGER_FILE
    lines = path.read_text(encoding="utf-8").splitlines()
    path.write_text("\n".join([*lines, lines[1]]) + "\n", encoding="utf-8")
    with pytest.raises(ValueError, match="twice"):
        M.run_pass(root, _FROM, _now(), None)
    path.write_text(lines[0] + "\nshort\tline\n", encoding="utf-8")
    with pytest.raises(ValueError, match="fields"):
        M.run_pass(root, _FROM, _now(), None)


def test_the_dashboard_serves_the_summary_and_both_blocks_read_it(tmp_path: pathlib.Path) -> None:
    root = _root(tmp_path)
    assert claude_worker.dashboard.pm_pnl_section(root) is None
    _run(root)
    M.run_pass(root, _FROM, _now(), None)
    got = claude_worker.dashboard.pm_pnl_section(root)
    assert got is not None and got["source"] == M.SOURCE
    html = (
        pathlib.Path(claude_worker.dashboard.__file__).parent / "dashboard" / "dashboard.html"
    ).read_text(encoding="utf-8")
    assert "function pmLedger(sid)" in html and "W.pm_pnl" in html
    # each slot's book shows with the engine's counters, and without them (engine down)
    assert html.count("pmLedger('9')") == 2 and html.count("pmLedger('11')") == 2
    # the strategies table and the P&L panel take slots 9 / 11 from the ledger too:
    # audit-pnl's day report settles only HIP-4 rolls and marks these binaries out
    assert "cells ? cells[0] : p ? n(p.fills, 0)" in html
    assert "Polymarket paper (slots 9 / 11)" in html and "not settled by audit-pnl" in html
    # audit-pnl's fee ladder is [@0, @1, @2 bps] (FEE_LADDER_BPS): never labelled "@tier" (J11)
    assert "'@tier'" not in html and "net @0/1/tier" not in html
    assert "'@2 bps'" in html and "net @0/1/2 bps" in html
    # R-PMJ-S62: the fills under the 0.90 floor shown apart, in both views
    assert html.count("pmFloorCell(g)") == 3 and "fill ≥0.90 · <0.90" in html


def test_the_launchd_job_is_wired_and_names_no_worker() -> None:
    plist = (REPO_ROOT / "launchd" / "com.multivenue.pm-pnl.plist").read_text(encoding="utf-8")
    assert "<string>com.multivenue.pm-pnl</string>" in plist
    for minute in (6, 21, 36, 51):
        assert f"<key>Minute</key><integer>{minute}</integer>" in plist
    assert "@REPO@/scripts/pm-pnl.sh" in plist and "<key>KeepAlive</key>" not in plist
    installer = (REPO_ROOT / "scripts" / "install-launchd.sh").read_text(encoding="utf-8")
    assert installer.count("com.multivenue.pm-pnl") == 2  # both label lists
    path = REPO_ROOT / "scripts" / "pm-pnl.sh"
    text = path.read_text(encoding="utf-8")
    execs = [line.split() for line in text.splitlines() if line.startswith("exec ")]
    assert execs == [["exec", '"$ALIAS/bin/python3"', '"$REPO/scripts/pm-pnl.py"', "--root",
                      '"$HOME/multivenue"']]
    assert not any("claude_worker" in t or "claude-worker" in t for t in execs[0])  # CMDLINE LAW
    assert ".env" not in text.split("set -u", 1)[1]  # no secret is sourced
    assert os.access(path, os.X_OK)
