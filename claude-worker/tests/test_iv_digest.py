"""iv_digest tests (M2 close, §9.8): kind-6 container decode pinned to
docs/wire-format.md offsets, manifest resolution (venue+descriptor law,
never bare SymbolId), harness-§3.3 wall mapping, 1m/1h snapshot
aggregation, the flags asymmetry (OKX 0/0, Deribit full, BN
`binance-opt:` namespace), cache-refresh upsert semantics, and the
best-effort skips (no manifest / no anchor / header-only / window).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3
import struct

import tests.craft

import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.pmlr

_HDR = claude_worker.pmlr.HEADER_SIZE
_SLOT = claude_worker.pmlr.SLOT_SIZE

# One deliberately "realistic" sym per venue (base-512 options block).
DERIBIT_SYM = 0x0300_0201
OKX_SYM = 0x0200_0201
BN_SYM = 0x0100_0401

EPOCH_NS = 1_700_000_000_000_000_000
EPOCH_MS = EPOCH_NS // 1_000_000
ANCHOR_TS = 5_000_000_000  # engine-monotonic first tick ts

MS_1M = claude_worker.iv_digest.MS_1M
MS_1H = claude_worker.iv_digest.MS_1H


def write_opt(
    path: pathlib.Path,
    epoch_ns: int,
    recs: list[tuple[int, int, int, int, int, int, int, int]],
) -> None:
    """One v2 kind-6 file. ``recs`` = (ts_ns, sym, venue, flags,
    mark_px_1e9, mark_iv_1e9, underlying_px_1e9, oi_1e6); greeks are
    stamped with distinct constants (decode covered by the layout
    pin test)."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately (craft.py precedent)
        claude_worker.pmlr.MAGIC,
        2,
        claude_worker.iv_digest.SLOT_KIND_OPT_SUMMARY,
        epoch_ns,
    )
    blob = bytearray(header + bytes(_HDR - len(header)))
    for ts, sym, venue, flags, mark, iv, uly, oi in recs:
        blob.extend(
            claude_worker.iv_digest._OPT.pack(  # noqa: SLF001
                ts, sym, venue, flags, mark, iv, uly, oi, 7, 8, 9, 10
            )
        )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(blob))


def make_run(
    tmp_path: pathlib.Path,
    manifest_lines: list[str] | None,
    label: str,
    recs: list[tuple[int, int, int, int, int, int, int, int]],
) -> pathlib.Path:
    """One run dir: anchor tick file + one opt-summary file
    (+ manifest unless None)."""
    run_dir = tmp_path / "logs" / f"run-{EPOCH_NS}"
    run_dir.mkdir(parents=True, exist_ok=True)
    tests.craft.write_ticks(run_dir / "deribit-ticks.pmlr", [ANCHOR_TS], EPOCH_NS)
    write_opt(run_dir / f"{label}-opt-summary.pmlr", EPOCH_NS, recs)
    if manifest_lines is not None:
        (run_dir / claude_worker.iv_digest.MANIFEST_FILE).write_text(
            "".join(line + "\n" for line in manifest_lines), encoding="utf-8"
        )
    return tmp_path / "logs"


def fold(root: pathlib.Path, lo_ms: int = 0, now_ms: int = EPOCH_MS + MS_1H):
    lines: list[str] = []
    snaps, stats = claude_worker.iv_digest.fold_iv_snapshots(
        root, lo_ms, now_ms, lines.append
    )
    return snaps, stats, lines


def wall(ts_ns: int) -> int:
    return (EPOCH_NS + (ts_ns - ANCHOR_TS)) // 1_000_000


# ---- container / layout ---------------------------------------------------


