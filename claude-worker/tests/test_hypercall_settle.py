# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""HC7: the settlement shadow - the Python mirror of ``core_settle``
pinned against the Rust law, the grid's sample-and-hold semantics, and
the shadow end to end over a synthetic capture and a payout pin.

The PIN TABLE mirrors crates/core-settle/src/tests.rs - change either
side only with the other in the same commit.

Convention: full ``import x`` only.
"""

import pathlib
import sqlite3
import struct

import claude_worker.hypercall_history
import claude_worker.hypercall_settle

T = 1_790_366_400_000  # 2026-09-25 20:00Z
U = 1_000_000
IDX_SYM = (9 << 24) | 1
MAKER = "0xe55b5e5e38f73c30aa367d310d6247f3f9a5e86e"


def lcg_series() -> list[int]:
    """The Rust tests' reference series, regenerated bit for bit."""
    s, px, out = 20_260_925, 224_000_000, []
    for i in range(claude_worker.hypercall_settle.GRID_POINTS):
        s = (s * 6_364_136_223_846_793_005 + 1_442_695_040_888_963_407) % (1 << 64)
        px += (s >> 33) % 95_001 - 40_000
        out.append(px + (3_000_000 if i % 211 == 7 else 0))
    return out


def test_the_mirror_matches_the_rust_law_bit_for_bit() -> None:
    mom = claude_worker.hypercall_settle.median_of_means
    v = lcg_series()
    assert v[:3] == [224_050_751, 224_035_445, 224_041_956]
    assert v[-1] == 237_348_491
    # THE PIN (core-settle tests.rs): sorted 230_730_637, time 230_747_518.
    assert (mom(v, "sorted"), mom(v, "time")) == (230_730_637, 230_747_518)
    for order in claude_worker.hypercall_settle.ORDERS:
        assert mom([5 * U], order) == 5 * U
        assert mom([3 * U, U, 2 * U], order) == 2 * U
        assert mom([10 * U, 20 * U, 30 * U, 40 * U], order) == 25 * U
        assert mom([x * U for x in range(1, 21)], order) == 9 * U + U // 2
        assert mom([7 * U] * 50, order) == 7 * U
        assert mom([], order) is None
    assert mom(list(reversed(v)), "sorted") == mom(v, "sorted")


def test_the_grid_holds_each_price_and_refuses_what_the_window_refuses() -> None:
    t0 = T - claude_worker.hypercall_settle.WINDOW_MS
    g = claude_worker.hypercall_settle.grid(
        [(t0 - 5_000, 100), (t0 + 2_000, 200), (t0 + 2_500, 300), (T - 1_000, 400)], T
    )
    assert len(g) == claude_worker.hypercall_settle.GRID_POINTS
    assert g[:3] == [100, 200, 300]
    assert g[-3:] == [300, 400, 400]
    late = claude_worker.hypercall_settle.grid([(T - 60_000, 5)], T)
    assert len(late) == 61
    refused = claude_worker.hypercall_settle.grid(
        [(T - 10_000, 7), (T - 9_000, 0), (T - 11_000, 8), (T + 1, 9), (T, 9)], T
    )
    assert refused == [7] * 10 + [9]
    assert claude_worker.hypercall_settle.grid([], T) == []


def _write_events(path: pathlib.Path, marks: list[tuple[int, int]]) -> None:
    header = struct.pack("<4sHBxQ", b"PMLR", 3, 5, 0).ljust(64, b"\0")
    slot = struct.Struct("<QIBB2xQQqq16x")
    body = b"".join(slot.pack(ts * 1_000_000, IDX_SYM, 9, 2, 0, ts, px, ts) for ts, px in marks)
    path.write_bytes(header + body)


def test_the_shadow_judges_a_pinned_expiry_from_the_captured_index(tmp_path: pathlib.Path) -> None:
    run = tmp_path / "logs" / "run-1790364000000000000"
    run.mkdir(parents=True)
    (run / "instrument-manifest.tsv").write_text(f"{IDX_SYM}\thypercall-idx:SP500\n")
    # A flat index at 7742.2214 with one spike, printed every 2 s (the
    # venue's cadence), from 31 minutes before T.
    marks = []
    ts = T - 31 * 60_000
    while ts <= T:
        px = 7_742_221_400 + (50_000_000 if ts == T - 600_000 else 0)
        marks.append((ts, px))
        ts += 2_000
    _write_events(run / "hypercall-events.pmlr", marks)
    conn = sqlite3.connect(tmp_path / "candles.db")
    claude_worker.hypercall_history.ensure_schema(conn)
    # One ITM call pins S = 7700 + 42.2214.
    conn.execute(
        "INSERT INTO hc_payouts VALUES (1, ?, 'SP500-20260925-7700-C', ?, -2.5, 42.2214, 0,"
        " NULL, NULL, NULL, 0)",
        (MAKER, T // 1000),
    )
    conn.commit()
    rows = claude_worker.hypercall_settle.shadow(conn, [run])
    assert len(rows) == 1
    r = rows[0]
    assert (r.underlying, r.expiry_s, r.points, r.s_1e6, r.consistent) == (
        "SP500",
        T // 1000,
        claude_worker.hypercall_settle.GRID_POINTS,
        7_742_221_400,
        True,
    )
    # The spike is trimmed: both orders settle exactly on the index.
    assert r.mom_1e6 == {"sorted": 7_742_221_400, "time": 7_742_221_400}
    assert r.err_bps == {"sorted": 0.0, "time": 0.0}
    # An underlying without a capture is not judged; a filter excludes.
    assert claude_worker.hypercall_settle.shadow(conn, [run], only={"BTC"}) == []


def test_the_gate_needs_twenty_expiries_on_three_underlyings_within_bounds() -> None:
    row = claude_worker.hypercall_settle.Row
    full = claude_worker.hypercall_settle.GRID_POINTS
    rows = [
        row(u, 1_790_000_000 + i * 86_400, full, 1, True, {"sorted": 1}, {"sorted": 0.5})
        for i in range(7)
        for u in ("SP500", "NVDA", "MU")
    ]
    ok, line = claude_worker.hypercall_settle.gate(rows, "sorted")
    assert ok and "21 expiries on 3 underlyings" in line
    bad = [*rows, row("MU", 1, full, 1, True, {"sorted": 1}, {"sorted": 3.5})]
    assert not claude_worker.hypercall_settle.gate(bad, "sorted")[0], "max over 3 bp"
    partial = [r._replace(points=full - 1) for r in rows]
    assert not claude_worker.hypercall_settle.gate(partial, "sorted")[0], "coverage"
    assert not claude_worker.hypercall_settle.gate(rows, "time")[0], "no time rows"
