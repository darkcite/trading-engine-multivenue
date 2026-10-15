# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hyparb_g1 — the HYPARB gate-G1 tally (operator 2026-10-15).

The session fold is the member's own P&L law integer for integer (the
``strategy-hyparb`` vectors, ported); a fill, a position or a mark it cannot
honour fails its session; only slot 0's PAPER trades count; gas is per
attempt; funding is the member's truncating 1 s pass; hedges and flattens are
told apart; the ledgers merge idempotently and never hold a live run; the
2-block rung alone carries the judge's latency override; a lean unit keeps
only the Hyperliquid and HyperEVM captures; the nightly replay keeps to its
budget; a failed rung is never summed; and condition 2 must cover condition
1's whole sample on the member's law before the verdict can pass.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import io
import os
import pathlib
import shutil
import sqlite3
import time
import typing
import urllib.request

import pytest

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.hyparb_g1
import claude_worker.objstore
import claude_worker.pmlr
import tests.craft
import tests.fake_s3

G1 = claude_worker.hyparb_g1
E6 = 1_000_000
HYPE = (4 << 24) | 8
ETH = (4 << 24) | 2
POOL_HYPE_ETH = (8 << 24) | 1
POOL_UNMAPPED = (8 << 24) | 2
POOL_HYPE_USD = (8 << 24) | 3
EPOCH = 1_790_000_000 * 10**9
DAY_NS = 86_400 * 10**9
T0 = 5_000 * 10**9
S = 10**9
GAS = 10_000

ARTIFACT = """[hyparb]
mode = "testnet"
gas_p50_usd_1e6 = 10000
perp_taker_bps_1e6 = 4500000
spot_taker_bps_1e6 = 7000000
cap_day_usd_1e6 = 2000000000
max_order_usd_1e6 = 100000000

[[coin]]
name = "HYPE"
perp = "hyperliquid:HYPE"

[[coin]]
name = "ETH"
perp = "hyperliquid:ETH"

[[pool]]
address = "0xCC"
coin0 = "HYPE"
coin1 = "USD"

[[pool]]
address = "0xaa"
coin0 = "HYPE"
coin1 = "ETH"
"""
UNIVERSE = '[hyperevm]\npools = ["0xaa:v3:18:18", "0xbb:v3:18:6", "0xcc:v3:18:6"]\n'
MANIFEST = "42\t123456789\n67108872\thyperliquid:HYPE\n67108866\thyperliquid:ETH\n"

# The member's vector (`the_session_pnl_is_cash_and_marks_less_fees_and_gas`):
# an AMM buy of WHYPE at the pool, the perp 1 % above it, the hedge a lot-
# rounded IoC sale at the perp's bid, the sub-lot residue at the perp mid.
O_PX, O_QTY = 97_612_345, 1_234_567
MID = 98_588_468
H_PX, H_QTY = MID - MID // 20_000, O_QTY - O_QTY % 10_000
#: An activeAssetCtx at rate 0: the perp position has its series, accrues 0.
RATE0 = [(T0, HYPE, 4, 5, 0, 0, 0, 0)]


def _expected_pnl(mid: int) -> int:
    """`expected_pnl` from strategy-hyparb's tests, every operand positive."""
    paid = O_QTY * O_PX // E6
    got = H_QTY * H_PX // E6
    fee = got * 4_500_000 // (10_000 * E6)
    residue = (O_QTY - H_QTY) * mid // E6
    return -paid + got - fee + residue - GAS


def _terms(tmp: pathlib.Path) -> claude_worker.hyparb_g1.Terms:
    (tmp / "hyparb.toml").write_text(ARTIFACT, encoding="utf-8")
    (tmp / "universe.toml").write_text(UNIVERSE, encoding="utf-8")
    return G1.terms(tmp / "hyparb.toml", tmp / "universe.toml")


def _paths(tmp: pathlib.Path) -> claude_worker.hyparb_g1.Paths:
    tmp.mkdir(parents=True, exist_ok=True)
    _terms(tmp)
    return G1.Paths(
        replay_dir=tmp / "logs",
        root=tmp / "g1",
        artifact=tmp / "hyparb.toml",
        universe=tmp / "universe.toml",
        candles=tmp / "absent.db",
        fees=tmp / "absent.toml",
    )