def test_layout_pin_matches_wire_format_offsets(tmp_path):
    """Field offsets exactly as the docs/wire-format.md OptSummary
    table — built by pack_into at the documented offsets, decoded by
    the module's struct."""
    slot = bytearray(_SLOT)
    struct.pack_into("<Q", slot, 0, 111)  # ts_ns
    struct.pack_into("<I", slot, 8, DERIBIT_SYM)  # sym
    struct.pack_into("<B", slot, 12, 3)  # venue
    struct.pack_into("<B", slot, 13, 3)  # flags
    struct.pack_into("<q", slot, 16, 51_200_000)  # mark_px_1e9
    struct.pack_into("<q", slot, 24, 400_000_000)  # mark_iv_1e9
    struct.pack_into("<q", slot, 32, 77_000_000_000_000)  # underlying_px_1e9
    struct.pack_into("<q", slot, 40, 1_234_500_000)  # open_interest_1e6
    struct.pack_into("<i", slot, 48, -7)  # delta_1e9
    struct.pack_into("<i", slot, 52, 8)  # gamma_1e9
    struct.pack_into("<i", slot, 56, 9)  # vega_1e6
    struct.pack_into("<i", slot, 60, -10)  # theta_1e6
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001
        claude_worker.pmlr.MAGIC,
        2,
        claude_worker.iv_digest.SLOT_KIND_OPT_SUMMARY,
        EPOCH_NS,
    )
    path = tmp_path / "deribit-opt-summary.pmlr"
    path.write_bytes(header + bytes(_HDR - len(header)) + bytes(slot))
    with claude_worker.iv_digest.OptReader(path) as reader:
        assert reader.version == 2 and reader.epoch_ns == EPOCH_NS
        rec = reader.record(0)
    assert rec == claude_worker.iv_digest.OptRec(
        111, DERIBIT_SYM, 3, 3, 51_200_000, 400_000_000,
        77_000_000_000_000, 1_234_500_000, -7, 8, 9, -10,
    )


def test_opt_reader_refuses_tick_kind_and_pmlr_reader_refuses_kind_6(tmp_path):
    """Cross pin of the deliberate split: pmlr.Reader does not know
    kind 6 (fold-in flagged for the next M3 window); OptReader accepts
    ONLY kind 6."""
    tick_path = tmp_path / "deribit-ticks.pmlr"
    tests.craft.write_ticks(tick_path, [1], EPOCH_NS)
    opt_path = tmp_path / "deribit-opt-summary.pmlr"
    write_opt(opt_path, EPOCH_NS, [(1, DERIBIT_SYM, 3, 3, 1, 1, 1, 1)])
    try:
        claude_worker.iv_digest.OptReader(tick_path)
        raise AssertionError("OptReader accepted a tick file")
    except claude_worker.pmlr.PmlrError:
        pass
    try:
        claude_worker.pmlr.Reader(opt_path)
        raise AssertionError("pmlr.Reader accepted kind 6")
    except claude_worker.pmlr.PmlrError:
        pass


def test_torn_tail_is_tolerated(tmp_path):
    path = tmp_path / "okx-opt-summary.pmlr"
    write_opt(path, EPOCH_NS, [(1, OKX_SYM, 2, 0, 0, 1, 1, 0)])
    with path.open("ab") as f:
        f.write(b"\x00" * 10)  # partial trailing slot (engine mid-flush)
    with claude_worker.iv_digest.OptReader(path) as reader:
        assert len(reader) == 1 and reader.torn


# ---- manifest -------------------------------------------------------------


def test_manifest_strict_parse_counts_malformed(tmp_path):
    run_dir = tmp_path / f"run-{EPOCH_NS}"
    run_dir.mkdir(parents=True)
    (run_dir / claude_worker.iv_digest.MANIFEST_FILE).write_text(
        f"deribit\t{DERIBIT_SYM}\tBTC-27MAR26-100000-C\n"
        "not a manifest line\n"
        "deribit\tnotanumber\tX\n"
        "venus\t42\tX\n"
        "deribit\t0\tX\n",
        encoding="utf-8",
    )
    parsed = claude_worker.iv_digest.read_manifest(run_dir)
    assert parsed is not None
    sym_map, malformed = parsed
    assert malformed == 4
    assert sym_map == {
        (claude_worker.frames.VENUE_DERIBIT, DERIBIT_SYM): "deribit:BTC-27MAR26-100000-C"
    }


def test_manifest_absent_is_none(tmp_path):
    run_dir = tmp_path / f"run-{EPOCH_NS}"
    run_dir.mkdir(parents=True)
    assert claude_worker.iv_digest.read_manifest(run_dir) is None


