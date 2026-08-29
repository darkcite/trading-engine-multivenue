# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6: the seed lane (kind-10 FundingSeed / kind-11 PositionSeed)
+ the pmlr kind-3 Order decode + the ÷8 cross-language mirror pin.

Convention: full ``import x`` only.
"""

import json
import pathlib
import sqlite3
import struct

import claude_worker.carry_signal
import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.pmlr
import claude_worker.seeds
import tests.craft

_HDR = claude_worker.pmlr.HEADER_SIZE
_SLOT = claude_worker.pmlr.SLOT_SIZE

EPOCH_NS = 1_700_000_000_000_000_000
EPOCH_MS = EPOCH_NS // 1_000_000
ANCHOR_TS = 5_000_000_000
MS_1H = claude_worker.seeds.MS_1H

USDM_SYM = 0x0100_0200
OKX_SYM = 0x0200_0001
SPOT_SYM = 7


def test_frame_kind_constants_pin_core_types():
    """AiCmdKind 10/11 (core-types; wire-stable, never renumber)."""
    assert claude_worker.frames.KIND_FUNDING_SEED == 10
    assert claude_worker.frames.KIND_POSITION_SEED == 11


def test_deribit_divisor_mirrors_engine_law():
    """The ÷8 cadence law lives in TWO places by design — the engine's
    ``core_types::funding_print_divisor`` (seeds arrive RAW, kind 10)
    and the worker's ``carry_signal.apr_from_prints`` (research side).
    Pin the worker half: one 24h-window print of rate r ⇒ deribit
    r/8×365, non-deribit r×365 — exactly the engine's
    ``(Σ×365)/divisor`` Apr24 law."""
    rows = [(1_000, 0.0008)]
    deribit = claude_worker.carry_signal.apr_from_prints(
        rows, 86_400_000, 86_400_000, "deribit:BTC-PERPETUAL"
    )
    binance = claude_worker.carry_signal.apr_from_prints(
        rows, 86_400_000, 86_400_000, "binance-usdm:btcusdt"
    )
    assert deribit == 0.0008 / 8.0 * 365.0
    assert binance == 0.0008 * 365.0


# ---- kind-3 Order decode --------------------------------------------------


def write_orders(
    path: pathlib.Path,
    epoch_ns: int,
    recs: list[tuple[int, int, int, int, int, int]],
) -> None:
    """One v2 kind-3 file. ``recs`` = (ts_ns, sym, side, px_1e6,
    qty_1e6, strategy_id)."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_ORDER, epoch_ns
    )
    blob = bytearray(header + bytes(_HDR - len(header)))
    for i, (ts, sym, side, px, qty, strategy_id) in enumerate(recs):
        slot = claude_worker.pmlr._ORDER.pack(  # noqa: SLF001
            ts, sym, side, 0, px, qty, i + 1, 5, strategy_id
        )
        blob.extend(slot + bytes(_SLOT - len(slot)))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(blob))


def test_order_layout_pin_matches_wire_format_offsets(tmp_path):
    """Field offsets exactly as the docs/wire-format.md Order table:
    side@12, px@16, qty@24, client_oid@32, venue@40,
    strategy_id@41."""
    slot = bytearray(_SLOT)
    struct.pack_into("<Q", slot, 0, 999)  # ts_ns
    struct.pack_into("<I", slot, 8, USDM_SYM)  # sym
    struct.pack_into("<B", slot, 12, 1)  # side ASK
    struct.pack_into("<B", slot, 13, 0)  # kind
    struct.pack_into("<q", slot, 16, 50_000_000_000)  # px
    struct.pack_into("<q", slot, 24, 2_000_000)  # qty
    struct.pack_into("<Q", slot, 32, 77)  # client_oid
    struct.pack_into("<B", slot, 40, 1)  # venue
    struct.pack_into("<B", slot, 41, 5)  # strategy_id (VM)
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_ORDER, EPOCH_NS
    )
    path = tmp_path / "engine-orders.pmlr"
    path.write_bytes(header + bytes(_HDR - len(header)) + bytes(slot))
    with claude_worker.pmlr.Reader(path) as reader:
        rec = reader.order(0)
    assert rec == claude_worker.pmlr.OrderRec(
        999, USDM_SYM, 1, 0, 50_000_000_000, 2_000_000, 77, 1, 5
    )


# ---- funding seed frames --------------------------------------------------