def _touch(ts: int, sym: int, mid: int) -> tuple[int, int, int, int, int]:
    return (ts, sym, mid - mid // 20_000, mid + mid // 20_000, 4)


def _fill(ts, sym, side, px, qty, oid, slot=0, origin=1, flags=0):
    return (ts, sym, side, slot, origin, flags, px, qty, oid)


def _order(ts, sym, side, kind, px, qty, oid, slot=0):
    return (ts, sym, side, kind, px, qty, oid, sym >> 24, slot)


def _age(run: pathlib.Path, seconds: float = 3_600.0) -> None:
    """Closed for good: every log older than the live-quiet bound."""
    then = time.time() - seconds
    for p in run.iterdir():
        os.utime(p, (then, then))


def _vector_run(
    logs: pathlib.Path,
    epoch: int = EPOCH,
    ticks: list | None = None,
    fills: list | None = None,
    orders: list | None = None,
    events: list | None = None,
    hl_ticks: bool = True,
) -> pathlib.Path:
    run = logs / f"run-{epoch}"
    run.mkdir(parents=True)
    rows = [_touch(T0, HYPE, MID)] if ticks is None else ticks
    tests.craft.write_ticks_px(
        run / ("hl-ticks.pmlr" if hl_ticks else "bn-ticks.pmlr"), rows, epoch
    )
    tests.craft.write_fills(
        run / "engine-fills.pmlr",
        [
            _fill(T0 + S, POOL_HYPE_USD, 0, O_PX, O_QTY, 1),
            _fill(T0 + 2 * S, HYPE, 1, H_PX, H_QTY, 2),
        ]
        if fills is None
        else fills,
        epoch,
    )
    tests.craft.write_orders(
        run / "engine-orders.pmlr",
        [
            _order(T0 + S - 5, POOL_HYPE_USD, 0, 2, O_PX, O_QTY, 1),
            _order(T0 + S + 5, HYPE, 1, 1, H_PX, H_QTY, 2),
        ]
        if orders is None
        else orders,
        epoch,
    )
    if events is None or events:
        tests.craft.write_events(run / "hl-events.pmlr", RATE0 if events is None else events, epoch)
    (run / "instrument-manifest.tsv").write_text(MANIFEST, encoding="utf-8")
    _age(run)
    return run


def _candles(
    path: pathlib.Path, bars: list[tuple[int, int]], rates: list[tuple[int, float]]
) -> None:
    """``bars`` = (open wall ms, close ×1e6) of hyperliquid:HYPE 1 m; ``rates``
    = (funding ts wall ms, hourly rate)."""
    conn = sqlite3.connect(path)
    conn.execute(
        "CREATE TABLE candles (venue INTEGER, descriptor TEXT, tf TEXT, open_ts INTEGER, c REAL)"
    )
    conn.execute("CREATE TABLE funding (venue INTEGER, descriptor TEXT, ts_ms INTEGER, rate REAL)")
    for o, c in bars:
        conn.execute("INSERT INTO candles VALUES (4, 'hyperliquid:HYPE', '1m', ?, ?)", (o, c / E6))
    for ts, r in rates:
        conn.execute("INSERT INTO funding VALUES (4, 'hyperliquid:HYPE', ?, ?)", (ts, r))
    conn.commit()
    conn.close()


EPOCH_MS = EPOCH // E6


# ---------------------------------------------------------------------
# The member's law
# ---------------------------------------------------------------------


def test_the_fold_equals_the_members_session_level(tmp_path):
    t = _terms(tmp_path)
    s = G1.fold_session(_vector_run(tmp_path / "logs"), t, None)
    assert s.ok, s.error
    want = _expected_pnl(MID)
    assert want > 0, "a 1 % dislocation round-trips at a profit"
    assert s.pnl_session_usd_1e6 == want
    assert (s.amm_attempts, s.amm_fills, s.hedge_attempts, s.hedge_fills) == (1, 1, 1, 1)
    assert (s.flatten_attempts, s.units) == (0, 1)
    assert s.gas_usd_1e6 == GAS and s.funding_usd_1e6 == 0 and s.mark_source == "tape"
    assert s.taker_fees_usd_1e6 == (H_QTY * H_PX // E6) * 4_500_000 // (10_000 * E6)
    assert s.amm_notional_usd_1e6 == O_QTY * MID // E6, "coin0 at the perp mid at the fill"
    assert s.artifact_sha256 == hashlib.sha256(ARTIFACT.encode()).hexdigest()
    assert s.start_utc == "2026-09-21T14:13:20Z" and s.end_utc == "2026-09-21T14:13:22Z"


def test_the_level_re_marks_at_the_close_both_ways(tmp_path):
    t = _terms(tmp_path)
    low = MID * 8_000 // 10_000
    for i, last in enumerate((low, MID * 10_200 // 10_000)):
        ticks = [_touch(T0, HYPE, MID), _touch(T0 + 3 * S, HYPE, last)]
        s = G1.fold_session(_vector_run(tmp_path / f"l{i}", ticks=ticks), t, None)
        assert s.inventory_usd_1e6 == (O_QTY - H_QTY) * last // E6
        assert s.pnl_session_usd_1e6 == _expected_pnl(last)
    # An unsound touch never overwrites the mark (the member's last_mid).
    ticks = [_touch(T0, HYPE, MID), (T0 + 3 * S, HYPE, 0, 0, 4)]
    s = G1.fold_session(_vector_run(tmp_path / "l3", ticks=ticks), t, None)
    assert s.pnl_session_usd_1e6 == _expected_pnl(MID)


def test_the_mark_is_the_book_sound_most_recently_the_perp_on_a_tie():
    perp = G1.Series([0, 100, 200], [10, 0, 0])
    spot = G1.Series([0, 150, 250], [20, 21, 0])
    marks = G1.Marks("tape", {0: perp}, {0: spot}, {})
    assert G1._mark(marks, 0, 300) == (21, 150), "the spot stayed sound longer"
    marks = G1.Marks("tape", {0: G1.Series([0, 100, 200], [10, 0, 30])}, {0: spot}, {})
    assert G1._mark(marks, 0, 300) == (30, 200), "the perp is sound now"
    both = G1.Marks("tape", {0: G1.Series([0], [10])}, {0: G1.Series([0], [20])}, {})
    assert G1._mark(both, 0, 300) == (10, 0), "both sound: the member's usd_mid takes the perp"
    assert G1._mark(G1.Marks("tape", {0: G1.Series([5], [0])}, {}, {}), 0, 300) is None


def test_an_unmarked_coin_fails_the_session_rather_than_guess(tmp_path):
    t = _terms(tmp_path)
    run = _vector_run(tmp_path / "logs", ticks=[_touch(T0, ETH, 2_000 * E6)])
    s = G1.fold_session(run, t, tmp_path / "absent.db")
    assert not s.ok and "no mark" in s.error
    assert s.pnl_session_usd_1e6 == 0 and s.amm_fills == 0, "an error row carries zeros"


def test_mul_div_truncates_toward_zero_like_the_member():
    assert G1.mul_div(-1_040_132, 79_550_000, E6) == -82_742_500
    assert G1.mul_div(1_040_132, 79_550_000, E6) == 82_742_500
    assert G1.mul_div(5, 5, 0) == 0
    assert G1.mul_div(1 << 62, 1 << 62, 1) == (1 << 63) - 1, "saturating"


# ---------------------------------------------------------------------
# Maps
# ---------------------------------------------------------------------


def test_pools_map_by_the_universe_index_and_coins_by_the_artifact(tmp_path):
    t = _terms(tmp_path)
    assert t.pools == {POOL_HYPE_USD: (0, G1.COIN_USD), POOL_HYPE_ETH: (0, 1)}
    assert t.books == {"hyperliquid:HYPE": (0, False), "hyperliquid:ETH": (1, False)}
    assert (t.gas_p50_usd_1e6, t.perp_taker_bps_1e6, t.coins) == (GAS, 4_500_000, ("HYPE", "ETH"))
    (tmp_path / "universe.toml").write_text('[hyperevm]\npools = ["0xaa:v3:18:18"]\n')
    with pytest.raises(G1.G1Error, match="not in"):
        G1.terms(tmp_path / "hyparb.toml", tmp_path / "universe.toml")


def test_a_fill_on_an_unmapped_pool_or_book_fails_the_session(tmp_path):
    t = _terms(tmp_path)
    run = _vector_run(tmp_path / "a", fills=[_fill(T0 + 1, POOL_UNMAPPED, 0, O_PX, O_QTY, 1)])
    s = G1.fold_session(run, t, None)
    assert not s.ok and f"pool symbol {POOL_UNMAPPED}" in s.error
    sol = (4 << 24) | 3
    run = _vector_run(tmp_path / "b", fills=[_fill(T0 + 1, sol, 0, 100 * E6, E6, 2)])
    s = G1.fold_session(run, t, None)
    assert not s.ok and "no [[coin]]" in s.error


def test_a_non_usd_pool_books_both_legs_as_inventory(tmp_path):
    t = _terms(tmp_path)
    eth_mid, hype_mid = 2_363_000_000, 79_550_000
    marks = G1.Marks("tape", {0: G1.Series([0], [hype_mid]), 1: G1.Series([0], [eth_mid])}, {}, {})
    sell = claude_worker.pmlr.FillRec(T0, POOL_HYPE_ETH, 1, 0, 1, 0, 33_697, 1_040_132, 1)
    v = G1.fold(t, [], [sell], {}, T0, marks)
    eth = 1_040_132 * 33_697 // E6
    assert v["cash_usd_1e6"] == 0, "no USD side: no cash"
    assert v["inventory_usd_1e6"] == G1.mul_div(-1_040_132, hype_mid, E6) + eth * eth_mid // E6
    assert v["amm_notional_usd_1e6"] == 1_040_132 * hype_mid // E6


# ---------------------------------------------------------------------
# What counts
# ---------------------------------------------------------------------


def test_only_slot_zero_paper_trades_are_folded(tmp_path):
    t = _terms(tmp_path)
    base = [
        _fill(T0 + S, POOL_HYPE_USD, 0, O_PX, O_QTY, 1),
        _fill(T0 + 2 * S, HYPE, 1, H_PX, H_QTY, 2),
    ]
    other = [
        _fill(T0 + S, HYPE, 0, MID, 9 * E6, 7, slot=3),
        _fill(T0 + S, HYPE, 0, MID, 9 * E6, 8, flags=1),
    ]
    orders = [
        _order(T0, POOL_HYPE_USD, 0, 2, O_PX, O_QTY, 1),
        _order(T0 + S + 5, HYPE, 1, 1, H_PX, H_QTY, 2),
        _order(T0, HYPE, 0, 1, MID, 9 * E6, 7, slot=3),
    ]
    s = G1.fold_session(_vector_run(tmp_path / "a", fills=base + other, orders=orders), t, None)
    assert s.ok and s.pnl_session_usd_1e6 == _expected_pnl(MID)
    assert (s.hedge_attempts, s.hedge_fills) == (1, 1), "slot 3 and the settlement are not ours"
    venue = [*base, _fill(T0 + 3 * S, HYPE, 0, MID, E6, 9, origin=0)]
    s = G1.fold_session(_vector_run(tmp_path / "b", fills=venue, orders=orders), t, None)
    assert not s.ok and "VENUE" in s.error and "never summed" in s.error


def test_gas_per_attempt_hedges_vs_flattens_and_a_torn_log_flagged(tmp_path):
    t = _terms(tmp_path)
    orders = [
        _order(T0, POOL_HYPE_USD, 0, 2, O_PX, O_QTY, 1),
        _order(T0 + 1, POOL_HYPE_USD, 0, 2, O_PX, O_QTY, 3),
        _order(T0 + 2, POOL_HYPE_ETH, 1, 2, 33_697, O_QTY, 4),
        _order(T0 + S + 5, HYPE, 1, 1, H_PX, H_QTY, 2),
        _order(T0 + S + 9, HYPE, 1, 1, H_PX, H_QTY, 5),
        _order(T0 + 3 * S, HYPE, 1, 1, H_PX, 10_000, 6),
    ]
    run = _vector_run(tmp_path / "logs", orders=orders)
    with (run / "engine-fills.pmlr").open("ab") as f:
        f.write(b"\x01" * 10)
    s = G1.fold_session(run, t, None)
    assert s.ok and s.torn == 1
    assert (s.amm_attempts, s.amm_fills, s.gas_usd_1e6) == (3, 1, 3 * GAS)
    assert (s.hedge_attempts, s.hedge_fills, s.hedge_unfilled) == (2, 1, 1)
    assert (s.flatten_attempts, s.flatten_fills) == (1, 0), "sent 2 s after any AMM fill"
    assert s.pnl_session_usd_1e6 == _expected_pnl(MID) - 2 * GAS


def test_funding_is_the_members_truncating_pass_with_its_sign():
    hour = 3_600 * S
    marks = G1.Marks("tape", {0: G1.Series([0], [100 * E6])}, {}, {0: G1.Series([0], [12_500])})
    # $1 000 at 1.25e-5 / h: 3.47 µ$ a pass, truncated to 3 by the member.
    long = G1.funding({0: [(1_000, 10 * E6)]}, marks, 1_000 + hour)
    short = G1.funding({0: [(1_000, -10 * E6)]}, marks, 1_000 + hour)
    assert (long, short) == (-3 * 3_600, 3 * 3_600), "longs pay a positive rate"
    flat = G1.funding({0: [(1_000, 10 * E6), (1_000 + hour // 2, 0)]}, marks, 1_000 + hour)
    assert flat == -3 * 1_799, "the pass AT the flattening fill sees no position"
    unsound = marks._replace(perp={0: G1.Series([0, 1_000 + hour // 2 + 1], [100 * E6, 0])})
    assert G1.funding({0: [(1_000, 10 * E6)]}, unsound, 1_000 + hour) == -3 * 1_800
    assert G1.funding({0: [(1_000, 0)]}, marks._replace(funding={}), 3_000) == 0, "never held"


def test_a_perp_position_without_a_rate_series_is_an_error_not_zero(tmp_path):
    marks = G1.Marks("tape", {0: G1.Series([0], [100 * E6])}, {}, {})
    with pytest.raises(G1.G1Error, match="HYPE: a perp position with no rate"):
        G1.funding({0: [(1_000, 10 * E6)]}, marks, 10 * S, ("HYPE",))
    t = _terms(tmp_path)
    s = G1.fold_session(_vector_run(tmp_path / "logs", events=[]), t, tmp_path / "absent.db")
    assert not s.ok and "no rate or mid series" in s.error


FUNDING_FILLS = [
    _fill(T0 + S, POOL_HYPE_USD, 1, O_PX, 20 * E6, 1),
    _fill(T0 + 2 * S, HYPE, 0, MID, 20 * E6, 2),
]
#: 20 HYPE long at MID, 1.25e-5 / h: 6 µ$ a pass after truncation.
PER_PASS = 20 * MID * 12_500 * S // (S * 3_600 * S)


def test_funding_reads_the_tapes_asset_ctx(tmp_path):
    t = _terms(tmp_path)
    events = [(T0, HYPE, 4, G1.CHANNEL_ASSET_CTX, 0, 0, 12_500, 0)]
    ticks = [_touch(T0, HYPE, MID), _touch(T0 + 62 * S, HYPE, MID)]
    run = _vector_run(tmp_path / "logs", ticks=ticks, fills=FUNDING_FILLS, events=events)
    s = G1.fold_session(run, t, None)
    assert s.ok and s.funding_usd_1e6 == -PER_PASS * 60 and PER_PASS == 6


def test_a_tape_without_rates_takes_them_from_candles(tmp_path):
    t = _terms(tmp_path)
    db = tmp_path / "candles.db"
    _candles(db, [], [(EPOCH_MS + 3_600_000, 1.25e-5)])
    ticks = [_touch(T0, HYPE, MID), _touch(T0 + 62 * S, HYPE, MID)]
    run = _vector_run(tmp_path / "logs", ticks=ticks, fills=FUNDING_FILLS, events=[])
    s = G1.fold_session(run, t, db)
    assert s.ok and s.mark_source == "tape+candles" and s.funding_usd_1e6 == -PER_PASS * 60


def test_a_stale_candle_mark_fails_the_session(tmp_path):
    t = _terms(tmp_path)
    for i, (age_min, ok) in enumerate(((2, True), (30, False))):
        db = tmp_path / f"c{i}.db"
        _candles(db, [(EPOCH_MS - age_min * 60_000, MID)], [(EPOCH_MS + 3_600_000, 0.0)])
        run = _vector_run(tmp_path / f"l{i}", ticks=[(T0, 7, 1, 2, 1)], hl_ticks=False, events=[])
        s = G1.fold_session(run, t, db)
        assert s.ok is ok, s.error
        if ok:
            assert s.mark_source == "candles" and s.pnl_session_usd_1e6 == _expected_pnl(MID)
        else:
            assert "min old at the close" in s.error


# ---------------------------------------------------------------------
# The ledgers
# ---------------------------------------------------------------------


def test_accrue_is_idempotent_and_never_stores_a_live_run(tmp_path):
    paths = _paths(tmp_path)
    old = _vector_run(paths.replay_dir, EPOCH)
    fresh = _vector_run(paths.replay_dir, EPOCH + 1_800 * S)
    os.utime(fresh / "engine-orders.pmlr")  # its logs move: live, though not the newest
    live = _vector_run(paths.replay_dir, EPOCH + 3_600 * S)
    day = G1._day(EPOCH)
    said: list[str] = []
    n, added, _ = G1.accrue(day, paths, replay=False, report=said.append)
    assert (n, added) == (1, 1)
    rows = G1.read_sessions(paths.sessions)
    assert [r.run for r in rows] == [old.name]
    assert (
        live.name not in paths.sessions.read_text() and fresh.name not in paths.sessions.read_text()
    )
    assert any(fresh.name in ln and "live" in ln for ln in said)
    before = paths.sessions.read_bytes()
    assert G1.accrue(day, paths, replay=False)[1] == 0
    assert paths.sessions.read_bytes() == before
    assert rows[0].pnl_session_usd_1e6 == _expected_pnl(MID)


def test_accrue_retries_an_earlier_days_error_row(tmp_path):
    paths = _paths(tmp_path)
    t = G1.terms(paths.artifact, paths.universe)
    yesterday = _vector_run(paths.replay_dir, EPOCH - DAY_NS)
    _vector_run(paths.replay_dir, EPOCH)
    _vector_run(paths.replay_dir, EPOCH + 3_600 * S)
    G1._write(paths.sessions, G1.SESSIONS_HEADER, [G1._error_row(yesterday.name, "local", t, "x")])
    G1.accrue(G1._day(EPOCH), paths, replay=False)
    rows = {r.run: r for r in G1.read_sessions(paths.sessions)}
    assert rows[yesterday.name].ok, "an error row is healed, not skipped"


def test_merge_prefers_ok_over_error_and_tape_over_candles_and_refold(tmp_path):
    t = _terms(tmp_path)
    ok = G1.fold_session(_vector_run(tmp_path / "logs"), t, None)
    bad = G1._error_row(ok.run, "s3", t, "pull failed")
    assert G1.merge_sessions([bad], [ok]) == ([ok], 0, 1)
    assert G1.merge_sessions([ok], [bad]) == ([ok], 0, 0), "an error never replaces an ok row"
    assert G1.merge_sessions([ok], [bad], refold=True) == ([ok], 0, 0)
    candle = ok._replace(mark_source="candles", pnl_session_usd_1e6=1)
    assert G1.merge_sessions([candle], [ok])[0] == [ok]
    assert G1.merge_sessions([ok], [candle])[0] == [ok]
    assert G1.merge_sessions([ok], [candle], refold=True) == ([candle], 0, 1)
    G1._write(tmp_path / "s.tsv", G1.SESSIONS_HEADER, [ok, bad._replace(run="run-1")])
    assert G1.read_sessions(tmp_path / "s.tsv") == [ok, bad._replace(run="run-1")]
    (tmp_path / "s.tsv").write_text("run-1\tok\n", encoding="utf-8")
    with pytest.raises(G1.G1Error, match="columns"):
        G1.read_sessions(tmp_path / "s.tsv")


def test_backfill_refold_replaces_ok_rows_and_a_plain_backfill_does_not(tmp_path):
    paths = _paths(tmp_path)
    _vector_run(paths.replay_dir, EPOCH)
    _vector_run(paths.replay_dir, EPOCH + 3_600 * S)
    G1.backfill(G1._day(EPOCH), paths, replay=False)
    paths.artifact.write_text(ARTIFACT.replace("= 10000", "= 20000"), encoding="utf-8")
    assert G1.backfill(G1._day(EPOCH), paths, replay=False)[1] == 0
    assert G1.read_sessions(paths.sessions)[0].gas_usd_1e6 == GAS
    assert G1.backfill(G1._day(EPOCH), paths, replay=False, refold=True)[1] == 1
    assert G1.read_sessions(paths.sessions)[0].gas_usd_1e6 == 2 * GAS


def test_a_session_older_than_the_artifact_is_flagged(tmp_path):
    t = _terms(tmp_path)
    run = _vector_run(tmp_path / "logs")
    assert G1.fold_session(run, t, None).artifact_may_differ == 1, "written today, after the run"
    os.utime(tmp_path / "hyparb.toml", ns=(EPOCH - S, EPOCH - S))
    t = G1.terms(tmp_path / "hyparb.toml", tmp_path / "universe.toml")
    s = G1.fold_session(run, t, None)
    assert (s.artifact_may_differ, s.artifact_mtime_utc) == (0, "2026-09-21T14:13:19Z")
    os.utime(tmp_path / "hyparb.toml", ns=(EPOCH + S, EPOCH + S))
    t = G1.terms(tmp_path / "hyparb.toml", tmp_path / "universe.toml")
    s = G1.fold_session(run, t, None)
    assert s.artifact_may_differ == 1
    lines = G1.render_status([s], [], t, ("unreachable", {}))
    assert any(
        f"artifact {t.sha256[:12]}" in ln and "sessions=1" in ln and "(artifact may differ)=1" in ln
        for ln in lines
    ), lines


# ---------------------------------------------------------------------
# The replay
# ---------------------------------------------------------------------

SCHEMA1 = (
    '{"schema_version":1,"ruleset_hash":"ab","split":"0/100","oos":{"net_pnl_usd":%s,'
    '"trades":2,"trading_days":1,"max_drawdown_usd":0.0,"round_trips":0,"legs":2},'
    '"bounds":{},"position_rows":1}\n'
)
COUNTERS = (
    "member: hyparb pool_events=9 pool_refused=0 maps_loaded=1 maps_refused=0 evaluations=3 "
    "arbs=4 side_buy=3 side_sell=1 skipped_below_min=0 skipped_not_live=0 skipped_no_hedge=0 "
    "skipped_inflight=0 skipped_cooldown=0 skipped_halted=0 size_capped=0 amm_fills=3 hedges=3 "
    "hedges_perp=3 hedges_spot=0 hedge_fills=3 hedges_missed=0 flattens=0 inventory_breaches=0 "
    "amm_judged_fills=3 amm_canceled=1 amm_partial=0 amm_not_live=0 gas_usd=0.0392 "
    "gas_oos_usd=0.0392 pnl_predicted_usd=0.9 funding_earned_usd=-0.001{pnl} orders_emitted=7 "
    "regime=not-replayed(v1)\n"
)


def _replay_run(logs: pathlib.Path, epoch: int = EPOCH) -> pathlib.Path:
    run = _vector_run(logs, epoch)
    tests.craft.write_ticks_px(run / "bn-ticks.pmlr", [(T0, 7, 1, 2, 1)], epoch)
    tests.craft.write_events(
        run / "hyperevm-signals.pmlr", [(T0, POOL_HYPE_USD, 8, 0, 0, 0, 0, 0)], epoch
    )
    _age(run)
    return run


class FakeHarness:
    def __init__(self, with_pnl: bool = True, fail: str = "") -> None:
        self.argv: list[list[str]] = []
        self.with_pnl, self.fail = with_pnl, fail

    def __call__(self, argv: list[str]) -> tuple[int, str, str]:
        self.argv.append(argv)
        art = pathlib.Path(argv[argv.index("--hyparb") + 1]).read_text(encoding="utf-8")
        assert 'mode = "paper"' in art, "a replay boots the paper member"
        unit = pathlib.Path(argv[argv.index("--replay-dir") + 1])
        (run,) = unit.glob("run-*")
        assert (run / "bn-ticks.pmlr").stat().st_size == 64, "lean: other venues cut empty"
        assert (run / "hl-ticks.pmlr").stat().st_size > 64
        two = "--latency-ns-venue" in argv
        if self.fail and (self.fail == "2blk") == two:
            return 1, "", "boom\n"
        net = "-0.5" if two else "1.25"
        pnl = ""
        if self.with_pnl:
            pnl = " pnl_session_usd=-0.25" if two else " pnl_session_usd=0.75"
        return 0, SCHEMA1 % net, COUNTERS.replace("{pnl}", pnl)


def test_only_the_two_block_rung_carries_the_latency_override(tmp_path):
    run = _replay_run(tmp_path / "logs")
    fake = FakeHarness()
    rows, pending = G1.replay_units(
        [run], ARTIFACT, ["--fee-bps", "hl:1:4"], {}, tmp_path / "s", fake
    )
    assert pending == 0
    assert [(r.rung, r.status, r.measure) for r in rows] == [
        ("r3", "ok", "pnl_session"),
        ("r3-2blk", "ok", "pnl_session"),
    ]
    r3, two = fake.argv
    assert "--latency-ns-venue" not in r3 and r3[-2:] == ["--fee-bps", "hl:1:4"]
    assert two[-4:] == ["--fee-bps", "hl:1:4", "--latency-ns-venue", "hyperevm:2000000000"]
    assert [r.value_usd_1e6 for r in rows] == [750_000, -250_000]
    assert rows[0].unit == f"{run.name}+0s" == G1.unit_names(run.name, 1)[0]
    assert rows[0].day == G1._day(EPOCH)
    assert (rows[1].latency_ns, rows[1].amm_judged_fills, rows[1].gas_usd_1e6) == (
        2_000_000_000,
        3,
        39_200,
    )
    stored = {(r.unit, r.rung): r for r in rows}
    again = FakeHarness()
    assert G1.replay_units([run], ARTIFACT, [], stored, tmp_path / "s", again) == ([], 0)
    assert again.argv == [], "a unit already on the member's measure is not replayed"


def test_a_spent_budget_leaves_units_pending_never_error_rows(tmp_path):
    run = _replay_run(tmp_path / "logs")
    fake = FakeHarness()
    rows, pending = G1.replay_units(
        [run], ARTIFACT, [], {}, tmp_path / "s", fake, deadline=time.monotonic() - 1
    )
    assert (rows, pending, fake.argv) == ([], 1, [])
    paths = _paths(tmp_path / "p")
    _replay_run(paths.replay_dir)
    _replay_run(paths.replay_dir, EPOCH + 3_600 * S)
    said: list[str] = []
    n_replay = G1.accrue(
        G1._day(EPOCH), paths, replay=True, run_fn=fake, budget_s=0.0, report=said.append
    )[2]
    assert n_replay == 0 and not paths.replay.exists()
    assert any("1 unit(s)" in ln and "PENDING" in ln for ln in said), said


def test_an_artifact_the_ladder_refuses_is_an_error_row(tmp_path):
    run = _replay_run(tmp_path / "logs")
    fake = FakeHarness()
    text = ARTIFACT.replace('mode = "testnet"\n', "")
    rows, _ = G1.replay_units([run], text, [], {}, tmp_path / "s", fake)
    assert [(r.rung, r.status) for r in rows] == [("r3", "error"), ("r3-2blk", "error")]
    assert "artifact refused" in rows[0].error and fake.argv == []


def test_accrue_replays_the_closed_days_units_into_the_replay_ledger(tmp_path):
    paths = _paths(tmp_path)
    old = _replay_run(paths.replay_dir)
    _replay_run(paths.replay_dir, EPOCH + 3_600 * S)  # the live run
    fake = FakeHarness()
    n, _, n_replay = G1.accrue(G1._day(EPOCH), paths, replay=True, run_fn=fake)
    assert (n, n_replay, len(fake.argv)) == (1, 2, 2)
    rows = G1.read_replay(paths.replay)
    assert [(r.unit, r.rung) for r in rows] == [
        (f"{old.name}+0s", "r3"),
        (f"{old.name}+0s", "r3-2blk"),
    ]
    assert sorted(p.name for p in paths.root.iterdir()) == [G1.REPLAY_NAME, G1.SESSIONS_NAME]
    assert G1.accrue(G1._day(EPOCH), paths, replay=True, run_fn=fake)[2] == 0
    assert len(fake.argv) == 2, "nothing re-run"


def test_lean_units_cut_every_capture_but_hl_and_hyperevm_empty(tmp_path):
    run = _replay_run(tmp_path / "logs")
    assert G1.lean_empty(run) == {"bn-ticks.pmlr", "engine-fills.pmlr", "engine-orders.pmlr"}


def test_an_old_binary_reports_the_proxy_labelled(tmp_path):
    run = _replay_run(tmp_path / "logs")
    rows, _ = G1.replay_units([run], ARTIFACT, [], {}, tmp_path / "s", FakeHarness(with_pnl=False))
    assert {r.measure for r in rows} == {G1.MEASURE_PROXY}
    assert [r.value_usd_1e6 for r in rows] == [1_250_000 - 1_000, -500_000 - 1_000]
    assert all(r.pnl_session_usd_1e6 == 0 for r in rows)


def test_a_failed_rung_is_an_error_row_and_never_summed(tmp_path):
    run = _replay_run(tmp_path / "logs")
    rows, _ = G1.replay_units([run], ARTIFACT, [], {}, tmp_path / "s", FakeHarness(fail="2blk"))
    by = {r.rung: r for r in rows}
    assert by["r3"].ok and not by["r3-2blk"].ok and "boom" in by["r3-2blk"].error
    lines = G1.render_status([], rows, None, ("unreachable", {}))
    two = next(ln for ln in lines if "replay r3-2blk" in ln)
    assert "units=0 failed=1" in two and "Σ[-]=$0.000000" in two
    merged, n = G1.merge_replay(rows, [by["r3-2blk"]._replace(status="ok", error="-")])
    assert n == 1 and all(r.ok for r in merged), "an ok re-run replaces the error row"


# ---------------------------------------------------------------------
# The verdict
# ---------------------------------------------------------------------


def _session(
    fills: int, pnl: int, day: int = 0, units: int = 1, status: str = "ok", k: int = 0
) -> claude_worker.hyparb_g1.Session:
    zeros = {f: 0 for f in G1.Session._fields if f not in G1._SESSION_TEXT}
    epoch = EPOCH + day * DAY_NS + k * S
    return G1.Session(
        **{
            **zeros,
            "run": f"run-{epoch}",
            "start_utc": G1._utc(epoch),
            "end_utc": "-",
            "status": status,
            "source": "local",
            "mark_source": "tape",
            "artifact_sha256": "ab" * 32,
            "artifact_mtime_utc": "2026-09-24T05:36:28Z",
            "error": "-",
            "amm_fills": fills,
            "pnl_session_usd_1e6": pnl,
            "units": units,
        }
    )


def _replay(
    unit: str,
    value: int,
    fills: int = 0,
    measure: str = G1.MEASURE_PNL,
    rung: str = G1.RUNG_R3_2BLK,
) -> claude_worker.hyparb_g1.Replay:
    zeros = {f: 0 for f in G1.Replay._fields if f not in G1._REPLAY_TEXT}
    return G1.Replay(
        **{
            **zeros,
            "unit": unit,
            "rung": rung,
            "day": "2026-09-21",
            "status": "ok",
            "measure": measure,
            "error": "-",
            "value_usd_1e6": value,
            "amm_judged_fills": fills,
        }
    )


def _covering(sessions, value: int, fills: int, measure: str = G1.MEASURE_PNL) -> list:
    return [
        _replay(u, value, fills, measure) for s in sessions for u in G1.unit_names(s.run, s.units)
    ]


def _verdict(sessions, replays) -> list[str]:
    return G1.render_status(sessions, replays, None, ("unreachable", {}))


def test_condition_two_must_cover_condition_ones_whole_sample():
    day0 = [_session(150, 5 * E6, units=2), _session(10, -E6, k=1)]
    day1 = [_session(60, E6, day=1)]
    sample = day0 + day1
    full = _covering(sample, 1, 70)
    assert _verdict(sample, full)[0].startswith("hyparb-g1: G1 = PASS")
    one_unit = full[:1]
    lines = _verdict(sample, one_unit)
    assert lines[0].startswith("hyparb-g1: G1 = NOT PASSED")
    assert any("condition 2" in ln and "INCOMPLETE (0 of 2 days replayed)" in ln for ln in lines)
    day1_only = _covering(day1, 10**9, 900)
    assert any("INCOMPLETE (1 of 2 days replayed)" in ln for ln in _verdict(sample, day1_only))
    proxy = _covering(sample, 1, 70, G1.MEASURE_PROXY)
    lines = _verdict(sample, proxy)
    assert any(
        "INCOMPLETE (0 of 2 days replayed; 4 unit(s) only on the oos_net+funding PROXY" in ln
        for ln in lines
    ), lines
    thin = _covering(sample, 1, 10)
    assert any("2-block judged fills 40 < 200" in ln for ln in _verdict(sample, thin))
    error_rows = [r._replace(status="error") for r in full]
    assert any("INCOMPLETE (0 of 2 days" in ln for ln in _verdict(sample, error_rows))


@pytest.mark.parametrize(
    ("sessions", "value", "want"),
    [
        ([(150, 5 * E6), (60, -E6)], 1, "PASS"),
        ([(150, 5 * E6), (49, -E6)], 1, "judged fills 199 < 200"),
        ([(250, -1)], 1, "Σ pnl_session <= 0"),
        ([(250, 1)], -1, "Σ 2-block -$0.000001 <= 0"),
    ],
)
def test_the_verdict_needs_both_conditions_on_the_members_law(sessions, value, want):
    rows = [_session(f, p, k=i) for i, (f, p) in enumerate(sessions)]
    replays = _covering(rows, value, 200)
    rows.append(_session(10_000, 10**9, status="error", k=99))
    lines = _verdict(rows, replays)
    if want == "PASS":
        assert lines[0].startswith("hyparb-g1: G1 = PASS")
    else:
        assert lines[0].startswith("hyparb-g1: G1 = NOT PASSED")
        assert any(want in ln for ln in lines), lines


def test_the_fidelity_line_puts_the_r3_replay_beside_the_live_fold():
    s = _session(9, 250_000, units=2)
    r3 = [_replay(u, 100_000, 4, rung=G1.RUNG_R3) for u in G1.unit_names(s.run, 2)]
    lines = _verdict([s], r3)
    assert any(
        "fidelity (r3 replay vs live fold, 1 session(s)): judged fills 8 vs 9; "
        "pnl_session $0.200000 vs $0.250000" in ln
        for ln in lines
    ), lines
    assert any("no session replayed whole at r3" in ln for ln in _verdict([s], r3[:1]))


def test_status_lines_live_states_hedges_flattens_and_the_caveat(tmp_path, monkeypatch):
    t = _terms(tmp_path)
    s = _session(9, 100)._replace(
        hedge_attempts=11, hedge_fills=5, flatten_attempts=1, flatten_fills=1
    )
    live = {
        "configured": 1,
        "pnl_session_usd_1e6": 250_741,
        "arbs_submitted": 20,
        "amm_fills": 9,
        "gas_charged_usd_1e6": 196_000,
        "funding_earned_usd_1e6": -2_379,
    }
    lines = G1.render_status([s], [], t, ("ok", live))
    assert any(
        "live session (not stored, never summed): pnl_session=$0.250741 arbs=20 amm_fills=9" in ln
        for ln in lines
    )
    assert any("first ~20 arbs" in ln for ln in lines)
    assert any("hedges 5/11 filled, flattens 1/1 filled" in ln for ln in lines)
    assert any("judged AMM fills=9/200" in ln and "Σ pnl_session=$0.000100" in ln for ln in lines)
    assert any("not configured" in ln for ln in G1.render_status([], [], t, ("not-configured", {})))

    class Resp(io.BytesIO):
        def __enter__(self):
            return self

        def __exit__(self, *exc):
            return False

    def answer(body: bytes) -> typing.Callable:
        return lambda url, timeout: Resp(body)

    monkeypatch.setattr(urllib.request, "urlopen", answer(b'{"hyparb": {"configured": 1, "x": 2}}'))
    assert G1.live_state() == ("ok", {"configured": 1, "x": 2})
    monkeypatch.setattr(urllib.request, "urlopen", answer(b'{"hyparb": {"configured": 0}}'))
    assert G1.live_state() == ("not-configured", {})

    def refuse(url, timeout):
        raise ConnectionRefusedError

    monkeypatch.setattr(urllib.request, "urlopen", refuse)
    assert G1.live_state() == ("unreachable", {})


def test_the_cli_status_and_fold_lanes(tmp_path, monkeypatch, capsys):
    paths = _paths(tmp_path)
    run = _vector_run(paths.replay_dir)
    monkeypatch.setattr(G1, "live_state", lambda: ("unreachable", {}))
    flags = [
        "--root",
        str(paths.root),
        "--hyparb",
        str(paths.artifact),
        "--universe",
        str(paths.universe),
        "--candles",
        str(paths.candles),
    ]
    assert G1.main(["fold", str(run), *flags]) == 0
    assert f'"pnl_session_usd_1e6": {_expected_pnl(MID)}' in capsys.readouterr().out
    assert not paths.root.exists(), "fold stores nothing"
    assert G1.main(["status", *flags]) == 0
    out = capsys.readouterr().out
    assert out.startswith("hyparb-g1: G1 = NOT PASSED") and "sessions=0" in out


# ---------------------------------------------------------------------
# The archive path
# ---------------------------------------------------------------------

ENV: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_ENABLED": "1",
    "MULTIVENUE_S3_BUCKET": "market-data-qu",
    "MULTIVENUE_S3_ENDPOINT": "fsn1.your-objectstorage.com",
    "MULTIVENUE_S3_REGION": "fsn1",
    "MULTIVENUE_S3_HOST_ID": "mbp-m4",
    "MULTIVENUE_S3_ACCESS_KEY_ID": "AK",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY": "sekrit",
}


def _archiver(tmp: pathlib.Path, fake: tests.fake_s3.FakeS3) -> claude_worker.archive.Archiver:
    cfg = claude_worker.archive_config.load(
        env=dict(ENV, MULTIVENUE_S3_CACHE_DIR=str(tmp / "cache")), env_file=tmp / "absent.env"
    )
    assert cfg.endpoint is not None and cfg.creds is not None
    store = claude_worker.objstore.ObjectStore(
        cfg.endpoint, cfg.creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )
    return claude_worker.archive.Archiver(cfg, store)


def test_backfill_pulls_two_logs_per_archived_run_and_marks_from_candles(tmp_path):
    paths = _paths(tmp_path)._replace(candles=tmp_path / "candles.db")
    fake = tests.fake_s3.FakeS3()
    archiver = _archiver(tmp_path, fake)
    old = _vector_run(paths.replay_dir, EPOCH)
    _vector_run(paths.replay_dir, EPOCH + 3_600 * S)  # the live run
    archiver.push_run(old, with_catalog=False)
    shutil.rmtree(old)
    _candles(paths.candles, [(EPOCH_MS - 120_000, MID)], [(EPOCH_MS + 3_600_000, 0.0)])
    fake.requests.clear()
    n, added, _ = G1.backfill(G1._day(EPOCH), paths, replay=False, archiver=archiver)
    assert (n, added) == (1, 1)
    (row,) = G1.read_sessions(paths.sessions)
    assert (row.run, row.source, row.mark_source, row.status) == (old.name, "s3", "candles", "ok")
    assert row.pnl_session_usd_1e6 == _expected_pnl(MID) and row.units == 1
    got = {p.rsplit("/", 1)[-1] for m, p, _ in fake.requests if m == "GET" and "/runs/" in p}
    assert got == {f"{f}.zst" for f in G1.FOLD_FILES} | {"_manifest.json"}, got
    assert list(paths.root.iterdir()) == [paths.sessions], "the pull left nothing behind"
    fake.requests.clear()
    assert G1.backfill(G1._day(EPOCH), paths, replay=False, archiver=archiver)[1] == 0
    assert not [p for m, p, _ in fake.requests if m == "GET" and p.endswith(".zst")]
    assert G1.backfill(G1._day(EPOCH), paths, replay=False, archiver=archiver, refold=True)[0] == 1
    assert [p for m, p, _ in fake.requests if m == "GET" and p.endswith(".zst")], "refold re-pulls"


def test_pull_files_refuses_anywhere_inside_the_archive_cache(tmp_path):
    archiver = _archiver(tmp_path, tests.fake_s3.FakeS3())
    for into in (tmp_path / "cache", tmp_path / "cache" / "g1" / "deeper"):
        with pytest.raises(claude_worker.archive.ArchiveError, match="cache"):
            archiver.pull_files("run-1", ["engine-fills.pmlr"], into)
