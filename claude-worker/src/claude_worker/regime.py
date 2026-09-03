# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Market-regime reference law (RG1 — ``docs/regime-and-dashboard-plan.md``
§3–§4; Rust counterpart: ``crates/core-regime``).

This module is the worker's HALF of operator decision D1: the SAME integer
law the engine runs (``core_regime``), over minute closes instead of ticks,
so a committed fixture (``tests/fixtures/regime/``) can pin the two
implementations bit for bit. Every arithmetic step mirrors the Rust code:
floor division (``//`` == ``i128::div_euclid`` for positive divisors),
``math.isqrt`` (== the Newton floor root), the 80 % presence laws, the
band machine, the confirm law and the declared-over-measured merge.

Words use the ``frames.py`` bit map (one byte per dimension, one-hot; bit 7
= the per-dimension unknown mark). Profiles: 0 = fast, 1 = slow.

Lanes (``python -m claude_worker.regime <lane>``; the 8-verb CLI surface
stays frozen — module-lane precedent ``pnl_report``/``candles``):

- ``seed-out --regime <regime.toml> --out <regime-seed.tsv>`` (RG2, plan
  §4.3): export the last ``--minutes`` (default 1536) 1-minute closes of
  the artifact's reference + breadth members from candles.db as the
  engine's warm-up seed (``descriptor \t minute \t close_1e6``). Called by
  ``scripts/engine-wrapper.sh`` right before every boot. Derived data,
  not a capture window.
- report / percentiles / declare land in RG5.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import dataclasses
import math
import os
import pathlib
import sqlite3
import sys
import time
import tomllib

import claude_worker.candles
import claude_worker.frames

# ---- constants (core_regime) ----

REGIME_PROFILES: int = 2
REGIME_MAX_MEMBERS: int = 31
REGIME_RING_MIN: int = 1536
MAX_BACK_MIN: int = 5
ABSENT: int = 0xFF
SCALE_1E9: int = 1_000_000_000
ER_STEP_MIN: int = 5
MINUTE_NS: int = 60_000_000_000

DIM_TREND: int = 0
DIM_SHAPE: int = 1
DIM_VOL: int = 2
DIM_FUND_SIGN: int = 3
DIM_FUND_LEVEL: int = 4
DIM_STRETCH: int = 5
DIM_SOURCE: int = 6
DIM_UNKNOWN_BIT: int = claude_worker.frames.REGIME_DIM_UNKNOWN_BIT

TREND_BEAR, TREND_NEUTRAL, TREND_BULL = 0, 1, 2
SHAPE_CHOP, SHAPE_MIXED, SHAPE_TREND = 0, 1, 2
VOL_LOW, VOL_NORMAL, VOL_HIGH = 0, 1, 2
FUND_NEG, FUND_POS = 0, 1
LEVEL_LOW, LEVEL_NORMAL, LEVEL_HIGH = 0, 1, 2
STRETCH_EXT_DOWN, STRETCH_NEUTRAL, STRETCH_EXT_UP = 0, 1, 2
SOURCE_MEASURED, SOURCE_DECLARED, SOURCE_UNKNOWN = 0, 1, 2
REL_LAGGING, REL_INLINE, REL_LEADING = 0, 1, 2
REL_UNKNOWN: int = 0xFF

UNKNOWN_WORD: int = claude_worker.frames.REGIME_UNKNOWN_WORD
EMPTY_WORD: int = 0

# ---- math (core_regime::math) ----


def floor_div(n: int, d: int) -> int:
    """``i128::div_euclid`` for a positive divisor == Python ``//``."""
    return n // d


def ret_bps_1e9(from_1e6: int, to_1e6: int) -> int:
    """Return of ``to`` over ``from`` in bps ×1e9 (floored)."""
    if from_1e6 <= 0:
        return 0
    return floor_div((to_1e6 - from_1e6) * 10_000_000_000_000, from_1e6)


def isqrt_i128(v: int) -> int:
    """Floor square root, saturating at ``i64::MAX`` like the Rust twin."""
    if v <= 0:
        return 0
    r = math.isqrt(v)
    return min(r, (1 << 63) - 1)


# ---- parameters ----


@dataclasses.dataclass(frozen=True, slots=True)
class ProfileParams:
    """One horizon profile's parameters (``core_regime::ProfileParams``)."""

    trend_w_min: int
    shape_w_min: int
    vol_w_min: int
    stretch_w_min: int
    rel_w_min: int
    fund_prints: int
    trend_thr_bps_1e9: int
    breadth_q_1e9: int
    er_lo_enter_1e9: int
    er_lo_exit_1e9: int
    er_hi_enter_1e9: int
    er_hi_exit_1e9: int
    rv_p30_bps_1e9: int
    rv_p70_bps_1e9: int
    stretch_k_1e9: int
    rel_thr_bps_1e9: int
    fund_p30_1e9: int
    fund_p70_1e9: int


FAST_DEFAULT: ProfileParams = ProfileParams(
    60, 60, 60, 60, 60, 9,
    30_000_000_000, 600_000_000,
    300_000_000, 350_000_000, 600_000_000, 550_000_000,
    0, 0, 2_000_000_000, 50_000_000_000, 0, 0,
)
SLOW_DEFAULT: ProfileParams = ProfileParams(
    240, 240, 240, 240, 240, 90,
    80_000_000_000, 600_000_000,
    300_000_000, 350_000_000, 600_000_000, 550_000_000,
    0, 0, 2_000_000_000, 150_000_000_000, 0, 0,
)


@dataclasses.dataclass(frozen=True, slots=True)
class RegimeParams:
    """The detector's parameters (``core_regime::RegimeParams``)."""

    btc_ref: int
    fund_ref: int
    members: tuple[int, ...]
    confirm_min: int
    profiles: tuple[ProfileParams, ...]


# ---- the pure law (mirrored function by function) ----


def ring_idx(minute: int) -> int:
    return minute % REGIME_RING_MIN


def close_at(ring: list[int], m: int) -> int:
    """The close at minute ``m``, walking back over holes ≤ MAX_BACK_MIN."""
    for k in range(MAX_BACK_MIN + 1):
        c = ring[ring_idx(m - k)]
        if c > 0:
            return c
    return 0


def ret_over(ring: list[int], m: int, w: int) -> int | None:
    a = close_at(ring, m - w)
    b = close_at(ring, m)
    if a <= 0 or b <= 0:
        return None
    return ret_bps_1e9(a, b)


def er_over(ring: list[int], m: int, w: int) -> int | None:
    steps = w // ER_STEP_MIN
    if steps == 0:
        return None
    den = 0
    present = 0
    for j in range(steps):
        a = close_at(ring, m - 5 * j)
        b = close_at(ring, m - 5 * j - 5)
        if a > 0 and b > 0:
            den += abs(a - b)
            present += 1
    if present * 5 < steps * 4:
        return None
    first = close_at(ring, m - w)
    last = close_at(ring, m)
    if first <= 0 or last <= 0:
        return None
    if den == 0:
        return 0
    return floor_div(abs(last - first) * SCALE_1E9, den)


def rv_over(ring: list[int], m: int, w: int) -> int | None:
    total = 0
    present = 0
    for k in range(w):
        a = close_at(ring, m - k - 1)
        b = close_at(ring, m - k)
        if a > 0 and b > 0:
            r = ret_bps_1e9(a, b)
            total += r * r
            present += 1
    if present * 5 < w * 4:
        return None
    return isqrt_i128(total)


def judge_trend(r: int, up: int, dn: int, present: int, n_members: int, pp: ProfileParams) -> int:
    agree_up = n_members == 0 or up * SCALE_1E9 >= pp.breadth_q_1e9 * present
    agree_dn = n_members == 0 or dn * SCALE_1E9 >= pp.breadth_q_1e9 * present
    if r > pp.trend_thr_bps_1e9 and agree_up:
        return TREND_BULL
    if r < -pp.trend_thr_bps_1e9 and agree_dn:
        return TREND_BEAR
    return TREND_NEUTRAL


def judge_shape(cur: int, er: int, pp: ProfileParams) -> int:
    if cur == SHAPE_CHOP:
        if er < pp.er_lo_exit_1e9:
            return SHAPE_CHOP
        return SHAPE_TREND if er > pp.er_hi_enter_1e9 else SHAPE_MIXED
    if cur == SHAPE_TREND:
        if er > pp.er_hi_exit_1e9:
            return SHAPE_TREND
        return SHAPE_CHOP if er < pp.er_lo_enter_1e9 else SHAPE_MIXED
    if er < pp.er_lo_enter_1e9:
        return SHAPE_CHOP
    if er > pp.er_hi_enter_1e9:
        return SHAPE_TREND
    return SHAPE_MIXED


def judge_vol(rv: int, pp: ProfileParams) -> int:
    if pp.rv_p30_bps_1e9 == 0 and pp.rv_p70_bps_1e9 == 0:
        return ABSENT
    if rv < pp.rv_p30_bps_1e9:
        return VOL_LOW
    if rv > pp.rv_p70_bps_1e9:
        return VOL_HIGH
    return VOL_NORMAL


def judge_fund_sign(rate_1e9: int) -> int:
    return FUND_NEG if rate_1e9 < 0 else FUND_POS


def judge_fund_level(rate_1e9: int, pp: ProfileParams) -> int:
    if pp.fund_p30_1e9 == 0 and pp.fund_p70_1e9 == 0:
        return ABSENT
    if rate_1e9 < pp.fund_p30_1e9:
        return LEVEL_LOW
    if rate_1e9 > pp.fund_p70_1e9:
        return LEVEL_HIGH
    return LEVEL_NORMAL


def judge_stretch(stretch_1e9: int, pp: ProfileParams) -> int:
    if stretch_1e9 > pp.stretch_k_1e9:
        return STRETCH_EXT_UP
    if stretch_1e9 < -pp.stretch_k_1e9:
        return STRETCH_EXT_DOWN
    return STRETCH_NEUTRAL


def judge_rel(rel_bps_1e9: int, thr: int) -> int:
    if rel_bps_1e9 < -thr:
        return REL_LAGGING
    if rel_bps_1e9 > thr:
        return REL_LEADING
    return REL_INLINE


def word_dim(word: int, d: int) -> int:
    return (word >> (8 * d)) & 0xFF


def word_with_source(word: int, source: int) -> int:
    cleared = word & ~(0xFF << (8 * DIM_SOURCE))
    return cleared | (1 << (8 * DIM_SOURCE + source))


def any_known(word: int) -> bool:
    for d in range(DIM_SOURCE):
        b = word_dim(word, d)
        if b != 0 and b != DIM_UNKNOWN_BIT:
            return True
    return False


def merge_declared(declared: int, measured: int) -> int:
    w = 0
    for d in range(DIM_SOURCE):
        db = word_dim(declared, d)
        byte = db if db != 0 else word_dim(measured, d)
        w |= byte << (8 * d)
    return word_with_source(w, SOURCE_DECLARED)


def declared_disagrees(declared: int, measured: int) -> bool:
    for d in range(DIM_SOURCE):
        db = word_dim(declared, d)
        if db != 0 and db != word_dim(measured, d):
            return True
    return False


class Judge:
    """Per-dimension confirm state (``core_regime::Judge``)."""

    __slots__ = ("cur", "pending", "pend_n")

    def __init__(self) -> None:
        self.cur = ABSENT
        self.pending = ABSENT
        self.pend_n = 0

    def feed(self, candidate: int, confirm_min: int) -> bool:
        if candidate == self.cur:
            self.pending = candidate
            self.pend_n = 0
            return False
        if candidate == self.pending:
            self.pend_n = min(self.pend_n + 1, 255)
        else:
            self.pending = candidate
            self.pend_n = 1
        if self.pend_n >= confirm_min:
            flip = self.cur != ABSENT and candidate != ABSENT
            self.cur = candidate
            self.pend_n = 0
            return flip
        return False


@dataclasses.dataclass(slots=True)
class Raw:
    """``core_regime::RegimeRaw`` (presence as ``None``)."""

    ret_bps_1e9: int | None = None
    er_1e9: int | None = None
    rv_bps_1e9: int | None = None
    stretch_1e9: int | None = None
    funding_1e9: int | None = None
    breadth_up: int = 0
    breadth_dn: int = 0
    breadth_n: int = 0
    breadth_ok: bool = False


class RegimeEvaluator:
    """The detector over minute closes (``core_regime::RegimeState`` minus
    the tick→minute attribution, which is engine mechanics).

    Feed ``close(minute, sym, close_1e6)`` for every member close of a
    minute (seed rows for minutes before the first live one are the same
    call), ``funding(rate_1e9, venue_time_ms)`` as prints arrive,
    ``set_declared(profile, word, now_ns, ttl_ns)`` for declarations, then
    ``roll(minute, now_ns)`` once per closed minute in order. Readers:
    ``measured``, ``effective``, ``rel_of``, ``raw``, ``flips``,
    ``disagree``.
    """

    def __init__(self, params: RegimeParams) -> None:
        if len(params.profiles) != REGIME_PROFILES:
            raise ValueError("exactly two profiles")
        if not 1 <= params.confirm_min <= 255:
            raise ValueError("confirm_min in 1..=255")
        if len(params.members) > REGIME_MAX_MEMBERS or len(set(params.members)) != len(params.members):
            raise ValueError("members: ≤ 31, unique")
        if params.btc_ref in params.members:
            raise ValueError("btc_ref must not be a member")
        self.params = params
        self.syms: list[int] = [params.btc_ref, *params.members]
        self.rings: dict[int, list[int]] = {s: [0] * REGIME_RING_MIN for s in self.syms}
        self.judges: list[list[Judge]] = [[Judge() for _ in range(8)] for _ in range(REGIME_PROFILES)]
        self.rel_judges: list[dict[int, Judge]] = [
            {s: Judge() for s in params.members} for _ in range(REGIME_PROFILES)
        ]
        self.measured: list[int] = [UNKNOWN_WORD] * REGIME_PROFILES
        self.declared: list[int] = [EMPTY_WORD] * REGIME_PROFILES
        self.effective: list[int] = [UNKNOWN_WORD] * REGIME_PROFILES
        self.declared_ts: list[int] = [0] * REGIME_PROFILES
        self.declared_ttl: list[int] = [0] * REGIME_PROFILES
        self.raw: list[Raw] = [Raw() for _ in range(REGIME_PROFILES)]
        self.funding_rate_1e9 = 0
        self.funding_ts_ms = 0
        self.flips: list[list[int]] = [[0] * 8 for _ in range(REGIME_PROFILES)]
        self.disagree: list[int] = [0] * REGIME_PROFILES
        self.minutes_judged = 0

    # ---- inputs ----

    def close(self, minute: int, sym: int, close_1e6: int) -> bool:
        """Record a minute close (seed or live). Non-members / non-positive
        closes are ignored; returns whether it was stored."""
        ring = self.rings.get(sym)
        if ring is None or close_1e6 <= 0:
            return False
        ring[ring_idx(minute)] = close_1e6
        return True

    def funding(self, rate_1e9: int, venue_time_ms: int) -> None:
        self.funding_rate_1e9 = rate_1e9
        self.funding_ts_ms = venue_time_ms if venue_time_ms != 0 else 1

    def set_declared(self, profile: int, word: int, now_ns: int, ttl_ns: int) -> None:
        if not 0 <= profile < REGIME_PROFILES:
            return
        self.declared[profile] = word
        self.declared_ts[profile] = now_ns
        self.declared_ttl[profile] = ttl_ns

    def clear_declared(self, profile: int) -> None:
        if 0 <= profile < REGIME_PROFILES:
            self.declared[profile] = EMPTY_WORD
            self.declared_ts[profile] = 0
            self.declared_ttl[profile] = 0

    def declared_fresh(self, profile: int, now_ns: int) -> bool:
        return (
            0 <= profile < REGIME_PROFILES
            and self.declared_ttl[profile] != 0
            and (now_ns - self.declared_ts[profile]) % (1 << 64) < self.declared_ttl[profile]
        )

    # ---- the roll ----

    def roll(self, m: int, now_ns: int, count: bool = True) -> int:
        """Judge the just-closed minute ``m`` then refresh the effective
        words. ``now_ns`` is the roll instant — the engine's timer tick
        that crossed the boundary — and is the ONE freshness reference for
        both the disagree law and the effective law. Returns the
        changed-profile bit mask."""
        self._judge_minute(m, now_ns, count)
        return self.refresh_effective(now_ns)

    def refresh_effective(self, now_ns: int) -> int:
        changed = 0
        for p in range(REGIME_PROFILES):
            m = self.measured[p]
            if self.declared_fresh(p, now_ns):
                eff = merge_declared(self.declared[p], m)
            elif any_known(m):
                eff = word_with_source(m, SOURCE_MEASURED)
            else:
                eff = UNKNOWN_WORD
            if eff != self.effective[p]:
                self.effective[p] = eff
                changed |= 1 << p
        return changed

    def _judge_minute(self, m: int, now_ns: int, count: bool) -> None:
        confirm = self.params.confirm_min
        members = self.params.members
        n_members = len(members)
        btc = self.rings[self.params.btc_ref]
        for p in range(REGIME_PROFILES):
            pp = self.params.profiles[p]
            raw = Raw()
            ret = ret_over(btc, m, pp.trend_w_min)
            raw.ret_bps_1e9 = ret
            er = er_over(btc, m, pp.shape_w_min)
            raw.er_1e9 = er
            rv = rv_over(btc, m, pp.vol_w_min)
            raw.rv_bps_1e9 = rv
            stretch = None
            s_ret = ret_over(btc, m, pp.stretch_w_min)
            s_rv = rv_over(btc, m, pp.stretch_w_min)
            if s_ret is not None and s_rv is not None and s_rv > 0:
                stretch = floor_div(s_ret * SCALE_1E9, s_rv)
            raw.stretch_1e9 = stretch

            up = dn = present = 0
            for sym in members:
                ring = self.rings[sym]
                a = ret_over(ring, m, pp.rel_w_min)
                b = ret_over(btc, m, pp.rel_w_min)
                rel_cand = judge_rel(a - b, pp.rel_thr_bps_1e9) if a is not None and b is not None else ABSENT
                self.rel_judges[p][sym].feed(rel_cand, confirm)
                r = ret_over(ring, m, pp.trend_w_min)
                if r is not None:
                    present += 1
                    if r > pp.trend_thr_bps_1e9:
                        up += 1
                    elif r < -pp.trend_thr_bps_1e9:
                        dn += 1
            raw.breadth_up, raw.breadth_dn, raw.breadth_n = up, dn, present
            breadth_ok = n_members == 0 or present * 2 >= n_members
            raw.breadth_ok = breadth_ok and n_members > 0

            funding = None
            if self.funding_ts_ms != 0:
                funding = self.funding_rate_1e9
                raw.funding_1e9 = funding

            cands = [
                judge_trend(ret, up, dn, present, n_members, pp) if ret is not None and breadth_ok else ABSENT,
                judge_shape(self.judges[p][DIM_SHAPE].cur, er, pp) if er is not None else ABSENT,
                judge_vol(rv, pp) if rv is not None else ABSENT,
                judge_fund_sign(funding) if funding is not None else ABSENT,
                judge_fund_level(funding, pp) if funding is not None else ABSENT,
                judge_stretch(stretch, pp) if stretch is not None else ABSENT,
            ]
            for d in range(DIM_SOURCE):
                if self.judges[p][d].feed(cands[d], confirm) and count:
                    self.flips[p][d] += 1

            w = 0
            for d in range(DIM_SOURCE):
                cur = self.judges[p][d].cur
                byte = DIM_UNKNOWN_BIT if cur == ABSENT else (1 << cur)
                w |= byte << (8 * d)
            measured = word_with_source(w, SOURCE_MEASURED)
            if count and self.declared_fresh(p, now_ns) and declared_disagrees(self.declared[p], measured):
                self.disagree[p] += 1
            self.measured[p] = measured
            self.raw[p] = raw
        if count:
            self.minutes_judged += 1

    # ---- readers ----

    def rel_of(self, profile: int, sym: int) -> int:
        if not 0 <= profile < REGIME_PROFILES:
            return REL_UNKNOWN
        j = self.rel_judges[profile].get(sym)
        if j is None or j.cur == ABSENT:
            return REL_UNKNOWN
        return j.cur


def word_hex(word: int) -> str:
    """Canonical 16-hex-digit rendering (the fixture format)."""
    return f"{word & ((1 << 64) - 1):016x}"


def describe(word: int) -> str:
    """``trend=bull shape=trend … source=measured`` for reports."""
    dims = claude_worker.frames.regime_word_dims(word)
    return " ".join(f"{k}={v}" for k, v in dims.items()) if dims else "empty"


# ---- RG2: the seed lane (plan §4.3) ----

SEED_MINUTES_DEFAULT: int = 1536  # == core_regime::REGIME_RING_MIN
SEED_HEADER: str = "# regime-seed.tsv — descriptor\tminute\tclose_1e6 (claude_worker.regime seed-out)\n"


@dataclasses.dataclass(frozen=True, slots=True)
class SeedRow:
    descriptor: str
    minute: int
    close_1e6: int


def read_regime_descriptors(path: pathlib.Path) -> list[str]:
    """The artifact's reference + breadth descriptors (standard TOML —
    the integer-only subset the engine parses is a strict subset)."""
    obj = tomllib.loads(path.read_text(encoding="utf-8"))
    refs = obj.get("refs", {})
    breadth = obj.get("breadth", {})
    out: list[str] = []
    for d in [refs.get("btc"), refs.get("fund"), *breadth.get("members", [])]:
        if isinstance(d, str) and d and d not in out:
            out.append(d)
    return out


def seed_rows_from_candles(
    conn: sqlite3.Connection, descriptors: list[str], since_ms: int, until_ms: int
) -> list[SeedRow]:
    """1-minute closes of ``descriptors`` with ``since_ms <= open_ts <
    until_ts`` (any source), as integer ×1e6 closes keyed by wall minute.
    A NULL or non-positive close is skipped (a hole the engine walks
    over)."""
    rows: list[SeedRow] = []
    for d in descriptors:
        cur = conn.execute(
            "SELECT open_ts, c FROM candles WHERE descriptor=? AND tf='1m'"
            " AND open_ts >= ? AND open_ts < ? ORDER BY open_ts",
            (d, since_ms, until_ms),
        )
        for open_ts, c in cur.fetchall():
            if c is None or c <= 0:
                continue
            close_1e6 = int(round(float(c) * 1_000_000))
            if close_1e6 <= 0:
                continue
            rows.append(SeedRow(d, int(open_ts) // 60_000, close_1e6))
    return rows


def write_seed_tsv(path: pathlib.Path, rows: list[SeedRow]) -> None:
    """Atomic write (tmp + rename) so a boot never reads a torn file."""
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(SEED_HEADER)
        for r in rows:
            f.write(f"{r.descriptor}\t{r.minute}\t{r.close_1e6}\n")
    os.replace(tmp, path)


def seed_out(
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    out_path: pathlib.Path,
    minutes: int,
    now_ms: int | None = None,
) -> tuple[int, int]:
    """The ``seed-out`` lane: returns ``(descriptors, rows)``."""
    descriptors = read_regime_descriptors(regime_path)
    now = int(time.time() * 1000) if now_ms is None else now_ms
    until_ms = (now // 60_000) * 60_000  # the accumulating minute is not a close
    since_ms = until_ms - minutes * 60_000
    conn = claude_worker.candles.open_db(db_path)
    try:
        rows = seed_rows_from_candles(conn, descriptors, since_ms, until_ms)
    finally:
        conn.close()
    write_seed_tsv(out_path, rows)
    return len(descriptors), len(rows)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="claude_worker.regime")
    sub = parser.add_subparsers(dest="lane", required=True)
    seed = sub.add_parser("seed-out", help="export the engine's warm-up seed from candles.db")
    seed.add_argument("--regime", required=True, help="path to regime.toml")
    seed.add_argument("--out", required=True, help="path to write regime-seed.tsv")
    seed.add_argument(
        "--db",
        default=os.environ.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH,
        help="candles.db (default: $CLAUDE_WORKER_CANDLES_DB or ~/multivenue/worker/candles.db)",
    )
    seed.add_argument("--minutes", type=int, default=SEED_MINUTES_DEFAULT)
    args = parser.parse_args(argv)
    if args.lane == "seed-out":
        regime_path = pathlib.Path(args.regime).expanduser()
        db_path = pathlib.Path(args.db).expanduser()
        out_path = pathlib.Path(args.out).expanduser()
        if not regime_path.exists():
            print(f"regime seed-out: {regime_path} absent — nothing to export", file=sys.stderr)
            return 2
        if not db_path.exists():
            print(f"regime seed-out: {db_path} absent — nothing to export", file=sys.stderr)
            return 2
        n_desc, n_rows = seed_out(regime_path, db_path, out_path, args.minutes)
        print(f"regime seed-out: {n_rows} rows for {n_desc} descriptors -> {out_path}")
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
