# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6: depth_digest + the pmlr kind-7 DepthReader.

Fixtures pack through the reader's own structs (craft.py precedent —
no second layout to drift). Convention: full ``import x`` only.
"""

import pathlib
import sqlite3
import struct

import claude_worker.depth_digest
import claude_worker.iv_digest
import claude_worker.pmlr
import tests.craft

_HDR = claude_worker.pmlr.HEADER_SIZE
_DSLOT = claude_worker.pmlr.DEPTH_SLOT_SIZE

EPOCH_NS = 1_700_000_000_000_000_000
EPOCH_MS = EPOCH_NS // 1_000_000
ANCHOR_TS = 5_000_000_000
MS_1H = claude_worker.depth_digest.MS_1H

OKX_SYM = 0x0200_0001


def write_depth(
    path: pathlib.Path,
    epoch_ns: int,
    recs: list[tuple[int, int, int, int, list[tuple[int, int]], list[tuple[int, int]]]],
) -> None:
    """One v2 kind-7 file. ``recs`` = (ts_ns, sym, venue, flags, bids,
    asks) with ≤5 (px_1e6, qty_1e6) levels per side, EMPTY-padded."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC,
        2,
        claude_worker.pmlr.SLOT_KIND_DEPTH,
        epoch_ns,
    )
    blob = bytearray(header + bytes(_HDR - len(header)))
    for ts, sym, venue, flags, bids, asks in recs:
        levels: list[int] = []
        for side in (bids, asks):
            padded = list(side) + [(0, 0)] * (claude_worker.pmlr.DEPTH_K - len(side))
            for px, qty in padded:
                levels.extend((px, qty))
        slot = claude_worker.pmlr._DEPTH.pack(  # noqa: SLF001
            ts, sym, venue, claude_worker.pmlr.DEPTH_K, flags, *levels
        )
        blob.extend(slot + bytes(_DSLOT - len(slot)))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(blob))


def make_run(
    tmp_path: pathlib.Path,
    manifest_lines: list[str] | None,
    recs: list[tuple[int, int, int, int, list[tuple[int, int]], list[tuple[int, int]]]],
) -> pathlib.Path:
    run_dir = tmp_path / "logs" / f"run-{EPOCH_NS}"
    run_dir.mkdir(parents=True, exist_ok=True)
    tests.craft.write_ticks(run_dir / "okx-ticks.pmlr", [ANCHOR_TS], EPOCH_NS)
    write_depth(run_dir / "okx-depth.pmlr", EPOCH_NS, recs)
    if manifest_lines is not None:
        (run_dir / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
            "".join(line + "\n" for line in manifest_lines), encoding="utf-8"
        )
    return tmp_path / "logs"


def _book(
    ts: int,
    flags: int = 0,
    bids: list[tuple[int, int]] | None = None,
    asks: list[tuple[int, int]] | None = None,
):
    return (
        ts,
        OKX_SYM,
        2,
        flags,
        bids if bids is not None else [(99_000_000, 2_000_000), (98_000_000, 1_000_000)],
        asks if asks is not None else [(101_000_000, 1_000_000)],
    )


MANIFEST = [f"{OKX_SYM}\tokx:BTC-USDT-SWAP"]


# ---- reader / layout ------------------------------------------------------


