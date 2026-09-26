# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vol_ref.LongVolEngine -- the Python half of the long-tenor parity (HAR H1/H2).

``tests/fixtures/vol/long-<n>.{input,expected}.tsv`` are the SAME files
``crates/core-vol/tests/long_parity.rs`` consumes; the expected rows are
written by the Rust law (``HAR_LONG_PARITY_WRITE=1``) and asserted here
row by row. The tape's ops are documented in that harness.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import claude_worker.vol_ref

_DIR: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "vol"
_FIXTURES: tuple[str, ...] = ("long-1",)
_NONE: int = claude_worker.vol_ref.LOG2_UNDEFINED
_U64: int = (1 << 64) - 1
_LCG_MUL: int = 6_364_136_223_846_793_005
_LCG_INC: int = 1_442_695_040_888_963_407
_PX_FLOOR: int = 1_000_000_000


def _opt(v: int | None) -> str:
    return "-" if v is None else str(v)


def _num(s: str) -> int:
    return _NONE if s == "-" else int(s)


class _Walk:
    """The tape's price walk, bit for bit the Rust harness's ``Walk``."""

    def __init__(self, seed: int, px: int) -> None:
        self.s = seed
        self.px = px

    def step(self, amp: int) -> int:
        self.s = (self.s * _LCG_MUL + _LCG_INC) & _U64
        self.px = max(self.px + (self.s >> 32) % (2 * amp + 1) - amp, _PX_FLOOR)
        return self.px


def _tenor_row(row: int, e: claude_worker.vol_ref.LongVolEngine, tau_days: int) -> str:
    t = tau_days * claude_worker.vol_ref.DAY_NS
    fit = e.fit(t)
    q = e.qlike_counters(t)
    n = e.n_pairs(t)
    last_pair = e.pair_at(t, n - 1) if n else None
    arm = e.arm_at(e.n_resident() - 1, t) if e.n_resident() else None
    cells = [
        str(row),
        "E",
        str(tau_days),
        _opt(e.x_1e9(t)),
        _opt(None if fit is None else fit[0]),
        _opt(None if fit is None else fit[1]),
        _opt(e.ln_sigma_fit_1e9(t)),
        _opt(e.sigma_ann_1e9(t, "raw")),
        _opt(e.sigma_ann_1e9(t, "fit")),
        str(n),
        str(q[0]),
        str(q[1]),
        str(q[2]),
        "1" if q[3] else "0",
        "-" if last_pair is None else ",".join(str(v) for v in last_pair),
        "-" if arm is None else f"{arm[0]},{arm[1]}",
    ]
    return "\t".join(cells)


def _state_row(row: int, e: claude_worker.vol_ref.LongVolEngine) -> str:
    open_day = e.open_day()
    cells = [
        str(row),
        "S",
        str(e.n_resident()),
        "-" if open_day is None else ",".join(str(v) for v in open_day),
        str(e.gaps),
        str(e.refused),
        str(e.last_min_ts_ms),
        str(e.prev_px_1e6),
        "1" if e.is_warm() else "0",
    ]
    return "\t".join(cells)


def _seed(e: claude_worker.vol_ref.LongVolEngine, f: list[str]) -> bool:
    op = f[0]
    tau = int(f[1]) * claude_worker.vol_ref.DAY_NS if op in "APQ" else 0
    if op == "W":
        return e.seed_day(int(f[1]), int(f[2]), int(f[3]))
    if op == "O":
        return e.seed_open(int(f[1]), int(f[2]), int(f[3]), int(f[4]), _num(f[5]))
    if op == "A":
        return e.seed_arm(tau, int(f[2]), _num(f[3]), _num(f[4]))
    if op == "P":
        return e.seed_pair(tau, int(f[2]), _num(f[3]), _num(f[4]))
    return e.seed_qlike(tau, _num(f[2]), _num(f[3]))


