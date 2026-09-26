# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har_seed -- the long-tenor HAR over candles.db (HAR H2, H3.1).

Pins the properties that make the lanes trustworthy: the replay is the
engine's law on exactly the stored closes, the seed file round-trips into
an engine that forecasts and continues identically, nothing at or after
``now`` is ever read, the ordered fallbacks each fill only before the next
newer source, and no return is ever formed across a source boundary.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3

import pytest

import claude_worker.har_seed
import claude_worker.vol_ref

_DAY_MS: int = 86_400_000
_MIN_MS: int = 60_000
#: 2026-01-01 00:00Z.
_T0: int = 1_767_225_600_000
_U64: int = (1 << 64) - 1


def _walk(days: int, seed: int, scale: int = 1) -> list[tuple[int, int]]:
    """``days`` full days of 1-minute closes from ``_T0`` on a slow vol
    regime (so the fits have real slopes)."""
    s, px, out = seed, 79_000_000_000, []
    for d in range(days):
        phase = d % 40
        amp = 2_000_000 * scale * (4 + (phase if phase < 20 else 40 - phase))
        for m in range(1440):
            s = (s * 6_364_136_223_846_793_005 + 1_442_695_040_888_963_407) & _U64
            px = max(px + (s >> 32) % (2 * amp + 1) - amp, 1_000_000_000)
            out.append((_T0 + d * _DAY_MS + m * _MIN_MS, px))
    return out


def _db(path: pathlib.Path, series: dict[str, list[tuple[int, int]]]) -> sqlite3.Connection:
    conn = sqlite3.connect(path)
    conn.execute(
        "CREATE TABLE candles (venue INTEGER, descriptor TEXT, tf TEXT, open_ts INTEGER,"
        " c REAL, PRIMARY KEY (venue, descriptor, tf, open_ts))"
    )
    for desc, closes in series.items():
        conn.executemany(
            "INSERT INTO candles VALUES (1, ?, '1m', ?, ?)",
            [(desc, ts, px / 1_000_000) for ts, px in closes],
        )
    conn.commit()
    return conn


def _load(path: pathlib.Path) -> claude_worker.vol_ref.LongVolEngine:
    """What the H3 reader will do: apply the rows in order, then refresh."""
    e = claude_worker.vol_ref.LongVolEngine()
    none = claude_worker.vol_ref.LOG2_UNDEFINED
    day_ns = claude_worker.vol_ref.DAY_NS
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        f = line.split("\t")
        if f[0] == "V":
            assert f[1] == "1"
        elif f[0] == "D":
            assert e.seed_day(int(f[1]), int(f[2]), int(f[3]))
        elif f[0] == "C":
            assert e.seed_open(int(f[1]), int(f[2]), int(f[3]), int(f[4]), int(f[5]))
        elif f[0] == "A":
            fit = none if f[4] == "-" else int(f[4])
            assert e.seed_arm(int(f[1]) * day_ns, int(f[2]), int(f[3]), fit)
        elif f[0] == "P":
            assert e.seed_pair(int(f[1]) * day_ns, int(f[2]), int(f[3]), int(f[4]))
        else:
            assert f[0] == "Q"
            assert e.seed_qlike(int(f[1]) * day_ns, int(f[2]), int(f[3]))
    e.refresh()
    return e


def _forecasts(e: claude_worker.vol_ref.LongVolEngine) -> list[tuple]:
    out = []
    for d in range(1, claude_worker.vol_ref.LONG_TAU_DAYS_MAX + 1):
        t = d * claude_worker.vol_ref.DAY_NS
        out.append(
            (
                d,
                e.x_1e9(t),
                e.fit(t),
                e.sigma_ann_1e9(t, "raw"),
                e.sigma_ann_1e9(t, "fit"),
                e.n_pairs(t),
                e.qlike_counters(t),
                [e.pair_at(t, i) for i in range(e.n_pairs(t))],
            )
        )
    out.append(([e.day_at(i) for i in range(e.n_resident())], e.open_day()))
    return out