# ---- fold -----------------------------------------------------------------


def test_fold_happy_path_deribit_1m_and_1h(tmp_path):
    """Three records, wall-mapped through a NONZERO anchor: two in one
    minute, one two minutes later; IV o/h/l/c + n + last-context per
    bucket; both timeframes."""
    recs = [
        (ANCHOR_TS, DERIBIT_SYM, 3, 3, 51_200_000, 400_000_000, 77_000_000_000_000, 1_000_000_000),
        (ANCHOR_TS + 30_000_000_000, DERIBIT_SYM, 3, 3, 52_000_000, 500_000_000, 77_100_000_000_000, 1_100_000_000),
        (ANCHOR_TS + 90_000_000_000, DERIBIT_SYM, 3, 3, 49_000_000, 450_000_000, 76_900_000_000_000, 1_200_000_000),
    ]
    root = make_run(
        tmp_path,
        [f"deribit\t{DERIBIT_SYM}\tBTC-27MAR26-100000-C"],
        "deribit",
        recs,
    )
    snaps, stats, _ = fold(root)
    assert stats.records == 3 and stats.unresolved_records == 0
    desc = "deribit:BTC-27MAR26-100000-C"
    venue = claude_worker.frames.VENUE_DERIBIT
    b0 = wall(recs[0][0]) - wall(recs[0][0]) % MS_1M
    b2 = wall(recs[2][0]) - wall(recs[2][0]) % MS_1M
    assert b2 > b0
    s0 = snaps[(venue, desc, "1m", b0)]
    assert (s0.iv_o, s0.iv_h, s0.iv_l, s0.iv_c, s0.n) == (0.4, 0.5, 0.4, 0.5, 2)
    assert (s0.underlying_c, s0.mark_px_c, s0.oi_c) == (77_100.0, 0.052, 1_100.0)
    s2 = snaps[(venue, desc, "1m", b2)]
    assert (s2.iv_o, s2.iv_h, s2.iv_l, s2.iv_c, s2.n) == (0.45, 0.45, 0.45, 0.45, 1)
    h0 = wall(recs[0][0]) - wall(recs[0][0]) % MS_1H
    sh = snaps[(venue, desc, "1h", h0)]
    assert (sh.iv_o, sh.iv_h, sh.iv_l, sh.iv_c, sh.n) == (0.4, 0.5, 0.4, 0.45, 3)


def test_fold_okx_flags_zero_means_null_context(tmp_path):
    """The venue-asymmetry law: OKX supplies neither mark px nor OI —
    flags 0 ⇒ NULL columns; fwdPx still fills underlying."""
    recs = [(ANCHOR_TS, OKX_SYM, 2, 0, 0, 387_806_395, 77_100_000_000_000, 0)]
    root = make_run(
        tmp_path, [f"okx\t{OKX_SYM}\tBTC-USD-260327-100000-C"], "okx", recs
    )
    snaps, stats, _ = fold(root)
    key = next(k for k in snaps if k[2] == "1m")
    assert key[0] == claude_worker.frames.VENUE_OKX
    assert key[1] == "okx:BTC-USD-260327-100000-C"
    snap = snaps[key]
    assert snap.mark_px_c is None and snap.oi_c is None
    assert snap.underlying_c == 77_100.0
    assert abs(snap.iv_c - 0.387806395) < 1e-12


def test_fold_bn_uses_binance_opt_namespace(tmp_path):
    recs = [(ANCHOR_TS, BN_SYM, 1, 1, 900_000_000_000, 600_000_000, 0, 0)]
    root = make_run(tmp_path, [f"bn\t{BN_SYM}\tBTC-260327-100000-C"], "bn", recs)
    snaps, _, _ = fold(root)
    key = next(k for k in snaps if k[2] == "1m")
    assert key[0] == claude_worker.frames.VENUE_BINANCE
    assert key[1] == "binance-opt:BTC-260327-100000-C"
    snap = snaps[key]
    # mark_px flagged; OI not; underlying 0 on the wire (index cache
    # not yet filled) → NULL.
    assert snap.mark_px_c == 900.0 and snap.oi_c is None and snap.underlying_c is None