def _funding_db(tmp_path: pathlib.Path, rows: list[tuple[int, str, int, float]]):
    db = tmp_path / "candles.db"
    conn = sqlite3.connect(db)
    conn.execute(
        "CREATE TABLE funding (venue INTEGER NOT NULL, descriptor TEXT NOT NULL,"
        " ts_ms INTEGER NOT NULL, rate REAL NOT NULL, fetched_ts INTEGER NOT NULL,"
        " PRIMARY KEY (venue, descriptor, ts_ms)) WITHOUT ROWID"
    )
    for venue, desc, ts_ms, rate in rows:
        conn.execute(
            "INSERT INTO funding VALUES (?,?,?,?,0)", (venue, desc, ts_ms, rate)
        )
    conn.commit()
    return db, conn


def test_funding_frames_raw_rate_law_and_caps_filter(tmp_path):
    """Deribit rows (stored interest_8h VERBATIM) go out RAW ×1e9 —
    the ENGINE divides. Spot descriptors (no CAP_FUNDING) never
    seed."""
    now_ms = EPOCH_MS
    _db, conn = _funding_db(
        tmp_path,
        [
            (3, "deribit:BTC-PERPETUAL", now_ms - MS_1H, 0.0008),
            (1, "binance:btcusdt", now_ms - MS_1H, 0.0001),  # spot: never seeded
        ],
    )
    manifest = {
        (3, 0x0300_0001): "deribit:BTC-PERPETUAL",
        (0, SPOT_SYM): "binance:btcusdt",
    }
    frames, stats = claude_worker.seeds.funding_seed_frames(conn, manifest, now_ms)
    conn.close()
    assert stats == claude_worker.seeds.FundingStats(1, 1, 0)
    f = frames[0]
    assert f.kind == claude_worker.frames.KIND_FUNDING_SEED
    assert f.sym == 0x0300_0001
    assert f.px == 800_000  # 0.0008 × 1e9 RAW — NOT ÷8
    assert f.qty == now_ms - MS_1H
    assert f.side == claude_worker.frames.SIDE_NONE


def test_funding_frames_window_and_order(tmp_path):
    now_ms = EPOCH_MS
    rows = [
        (1, "binance-usdm:btcusdt", now_ms - i * MS_1H, 0.0001 * (i + 1))
        for i in range(100)
    ]
    _db, conn = _funding_db(tmp_path, rows)
    manifest = {(1, USDM_SYM): "binance-usdm:btcusdt"}
    frames, stats = claude_worker.seeds.funding_seed_frames(conn, manifest, now_ms)
    conn.close()
    # 73h window keeps prints i=0..72 (73 of them) — under the 640 cap.
    assert stats.capped == 0
    assert len(frames) == 73
    assert frames[0].qty < frames[-1].qty  # oldest first
    assert all(f.qty >= now_ms - 73 * MS_1H for f in frames)


def test_funding_frames_engine_capacity_cap_keeps_newest(tmp_path):
    """> 640 in-window prints (5-minute cadence fixture) truncate to
    the NEWEST 640 — the engine's per-sym FUNDING_BLOCKS capacity."""
    now_ms = EPOCH_MS
    step_ms = 5 * 60 * 1000
    rows = [
        (1, "binance-usdm:btcusdt", now_ms - i * step_ms, 0.0001)
        for i in range(700)  # ~58h span — all inside the 73h window
    ]
    _db, conn = _funding_db(tmp_path, rows)
    manifest = {(1, USDM_SYM): "binance-usdm:btcusdt"}
    frames, stats = claude_worker.seeds.funding_seed_frames(conn, manifest, now_ms)
    conn.close()
    assert stats.capped == 1
    assert len(frames) == claude_worker.seeds.MAX_PRINTS_PER_SYM
    # Window is now-EXCLUSIVE (ts < now, the digest convention) — the
    # i=0 print AT now_ms is out; newest survivor is one step back.
    assert frames[-1].qty == now_ms - step_ms
    assert frames[0].qty == now_ms - 640 * step_ms  # oldest kept = 640th-newest


# ---- artifact rows --------------------------------------------------------


ARTIFACT = {
    "rows": [
        {"name": "refire", "instrument": "okx:BTC-USDT-SWAP", "feature": "mid",
         "enter": 1.0},
        {"name": "carry", "instrument": "binance-usdm:btcusdt",
         "ref": "okx:BTC-USDT-SWAP", "feature": "apr24", "combine": "diff",
         "enter": 0.2, "exit": 0.0, "max_risk_usd": 100.0},
    ]
}


