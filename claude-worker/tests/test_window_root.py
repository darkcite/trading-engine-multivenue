# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""window_root (ICDP I6): ≤ 2 h windows of a capture run as harness runs.

Slice law (binary-searched ``[lo, hi)`` on ``ts_ns``), header epoch
advance + directory rename, manifests copied, the 2 h refusal, the
torn trailing slot dropped, empty files kept as header-only.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import struct

import pytest

import tests.craft

import claude_worker.frames
import claude_worker.pmlr
import claude_worker.window_root

S = 10**9
EPOCH = 1_788_000_000_000_000_000


def _mk_run(tmp_path: pathlib.Path, ts_list: list[int]) -> pathlib.Path:
    run = tests.craft.write_run(tmp_path / "logs", EPOCH, ts_list)
    (run / "instrument-manifest.tsv").write_text("42\tPMTOK\n")
    # a second lane starting later, plus an empty file
    tests.craft.write_ticks(run / "bn-ticks.pmlr", [t + 7 for t in ts_list[1:]], EPOCH, sym=7, venue=1)
    tests.craft.write_ticks(run / "okx-ticks.pmlr", [], EPOCH, sym=9, venue=2)
    return run


def test_windows_cover_the_run_in_two_hour_steps(tmp_path):
    run = _mk_run(tmp_path, [1_000, 3 * 3600 * S, 5 * 3600 * S + 1])
    w = claude_worker.window_root.windows_of(run)
    assert w == [(0.0, 7200.0), (7200.0, 14400.0), (14400.0, 21600.0)]
    assert claude_worker.window_root.windows_of(run, 3600.0)[:2] == [(0.0, 3600.0), (3600.0, 7200.0)]
    with pytest.raises(claude_worker.window_root.WindowError):
        claude_worker.window_root.windows_of(run, 7201.0)
    empty = tests.craft.write_run(tmp_path / "logs2", EPOCH, [])
    assert claude_worker.window_root.windows_of(empty) == []


def test_cut_run_slices_every_file_and_advances_the_epoch(tmp_path):
    ts = [1_000, 1_000 + 30 * S, 1_000 + 3_000 * S, 1_000 + 7_199 * S, 1_000 + 7_200 * S, 1_000 + 9_000 * S]
    run = _mk_run(tmp_path, ts)
    lines: list[str] = []
    out = claude_worker.window_root.cut_run(run, tmp_path / "roots", 0.0, 7200.0, lines.append)
    assert out.name == f"run-{EPOCH}"
    r = claude_worker.pmlr.Reader(out / "pm-ticks.pmlr")
    assert [t.ts_ns for t in r.ticks()] == ts[:4], "[lo, hi) on ts_ns"
    r = claude_worker.pmlr.Reader(out / "bn-ticks.pmlr")
    assert [t.ts_ns for t in r.ticks()] == [t + 7 for t in ts[1:4]]
    assert (out / "okx-ticks.pmlr").stat().st_size == claude_worker.window_root.HEADER_SIZE
    assert (out / "instrument-manifest.tsv").read_text() == "42\tPMTOK\n"
    assert any("pm-ticks.pmlr 6 -> 4" in l for l in lines)
    # Second window: epoch advanced by 7200 s, dir renamed, the rest kept.
    out2 = claude_worker.window_root.cut_run(run, tmp_path / "roots", 7200.0, 14400.0)
    assert out2.name == f"run-{EPOCH + 7200 * S}"
    head = (out2 / "pm-ticks.pmlr").read_bytes()[:16]
    assert struct.unpack_from("<Q", head, 8)[0] == EPOCH + 7200 * S
    r = claude_worker.pmlr.Reader(out2 / "pm-ticks.pmlr")
    assert [t.ts_ns for t in r.ticks()] == ts[4:]
    # Re-cutting an existing window replaces it.
    out2b = claude_worker.window_root.cut_run(run, tmp_path / "roots", 7200.0, 14400.0)
    assert out2b == out2 and [t.ts_ns for t in claude_worker.pmlr.Reader(out2 / "pm-ticks.pmlr").ticks()] == ts[4:]


def test_cut_run_refuses_over_long_windows_and_tickless_runs(tmp_path):
    run = _mk_run(tmp_path, [1_000, 2_000])
    with pytest.raises(claude_worker.window_root.WindowError):
        claude_worker.window_root.cut_run(run, tmp_path / "roots", 0.0, 7201.0)
    with pytest.raises(claude_worker.window_root.WindowError):
        claude_worker.window_root.cut_run(run, tmp_path / "roots", 10.0, 10.0)
    empty = tests.craft.write_run(tmp_path / "logs2", EPOCH, [])
    with pytest.raises(claude_worker.window_root.WindowError):
        claude_worker.window_root.cut_run(empty, tmp_path / "roots", 0.0, 60.0)


def test_torn_trailing_slot_is_dropped(tmp_path):
    run = _mk_run(tmp_path, [1_000, 2_000, 3_000])
    p = run / "pm-ticks.pmlr"
    p.write_bytes(p.read_bytes() + b"\x01" * 17)  # a torn slot the engine is still flushing
    out = claude_worker.window_root.cut_run(run, tmp_path / "roots", 0.0, 60.0)
    r = claude_worker.pmlr.Reader(out / "pm-ticks.pmlr")
    assert [t.ts_ns for t in r.ticks()] == [1_000, 2_000, 3_000]