def test_depth_layout_pin_matches_wire_format_offsets(tmp_path):
    """Field offsets exactly as the docs/wire-format.md DepthTopK
    table (192-byte slots — the FIRST kind-determined slot size):
    bids at 16, asks at 96, 16-byte tail pad."""
    slot = bytearray(_DSLOT)
    struct.pack_into("<Q", slot, 0, 777)  # ts_ns
    struct.pack_into("<I", slot, 8, OKX_SYM)  # sym
    struct.pack_into("<B", slot, 12, 2)  # venue
    struct.pack_into("<B", slot, 13, 5)  # k
    struct.pack_into("<B", slot, 14, 1)  # flags (STALE)
    struct.pack_into("<qq", slot, 16, 99_000_000, 3_000_000)  # bids[0]
    struct.pack_into("<qq", slot, 96, 101_000_000, 4_000_000)  # asks[0]
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_DEPTH, EPOCH_NS
    )
    path = tmp_path / "okx-depth.pmlr"
    path.write_bytes(header + bytes(_HDR - len(header)) + bytes(slot))
    with claude_worker.pmlr.DepthReader(path) as reader:
        assert reader.version == 2 and reader.epoch_ns == EPOCH_NS
        assert len(reader) == 1
        rec = reader.record(0)
    assert rec.ts_ns == 777 and rec.sym == OKX_SYM and rec.venue == 2
    assert rec.k == 5 and rec.flags == claude_worker.pmlr.DEPTH_FLAG_STALE
    assert rec.bids[0] == (99_000_000, 3_000_000)
    assert rec.bids[1] == (0, 0)
    assert rec.asks[0] == (101_000_000, 4_000_000)


def test_depth_reader_refuses_other_kinds_and_reader_refuses_kind_7(tmp_path):
    tick_path = tmp_path / "okx-ticks.pmlr"
    tests.craft.write_ticks(tick_path, [1], EPOCH_NS)
    depth_path = tmp_path / "okx-depth.pmlr"
    write_depth(depth_path, EPOCH_NS, [_book(1)])
    try:
        claude_worker.pmlr.DepthReader(tick_path)
        raise AssertionError("DepthReader accepted a tick file")
    except claude_worker.pmlr.PmlrError:
        pass
    try:
        claude_worker.pmlr.Reader(depth_path)
        raise AssertionError("Reader accepted a kind-7 file")
    except claude_worker.pmlr.PmlrError:
        pass


def test_depth_reader_tolerates_torn_trailing_slot(tmp_path):
    path = tmp_path / "okx-depth.pmlr"
    write_depth(path, EPOCH_NS, [_book(1), _book(2)])
    blob = path.read_bytes()
    path.write_bytes(blob[: _HDR + _DSLOT + 40])  # second slot torn
    with claude_worker.pmlr.DepthReader(path) as reader:
        assert len(reader) == 1
        assert reader.torn is True


# ---- measure --------------------------------------------------------------


def test_measure_imbalance_spread_and_near_notional():
    rec = claude_worker.pmlr.DepthRec(
        1,
        OKX_SYM,
        2,
        5,
        0,
        ((99_000_000, 2_000_000), (98_000_000, 1_000_000), (0, 0), (0, 0), (0, 0)),
        ((101_000_000, 1_000_000), (0, 0), (0, 0), (0, 0), (0, 0)),
    )
    measured = claude_worker.depth_digest.measure(rec)
    assert measured is not None
    imb, spread_bps, near = measured
    # (3e6 − 1e6) / 4e6 = 0.5
    assert imb == 0.5
    # (101 − 99) / 100 × 1e4 = 200 bps
    assert spread_bps == 200.0
    # (99×2 + 98×1 + 101×1) in px_1e6×qty_1e6 ÷ 1e12 = 397.0
    assert near == 397.0


def test_measure_refuses_empty_side():
    rec = claude_worker.pmlr.DepthRec(
        1, OKX_SYM, 2, 5, 0,
        ((99_000_000, 1_000_000), (0, 0), (0, 0), (0, 0), (0, 0)),
        ((0, 0), (0, 0), (0, 0), (0, 0), (0, 0)),
    )
    assert claude_worker.depth_digest.measure(rec) is None


# ---- fold -----------------------------------------------------------------


def fold(root: pathlib.Path, lo_ms: int = 0, now_ms: int = EPOCH_MS + MS_1H):
    lines: list[str] = []
    snaps, stats = claude_worker.depth_digest.fold_depth_snapshots(
        root, lo_ms, now_ms, lines.append
    )
    return snaps, stats, lines