def test_the_replay_is_the_engine_on_exactly_the_stored_closes(tmp_path: pathlib.Path) -> None:
    closes = _walk(110, 5)
    now = _T0 + 100 * _DAY_MS + 6 * 3_600_000
    conn = _db(tmp_path / "candles.db", {"binance:btcusdt": closes})
    engine, stats = claude_worker.har_seed.replay(conn, "binance:btcusdt", now, days=100)
    direct = claude_worker.vol_ref.LongVolEngine()
    fed = [c for c in closes if c[0] < now]
    for ts, px in fed:
        direct.on_minute_close_at(px, ts)
    assert _forecasts(engine) == _forecasts(direct)
    # The lookahead law: nothing at or after `now` was read.
    assert (stats.minutes, stats.last_ts_ms) == (len(fed), fed[-1][0])
    assert engine.open_day()[0] == _T0 + 100 * _DAY_MS
    assert engine.is_warm()


def test_a_bar_still_open_at_now_is_never_read(tmp_path: pathlib.Path) -> None:
    """A bar is stamped with its open and closes a minute later: at
    ``now = 05:00:30`` the 05:00 bar is still open and is not read."""
    closes = _walk(35, 7)
    now = _T0 + 34 * _DAY_MS + 5 * 3_600_000 + 30_000
    conn = _db(tmp_path / "candles.db", {"s": closes})
    engine, stats = claude_worker.har_seed.replay(conn, "s", now, days=40)
    assert stats.last_ts_ms == now - 30_000 - _MIN_MS, "the 04:59 bar, closed at 05:00"
    assert engine.last_min_ts_ms == stats.last_ts_ms
    # The engine can still take the 05:00 close itself once it lands.
    px = next(p for ts, p in closes if ts == now - 30_000)
    engine.on_minute_close_at(px, now - 30_000)
    assert engine.refused == 0


def test_the_seed_round_trips_and_continues_identically(tmp_path: pathlib.Path) -> None:
    closes = _walk(200, 11)
    now = _T0 + 190 * _DAY_MS + 3 * 3_600_000 + 17 * _MIN_MS
    conn = _db(tmp_path / "candles.db", {"binance-usdm:btcusdt": closes})
    engine, _ = claude_worker.har_seed.replay(conn, "binance-usdm:btcusdt", now)
    t1 = claude_worker.vol_ref.DAY_NS
    assert engine.fit(t1) is not None, "a real seed has fitted short tenors"
    assert engine.qlike_counters(t1)[0] == claude_worker.vol_ref.QLIKE_RING
    out = tmp_path / "har-seed.tsv"
    claude_worker.har_seed.write_seed(out, "# test\n", claude_worker.har_seed.seed_rows(engine))
    rows = out.read_text(encoding="utf-8").splitlines()
    assert rows[1] == "V\t1"
    assert sum(r.startswith("D\t") for r in rows) == claude_worker.vol_ref.DAY_RING
    assert sum(r.startswith("C\t") for r in rows) == 1
    # Only the pending arms: sum over tau of min(tau, resident days).
    assert sum(r.startswith("A\t") for r in rows) == sum(range(1, 41))
    restored = _load(out)
    assert _forecasts(restored) == _forecasts(engine)
    # And both continue identically through the next four days.
    for ts, px in closes:
        if now <= ts < now + 4 * _DAY_MS:
            engine.on_minute_close_at(px, ts)
            restored.on_minute_close_at(px, ts)
    assert _forecasts(restored) == _forecasts(engine)
    assert (restored.gaps, engine.gaps) == (0, 0)


