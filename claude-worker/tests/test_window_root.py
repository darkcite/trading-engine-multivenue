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