def test_fold_no_manifest_run_is_skipped_and_counted(tmp_path):
    recs = [(ANCHOR_TS, DERIBIT_SYM, 3, 3, 1, 400_000_000, 1, 1)]
    root = make_run(tmp_path, None, "deribit", recs)
    snaps, stats, lines = fold(root)
    assert snaps == {} and stats.runs_no_manifest == 1
    assert any("no instrument-manifest.tsv" in line for line in lines)


def test_instrument_manifest_is_preferred_and_two_col_strict(tmp_path):
    """D3: the generalized manifest wins over the legacy options file;
    venue derives from the SymbolId namespace byte; malformed 2-col
    lines counted."""
    recs = [(ANCHOR_TS, DERIBIT_SYM, 3, 3, 1, 400_000_000, 1, 1)]
    root = make_run(
        tmp_path, [f"deribit\t{DERIBIT_SYM}\tOLD-NAME"], "deribit", recs
    )
    run_dir = root / f"run-{EPOCH_NS}"
    (run_dir / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        f"{DERIBIT_SYM}\tderibit:BTC-27MAR26-100000-C\n"
        "notanumber\tx\n"
        f"{DERIBIT_SYM}\n"
        "42\t\n",
        encoding="utf-8",
    )
    parsed = claude_worker.iv_digest.read_manifest(run_dir)
    assert parsed is not None
    sym_map, malformed = parsed
    assert malformed == 3
    assert sym_map == {
        (claude_worker.frames.VENUE_DERIBIT, DERIBIT_SYM): "deribit:BTC-27MAR26-100000-C"
    }
    # The fold resolves through the NEW descriptor, not OLD-NAME.
    snaps, stats, _ = fold(root)
    assert stats.unresolved_records == 0
    assert all(k[1] == "deribit:BTC-27MAR26-100000-C" for k in snaps)


def test_legacy_options_manifest_still_resolves_when_new_file_absent(tmp_path):
    recs = [(ANCHOR_TS, OKX_SYM, 2, 0, 0, 400_000_000, 1, 0)]
    root = make_run(
        tmp_path, [f"okx\t{OKX_SYM}\tBTC-USD-260327-100000-C"], "okx", recs
    )
    snaps, stats, _ = fold(root)
    assert stats.unresolved_records == 0
    assert all(k[1] == "okx:BTC-USD-260327-100000-C" for k in snaps)


def test_fold_no_anchor_run_is_skipped_and_counted(tmp_path):
    recs = [(ANCHOR_TS, DERIBIT_SYM, 3, 3, 1, 400_000_000, 1, 1)]
    root = make_run(
        tmp_path, [f"deribit\t{DERIBIT_SYM}\tX"], "deribit", recs
    )
    (root / f"run-{EPOCH_NS}" / "deribit-ticks.pmlr").unlink()
    snaps, stats, _ = fold(root)
    assert snaps == {} and stats.runs_no_anchor == 1


def test_fold_unresolved_sym_is_counted_not_stored(tmp_path):
    recs = [(ANCHOR_TS, DERIBIT_SYM + 1, 3, 3, 1, 400_000_000, 1, 1)]
    root = make_run(
        tmp_path, [f"deribit\t{DERIBIT_SYM}\tX"], "deribit", recs
    )
    snaps, stats, _ = fold(root)
    assert snaps == {} and stats.unresolved_records == 1


def test_fold_window_excludes_and_counts(tmp_path):
    recs = [
        (ANCHOR_TS, DERIBIT_SYM, 3, 3, 1, 400_000_000, 1, 1),
        (ANCHOR_TS + 120_000_000_000, DERIBIT_SYM, 3, 3, 1, 410_000_000, 1, 1),
    ]
    root = make_run(tmp_path, [f"deribit\t{DERIBIT_SYM}\tX"], "deribit", recs)
    lo = wall(recs[1][0])  # only the second record is in-window
    snaps, stats, _ = fold(root, lo_ms=lo)
    assert stats.windowed_out == 1
    assert all(bucket >= lo - lo % MS_1H for (_, _, _, bucket) in snaps)
    assert sum(1 for k in snaps if k[2] == "1m") == 1