def test_the_fallback_fills_only_before_the_primary(tmp_path: pathlib.Path) -> None:
    spot = _walk(60, 13)
    usdm = [c for c in _walk(60, 17) if c[0] >= _T0 + 40 * _DAY_MS]
    now = _T0 + 60 * _DAY_MS
    conn = _db(tmp_path / "candles.db", {"binance:btcusdt": spot, "binance-usdm:btcusdt": usdm})
    engine, stats = claude_worker.har_seed.replay(
        conn, "binance-usdm:btcusdt", now, days=60, fallbacks=("binance:btcusdt",)
    )
    assert stats.from_fallback == 40 * 1440
    assert stats.minutes == 60 * 1440
    assert stats.splices == 1
    assert [(s.descriptor, s.minutes) for s in stats.spans] == [
        ("binance:btcusdt", 40 * 1440),
        ("binance-usdm:btcusdt", 20 * 1440),
    ]
    assert engine.n_resident() == 59 and engine.is_warm()
    # The fallback's seed round-trips like any other.
    out = tmp_path / "fb.tsv"
    claude_worker.har_seed.write_seed(out, "", claude_worker.har_seed.seed_rows(engine))
    assert _forecasts(_load(out)) == _forecasts(engine)
    # Without the fallback the same replay is 20 days and cold.
    cold, _ = claude_worker.har_seed.replay(conn, "binance-usdm:btcusdt", now, days=60)
    assert not cold.is_warm()


def test_compare_measures_the_per_day_vol_agreement(tmp_path: pathlib.Path) -> None:
    a = _walk(12, 19)
    twice = _walk(12, 19, scale=2)
    conn = _db(tmp_path / "candles.db", {"a": a, "copy": a, "twice": twice})
    until = _T0 + 12 * _DAY_MS
    n, med, p90 = claude_worker.har_seed.compare(conn, "a", "copy", _T0, until)
    # Day 0 has 1 439 returns (its first close only primes): still a full day.
    assert (n, med, p90) == (12, 0.0, 0.0)
    n, med, _ = claude_worker.har_seed.compare(conn, "a", "twice", _T0, until)
    # Twice the step amplitude is ~twice the vol: |ln ratio| ~ ln 2.
    assert n == 12 and 0.6 < med < 0.8


def test_the_lanes_print_and_write(tmp_path: pathlib.Path, capsys) -> None:
    db = tmp_path / "candles.db"
    _db(db, {"binance:btcusdt": _walk(45, 23)}).close()
    now = str(_T0 + 45 * _DAY_MS)
    assert (
        claude_worker.har_seed.main(
            ["show", "--db", str(db), "--descriptor", "binance:btcusdt", "--now-ms", now]
        )
        == 0
    )
    shown = capsys.readouterr().out
    assert "warm=True" in shown and "\n1d\t" in shown and "\n30d\t" in shown
    out = tmp_path / "seed.tsv"
    argv = [
        "seed-out",
        "--db",
        str(db),
        "--descriptor",
        "binance:btcusdt",
        "--now-ms",
        now,
        "--out",
        str(out),
    ]
    assert claude_worker.har_seed.main(argv) == 0
    assert out.read_text(encoding="utf-8").startswith("# har-seed.tsv v1")
    argv = [
        "compare",
        "--db",
        str(db),
        "--descriptor",
        "binance:btcusdt",
        "--against",
        "binance:btcusdt",
        "--now-ms",
        now,
    ]
    assert claude_worker.har_seed.main(argv) == 0
    assert "45 full day(s)" in capsys.readouterr().out


def _fed_with_splices(spans: list[list[tuple[int, int]]]) -> claude_worker.vol_ref.LongVolEngine:
    """The splice law by hand: each span's first minute only primes."""
    e = claude_worker.vol_ref.LongVolEngine()
    for k, closes in enumerate(spans):
        if k > 0:
            e.prev_px_1e6 = 0
        for ts, px in closes:
            e.on_minute_close_at(px, ts)
    return e


