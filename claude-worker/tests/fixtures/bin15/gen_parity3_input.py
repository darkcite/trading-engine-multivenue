# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""gen_parity3_input -- writes ``parity-3.input.tsv`` (BIN15 H90 late pricer).

``parity-1`` (O4b) and ``parity-2`` (S3) stay pinned as they are. BIN15 H90
adds the LATE-WINDOW pricer (``crates/strategy-bin15/src/late.rs``,
mirrored by ``claude_worker.bin15_ref``'s ``late_*``), and its records
travel here:

* ``LPHI`` -- the shipped late table (``claude_worker.bin15_fit``);
* ``LG`` / ``LM`` / ``LP`` -- a fresh 1-s grid, marks and perp BBOs;
* ``LQ`` -- a late price (or the hold's name) at a query instant.

Synthetic, seeded, self-contained: random walks for a BTC-sized and a
HYPE-sized underlying, with jittered prints, stale stretches of both feeds,
a gap longer than the return ring, queries across the last two minutes of
many instances and every refusal the law has. No market data; nothing here
is research material.

Regenerate with ``cd claude-worker && uv run python
tests/fixtures/bin15/gen_parity3_input.py``, then
``BIN15_PARITY_WRITE=parity-3 cargo nextest run -p strategy-bin15 --test parity``
and ``uv run pytest tests/test_bin15_ref.py``. Changing this file changes the
fixture's identity -- add ``parity-4`` rather than rewriting a pinned one.

Convention: full ``import x`` only. No ``from x import y``.
"""

import bisect
import pathlib
import random

import claude_worker.bin15_fit

S: int = 1_000_000_000

#: The scenarios: (seed, base mark x1e6, mark tick x1e6, per-print walk
#: sd as a fraction x1e9, first instance expiry in seconds, instances).
SCENARIOS: tuple[tuple[int, int, int, int, int, int], ...] = (
    (11, 80_000_000_000, 1_000_000, 40_000, 10_800, 4),
    (23, 41_000_000, 1_000, 90_000, 21_600, 4),
    (37, 2_500_000_000, 10_000, 60_000, 32_400, 3),
)

#: The sigma scales the queries cycle through, x1e3.
KS: tuple[int, ...] = (1_550, 1_000, 2_500, 250, 4_000)


def scenario(out: list[str], seed: int, base: int, tick: int, sd_1e9: int, first_exp: int, n_inst: int) -> None:
    rng = random.Random(seed)
    out.append("LG")
    start = first_exp - 1_200
    end = first_exp + 900 * (n_inst - 1) + 5
    t = start * S + rng.randrange(0, S)
    perp_t = t + 50_000_000
    mark = base
    basis_ppm = 0
    events: list[tuple[int, str]] = []
    mark_ts: list[int] = []
    mark_px: list[int] = []
    # Marks ~1 Hz with jitter; one long silence; two stale stretches.
    silence = (first_exp + 900 + 200) * S
    while t < end * S:
        if silence <= t < silence + 1_200 * S:
            t = silence + 1_200 * S
        step = rng.gauss(0.0, 1.0) * sd_1e9 / 1e9 * mark
        mark = max(tick, (int(mark + step) // tick) * tick)
        events.append((t, f"LM\t{t}\t{mark}"))
        mark_ts.append(t)
        mark_px.append(mark)
        gap = rng.choice((850, 950, 1_000, 1_020, 1_100, 1_200)) * 1_000_000
        if rng.random() < 0.004:
            gap = rng.choice((5_500, 7_000)) * 1_000_000
        t += gap
    # Perp BBO ~5 Hz around the mark with a slowly drifting basis.
    while perp_t < end * S:
        basis_ppm += rng.choice((-3, 0, 0, 3))
        basis_ppm = max(-400, min(400, basis_ppm))
        # The mark in force at perp_t.
        k = bisect.bisect_right(mark_ts, perp_t) - 1
        m_now = mark_px[k] if k >= 0 else base
        half = max(tick, (m_now // 20_000 // tick) * tick)
        mid_px = m_now + m_now * basis_ppm // 1_000_000
        bid = (mid_px - half) // tick * tick
        ask = bid + 2 * half
        events.append((perp_t, f"LP\t{perp_t}\t{bid + ask}"))
        gap = rng.choice((350, 450, 550)) * 1_000_000
        if rng.random() < 0.002:
            gap = rng.choice((3_500, 4_000)) * 1_000_000
        perp_t += gap
    # Queries: across the last 120 s of every instance, plus a few past it.
    q = 0
    i = 0
    while i < n_inst:
        exp = first_exp + 900 * i
        qt = (exp - 120) * S + rng.randrange(0, S)
        while qt < (exp + 3) * S:
            strike_off = rng.choice((-60, -20, -5, -2, -1, 0, 1, 2, 5, 20, 60)) * tick * (1 if tick > 1_000 else 50)
            rel = "N" if rng.random() < 0.5 else "S"
            events.append((qt, f"Q\t{qt}\t{exp}\t{strike_off}\t{KS[q % len(KS)]}\t{rel}"))
            q += 1
            qt += rng.choice((400, 700, 1_300, 1_700, 2_600)) * 1_000_000
        i += 1
    events.sort(key=lambda e: (e[0], e[1][:2]))
    # The strike: the mark in force 900 s before expiry, offset.
    last_mark = base
    marks_at: dict[int, int] = {}
    for ts, row in events:
        if row.startswith("LM\t"):
            last_mark = int(row.split("\t")[2])
            marks_at[ts // S] = last_mark
    for ts, row in events:
        if row.startswith("Q\t"):
            _, qts, exp, off, k, rel = row.split("\t")
            sec = int(exp) - 900 if rel == "S" else int(qts) // S
            k0 = sec
            while k0 > sec - 30 and k0 not in marks_at:
                k0 -= 1
            strike = max(tick, marks_at.get(k0, base) + int(off))
            out.append(f"LQ\t{qts}\t{strike}\t{int(exp) * S}\t{k}")
        else:
            out.append(row)


def main() -> int:
    out: list[str] = [
        "# parity-3.input.tsv - the BIN15 H90 late-pricer tape. Generated by",
        "# tests/fixtures/bin15/gen_parity3_input.py; do not hand-edit.",
        "#   LPHI v0 .. v4096                 load the shipped late Phi table",
        "#   LG                               a fresh late grid",
        "#   LM   wall mark                   LateGrid::on_mark",
        "#   LP   wall bid_plus_ask           LateGrid::on_perp",
        "#   LQ   wall strike expiry k        advance + late_fair",
        "LPHI\t" + "\t".join(str(v) for v in claude_worker.bin15_fit.late_phi_table_1e6()),
    ]
    for sc in SCENARIOS:
        scenario(out, *sc)
    # Shape refusals: a bad strike, a bad k, an expiry under one window.
    out.append("LG")
    out.append(f"LQ\t{100 * S}\t0\t{900 * S}\t1550")
    out.append(f"LQ\t{100 * S}\t1000000\t{900 * S}\t0")
    out.append(f"LQ\t{10 * S}\t1000000\t{59 * S}\t1550")
    path = pathlib.Path(__file__).parent / "parity-3.input.tsv"
    path.write_text("\n".join(out) + "\n", encoding="utf-8")
    print(f"wrote {path} ({len(out)} lines)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
