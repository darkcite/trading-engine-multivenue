# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vrp_seed.py — the VRP boot seed cut (VRP V5).

The engine restarts about three times a day and the forecast needs 60
settled pairs before it will produce a bound, so without a seed the
member would be blind for two months. These tests pin the two properties
that make the seed trustworthy: it is arithmetically the same law the
engine runs, and it cannot see the future.

Convention: full ``import x`` only. No ``from x import y``.
"""

import itertools
import pathlib
import sqlite3

import pytest

import claude_worker.vol_ref
import claude_worker.vrp_seed

_MINUTE_MS: int = 60_000
_DAY_MS: int = 86_400_000
#: 2026-09-10 08:00:00 UTC — a daily expiry instant.
_EXPIRY_MS: int = 1_789_027_200_000


def _make_db(path: pathlib.Path, descriptor: str, first_ms: int, closes: list[int]) -> None:
    """A minimal `candles` table carrying 1-minute closes."""
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE candles (venue TEXT, descriptor TEXT, tf TEXT,"
            " open_ts INTEGER, c REAL, PRIMARY KEY (venue, descriptor, tf, open_ts))"
        )
        conn.executemany(
            "INSERT INTO candles (venue, descriptor, tf, open_ts, c) VALUES (?,?,?,?,?)",
            [
                ("deribit", descriptor, "1m", first_ms + i * _MINUTE_MS, c / 1_000_000)
                for i, c in enumerate(closes)
            ],
        )
        conn.commit()
    finally:
        conn.close()


def _tape(n: int, seed: int = 20260910) -> list[int]:
    """A deterministic integer walk in x1e6 dollars."""
    out: list[int] = []
    px = 79_000_000_000
    s = seed
    for _ in range(n):
        s = (s * 6364136223846793005 + 1442695040888963407) % (2**64)
        px += ((s >> 32) % 40_000_000) - 20_000_000
        px = max(px, 1_000_000_000)
        out.append(px)
    return out


def test_daily_expiries_are_0800_utc_and_never_in_the_future() -> None:
    now = _EXPIRY_MS + 3 * 3_600_000  # 11:00 UTC, three hours after settle
    got = claude_worker.vrp_seed.daily_expiries_ms(now, 3)
    assert got == [_EXPIRY_MS - 2 * _DAY_MS, _EXPIRY_MS - _DAY_MS, _EXPIRY_MS]
    assert all(ts <= now for ts in got)
    # An hour BEFORE settle, today's expiry has not happened yet.
    earlier = claude_worker.vrp_seed.daily_expiries_ms(_EXPIRY_MS - 3_600_000, 2)
    assert earlier[-1] == _EXPIRY_MS - _DAY_MS


def test_a_pair_is_cut_from_two_disjoint_windows(tmp_path: pathlib.Path) -> None:
    """``x`` sees only the 1440 minutes before entry; ``y`` only the hold.

    The check is not a comment: the pair is recomputed here from the two
    windows independently and must come out identical.
    """
    tau_ns = claude_worker.vol_ref.TAU_8H_NS
    tau_min = tau_ns // 60_000_000_000
    warm = claude_worker.vol_ref.HAR_WINDOWS[-1]
    first_ms = _EXPIRY_MS - (warm + tau_min + 10) * _MINUTE_MS
    closes = _tape(warm + tau_min + 11)
    assert first_ms + (warm + 1) * _MINUTE_MS <= _EXPIRY_MS - tau_min * _MINUTE_MS
    db = tmp_path / "candles.db"
    _make_db(db, "deribit:BTC-PERPETUAL", first_ms, closes)

    stats = claude_worker.vrp_seed.CutStats()
    conn = sqlite3.connect(db)
    try:
        row = claude_worker.vrp_seed.pair_for_expiry(
            conn,
            claude_worker.vrp_seed.CutSpec("deribit:BTC-PERPETUAL", tau_ns),
            _EXPIRY_MS,
            _EXPIRY_MS + 3_600_000,
            stats,
        )
    finally:
        conn.close()
    assert row is not None
    assert row.expiry_ts_ms == _EXPIRY_MS
    assert stats.emitted == 1

    entry_ms = _EXPIRY_MS - tau_min * _MINUTE_MS
    # x, from the history window alone.
    engine = claude_worker.vol_ref.VolEngine()
    for i, c in enumerate(closes):
        ts = first_ms + i * _MINUTE_MS
        if entry_ms - (warm + 1) * _MINUTE_MS <= ts < entry_ms:
            engine.on_minute_close(c)
    assert row.x_1e9 == engine.x_1e9(tau_ns)

    # y, from the hold window alone.
    hold = [c for i, c in enumerate(closes) if entry_ms <= first_ms + i * _MINUTE_MS <= _EXPIRY_MS]
    acc = 0
    for a, b in itertools.pairwise(hold):
        r = claude_worker.vol_ref.ret_bps_1e9(a, b)
        acc += r * r
    assert row.y_1e9 == claude_worker.vol_ref.ln_1e9(claude_worker.vol_ref.isqrt_i64(acc))


def test_an_unsettled_hold_is_never_emitted(tmp_path: pathlib.Path) -> None:
    """A.3, the lookahead law.

    An expiry that has not happened has no realised ``y``. Emitting one
    anyway — from a partial hold, or from the forecast itself — is the
    single defect that would make an offline backtest of this strategy
    look profitable when it is not. The cut refuses it and counts it.
    """
    tau_ns = claude_worker.vol_ref.TAU_8H_NS
    tau_min = tau_ns // 60_000_000_000
    warm = claude_worker.vol_ref.HAR_WINDOWS[-1]
    first_ms = _EXPIRY_MS - (warm + tau_min + 10) * _MINUTE_MS
    db = tmp_path / "candles.db"
    _make_db(db, "d", first_ms, _tape(warm + tau_min + 11))

    stats = claude_worker.vrp_seed.CutStats()
    conn = sqlite3.connect(db)
    try:
        # `now` one minute BEFORE the expiry: the hold is still running.
        row = claude_worker.vrp_seed.pair_for_expiry(
            conn,
            claude_worker.vrp_seed.CutSpec("d", tau_ns),
            _EXPIRY_MS,
            _EXPIRY_MS - _MINUTE_MS,
            stats,
        )
    finally:
        conn.close()
    assert row is None
    assert stats.skipped_unsettled == 1
    assert stats.emitted == 0


def test_a_short_hold_is_refused_rather_than_annualised(tmp_path: pathlib.Path) -> None:
    """A hold with half its minutes missing is a hole, not a cheap pair."""
    tau_ns = claude_worker.vol_ref.TAU_8H_NS
    tau_min = tau_ns // 60_000_000_000
    warm = claude_worker.vol_ref.HAR_WINDOWS[-1]
    entry_ms = _EXPIRY_MS - tau_min * _MINUTE_MS
    first_ms = entry_ms - (warm + 1) * _MINUTE_MS
    # History complete, hold only a third present.
    closes = _tape(warm + 1 + tau_min // 3)
    db = tmp_path / "candles.db"
    _make_db(db, "d", first_ms, closes)

    stats = claude_worker.vrp_seed.CutStats()
    conn = sqlite3.connect(db)
    try:
        row = claude_worker.vrp_seed.pair_for_expiry(
            conn,
            claude_worker.vrp_seed.CutSpec("d", tau_ns),
            _EXPIRY_MS,
            _EXPIRY_MS + _DAY_MS,
            stats,
        )
    finally:
        conn.close()
    assert row is None
    assert stats.skipped_short_hold == 1


def test_short_history_is_refused(tmp_path: pathlib.Path) -> None:
    """Fewer than 1440 minutes before entry is not a day's realised vol."""
    tau_ns = claude_worker.vol_ref.TAU_8H_NS
    tau_min = tau_ns // 60_000_000_000
    entry_ms = _EXPIRY_MS - tau_min * _MINUTE_MS
    first_ms = entry_ms - 600 * _MINUTE_MS
    db = tmp_path / "candles.db"
    _make_db(db, "d", first_ms, _tape(600 + tau_min + 2))

    stats = claude_worker.vrp_seed.CutStats()
    conn = sqlite3.connect(db)
    try:
        row = claude_worker.vrp_seed.pair_for_expiry(
            conn,
            claude_worker.vrp_seed.CutSpec("d", tau_ns),
            _EXPIRY_MS,
            _EXPIRY_MS + _DAY_MS,
            stats,
        )
    finally:
        conn.close()
    assert row is None
    assert stats.skipped_short_history == 1