def test_the_fallbacks_fill_in_order_and_no_return_crosses_a_boundary(
    tmp_path: pathlib.Path,
) -> None:
    """Three sources of one path: the oldest from day 0, the middle one at
    TEN TIMES the level from day 20 (the ``xyz`` index against SPY), the
    feed from day 40. Each fills only before the next newer one's first
    minute, and the level jump never enters a day's sum."""
    path = _walk(60, 29)
    oldest = path
    middle = [(ts, px * 10) for ts, px in path if ts >= _T0 + 20 * _DAY_MS]
    feed = [c for c in path if c[0] >= _T0 + 40 * _DAY_MS]
    conn = _db(tmp_path / "candles.db", {"old:a": oldest, "mid:a": middle, "feed:a": feed})
    now = _T0 + 60 * _DAY_MS
    engine, stats = claude_worker.har_seed.replay(
        conn, "feed:a", now, days=60, fallbacks=("mid:a", "old:a")
    )
    assert [(s.descriptor, s.minutes) for s in stats.spans] == [
        ("old:a", 20 * 1440),
        ("mid:a", 20 * 1440),
        ("feed:a", 20 * 1440),
    ]
    assert (stats.splices, stats.from_fallback, stats.minutes) == (2, 40 * 1440, 60 * 1440)
    by_hand = _fed_with_splices(
        [
            [c for c in oldest if c[0] < _T0 + 20 * _DAY_MS],
            [c for c in middle if c[0] < _T0 + 40 * _DAY_MS],
            feed,
        ]
    )
    assert _forecasts(engine) == _forecasts(by_hand)
    # A return is scale-free, so with no return across a boundary the one
    # path's day sums are the single-source replay's, less one return on
    # each boundary day.
    single, _ = claude_worker.har_seed.replay(conn, "old:a", now, days=60)
    days = {engine.day_at(i)[0]: engine.day_at(i)[1:] for i in range(engine.n_resident())}
    ref = {single.day_at(i)[0]: single.day_at(i)[1:] for i in range(single.n_resident())}
    for d, (sq, n) in days.items():
        boundary = d in (_T0 + 20 * _DAY_MS, _T0 + 40 * _DAY_MS)
        assert n == ref[d][1] - (1 if boundary else 0)
        assert sq <= ref[d][0] and (boundary or sq == ref[d][0])


def test_a_source_with_no_rows_passes_its_bound_on(tmp_path: pathlib.Path) -> None:
    path = _walk(40, 31)
    feed = [c for c in path if c[0] >= _T0 + 30 * _DAY_MS]
    conn = _db(tmp_path / "candles.db", {"old:a": path, "feed:a": feed})
    now = _T0 + 40 * _DAY_MS
    engine, stats = claude_worker.har_seed.replay(
        conn, "feed:a", now, days=40, fallbacks=("absent:a", "old:a")
    )
    assert [(s.descriptor, s.minutes) for s in stats.spans] == [
        ("old:a", 30 * 1440),
        ("feed:a", 10 * 1440),
    ]
    assert stats.splices == 1 and engine.is_warm()


def _toml(path: pathlib.Path) -> pathlib.Path:
    path.write_text(
        '[[series]]\nname = "BTC"\nfeed = "binance-usdm:btcusdt"\nfallback = ["binance:btcusdt"]\n'
        '[[series]]\nname = "ETH"\nfeed = "binance-usdm:ethusdt"\n',
        encoding="utf-8",
    )
    return path


def test_seed_out_har_toml_writes_every_series(tmp_path: pathlib.Path) -> None:
    spot = _walk(50, 37)
    usdm = [c for c in _walk(50, 41) if c[0] >= _T0 + 30 * _DAY_MS]
    db = tmp_path / "candles.db"
    _db(
        db,
        {
            "binance:btcusdt": spot,
            "binance-usdm:btcusdt": usdm,
            "binance-usdm:ethusdt": _walk(50, 43),
        },
    ).close()
    toml = _toml(tmp_path / "har.toml")
    out_dir = tmp_path / "har"
    now = _T0 + 50 * _DAY_MS
    argv = [
        "seed-out",
        "--db",
        str(db),
        "--har-toml",
        str(toml),
        "--now-ms",
        str(now),
        "--out-dir",
        str(out_dir),
    ]
    assert claude_worker.har_seed.main(argv) == 0
    assert sorted(p.name for p in out_dir.iterdir()) == ["seed-BTC.tsv", "seed-ETH.tsv"]
    btc = (out_dir / "seed-BTC.tsv").read_text(encoding="utf-8").splitlines()
    assert btc[0].startswith("# har-seed.tsv v1 (HAR H3) -- BTC: feed binance-usdm:btcusdt")
    spans = [line.split() for line in btc if line.startswith("# span ")]
    assert [(s[2], int(s[5])) for s in spans] == [
        ("binance:btcusdt", 30 * 1440),
        ("binance-usdm:btcusdt", 20 * 1440),
    ]
    conn = sqlite3.connect(db)
    engine, _ = claude_worker.har_seed.replay(
        conn, "binance-usdm:btcusdt", now, fallbacks=("binance:btcusdt",)
    )
    assert _forecasts(_load(out_dir / "seed-BTC.tsv")) == _forecasts(engine)
    # --series narrows; a series with no minutes writes nothing and fails the run.
    (out_dir / "seed-ETH.tsv").unlink()
    argv_eth = [*argv, "--series", "ETH"]
    assert claude_worker.har_seed.main(argv_eth) == 0
    assert (out_dir / "seed-ETH.tsv").exists()
    empty = tmp_path / "empty.db"
    _db(empty, {}).close()
    argv[2] = str(empty)
    before = (out_dir / "seed-BTC.tsv").read_bytes()
    assert claude_worker.har_seed.main(argv) == 1
    assert (out_dir / "seed-BTC.tsv").read_bytes() == before, "a failed cut keeps the last seed"