def test_fold_two_snapshots_one_bucket(tmp_path):
    """Two same-hour snapshots aggregate: imb OHLC over both, n=2."""
    recs = [
        _book(ANCHOR_TS + 1_000_000_000),
        _book(
            ANCHOR_TS + 2_000_000_000,
            bids=[(100_000_000, 1_000_000)],
            asks=[(102_000_000, 3_000_000)],
        ),
    ]
    root = make_run(tmp_path, MANIFEST, recs)
    snaps, stats, _ = fold(root)
    assert stats.records == 2 and stats.unresolved == 0
    bucket_ms = (EPOCH_NS // 1_000_000 + 1_000) // MS_1H * MS_1H
    key = (2, "okx:BTC-USDT-SWAP", "1h", bucket_ms)
    assert key in snaps
    snap = snaps[key]
    assert snap.n == 2
    assert snap.imb_o == 0.5
    assert snap.imb_c == -0.5
    assert snap.imb_h == 0.5 and snap.imb_l == -0.5


def test_fold_skips_stale_snapshots(tmp_path):
    recs = [
        _book(ANCHOR_TS + 1_000_000_000, flags=claude_worker.pmlr.DEPTH_FLAG_STALE),
        _book(ANCHOR_TS + 2_000_000_000),
    ]
    root = make_run(tmp_path, MANIFEST, recs)
    snaps, stats, _ = fold(root)
    assert stats.stale_skipped == 1
    assert sum(s.n for s in snaps.values()) == 1


def test_fold_skips_manifestless_run_with_report(tmp_path):
    root = make_run(tmp_path, None, [_book(ANCHOR_TS + 1_000_000_000)])
    snaps, stats, lines = fold(root)
    assert snaps == {}
    assert stats.runs_no_manifest == 1
    assert any("pre-manifest" in line for line in lines)


def test_fold_counts_unresolved_syms(tmp_path):
    other = [f"{OKX_SYM + 7}\tokx:ETH-USDT-SWAP"]
    root = make_run(tmp_path, other, [_book(ANCHOR_TS + 1_000_000_000)])
    snaps, stats, _ = fold(root)
    assert snaps == {}
    assert stats.unresolved == 1


# ---- upsert + main --------------------------------------------------------


def test_upsert_insert_refresh_unchanged(tmp_path):
    conn = claude_worker.depth_digest.open_db(tmp_path / "candles.db")
    snap = claude_worker.depth_digest.Snapshot(0.5, 0.5, -0.5, -0.5, 400.0, 794.0, 2, 0)
    key = (2, "okx:BTC-USDT-SWAP", "1h", 1_000_000)
    ins, ref, unc = claude_worker.depth_digest.upsert_snapshots(conn, {key: snap}, 1)
    assert (ins, ref, unc) == (1, 0, 0)
    ins, ref, unc = claude_worker.depth_digest.upsert_snapshots(conn, {key: snap}, 2)
    assert (ins, ref, unc) == (0, 0, 1)
    grown = snap._replace(n=3, spread_sum=600.0)
    ins, ref, unc = claude_worker.depth_digest.upsert_snapshots(conn, {key: grown}, 3)
    assert (ins, ref, unc) == (0, 1, 0)
    row = conn.execute(
        "SELECT spread_bps_avg, near_notional_avg, n FROM depth_digest"
    ).fetchone()
    assert row == (200.0, 794.0 / 3, 3)
    conn.close()


def test_main_end_to_end(tmp_path, capsys):
    root = make_run(tmp_path, MANIFEST, [_book(ANCHOR_TS + 1_000_000_000)])
    db = tmp_path / "candles.db"
    rc = claude_worker.depth_digest.main(
        [
            "--db", str(db),
            "--replay-dir", str(root),
            "--now-ms", str(EPOCH_MS + MS_1H),
            "--backfill",
        ]
    )
    assert rc == 0
    err = capsys.readouterr().err
    assert "buckets=1 +1" in err
    conn = sqlite3.connect(db)
    n = conn.execute("SELECT count(*) FROM depth_digest").fetchone()[0]
    conn.close()
    assert n == 1