def test_an_untradeable_tenor_is_refused(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "candles.db"
    _make_db(db, "d", _EXPIRY_MS - 100 * _MINUTE_MS, _tape(100))
    conn = sqlite3.connect(db)
    try:
        with pytest.raises(ValueError, match="not a tradeable tenor"):
            claude_worker.vrp_seed.pair_for_expiry(
                conn,
                claude_worker.vrp_seed.CutSpec("d", 86_400_000_000_000),
                _EXPIRY_MS,
                _EXPIRY_MS + _DAY_MS,
                claude_worker.vrp_seed.CutStats(),
            )
    finally:
        conn.close()


def test_the_written_file_round_trips_through_the_engines_parser(tmp_path: pathlib.Path) -> None:
    """Rows are exactly what ``core_config::vrp::parse_seed_row`` reads.

    Three tab-separated integers, oldest first, comments skipped. This
    test is the Python half of that contract; the Rust half is
    ``core_config::vrp::tests::seed_rows_are_integers_or_nothing``.
    """
    rows = [
        claude_worker.vrp_seed.SeedRow(_EXPIRY_MS - _DAY_MS, 24_659_086_751, 24_700_000_000),
        claude_worker.vrp_seed.SeedRow(_EXPIRY_MS, 24_671_149_241, -5),
    ]
    out = tmp_path / claude_worker.vrp_seed.SEED_FILE
    claude_worker.vrp_seed.write_seed_tsv(out, rows)
    body = [
        line
        for line in out.read_text(encoding="utf-8").splitlines()
        if line and not line.startswith("#")
    ]
    assert body == [
        f"{_EXPIRY_MS - _DAY_MS}\t24659086751\t24700000000",
        f"{_EXPIRY_MS}\t24671149241\t-5",
    ]
    for line in body:
        parts = line.split("\t")
        assert len(parts) == 3
        assert all(p.lstrip("-").isdigit() for p in parts)


def test_the_cut_is_oldest_first_and_bounded(tmp_path: pathlib.Path) -> None:
    tau_ns = claude_worker.vol_ref.TAU_8H_NS
    tau_min = tau_ns // 60_000_000_000
    warm = claude_worker.vol_ref.HAR_WINDOWS[-1]
    days = 6
    first_ms = _EXPIRY_MS - days * _DAY_MS - (warm + tau_min) * _MINUTE_MS
    n = (_EXPIRY_MS + _MINUTE_MS - first_ms) // _MINUTE_MS
    db = tmp_path / "candles.db"
    _make_db(db, "d", first_ms, _tape(int(n)))

    rows, stats = claude_worker.vrp_seed.cut_rows(
        db,
        claude_worker.vrp_seed.CutSpec("d", tau_ns, pairs=3),
        now_ms=_EXPIRY_MS + 3_600_000,
    )
    assert len(rows) == 3, stats
    assert [r.expiry_ts_ms for r in rows] == sorted(r.expiry_ts_ms for r in rows)
    assert rows[-1].expiry_ts_ms == _EXPIRY_MS
    assert stats.emitted >= 3