def test_show_and_compare_har_toml(tmp_path: pathlib.Path, capsys) -> None:
    spot = _walk(50, 47)
    db = tmp_path / "candles.db"
    _db(
        db,
        {
            "binance:btcusdt": spot,
            "binance-usdm:btcusdt": spot,
            "binance-usdm:ethusdt": _walk(50, 53),
        },
    ).close()
    toml = _toml(tmp_path / "har.toml")
    now = str(_T0 + 50 * _DAY_MS)
    assert (
        claude_worker.har_seed.main(
            ["show", "--db", str(db), "--har-toml", str(toml), "--now-ms", now]
        )
        == 0
    )
    shown = capsys.readouterr().out
    assert "har-seed BTC: " in shown and "har-seed ETH: " in shown and shown.count("\n30d\t") == 2
    assert (
        claude_worker.har_seed.main(
            ["compare", "--db", str(db), "--har-toml", str(toml), "--now-ms", now]
        )
        == 0
    )
    lines = capsys.readouterr().out.splitlines()
    assert lines[0].startswith(
        "har-seed compare BTC binance-usdm:btcusdt vs binance:btcusdt: 50 full day(s)"
    )
    assert lines[0].endswith("median ln vol ratio +0.0000")
    assert lines[1] == "har-seed compare ETH binance-usdm:ethusdt: no fallback"


def test_the_store_is_read_never_created_or_written(tmp_path: pathlib.Path) -> None:
    absent = tmp_path / "absent.db"
    argv = ["show", "--db", str(absent), "--descriptor", "binance:btcusdt", "--now-ms", str(_T0)]
    assert claude_worker.har_seed.main(argv) == 2
    assert not absent.exists()
    db = tmp_path / "candles.db"
    _db(db, {"s": _walk(2, 59)}).close()
    conn = claude_worker.har_seed.connect_store(db)
    try:
        with pytest.raises(sqlite3.OperationalError):
            conn.execute("DELETE FROM candles")
    finally:
        conn.close()


def test_the_cli_refuses_mixed_selectors(tmp_path: pathlib.Path) -> None:
    toml = _toml(tmp_path / "har.toml")
    with pytest.raises(SystemExit):
        claude_worker.har_seed.main(["show", "--har-toml", str(toml), "--fallback", "x:y"])
    with pytest.raises(SystemExit):
        claude_worker.har_seed.main(["show", "--har-toml", str(toml), "--descriptor", "x:y"])
    with pytest.raises(SystemExit):
        claude_worker.har_seed.main(["seed-out", "--descriptor", "x:y"])
    with pytest.raises(SystemExit):
        claude_worker.har_seed.main(["compare", "--descriptor", "x:y"])
    bad = tmp_path / "bad.toml"
    bad.write_text('[[series]]\nname = "x"\n', encoding="utf-8")
    assert claude_worker.har_seed.main(["show", "--har-toml", str(bad)]) == 2