# ---- RG3: ai-cmds.pmlr — cut by ts_ns + the SetRegime carry-over ----

_AI = struct.Struct("<QIIqqQBBBBHH")  # AiCmd head (48 B) — frames.py _HEAD without the len prefix


def _ai_frame(ts: int, kind: int, ttl: int = 0, profile: int = 0) -> bytes:
    body = _AI.pack(ts, 1, 0xFFFF_FFFF, 0, 0, ttl, kind, 5, 0xFF, 0xFF, profile, 0)
    return body + bytes(claude_worker.window_root.SLOT - len(body))


def _write_ai_cmds(path: pathlib.Path, frames: list[bytes], epoch_ns: int) -> None:
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.window_root.SLOT_KIND_AI_CMD, epoch_ns
    )
    path.write_bytes(header + bytes(claude_worker.window_root.HEADER_SIZE - len(header)) + b"".join(frames))


def _ai_slots(path: pathlib.Path) -> list[tuple[int, int, int, int]]:
    """(ts, kind, ttl, profile) per slot of a cut ai-cmds file."""
    blob = path.read_bytes()[claude_worker.window_root.HEADER_SIZE :]
    out = []
    for i in range(len(blob) // claude_worker.window_root.SLOT):
        off = i * claude_worker.window_root.SLOT
        ts, _, _, _, _, ttl, kind, _, _, _, profile, _ = _AI.unpack_from(blob, off)
        out.append((ts, kind, ttl, profile))
    return out


def test_ai_cmds_are_cut_by_ts_and_the_live_declaration_is_carried(tmp_path):
    hb = claude_worker.frames.KIND_HEARTBEAT
    sr = claude_worker.frames.KIND_SET_REGIME
    ts0 = 1_000
    lo = ts0 + 7200 * S  # the second window's first instant
    run = _mk_run(tmp_path, [ts0, ts0 + 3600 * S, lo + 5, lo + 3600 * S])
    frames = [  # file order == ts order (the capture is per-thread monotone)
        _ai_frame(ts0 + 10, hb),
        # fast: an early long-TTL declaration REPLACED by a later, expired
        # one ⇒ nothing carried for fast (the latest frame decides).
        _ai_frame(ts0 + 60 * S, sr, ttl=4 * 3600 * S, profile=0),
        # slow: declared 10 min before the cut with a 15 min TTL ⇒ carried.
        _ai_frame(ts0 + 6600 * S, sr, ttl=900 * S, profile=1),
        _ai_frame(ts0 + 7000 * S, sr, ttl=100 * S, profile=0),  # expires at 7100 s < lo
        _ai_frame(ts0 + 7100 * S, hb),
        # in-window frames: sliced as-is.
        _ai_frame(lo + 30 * S, sr, ttl=900 * S, profile=0),
        _ai_frame(lo + 7300 * S, hb),  # beyond the window
    ]
    _write_ai_cmds(run / "ai-cmds.pmlr", frames, EPOCH)
    lines: list[str] = []
    out = claude_worker.window_root.cut_run(run, tmp_path / "roots", 7200.0, 14400.0, lines.append)
    slots = _ai_slots(out / "ai-cmds.pmlr")
    assert slots == [
        (ts0 + 6600 * S, sr, 900 * S, 1),  # carried: the slow declaration still in force at lo
        (lo + 30 * S, sr, 900 * S, 0),
    ]
    assert any("ai-cmds.pmlr 7 -> 2" in l for l in lines), lines
    # The first window carries nothing (no declaration precedes ts0).
    out0 = claude_worker.window_root.cut_run(run, tmp_path / "roots", 0.0, 7200.0)
    assert [s[1] for s in _ai_slots(out0 / "ai-cmds.pmlr")] == [hb, sr, sr, sr, hb]
    # An ai-cmds file with no slot stays header-only.
    _write_ai_cmds(run / "ai-cmds.pmlr", [], EPOCH)
    out_e = claude_worker.window_root.cut_run(run, tmp_path / "roots", 7200.0, 14400.0)
    assert (out_e / "ai-cmds.pmlr").stat().st_size == claude_worker.window_root.HEADER_SIZE


def test_cut_run_writes_the_windows_own_regime_seed_from_candles(tmp_path):
    import claude_worker.candles

    ts = [1_000, 1_000 + 30 * S, 1_000 + 7_200 * S, 1_000 + 9_000 * S]
    run = _mk_run(tmp_path, ts)
    regime_toml = tmp_path / "regime.toml"
    regime_toml.write_text(
        '[refs]\nbtc = "binance-usdm:btcusdt"\nfund = "binance-usdm:btcusdt"\n'
        '[breadth]\nmembers = ["binance-usdm:ethusdt"]\n',
        encoding="utf-8",
    )
    db = tmp_path / "candles.db"
    conn = claude_worker.candles.open_db(db)
    # 1 m closes around the SECOND window's first wall minute
    # (epoch + 7200 s): the seed takes closes strictly before it.
    w2_ms = (EPOCH + 7200 * S) // 1_000_000
    rows = []
    for k in range(5):
        open_ts = w2_ms - (5 - k) * 60_000
        rows.append((1, "binance-usdm:btcusdt", "1m", open_ts, 1.0, 1.0, 1.0, 100.0 + k, 1.0, "rest", w2_ms))
    rows.append((1, "binance-usdm:btcusdt", "1m", w2_ms, 1.0, 1.0, 1.0, 999.0, 1.0, "rest", w2_ms))  # at the cut: excluded
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        rows,
    )
    conn.commit()
    conn.close()
    lines: list[str] = []
    out = claude_worker.window_root.cut_run(
        run, tmp_path / "roots", 7200.0, 14400.0, lines.append, seed=(regime_toml, db)
    )
    seed = out / claude_worker.window_root.SEED_FILE
    body = [l for l in seed.read_text("utf-8").splitlines() if l and not l.startswith("#")]
    assert len(body) == 5, body
    assert body[-1].split("\t") == ["binance-usdm:btcusdt", str((w2_ms - 60_000) // 60_000), "104000000"]
    assert any("regime-seed.tsv 5 rows for 2 descriptors" in l for l in lines), lines
    # Either input absent ⇒ no seed file, no error.
    out_no = claude_worker.window_root.cut_run(
        run, tmp_path / "roots", 0.0, 7200.0, seed=(tmp_path / "nope.toml", db)
    )
    assert not (out_no / claude_worker.window_root.SEED_FILE).exists()


def test_cut_run_writes_the_windows_own_funding_seed_from_the_funding_table(tmp_path):
    """The harness funding seed: the window's manifest × the funding
    table, prints strictly before the window's first instant (the boot
    lane's 73 h law), rate ×1e9 RAW; spot descriptors never seed; a
    print AT the cut is excluded. No funding table ⇒ no file."""
    import claude_worker.candles
    import claude_worker.seeds

    ts = [1_000, 1_000 + 30 * S, 1_000 + 7_200 * S, 1_000 + 9_000 * S]
    run = _mk_run(tmp_path, ts)
    (run / "instrument-manifest.tsv").write_text(
        "16777728\tbinance-usdm:btcusdt\n7\tbinance:btcusdt\n", encoding="utf-8"
    )
    db = tmp_path / "candles.db"
    conn = claude_worker.candles.open_db(db)
    conn.execute(
        "CREATE TABLE IF NOT EXISTS funding (venue INTEGER NOT NULL, descriptor TEXT NOT NULL,"
        " ts_ms INTEGER NOT NULL, rate REAL NOT NULL, fetched_ts INTEGER NOT NULL,"
        " PRIMARY KEY (venue, descriptor, ts_ms)) WITHOUT ROWID"
    )
    w2_ms = (EPOCH + 7200 * S) // 1_000_000
    h = 3_600_000
    prints = [
        (1, "binance-usdm:btcusdt", w2_ms - 80 * h, 0.0009),  # outside the 73 h look-back
        (1, "binance-usdm:btcusdt", w2_ms - 16 * h, 0.0001),
        (1, "binance-usdm:btcusdt", w2_ms - 8 * h, -0.0002),
        (1, "binance-usdm:btcusdt", w2_ms, 0.0003),  # at the cut: excluded
        (1, "binance:btcusdt", w2_ms - 8 * h, 0.0001),  # spot: never seeded
    ]
    conn.executemany("INSERT INTO funding VALUES (?,?,?,?,0)", prints)
    conn.commit()
    conn.close()
    lines: list[str] = []
    out = claude_worker.window_root.cut_run(
        run, tmp_path / "roots", 7200.0, 14400.0, lines.append, seed=(tmp_path / "nope.toml", db)
    )
    seed = out / claude_worker.seeds.FUNDING_SEED_FILE
    body = [l for l in seed.read_text("utf-8").splitlines() if l and not l.startswith("#")]
    assert body == [
        f"binance-usdm:btcusdt\t{w2_ms - 16 * h}\t100000",
        f"binance-usdm:btcusdt\t{w2_ms - 8 * h}\t-200000",
    ]
    assert any("funding-seed.tsv 2 prints for 1 descriptors" in l for l in lines), lines
    assert not (out / claude_worker.window_root.SEED_FILE).exists()
    # A window with no print before it gets no file (the harness warms from the window).
    out_first = claude_worker.window_root.cut_run(run, tmp_path / "roots2", 0.0, 7200.0, seed=(tmp_path / "nope.toml", db))
    assert (out_first / claude_worker.seeds.FUNDING_SEED_FILE).exists()  # 2 prints precede it too
    db2 = tmp_path / "bare.db"
    claude_worker.candles.open_db(db2).close()
    out_bare = claude_worker.window_root.cut_run(run, tmp_path / "roots3", 0.0, 7200.0, seed=(tmp_path / "nope.toml", db2))
    assert not (out_bare / claude_worker.seeds.FUNDING_SEED_FILE).exists()