def test_fold_header_only_file_is_clean_skip(tmp_path):
    root = make_run(tmp_path, [f"deribit\t{DERIBIT_SYM}\tX"], "deribit", [])
    snaps, stats, _ = fold(root)
    assert snaps == {} and stats.records == 0 and stats.runs_no_manifest == 0


# ---- store ----------------------------------------------------------------


def test_upsert_insert_refresh_unchanged_cycle(tmp_path):
    recs = [(ANCHOR_TS, DERIBIT_SYM, 3, 3, 51_200_000, 400_000_000, 77_000_000_000_000, 1_000_000_000)]
    root = make_run(tmp_path, [f"deribit\t{DERIBIT_SYM}\tBTC-27MAR26-100000-C"], "deribit", recs)
    db = tmp_path / "candles.db"
    conn = claude_worker.iv_digest.open_db(db)
    try:
        snaps, _, _ = fold(root)
        assert claude_worker.iv_digest.upsert_snapshots(conn, snaps, 1) == (2, 0, 0)
        # Identical re-fold: everything unchanged.
        snaps, _, _ = fold(root)
        assert claude_worker.iv_digest.upsert_snapshots(conn, snaps, 2) == (0, 0, 2)
        # A late record lands in the same buckets: cache refreshes.
        write_opt(
            root / f"run-{EPOCH_NS}" / "deribit-opt-summary.pmlr",
            EPOCH_NS,
            recs + [(ANCHOR_TS + 1_000_000_000, DERIBIT_SYM, 3, 3, 51_000_000, 420_000_000, 77_000_000_000_000, 1_000_000_000)],
        )
        snaps, _, _ = fold(root)
        assert claude_worker.iv_digest.upsert_snapshots(conn, snaps, 3) == (0, 2, 0)
        row = conn.execute(
            "SELECT iv_c, n FROM iv_digest WHERE tf='1m'"
        ).fetchone()
        assert row == (0.42, 2)
    finally:
        conn.close()


def test_open_db_is_additive_beside_candles_schema(tmp_path):
    """Opening an EXISTING candles.db adds only iv_digest — the M3
    candles tables are never created or touched here."""
    db = tmp_path / "candles.db"
    pre = sqlite3.connect(db)
    pre.execute("CREATE TABLE candles (venue INTEGER, x TEXT)")
    pre.execute("INSERT INTO candles VALUES (1, 'keep')")
    pre.commit()
    pre.close()
    conn = claude_worker.iv_digest.open_db(db)
    try:
        names = {
            r[0]
            for r in conn.execute(
                "SELECT name FROM sqlite_master WHERE type='table'"
            )
        }
        assert "iv_digest" in names and "candles" in names
        assert conn.execute("SELECT x FROM candles").fetchone() == ("keep",)
    finally:
        conn.close()


def test_main_end_to_end(tmp_path):
    recs = [(ANCHOR_TS, DERIBIT_SYM, 3, 3, 51_200_000, 400_000_000, 77_000_000_000_000, 1_000_000_000)]
    root = make_run(tmp_path, [f"deribit\t{DERIBIT_SYM}\tBTC-27MAR26-100000-C"], "deribit", recs)
    db = tmp_path / "candles.db"
    rc = claude_worker.iv_digest.main(
        [
            "--db", str(db),
            "--replay-dir", str(root),
            "--now-ms", str(EPOCH_MS + MS_1H),
            "--backfill",
        ]
    )
    assert rc == 0
    conn = sqlite3.connect(db)
    try:
        assert conn.execute("SELECT count(*) FROM iv_digest").fetchone() == (2,)
    finally:
        conn.close()


def test_main_missing_root_is_clean_zero(tmp_path):
    rc = claude_worker.iv_digest.main(
        ["--db", str(tmp_path / "x.db"), "--replay-dir", str(tmp_path / "absent")]
    )
    assert rc == 0