def test_position_rows_are_the_exit_key_rows():
    rows = claude_worker.seeds.position_rows_of_artifact(json.dumps(ARTIFACT))
    assert rows == [claude_worker.seeds._Row(1, "binance-usdm:btcusdt")]  # noqa: SLF001


def test_position_rows_refuse_malformed_artifact():
    assert claude_worker.seeds.position_rows_of_artifact("]{") is None
    assert claude_worker.seeds.position_rows_of_artifact('{"rows": 3}') is None
    assert (
        claude_worker.seeds.position_rows_of_artifact('{"rows": [{"exit": 0.0}]}')
        is None
    )  # position row without instrument


# ---- fold_vm_orders -------------------------------------------------------


def _order(ts, sym, side, px, qty, sid=5):
    return claude_worker.pmlr.OrderRec(ts, sym, side, 0, px, qty, 0, 1, sid)


def test_fold_flat_and_filters():
    orders = [
        _order(1, USDM_SYM, 0, 100_000_000, 1_000_000),
        _order(2, USDM_SYM, 1, 101_000_000, 1_000_000),  # closes
        _order(3, USDM_SYM, 0, 99_000_000, 1_000_000, sid=4),  # not VM
        _order(4, OKX_SYM, 0, 98_000_000, 1_000_000),  # other sym
    ]
    assert claude_worker.seeds.fold_vm_orders(orders, USDM_SYM) is None


def test_fold_open_long_fifo_vwap_and_age_ts():
    orders = [
        _order(10, USDM_SYM, 0, 100_000_000, 2_000_000),
        _order(20, USDM_SYM, 0, 110_000_000, 2_000_000),
        _order(30, USDM_SYM, 1, 120_000_000, 1_000_000),  # FIFO-reduces the 100 lot
    ]
    basket = claude_worker.seeds.fold_vm_orders(orders, USDM_SYM)
    assert basket is not None
    assert basket.side == claude_worker.frames.SIDE_BID
    # Surviving: 1e6 @ 100 + 2e6 @ 110 → vwap 106.666667e6 (floor)
    assert basket.vwap_px_1e6 == (100_000_000 * 1 + 110_000_000 * 2) // 3
    assert basket.last_entry_ts_ns == 20


def test_fold_sign_flip_opens_fresh_basket():
    orders = [
        _order(10, USDM_SYM, 0, 100_000_000, 1_000_000),
        _order(20, USDM_SYM, 1, 90_000_000, 3_000_000),  # flips to −2e6
    ]
    basket = claude_worker.seeds.fold_vm_orders(orders, USDM_SYM)
    assert basket is not None
    assert basket.side == claude_worker.frames.SIDE_ASK
    assert basket.vwap_px_1e6 == 90_000_000
    assert basket.last_entry_ts_ns == 20


# ---- position_seed_frames -------------------------------------------------


def _two_runs(tmp_path: pathlib.Path, prev_orders, cur_sym: int):
    """prev run (with orders + manifest) + current run (manifest with
    a RESHUFFLED sym for the same descriptor)."""
    root = tmp_path / "logs"
    prev_epoch = EPOCH_NS
    cur_epoch = EPOCH_NS + 3_600 * 1_000_000_000
    prev_run = root / f"run-{prev_epoch}"
    cur_run = root / f"run-{cur_epoch}"
    prev_run.mkdir(parents=True)
    cur_run.mkdir(parents=True)
    tests.craft.write_ticks(prev_run / "bn-ticks.pmlr", [ANCHOR_TS], prev_epoch)
    tests.craft.write_ticks(cur_run / "bn-ticks.pmlr", [ANCHOR_TS], cur_epoch)
    write_orders(prev_run / "engine-orders.pmlr", prev_epoch, prev_orders)
    (prev_run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        f"{USDM_SYM}\tbinance-usdm:btcusdt\n", encoding="utf-8"
    )
    (cur_run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        f"{cur_sym}\tbinance-usdm:btcusdt\n", encoding="utf-8"
    )
    return prev_run, cur_run