def _emit(e: claude_worker.vol_ref.LongVolEngine, f: list[str], out: list[str]) -> None:
    """The ops that append rows: the seeds' verdicts, ``S``, ``D``, ``E``."""
    op = f[0]
    if op == "S":
        out.append(_state_row(len(out), e))
    elif op == "D":
        for i in range(e.n_resident()):
            ts, sq, n = e.day_at(i)
            out.append(f"{len(out)}\tD\t{i}\t{ts}\t{sq}\t{n}")
    elif op == "E":
        for d in f[1:]:
            out.append(_tenor_row(len(out), e, int(d)))
    else:
        out.append(f"{len(out)}\t{op}\t{1 if _seed(e, f) else 0}")


def _replay(name: str) -> list[str]:
    """Run the op tape, returning the emitted rows."""
    src = (_DIR / f"{name}.input.tsv").read_text(encoding="utf-8")
    e = claude_worker.vol_ref.LongVolEngine()
    w = _Walk(0, 0)
    out: list[str] = []
    for line in src.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        f = stripped.split("\t")
        op = f[0]
        if op == "L":
            w = _Walk(int(f[1]), _num(f[2]))
        elif op == "G":
            start, count, amp = int(f[1]), int(f[2]), _num(f[3])
            for k in range(count):
                e.on_minute_close_at(w.step(amp), start + k * 60_000)
        elif op == "M":
            e.on_minute_close_at(_num(f[1]), int(f[2]))
        elif op == "N":
            e = claude_worker.vol_ref.LongVolEngine()
        elif op == "F":
            e.refresh()
        elif op in {"W", "O", "A", "P", "Q", "S", "D", "E"}:
            _emit(e, f, out)
        else:
            raise AssertionError(f"{name}: unknown op {op!r}")
    return out


def test_the_long_law_matches_the_rust_rows_bit_for_bit() -> None:
    for name in _FIXTURES:
        want = [
            line.strip()
            for line in (_DIR / f"{name}.expected.tsv").read_text(encoding="utf-8").splitlines()
            if line.strip() and not line.startswith("#")
        ]
        got = _replay(name)
        assert len(got) == len(want), f"{name}: row count drifted"
        for g, w in zip(got, want, strict=True):
            assert g == w, f"{name}: the long-tenor law drifted from the shared fixture"


def test_the_grid_and_its_annualisers_are_the_rust_constants() -> None:
    vr = claude_worker.vol_ref
    assert (vr.ANNUALISE_LONG_1E9[0], vr.ANNUALISE_LONG_1E9[6], vr.ANNUALISE_LONG_1E9[29]) == (
        19_111_514_854,
        7_223_473_640,
        3_489_269_264,
    )
    assert vr.annualiser_1e9(15) == vr.ANNUALISE_15M_1E9
    assert vr.annualiser_1e9(240) == vr.ANNUALISE_4H_1E9
    assert vr.annualiser_1e9(480) == vr.ANNUALISE_8H_1E9
    assert vr.long_tenor_of(7 * vr.DAY_NS) == (7, 10_080, 7_223_473_640)
    for bad in (0, 41 * vr.DAY_NS, vr.DAY_NS + 1, vr.TAU_8H_NS):
        assert vr.long_tenor_of(bad) is None


def test_the_lifted_fit_is_the_vol_engine_fit() -> None:
    vr = claude_worker.vol_ref
    engine = vr.VolEngine()
    s = 3
    xs: list[int] = []
    ys: list[int] = []
    for _ in range(140):
        s = (s * _LCG_MUL + 1) & _U64
        x = 23_000_000_000 + (s >> 33) % 900_000_000
        s = (s * _LCG_MUL + 1) & _U64
        y = x * 7 // 10 + 6_000_000_000 + (s >> 33) % 400_000_000 - 200_000_000
        engine.seed_pair(x, y)
        xs.append(x)
        ys.append(y)
    fit = vr.ols_fit_1e9(xs[-vr.PAIR_RING :], ys[-vr.PAIR_RING :])
    assert fit is not None
    assert engine.fit() == fit
    assert vr.ols_fit_1e9(xs[:59], ys[:59]) is None
    assert vr.ols_fit_1e9([xs[0]] * 60, ys[:60]) is None