def test_position_seed_reresolves_sym_and_computes_age(tmp_path):
    entry_ts = ANCHOR_TS + 1_000_000_000
    prev_run, cur_run = _two_runs(
        tmp_path,
        [(entry_ts, USDM_SYM, 0, 100_000_000, 2_000_000, 5)],
        cur_sym=USDM_SYM + 3,  # ordinals reshuffle per boot (§1.4)
    )
    rows = [claude_worker.seeds._Row(1, "binance-usdm:btcusdt")]  # noqa: SLF001
    now_ns = EPOCH_NS + 7_200 * 1_000_000_000
    lines: list[str] = []
    frames, stats = claude_worker.seeds.position_seed_frames(
        rows,
        prev_run,
        claude_worker.iv_digest.read_manifest(prev_run)[0],
        claude_worker.iv_digest.read_manifest(cur_run)[0],
        now_ns,
        lines.append,
    )
    assert stats.seeded == 1 and stats.flat == 0
    f = frames[0]
    assert f.kind == claude_worker.frames.KIND_POSITION_SEED
    assert f.sym == USDM_SYM + 3  # the CURRENT boot's sym
    assert f.px == 100_000_000
    assert f.side == claude_worker.frames.SIDE_BID
    assert f.param_id == 1
    # wall(entry) = prev_epoch + (entry_ts − anchor) = EPOCH_NS + 1s
    assert f.qty == 7_200 - 1


def test_position_seed_ambiguous_action_descriptor_skips_both(tmp_path):
    prev_run, cur_run = _two_runs(
        tmp_path,
        [(ANCHOR_TS + 1, USDM_SYM, 0, 100_000_000, 2_000_000, 5)],
        cur_sym=USDM_SYM,
    )
    rows = [
        claude_worker.seeds._Row(0, "binance-usdm:btcusdt"),  # noqa: SLF001
        claude_worker.seeds._Row(2, "binance-usdm:btcusdt"),  # noqa: SLF001
    ]
    lines: list[str] = []
    frames, stats = claude_worker.seeds.position_seed_frames(
        rows,
        prev_run,
        claude_worker.iv_digest.read_manifest(prev_run)[0],
        claude_worker.iv_digest.read_manifest(cur_run)[0],
        EPOCH_NS,
        lines.append,
    )
    assert frames == []
    assert stats.ambiguous == 2
    assert sum("AMBIGUOUS" in line for line in lines) == 2


def test_position_seed_descriptor_gone_from_current_universe(tmp_path):
    prev_run, cur_run = _two_runs(
        tmp_path,
        [(ANCHOR_TS + 1, USDM_SYM, 0, 100_000_000, 2_000_000, 5)],
        cur_sym=USDM_SYM,
    )
    (cur_run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        f"{OKX_SYM}\tokx:ETH-USDT-SWAP\n", encoding="utf-8"
    )
    rows = [claude_worker.seeds._Row(1, "binance-usdm:btcusdt")]  # noqa: SLF001
    lines: list[str] = []
    frames, stats = claude_worker.seeds.position_seed_frames(
        rows,
        prev_run,
        claude_worker.iv_digest.read_manifest(prev_run)[0],
        claude_worker.iv_digest.read_manifest(cur_run)[0],
        EPOCH_NS,
        lines.append,
    )
    assert frames == []
    assert stats.unresolved_cur == 1
    assert any("absent from CURRENT universe" in line for line in lines)


# ---- main (dry-run E2E; no socket, no config) -----------------------------


def test_main_dry_run_end_to_end(tmp_path, capsys):
    entry_ts = ANCHOR_TS + 1_000_000_000
    prev_run, cur_run = _two_runs(
        tmp_path,
        [(entry_ts, USDM_SYM, 0, 100_000_000, 2_000_000, 5)],
        cur_sym=USDM_SYM,
    )
    db, conn = _funding_db(
        tmp_path, [(1, "binance-usdm:btcusdt", EPOCH_MS + 3_600_000, 0.0002)]
    )
    conn.close()
    artifact = tmp_path / "ruleset.json"
    artifact.write_text(json.dumps(ARTIFACT), encoding="utf-8")
    now_ms = EPOCH_MS + 2 * 3_600_000
    rc = claude_worker.seeds.main(
        [
            "--db", str(db),
            "--replay-dir", str(tmp_path / "logs"),
            "--ruleset", str(artifact),
            "--now-ms", str(now_ms),
            "--dry-run",
        ]
    )
    assert rc == 0
    out = capsys.readouterr()
    lines = [line for line in out.out.splitlines() if line.startswith("kind=")]
    assert len(lines) == 2  # one funding print + one position seed
    assert any(f"kind={claude_worker.frames.KIND_FUNDING_SEED} " in line for line in lines)
    assert any(f"kind={claude_worker.frames.KIND_POSITION_SEED} " in line for line in lines)
    assert "NOT sent" in out.err
