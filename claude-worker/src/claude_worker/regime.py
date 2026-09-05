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
- ``report`` (RG5, plan §5.1): the worker-MEASURED words per profile with
  every raw value and band, the declaration in force (``declared.json``),
  the engine's own words read from ``/metrics`` (``/state`` is RG6), and
  the last 24 h of worker words — the AI's input in semi-manual mode.
  The measurement is the engine's SEED law over candles.db: fill the
  rings, latch the latest funding print, judge the last ``2·confirm_min``
  closed minutes.
- ``declare --fast "trend:bull,shape:trend" --slow measured --ttl 900``:
  persist ``declared.json`` and send one ``SetRegime`` frame per profile
  (heartbeat first; seq from state.db — the one-writer law). ``measured``
  declares the worker-measured word (the "AI confirms the measurement"
  case). ``repush`` re-sends a still-fresh persisted declaration with its
  REMAINING TTL — the post-boot lane (``recommit.py`` calls it after the
  #7b re-commit: same waiter, one more frame).
- ``refresh-params``: recompute the RV / funding p30/p70 percentile lines
  of ``regime.toml`` from candles.db (7 d hourly RV for fast, 30 d
  4-hourly for slow, the profile's ``fund_prints`` prints) and rewrite
  ONLY those six lines; the next T2 restart applies them.
- ``cycle``: the ``com.multivenue.regime`` 5-minute job — measure +
  history line (RG7: + the engine's own regime block from ``/state`` —
  pid, flips, minutes judged, effective words — when it answers) + the
  daily percentile refresh. Never declares.
- ``history``: the 24 h tail under ``~/multivenue/worker/regime/``.
- ``soak`` (RG7, plan §7.1): the ≤ 2 h-law soak judge — for every
  window of the standing pool (``~/multivenue/worker/windows/``): the
  history samples inside it, the per-profile x dimension flip count from
  the engine's counters (same pid throughout) or the worker mirror's
  word changes, the ``FLIPS_MAX_PER_WINDOW`` bound, the sample-coverage
  floor and the per-regime P&L presence in the window's day report;
  the pooled verdict needs ``SOAK_MIN_WINDOWS`` counted windows. Never
  waits: fewer windows = ``INSUFFICIENT``.
- ``seed-out --refresh-tail``: before exporting, gap-fill the 1 m
  candles of the artifact's own descriptors (≤ 8 instruments, one or
  two REST pages each) so the seed reaches the boot minute — the RG7
  fix of the seed hole (a restart no longer leaves the fast profile
  UNKNOWN for its first hour).
- ``regime_allows(terms, words)``: the coded-lane gate the intent crons
  (``xv_signal`` / ``carry_signal``) call — the §3.3 label grammar folded
  exactly like ``RegimeLabelBuilder``, judged against the engine's
  effective words (``/metrics``), else the fresh declaration, else
  UNKNOWN (fail-closed for a constrained lane, open for an unlabelled one).

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import dataclasses
import datetime
import json
import math
import os
import pathlib
import sqlite3
import sys
import time
import tomllib
import typing
import urllib.request

import httpx

import claude_worker.candles
import claude_worker.config
import claude_worker.frames
import claude_worker.features
import claude_worker.fetchers
import claude_worker.state
import claude_worker.uds
import claude_worker.window_root

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


def refresh_tail(
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    universe_path: pathlib.Path,
    now_ms: int,
    env: typing.Mapping[str, str],
    report: typing.Callable[[str], None] = print,
    http: claude_worker.candles.Http | None = None,
) -> int:
    """RG7 (plan §7.1 seed hole): gap-fill the 1 m candles of the
    artifact's own descriptors — ref, funding ref, breadth members — from
    ``max(open_ts)`` to now, the §9.6 law over a subset. Returns the
    descriptors touched (0 when the universe holds none of them). Boot
    path: one or two pages per instrument under a demand-sized budget;
    any failure is counted and skipped (the seed then reaches as far as
    the store does — the pre-RG7 behaviour)."""
    wanted = set(read_regime_descriptors(regime_path))
    lanes = claude_worker.candles.read_universe_lanes(universe_path) or []
    picked: list[tuple[claude_worker.candles.Lane, claude_worker.candles.LaneTarget]] = [
        (lane, t) for lane in lanes for t in lane.targets if t.descriptor in wanted
    ]
    if not picked:
        report(f"regime refresh-tail: none of {sorted(wanted)} in {universe_path} — skipped")
        return 0
    budget = claude_worker.features.RestBudget(
        2 * len(picked) + 2, claude_worker.fetchers.BUDGET_WINDOW_NS
    )
    conn = claude_worker.candles.open_db(db_path)
    touched = 0

    def fill(h: claude_worker.candles.Http) -> int:
        n_ok = 0
        for lane, target in picked:
            if lane.backward:
                st = claude_worker.candles.fill_okx_backward(
                    conn, h, target, "1m", now_ms, budget, env
                )
            else:
                st = claude_worker.candles.fill_forward(
                    conn, h, lane, target, "1m", now_ms, budget, env
                )
            report(
                f"regime refresh-tail: {target.descriptor} 1m pages={st.pages} bars={st.bars}"
                f" +{st.upsert.inserted}{' BUDGET' if st.budget_out else ''}"
                f"{' FAILED' if st.failed else ''}"
            )
            n_ok += 0 if st.failed else 1
        return n_ok

    try:
        if http is not None:
            touched = fill(http)
        else:
            with httpx.Client() as client:
                touched = fill(claude_worker.candles.make_http(client, env))
    finally:
        conn.close()
    return touched


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


# ---- RG5: the worker regime lane (plan §5.1) ----

REGIME_PATH_DEFAULT: str = "~/multivenue/regime.toml"
REGIME_DIR_ENV: str = "CLAUDE_WORKER_REGIME_DIR"
DEFAULT_REGIME_DIR: str = "~/multivenue/worker/regime"
DECLARED_FILE: str = "declared.json"
HISTORY_FILE: str = "history.ndjson"
METRICS_URL_ENV: str = "CLAUDE_WORKER_METRICS_URL"
DEFAULT_METRICS_URL: str = "http://127.0.0.1:9191/metrics"
DECLARE_TTL_S_ENV: str = "CLAUDE_WORKER_REGIME_DECLARE_TTL_S"
DECLARE_TTL_S_DEFAULT: int = 900
HISTORY_HOURS: int = 24
PROFILE_NAMES: tuple[str, ...] = ("fast", "slow")
DIM_NAMES: tuple[str, ...] = ("trend", "shape", "vol", "fund", "level", "stretch", "source")
#: The market dimensions the RG7 flip bound judges (SOURCE is not a flip).
MARKET_DIMS: tuple[str, ...] = DIM_NAMES[:-1]
#: RG7 (plan §7.1): flips per profile x market dimension per <= 2 h window —
#: the old "24 per day" restated for 2 h.
FLIPS_MAX_PER_WINDOW: int = 2
#: RG7: counted windows a pooled verdict needs (= the RG4 pool size).
SOAK_MIN_WINDOWS: int = 8
#: RG7: 5-min history samples a window needs to be judged (24 nominal).
SOAK_MIN_SAMPLES: int = 20
#: Declaration provenance (persisted in ``declared.json``).
SOURCE_OPERATOR: str = "operator"
SOURCE_MEASURED_WORKER: str = "measured"
SOURCE_STRATEGIST: str = "strategist"
#: Percentile lookbacks (plan §5.1): fast = 7 d of hourly RV samples,
#: slow = 30 d of 4-hourly samples; funding = the profile's ``fund_prints``.
RV_LOOKBACK_DAYS: tuple[int, int] = (7, 30)
RV_SAMPLE_STEP_MIN: tuple[int, int] = (60, 240)
#: Below these sample counts a percentile stays 0 (dimension ABSENT).
MIN_RV_SAMPLES: int = 10
MIN_FUND_SAMPLES: int = 3
_METRIC_NAME_PARTS: int = 4  # engine_regime_<profile>_<kind>
_TERM_PARTS_PREFIXED: int = 3
_TERM_PARTS: int = 2
_PROFILE_KEYS: tuple[str, ...] = tuple(f.name for f in dataclasses.fields(ProfileParams))


def regime_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    """``~/multivenue/worker/regime`` (``$CLAUDE_WORKER_REGIME_DIR``) — the
    module lanes' default."""
    source = os.environ if env is None else env
    return pathlib.Path(source.get(REGIME_DIR_ENV, "") or DEFAULT_REGIME_DIR).expanduser()


def regime_dir_for(db_path: pathlib.Path) -> pathlib.Path:
    """The regime state dir beside a worker ``state.db`` (the
    ``candidates_dir`` precedent: the worker dir + ``regime``, no new env
    key) — what config-carrying callers (recommit, serve) use, so tests
    with a tmp db never touch the operator's directory."""
    return db_path.parent / "regime"


def metrics_url(env: typing.Mapping[str, str] | None = None) -> str:
    source = os.environ if env is None else env
    return source.get(METRICS_URL_ENV, "") or DEFAULT_METRICS_URL


def declare_ttl_s(env: typing.Mapping[str, str] | None = None) -> int:
    source = os.environ if env is None else env
    raw = source.get(DECLARE_TTL_S_ENV, "")
    if not raw:
        return DECLARE_TTL_S_DEFAULT
    value = int(raw)
    if value < 1:
        raise ValueError(f"{DECLARE_TTL_S_ENV} must be >= 1: {value}")
    return value


@dataclasses.dataclass(frozen=True, slots=True)
class Artifact:
    """``regime.toml`` as the worker reads it: descriptors + the evaluator's
    integer parameters (descriptor → a dense id; 0 = the BTC ref)."""

    btc: str
    fund: str
    members: tuple[str, ...]
    params: RegimeParams
    ids: dict[str, int]

    @property
    def descriptors(self) -> list[str]:
        out = [self.btc]
        if self.fund not in out:
            out.append(self.fund)
        for m in self.members:
            if m not in out:
                out.append(m)
        return out


def read_regime_params(path: pathlib.Path) -> Artifact:
    """Parse the artifact (standard TOML — the engine's integer subset is a
    strict subset). Missing/extra profile keys are errors, like the engine."""
    obj = tomllib.loads(path.read_text(encoding="utf-8"))
    refs = obj.get("refs") or {}
    btc = refs.get("btc")
    fund = refs.get("fund")
    if not isinstance(btc, str) or not isinstance(fund, str) or not btc or not fund:
        raise ValueError(f"{path}: [refs] btc/fund missing")
    members = tuple(str(m) for m in (obj.get("breadth") or {}).get("members", []))
    if len(members) > REGIME_MAX_MEMBERS or len(set(members)) != len(members) or btc in members:
        raise ValueError(f"{path}: [breadth] members invalid")
    confirm = int((obj.get("hysteresis") or {}).get("confirm_min", 0))
    profiles: list[ProfileParams] = []
    for name in PROFILE_NAMES:
        pp = (obj.get("profile") or {}).get(name)
        if not isinstance(pp, dict):
            raise ValueError(f"{path}: [profile.{name}] missing")
        unknown = set(pp) - set(_PROFILE_KEYS)
        missing = set(_PROFILE_KEYS) - set(pp)
        if unknown or missing:
            raise ValueError(
                f"{path}: [profile.{name}] unknown={sorted(unknown)} missing={sorted(missing)}"
            )
        profiles.append(ProfileParams(**{k: int(pp[k]) for k in _PROFILE_KEYS}))
    ids: dict[str, int] = {btc: 0}
    for i, m in enumerate(members):
        ids[m] = i + 1
    ids.setdefault(fund, len(ids))
    params = RegimeParams(
        btc_ref=0,
        fund_ref=ids[fund],
        members=tuple(ids[m] for m in members),
        confirm_min=confirm,
        profiles=tuple(profiles),
    )
    return Artifact(btc=btc, fund=fund, members=members, params=params, ids=ids)


def latest_funding(
    conn: sqlite3.Connection, descriptor: str, until_ms: int
) -> tuple[int, int] | None:
    """``(rate_1e9, ts_ms)`` of the latest funding print at/before ``until_ms``."""
    row = conn.execute(
        "SELECT ts_ms, rate FROM funding WHERE descriptor=? AND ts_ms<=?"
        " ORDER BY ts_ms DESC LIMIT 1",
        (descriptor, until_ms),
    ).fetchone()
    if row is None or row[1] is None:
        return None
    return round(float(row[1]) * SCALE_1E9), int(row[0])


def funding_prints(conn: sqlite3.Connection, descriptor: str, n: int, until_ms: int) -> list[int]:
    """The last ``n`` funding rates x1e9 at/before ``until_ms``, oldest first."""
    rows = conn.execute(
        "SELECT rate FROM funding WHERE descriptor=? AND ts_ms<=? ORDER BY ts_ms DESC LIMIT ?",
        (descriptor, until_ms, n),
    ).fetchall()
    return [round(float(r[0]) * SCALE_1E9) for r in reversed(rows) if r[0] is not None]


@dataclasses.dataclass(slots=True)
class Measurement:
    """One worker-side measurement at ``minute`` (the last CLOSED minute)."""

    artifact: Artifact
    evaluator: RegimeEvaluator
    #: The judged minute = the LAST minute holding a BTC-ref close (the
    #: candles lane is hourly, so candles.db lags the wall clock).
    minute: int
    #: Minutes between the judged minute and the last closed wall minute.
    age_min: int
    rows: int
    funding: tuple[int, int] | None


def measure(
    regime_path: pathlib.Path, db_path: pathlib.Path, now_ms: int, minutes: int = REGIME_RING_MIN
) -> Measurement:
    """The engine's seed law over candles.db: fill the rings with the last
    ``minutes`` closes, latch the latest funding print, then judge the last
    ``2·confirm_min`` closed minutes in order (``RegimeState::seed``) —
    ending at the last minute candles.db actually holds for the BTC ref
    (``age_min`` says how far behind the wall clock that is)."""
    art = read_regime_params(regime_path)
    ev = RegimeEvaluator(art.params)
    until_ms = (now_ms // 60_000) * 60_000
    since_ms = until_ms - minutes * 60_000
    conn = claude_worker.candles.open_db(db_path)
    try:
        rows = seed_rows_from_candles(conn, art.descriptors, since_ms, until_ms)
        funding = latest_funding(conn, art.fund, until_ms)
    finally:
        conn.close()
    last_closed = until_ms // 60_000 - 1
    last = last_closed
    btc_minutes = [r.minute for r in rows if r.descriptor == art.btc]
    if btc_minutes:
        last = min(last_closed, max(btc_minutes))
    for r in rows:
        ev.close(r.minute, art.ids[r.descriptor], r.close_1e6)
    if funding is not None:
        ev.funding(funding[0], funding[1])
    replay = 2 * art.params.confirm_min
    for m in range(last - replay + 1, last + 1):
        ev.roll(m, (m + 1) * MINUTE_NS, count=False)
    return Measurement(
        artifact=art,
        evaluator=ev,
        minute=last,
        age_min=last_closed - last,
        rows=len(rows),
        funding=funding,
    )


def raw_dict(raw: Raw) -> dict[str, object]:
    return {
        "ret_bps_1e9": raw.ret_bps_1e9,
        "er_1e9": raw.er_1e9,
        "rv_bps_1e9": raw.rv_bps_1e9,
        "stretch_1e9": raw.stretch_1e9,
        "funding_1e9": raw.funding_1e9,
        "breadth": [raw.breadth_up, raw.breadth_dn, raw.breadth_n],
    }


# ---- engine words (/metrics gauges; /state is RG6) ----


def parse_metrics_words(text: str) -> dict[str, int]:
    """``engine_regime_<profile>_<measured|declared|effective>`` gauges →
    words (the gauge is the u64 word cast to i64)."""
    out: dict[str, int] = {}
    for line in text.splitlines():
        if not line.startswith("engine_regime_"):
            continue
        name, _, value = line.partition(" ")
        parts = name.split("_")
        if len(parts) != _METRIC_NAME_PARTS or parts[2] not in PROFILE_NAMES:
            continue
        if parts[3] not in ("measured", "declared", "effective"):
            continue
        try:
            out[f"{parts[2]}_{parts[3]}"] = int(value.strip()) & ((1 << 64) - 1)
        except ValueError:
            continue
    return out


def engine_words(url: str, timeout_s: float = 2.0) -> dict[str, int] | None:
    """The engine's words from its ``/metrics`` page; ``None`` when the
    engine is unreachable (never an error — the caller falls back)."""
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as resp:  # loopback only
            text = resp.read().decode("utf-8", errors="replace")
    except OSError, ValueError:
        return None
    words = parse_metrics_words(text)
    return words or None


def state_url(env: typing.Mapping[str, str] | None = None) -> str:
    """The engine's ``/state`` (RG6) beside its ``/metrics`` URL."""
    url = metrics_url(env)
    return url[: -len("/metrics")] + "/state" if url.endswith("/metrics") else url + "/state"


def engine_state(url: str, timeout_s: float = 2.0) -> dict[str, object] | None:
    """The engine's ``/state`` document; ``None`` when unreachable or not
    served (a pre-RG6 binary answers 404) — never an error."""
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as resp:  # loopback only
            obj = json.loads(resp.read().decode("utf-8", errors="replace"))
    except OSError, ValueError:
        return None
    return obj if isinstance(obj, dict) and obj.get("v") == 1 else None


def engine_regime_sample(state: dict[str, object]) -> dict[str, object] | None:
    """The RG7 history sample cut from ``/state``: the pid (a counter
    reset tell), the detector's cumulative flips per profile x market
    dimension, minutes judged, effective words, the vm's hard exits.
    ``None`` when the document lacks the regime block."""
    regime = state.get("regime")
    boot = state.get("boot")
    if not isinstance(regime, dict) or not isinstance(boot, dict):
        return None
    flips: dict[str, list[int]] = {}
    effective: dict[str, str] = {}
    for prof in regime.get("profiles") or []:
        if not isinstance(prof, dict) or prof.get("name") not in PROFILE_NAMES:
            continue
        name = str(prof["name"])
        raw = prof.get("flips") or []
        flips[name] = [int(v) for v in raw[: len(MARKET_DIMS)]]
        eff = prof.get("effective")
        if isinstance(eff, dict) and isinstance(eff.get("hex"), str):
            effective[name] = str(eff["hex"])
    if len(flips) != len(PROFILE_NAMES):
        return None
    vm = state.get("vm") if isinstance(state.get("vm"), dict) else {}
    return {
        "pid": int(boot.get("pid", 0)),
        "seq": int(state.get("seq", 0)),
        "configured": int(regime.get("configured", 0)),
        "minutes_judged": int(regime.get("minutes_judged", 0)),
        "flips": flips,
        "effective": effective,
        "vm_rows": int(vm.get("rows_active", 0)),
        "hard_exits": int(vm.get("regime_hard_exits", 0)),
    }


# ---- declarations (declared.json + SetRegime frames) ----


def parse_declaration(spec: str) -> dict[str, str]:
    """``"trend:bull,shape:trend"`` → ``{"trend": "bull", "shape": "trend"}``.
    One VALUE per dimension (a declaration is a word, not a region); the
    ``unknown`` token marks a market dimension unjudgeable."""
    dims: dict[str, str] = {}
    for part in spec.split(","):
        term = part.strip()
        if not term:
            continue
        dim, sep, value = term.partition(":")
        if not sep or dim not in claude_worker.frames.REGIME_DIMS or dim == "source":
            raise ValueError(f"declaration term {term!r}: want <dim>:<value> over {DIM_NAMES[:-1]}")
        values = claude_worker.frames.REGIME_VALUES[dim]
        if value != "unknown" and value not in values:
            raise ValueError(f"declaration term {term!r}: value must be one of {values} or unknown")
        if dim in dims:
            raise ValueError(f"declaration term {term!r}: dimension named twice")
        dims[dim] = value
    if not dims:
        raise ValueError("declaration names no dimension")
    return dims


def declaration_word(dims: dict[str, str]) -> int:
    """The wire word of a declaration (SOURCE byte empty — the engine stamps it)."""
    return claude_worker.frames.regime_word(**dims)


def word_dims(word: int) -> dict[str, str]:
    return claude_worker.frames.regime_word_dims(word)


def declared_path(directory: pathlib.Path) -> pathlib.Path:
    return directory / DECLARED_FILE


def load_declared(directory: pathlib.Path) -> dict[str, dict[str, object]]:
    """``{profile: {word, dims, ts_ms, ttl_s, source}}`` (empty when absent
    or unreadable — a bad file is a fresh start, never a crash)."""
    path = declared_path(directory)
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except OSError, ValueError:
        return {}
    profiles = obj.get("profiles") if isinstance(obj, dict) else None
    if not isinstance(profiles, dict):
        return {}
    out: dict[str, dict[str, object]] = {}
    for name in PROFILE_NAMES:
        entry = profiles.get(name)
        if isinstance(entry, dict) and isinstance(entry.get("word"), str):
            out[name] = entry
    return out


def declared_is_fresh(entry: dict[str, object], now_ms: int) -> bool:
    try:
        ts_ms = int(entry["ts_ms"])  # type: ignore[arg-type]
        ttl_s = int(entry["ttl_s"])  # type: ignore[arg-type]
    except KeyError, TypeError, ValueError:
        return False
    return ttl_s > 0 and 0 <= now_ms - ts_ms < ttl_s * 1000


def persist_declared(
    directory: pathlib.Path,
    words: dict[str, int],
    ts_ms: int,
    ttl_s: int,
    source: str,
) -> pathlib.Path:
    """Merge ``words`` (profile → declared word) into ``declared.json``
    (atomic write); untouched profiles keep their entry."""
    directory.mkdir(parents=True, exist_ok=True)
    current = load_declared(directory)
    for name, word in words.items():
        current[name] = {
            "word": word_hex(word),
            "dims": word_dims(word),
            "ts_ms": ts_ms,
            "ttl_s": ttl_s,
            "source": source,
        }
    path = declared_path(directory)
    tmp = path.with_suffix(".json.tmp")
    tmp.write_text(
        json.dumps({"profiles": current}, indent=1, sort_keys=True) + "\n", encoding="utf-8"
    )
    os.replace(tmp, path)
    return path


def send_declarations(
    client: typing.Any,
    words: dict[str, int],
    ttl_ns: int,
    measured: dict[str, int] | None = None,
    ts_ns: int | None = None,
) -> list[int]:
    """One ``SetRegime`` frame per profile over an already-heartbeated
    ``UdsClient`` (``px`` = the declared word, ``qty`` = the worker-measured
    word for the audit trail). Returns the seqs used."""
    seqs: list[int] = []
    for name, word in words.items():
        profile = PROFILE_NAMES.index(name)
        audit = (measured or {}).get(name, 0)
        seqs.append(
            client.send_cmd(
                ts_ns=ts_ns,
                sym=claude_worker.frames.SYMBOL_ID_NONE,
                px=word,
                qty=audit,
                ttl_ns=ttl_ns,
                kind=claude_worker.frames.KIND_SET_REGIME,
                venue=claude_worker.frames.VENUE_AI,
                strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
                side=claude_worker.frames.SIDE_NONE,
                param_id=profile,
                flags=0,
            )
        )
    return seqs


def repush_declared(
    client: typing.Any,
    directory: pathlib.Path,
    now_ms: int,
    report: typing.Callable[[str], None] = print,
) -> int:
    """The post-boot lane (plan §4.3): re-send every still-fresh entry of
    ``declared.json`` with its OWN remaining TTL (the engine forgot it at
    the restart; the wall clock did not). Returns frames sent."""
    entries = load_declared(directory)
    sent: list[int] = []
    left_by_name: dict[str, int] = {}
    for name, entry in entries.items():
        if not declared_is_fresh(entry, now_ms):
            continue
        left = int(entry["ttl_s"]) - (now_ms - int(entry["ts_ms"])) // 1000  # type: ignore[arg-type]
        if left <= 0:
            continue
        word = int(str(entry["word"]), 16)
        sent.extend(send_declarations(client, {name: word}, left * 1_000_000_000))
        left_by_name[name] = left
    if not sent:
        report("regime repush: no fresh declaration persisted — nothing to re-send")
        return 0
    report(
        f"regime repush: re-sent {len(sent)} declaration(s) {left_by_name} s left (seqs {sent})"
    )
    return len(sent)


# ---- labels (the §3.3 grammar, mirror of core_types::regime::RegimeLabelBuilder) ----

_DIM_VALUE_COUNT: dict[str, int] = {
    name: len(v) for name, v in claude_worker.frames.REGIME_VALUES.items()
}
_SOURCE_DEFAULT_MASK: int = (1 << SOURCE_MEASURED) | (1 << SOURCE_DECLARED)


def _dim_any_mask(dim: str) -> int:
    known = (1 << _DIM_VALUE_COUNT[dim]) - 1
    return known if dim == "source" else known | DIM_UNKNOWN_BIT


def parse_label_term(term: str) -> tuple[int, str, int]:  # noqa: PLR0912 — one branch per grammar arm
    """``[fast:|slow:]<dim>:<values>`` → ``(profile, dim, mask)`` — the Rust
    ``parse_label_term`` law (``*`` = any incl. the unknown mark, ``!v`` =
    known values but ``v``, ``v1|v2|unknown`` = exactly those). ``rel`` is
    refused here: coded lanes are not per-symbol (rows carry it)."""
    parts = term.split(":")
    profile = 0
    if len(parts) == _TERM_PARTS_PREFIXED:
        if parts[0] not in PROFILE_NAMES:
            raise ValueError(f"label term {term!r}: unknown profile prefix")
        profile = PROFILE_NAMES.index(parts[0])
        parts = parts[1:]
    if len(parts) != _TERM_PARTS or not parts[0] or not parts[1]:
        raise ValueError(f"label term {term!r}: want [fast:|slow:]<dim>:<values>")
    dim, values = parts
    if dim == "rel":
        raise ValueError(f"label term {term!r}: rel: terms are per-symbol (rows only)")
    if dim not in _DIM_VALUE_COUNT:
        raise ValueError(f"label term {term!r}: unknown dimension")
    vocab = claude_worker.frames.REGIME_VALUES[dim]
    if values == "*":
        return profile, dim, _dim_any_mask(dim)
    known = (1 << _DIM_VALUE_COUNT[dim]) - 1
    if values.startswith("!"):
        if values[1:] not in vocab:
            raise ValueError(f"label term {term!r}: unknown value")
        mask = known & ~(1 << vocab.index(values[1:]))
    else:
        mask = 0
        for v in values.split("|"):
            if v == "unknown" and dim != "source":
                mask |= DIM_UNKNOWN_BIT
            elif v in vocab:
                mask |= 1 << vocab.index(v)
            else:
                raise ValueError(f"label term {term!r}: unknown value {v!r}")
    if mask == 0:
        raise ValueError(f"label term {term!r}: allows nothing")
    return profile, dim, mask


def label_masks(terms: typing.Iterable[str]) -> dict[str, int]:
    """Fold terms into one mask per profile (``{"fast": u64, "slow": u64}``;
    0 = unconstrained). Omitted dimensions of a touched profile fill with
    the any-mask (SOURCE with measured|declared) — the builder's law."""
    seen: dict[int, dict[str, int]] = {0: {}, 1: {}}
    for term in terms:
        profile, dim, mask = parse_label_term(term)
        if dim in seen[profile]:
            raise ValueError(f"label term {term!r}: dimension named twice on this profile")
        seen[profile][dim] = mask
    out: dict[str, int] = {}
    for profile, name in enumerate(PROFILE_NAMES):
        dims = seen[profile]
        if not dims:
            out[name] = 0
            continue
        word = 0
        for d, dim in enumerate(DIM_NAMES):
            byte = dims.get(dim, _SOURCE_DEFAULT_MASK if dim == "source" else _dim_any_mask(dim))
            word |= byte << (8 * d)
        out[name] = word
    return out


def label_allows(mask: int, word: int) -> bool:
    """``core_types::regime::RegimeLabel::allows``."""
    return mask == 0 or (word & mask) == word


def regime_allows(terms: typing.Iterable[str], words: dict[str, int]) -> bool:
    """A coded lane's gate: every profile's mask allows the effective word
    (``words`` = ``{"fast": w, "slow": w}``; a missing profile is UNKNOWN —
    fail-closed for a constrained profile, open for ``0``)."""
    masks = label_masks(terms)
    return all(label_allows(masks[name], words.get(name, UNKNOWN_WORD)) for name in PROFILE_NAMES)


def current_words(
    directory: pathlib.Path,
    now_ms: int,
    url: str | None = None,
) -> tuple[dict[str, int], str]:
    """The effective words a lane gates on: the ENGINE's (``/metrics``)
    when reachable; else the fresh ``declared.json`` entries (stamped
    DECLARED over unknown dimensions); else UNKNOWN. Returns
    ``(words, source)`` with source ∈ engine|declared|unknown."""
    live = engine_words(metrics_url() if url is None else url)
    if live is not None and all(f"{n}_effective" in live for n in PROFILE_NAMES):
        return {n: live[f"{n}_effective"] for n in PROFILE_NAMES}, "engine"
    entries = load_declared(directory)
    words: dict[str, int] = {}
    for name in PROFILE_NAMES:
        entry = entries.get(name)
        if entry is not None and declared_is_fresh(entry, now_ms):
            words[name] = merge_declared(int(str(entry["word"]), 16), UNKNOWN_WORD)
        else:
            words[name] = UNKNOWN_WORD
    return words, ("declared" if any(w != UNKNOWN_WORD for w in words.values()) else "unknown")


def lane_gate(
    label: typing.Iterable[str],
    now_ms: int,
    words: dict[str, int] | None = None,
    directory: pathlib.Path | None = None,
    url: str | None = None,
) -> tuple[bool, str]:
    """A coded intent lane's entry gate (plan §5.1 — the xv / carry
    cron modules): ``(entries_open, tell)``. An empty label is ANY —
    open without touching the engine or the files (every lane's default;
    bit-identical to pre-RG5). A labelled lane judges ``words`` when the
    caller supplies them, else the ``current_words`` chain (engine →
    fresh declaration → UNKNOWN, which fails a constrained profile
    closed). Exits are never gated — callers only consult this on entry."""
    terms = tuple(label)
    if not terms:
        return True, "regime: any"
    if words is None:
        words, source = current_words(regime_dir() if directory is None else directory, now_ms, url)
    else:
        source = "given"
    open_ = regime_allows(terms, words)
    shown = " ".join(f"{n}=[{describe(words.get(n, UNKNOWN_WORD))}]" for n in PROFILE_NAMES)
    return open_, f"regime: {'open' if open_ else 'ENTRIES BLOCKED'} label={list(terms)} ({source}) {shown}"


# ---- history (24 h of worker words under ~/multivenue/worker/regime/) ----


def append_history(directory: pathlib.Path, entry: dict[str, object]) -> pathlib.Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / HISTORY_FILE
    with path.open("a", encoding="utf-8") as f:
        f.write(json.dumps(entry, sort_keys=True, separators=(",", ":")) + "\n")
    return path


def history_tail(
    directory: pathlib.Path, now_ms: int, hours: int = HISTORY_HOURS
) -> list[dict[str, object]]:
    path = directory / HISTORY_FILE
    if not path.is_file():
        return []
    since = now_ms - hours * 3_600_000
    out: list[dict[str, object]] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and int(obj.get("ts_ms", 0)) >= since:
            out.append(obj)
    return out


def history_entry(
    m: Measurement, now_ms: int, engine: dict[str, object] | None = None
) -> dict[str, object]:
    ev = m.evaluator
    entry: dict[str, object] = {
        "ts_ms": now_ms,
        "minute": m.minute,
        "age_min": m.age_min,
        "rows": m.rows,
    }
    for p, name in enumerate(PROFILE_NAMES):
        entry[name] = word_hex(ev.measured[p])
        entry[f"{name}_raw"] = raw_dict(ev.raw[p])
    if engine is not None:
        entry["engine"] = engine  # RG7: the soak judge's source of truth
    return entry


def words_timeline(entries: list[dict[str, object]], profile: str) -> list[tuple[str, int]]:
    """Consecutive ``(describe(word), samples)`` runs of a history tail."""
    out: list[tuple[str, int]] = []
    for e in entries:
        w = e.get(profile)
        if not isinstance(w, str):
            continue
        text = describe(int(w, 16))
        if out and out[-1][0] == text:
            out[-1] = (text, out[-1][1] + 1)
        else:
            out.append((text, 1))
    return out


# ---- RG7: the ≤ 2 h-law soak judge (plan §7.1) ----


class SoakWindow(typing.NamedTuple):
    """One pooled window: the cut's name and its wall span (ms)."""

    name: str
    start_ms: int
    end_ms: int


class WindowVerdict(typing.NamedTuple):
    """One window's judgement."""

    name: str
    start_ms: int
    end_ms: int
    samples: int
    source: str  # engine | mirror | none
    flips: dict[str, list[int]]  # profile -> per MARKET_DIMS
    worst: tuple[str, str, int]  # (profile, dim, flips)
    hard_exits: int
    pnl_regime: bool
    verdict: str  # PASS | FAIL | short | ungated

    def as_json(self) -> dict[str, object]:
        return {**self._asdict(), "worst": list(self.worst)}


def soak_windows_from_pool(pool_dir: pathlib.Path) -> list[SoakWindow]:
    """The pool's cuts as wall spans: a cut is named ``run-<epoch_ns>`` by
    its own start and is exactly ``WINDOW_MAX_S`` long (complete windows
    only — the RG4 pool law)."""
    out: list[SoakWindow] = []
    for cut in claude_worker.window_root.pool_windows(pool_dir):
        try:
            epoch_ns = int(cut.name[4:])
        except ValueError:
            continue
        start_ms = epoch_ns // 1_000_000
        out.append(
            SoakWindow(
                cut.name, start_ms, start_ms + int(claude_worker.window_root.WINDOW_MAX_S * 1000)
            )
        )
    return out


def soak_windows_from_runs(logs_dir: pathlib.Path, since_ms: int) -> list[SoakWindow]:
    """Every complete ≤ 2 h window of every judged (v3+) run under
    ``logs_dir`` starting at/after ``since_ms`` — the pool's candidate
    law WITHOUT cutting anything (the judge needs spans, not files).
    Oldest first."""
    out: list[SoakWindow] = []
    for c in claude_worker.window_root.pool_candidates(logs_dir):
        try:
            epoch_ns = int(c.name[4:])
        except ValueError:
            continue
        start_ms = epoch_ns // 1_000_000
        if start_ms < since_ms:
            continue
        out.append(SoakWindow(c.name, start_ms, start_ms + int((c.to_s - c.from_s) * 1000)))
    out.sort(key=lambda w: w.start_ms)
    return out


def _engine_of(entry: dict[str, object]) -> dict[str, object] | None:
    e = entry.get("engine")
    return e if isinstance(e, dict) and isinstance(e.get("flips"), dict) else None


def _mirror_flips(inside: list[dict[str, object]]) -> dict[str, list[int]]:
    """Word changes per profile x market dimension between consecutive
    worker samples (5-min resolution — a bound, not a count)."""
    flips = {name: [0] * len(MARKET_DIMS) for name in PROFILE_NAMES}
    prev: dict[str, int] = {}
    for e in inside:
        for name in PROFILE_NAMES:
            w = e.get(name)
            if not isinstance(w, str):
                continue
            word = int(w, 16)
            if name in prev:
                for d in range(len(MARKET_DIMS)):
                    if word_dim(word, d) != word_dim(prev[name], d):
                        flips[name][d] += 1
            prev[name] = word
    return flips


def _engine_flips(
    before: dict[str, object] | None, inside: list[dict[str, object]]
) -> dict[str, list[int]] | None:
    """Counter deltas from the engine samples: the last sample before the
    window (same pid) or the first inside, to the last inside. ``None``
    when the pid changed inside the window (a restart reset the counters)
    or no engine sample exists."""
    engines = [e for e in (_engine_of(x) for x in inside) if e is not None]
    if not engines:
        return None
    pid = engines[0]["pid"]
    if any(e["pid"] != pid for e in engines):
        return None
    base = before if before is not None and before.get("pid") == pid else engines[0]
    last = engines[-1]
    out: dict[str, list[int]] = {}
    for name in PROFILE_NAMES:
        b = typing.cast(dict[str, list[int]], base["flips"]).get(name, [])
        cur = typing.cast(dict[str, list[int]], last["flips"]).get(name, [])
        out[name] = [
            max(0, int(cur[d]) - int(b[d])) if d < len(cur) and d < len(b) else 0
            for d in range(len(MARKET_DIMS))
        ]
    return out


def pnl_regime_present(reports_dir: pathlib.Path, day: str) -> bool:
    """The day's ``pnl-<day>.json`` carries per-regime fill-model rows
    (RG5's merged section) for at least one word per profile."""
    path = reports_dir / f"pnl-{day}.json"
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except OSError, ValueError:
        return False
    section = obj.get("regime") if isinstance(obj, dict) else None
    profiles = section.get("profiles") if isinstance(section, dict) else None
    if not isinstance(profiles, list) or not profiles:
        return False
    seen = {
        str(p.get("profile"))
        for p in profiles
        if isinstance(p, dict)
        and any(isinstance(w, dict) and w.get("strategies") for w in (p.get("words") or []))
    }
    return all(name in seen for name in PROFILE_NAMES)


def judge_window(
    entries: list[dict[str, object]],
    w: SoakWindow,
    reports_dir: pathlib.Path | None,
    flips_max: int = FLIPS_MAX_PER_WINDOW,
    min_samples: int = SOAK_MIN_SAMPLES,
) -> WindowVerdict:
    """One window under §7.1: coverage, gating live at every engine
    sample, the flip bound from the engine's counters (else the mirror),
    hard exits, the day report's per-regime section."""
    inside = [e for e in entries if w.start_ms <= int(e.get("ts_ms", 0)) < w.end_ms]
    before_entries = [e for e in entries if int(e.get("ts_ms", 0)) < w.start_ms]
    before = _engine_of(before_entries[-1]) if before_entries else None
    flips = _engine_flips(before, inside)
    source = "engine"
    if flips is None:
        flips = _mirror_flips(inside)
        source = "mirror" if inside else "none"
    engines = [e for e in (_engine_of(x) for x in inside) if e is not None]
    hard = 0
    if engines and engines[0]["pid"] == engines[-1]["pid"]:
        hard = max(0, int(engines[-1]["hard_exits"]) - int(engines[0]["hard_exits"]))
    worst = ("", "", -1)
    for name in PROFILE_NAMES:
        for d, dim in enumerate(MARKET_DIMS):
            if flips[name][d] > worst[2]:
                worst = (name, dim, flips[name][d])
    day = datetime.datetime.fromtimestamp(w.start_ms / 1000, tz=datetime.timezone.utc).strftime(
        "%Y-%m-%d"
    )
    pnl_ok = pnl_regime_present(reports_dir, day) if reports_dir is not None else False
    # Gating must have been LIVE through the window: the detector
    # configured and a table active at every engine sample (§7.1).
    ungated = any(
        int(e.get("configured", 0)) == 0 or int(e.get("vm_rows", 0)) == 0 for e in engines
    )
    if len(inside) < min_samples:
        verdict = "short"
    elif ungated:
        verdict = "ungated"
    elif worst[2] > flips_max:
        verdict = "FAIL"
    else:
        verdict = "PASS"
    return WindowVerdict(
        w.name, w.start_ms, w.end_ms, len(inside), source, flips, worst, hard, pnl_ok, verdict
    )


def run_soak(  # noqa: PLR0913 — one parameter per input source, deliberately
    directory: pathlib.Path,
    windows: list[SoakWindow],
    reports_dir: pathlib.Path | None,
    now_ms: int,
    report: typing.Callable[[str], None] = print,
    hours: int = HISTORY_HOURS,
    min_windows: int = SOAK_MIN_WINDOWS,
    source: str = "runs",
) -> dict[str, object]:
    """The ``soak`` lane: judge every window against the history tail,
    print one line per window and the pooled verdict, write the JSON
    beside the history. Never waits."""
    entries = history_tail(directory, now_ms, hours)
    verdicts = [judge_window(entries, w, reports_dir) for w in windows]
    counted = [v for v in verdicts if v.verdict in ("PASS", "FAIL")]
    failed = [v for v in counted if v.verdict == "FAIL"]
    if len(counted) < min_windows:
        pooled = "INSUFFICIENT"
    elif failed:
        pooled = "FAIL"
    else:
        pooled = "PASS"
    for v in verdicts:
        stamp = datetime.datetime.fromtimestamp(v.start_ms / 1000, tz=datetime.timezone.utc)
        report(
            f"soak {v.name} {stamp.strftime('%Y-%m-%dT%H:%MZ')} samples={v.samples}"
            f" src={v.source} worst={v.worst[0]}/{v.worst[1]}={v.worst[2]}"
            f" hard_exits={v.hard_exits} pnl_regime={'yes' if v.pnl_regime else 'no'}"
            f" -> {v.verdict}"
        )
    report(
        f"soak verdict: {pooled} (windows {len(windows)}, counted {len(counted)},"
        f" failed {len(failed)}, need {min_windows}; flips ≤ {FLIPS_MAX_PER_WINDOW}"
        f" per profile x dim per window; history {len(entries)} samples / {hours} h)"
    )
    out: dict[str, object] = {
        "v": 1,
        "now_ms": now_ms,
        "law": "docs/regime-and-dashboard-plan.md §7.1 — N pooled ≤ 2 h windows, never a wait",
        "flips_max_per_window": FLIPS_MAX_PER_WINDOW,
        "min_windows": min_windows,
        "min_samples": SOAK_MIN_SAMPLES,
        "history_samples": len(entries),
        "windows_source": source,
        "windows": [v.as_json() for v in verdicts],
        "counted": len(counted),
        "failed": len(failed),
        "verdict": pooled,
    }
    directory.mkdir(parents=True, exist_ok=True)
    stamp = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc)
    path = directory / f"soak-{stamp.strftime('%Y%m%dT%H%M%SZ')}.json"
    path.write_text(json.dumps(out, indent=1, sort_keys=True), encoding="utf-8")
    out["path"] = str(path)
    return out


# ---- percentiles → regime.toml (refresh-params) ----


def _closes_series(
    conn: sqlite3.Connection, descriptor: str, since_ms: int, until_ms: int
) -> dict[int, int]:
    rows = seed_rows_from_candles(conn, [descriptor], since_ms, until_ms)
    return {r.minute: r.close_1e6 for r in rows}


def _series_close_at(series: dict[int, int], m: int) -> int:
    for k in range(MAX_BACK_MIN + 1):
        c = series.get(m - k, 0)
        if c > 0:
            return c
    return 0


def _series_rv(series: dict[int, int], m: int, w: int) -> int | None:
    total = 0
    present = 0
    for k in range(w):
        a = _series_close_at(series, m - k - 1)
        b = _series_close_at(series, m - k)
        if a > 0 and b > 0:
            r = ret_bps_1e9(a, b)
            total += r * r
            present += 1
    if present * 5 < w * 4:
        return None
    return isqrt_i128(total)


def percentile_nearest_rank(samples: list[int], pct: int) -> int:
    """Nearest-rank percentile of integer samples (deterministic, no
    interpolation — the engine compares integers)."""
    if not samples:
        return 0
    ordered = sorted(samples)
    rank = max(1, math.ceil(pct / 100 * len(ordered)))
    return ordered[min(rank, len(ordered)) - 1]


def compute_percentiles(
    regime_path: pathlib.Path, db_path: pathlib.Path, now_ms: int
) -> dict[str, dict[str, int]]:
    """Per profile: ``rv_p30/p70_bps_1e9`` over the lookback's periodic RV
    samples and ``fund_p30/p70_1e9`` over the last ``fund_prints`` prints.
    A profile with too few samples (< 10 RV samples / < 3 prints) keeps
    zeros — the engine then judges that dimension ABSENT, honestly."""
    art = read_regime_params(regime_path)
    until_ms = (now_ms // 60_000) * 60_000
    last = until_ms // 60_000 - 1
    out: dict[str, dict[str, int]] = {}
    conn = claude_worker.candles.open_db(db_path)
    try:
        for p, name in enumerate(PROFILE_NAMES):
            pp = art.params.profiles[p]
            days = RV_LOOKBACK_DAYS[p]
            step = RV_SAMPLE_STEP_MIN[p]
            since_ms = until_ms - (days * 1440 + pp.vol_w_min + MAX_BACK_MIN) * 60_000
            series = _closes_series(conn, art.btc, since_ms, until_ms)
            samples: list[int] = []
            m = last
            first = last - days * 1440
            while m > first:
                rv = _series_rv(series, m, pp.vol_w_min)
                if rv is not None:
                    samples.append(rv)
                m -= step
            prints = funding_prints(conn, art.fund, pp.fund_prints, until_ms)
            rv_ok = len(samples) >= MIN_RV_SAMPLES
            fund_ok = len(prints) >= MIN_FUND_SAMPLES
            entry = {
                "rv_p30_bps_1e9": percentile_nearest_rank(samples, 30) if rv_ok else 0,
                "rv_p70_bps_1e9": percentile_nearest_rank(samples, 70) if rv_ok else 0,
                "fund_p30_1e9": percentile_nearest_rank(prints, 30) if fund_ok else 0,
                "fund_p70_1e9": percentile_nearest_rank(prints, 70) if fund_ok else 0,
                "rv_samples": len(samples),
                "fund_samples": len(prints),
            }
            out[name] = entry
    finally:
        conn.close()
    return out


_PERCENTILE_KEYS: tuple[str, ...] = (
    "rv_p30_bps_1e9",
    "rv_p70_bps_1e9",
    "fund_p30_1e9",
    "fund_p70_1e9",
)


def rewrite_percentile_lines(text: str, values: dict[str, dict[str, int]]) -> str:
    """Rewrite ONLY the four percentile lines of each ``[profile.<name>]``
    section, keeping every other byte (comments included) — the artifact is
    the operator's; the worker owns these six numbers (plan §4.6)."""
    lines = text.splitlines(keepends=True)
    section: str | None = None
    for i, raw in enumerate(lines):
        stripped = raw.split("#", 1)[0].strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            head = stripped[1:-1].strip()
            section = head[len("profile.") :] if head.startswith("profile.") else None
            continue
        if section not in values:
            continue
        key, sep, _rest = stripped.partition("=")
        key = key.strip()
        if not sep or key not in _PERCENTILE_KEYS:
            continue
        comment = ""
        if "#" in raw:
            comment = "  #" + raw.split("#", 1)[1].rstrip("\n")
        newline = "\n" if raw.endswith("\n") else ""
        lines[i] = f"{key} = {values[section][key]}{comment}{newline}"
    return "".join(lines)


def refresh_params(
    regime_path: pathlib.Path, db_path: pathlib.Path, now_ms: int
) -> dict[str, dict[str, int]]:
    """Compute the percentiles and rewrite them into the artifact (atomic;
    ``.bak`` beside it). The engine applies them at its next restart."""
    values = compute_percentiles(regime_path, db_path, now_ms)
    text = regime_path.read_text(encoding="utf-8")
    new = rewrite_percentile_lines(text, values)
    if new != text:
        backup = regime_path.with_suffix(regime_path.suffix + ".bak")
        backup.write_text(text, encoding="utf-8")
        tmp = regime_path.with_suffix(regime_path.suffix + ".tmp")
        tmp.write_text(new, encoding="utf-8")
        os.replace(tmp, regime_path)
    return values


# ---- the report (the AI's input in semi-manual mode) ----


def render_report(
    m: Measurement,
    declared: dict[str, dict[str, object]],
    engine: dict[str, int] | None,
    history: list[dict[str, object]],
    now_ms: int,
) -> str:
    ev = m.evaluator
    stamp = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )
    out = [
        f"regime report {stamp} minute={m.minute} (candles lag {m.age_min} min) rows={m.rows}"
        f" funding={m.funding}"
    ]
    for p, name in enumerate(PROFILE_NAMES):
        pp = m.artifact.params.profiles[p]
        out.append(f"[{name}] measured: {describe(ev.measured[p])}")
        r = ev.raw[p]
        out.append(
            f"  raw: ret_bps={_fmt_1e9(r.ret_bps_1e9)} er={_fmt_1e9(r.er_1e9)}"
            f" rv_bps={_fmt_1e9(r.rv_bps_1e9)}"
            f" stretch={_fmt_1e9(r.stretch_1e9)} funding={_fmt_1e9(r.funding_1e9)}"
            f" breadth={r.breadth_up}/{r.breadth_dn}/{r.breadth_n}"
            f" bands: trend_thr={_fmt_1e9(pp.trend_thr_bps_1e9)}bps"
            f" er={_fmt_1e9(pp.er_lo_enter_1e9)}..{_fmt_1e9(pp.er_hi_enter_1e9)}"
            f" rv_p30/p70={_fmt_1e9(pp.rv_p30_bps_1e9)}/{_fmt_1e9(pp.rv_p70_bps_1e9)}"
            f" stretch_k={_fmt_1e9(pp.stretch_k_1e9)}"
        )
        entry = declared.get(name)
        if entry is not None:
            fresh = declared_is_fresh(entry, now_ms)
            age_s = (now_ms - int(entry.get("ts_ms", 0))) // 1000  # type: ignore[arg-type]
            out.append(
                f"  declared: {describe(int(str(entry['word']), 16))} source={entry.get('source')}"
                f" age={age_s}s ttl={entry.get('ttl_s')}s {'FRESH' if fresh else 'expired'}"
            )
        else:
            out.append("  declared: none")
        if engine is not None:
            out.append(
                f"  engine: measured={describe(engine.get(f'{name}_measured', UNKNOWN_WORD))}"
                f" | effective={describe(engine.get(f'{name}_effective', UNKNOWN_WORD))}"
            )
        else:
            out.append("  engine: unreachable (/metrics)")
        timeline = words_timeline(history, name)
        if timeline:
            out.append("  24h: " + " -> ".join(f"{t} x{n}" for t, n in timeline[-8:]))
        else:
            out.append("  24h: (no history yet)")
    return "\n".join(out) + "\n"


def _fmt_1e9(v: int | None) -> str:
    if v is None:
        return "absent"
    return f"{v / SCALE_1E9:.4f}"


REGIME_UNMEASURED_TEXT: str = "  (no regime artifact / candles.db — the regime is unmeasured)"


def regime_digest_text(
    directory: pathlib.Path,
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    now_ms: int,
    pnl_regimes: list[dict[str, object]] | None = None,
) -> str:
    """The strategist's REGIME digest section (plan §5.4): the worker's
    measured words + raw values, the declaration in force, the engine's
    effective words, the 24 h timeline, and the per-regime P&L rows of the
    latest nightly report. Absent inputs render as honest text."""
    if not regime_path.is_file() or not db_path.is_file():
        return REGIME_UNMEASURED_TEXT
    try:
        m = measure(regime_path, db_path, now_ms)
    except (ValueError, sqlite3.Error) as exc:
        return f"  (regime measurement failed: {exc})"
    return regime_digest_from(m, directory, now_ms, pnl_regimes)


def regime_digest_from(
    m: Measurement,
    directory: pathlib.Path,
    now_ms: int,
    pnl_regimes: list[dict[str, object]] | None = None,
    url: str | None = None,
) -> str:
    """:func:`regime_digest_text` from an existing measurement (the serve
    phase measures once and renders from it)."""
    text = render_report(
        m,
        load_declared(directory),
        engine_words(metrics_url() if url is None else url),
        history_tail(directory, now_ms),
        now_ms,
    )
    lines = ["  " + line for line in text.rstrip("\n").splitlines()]
    if pnl_regimes:
        lines.append("  per-regime P&L (latest nightly report; net @0/@1/@2 bps, tier):")
        for prof in pnl_regimes:
            for w in prof.get("words") or []:  # type: ignore[union-attr]
                for srow in w.get("strategies") or []:  # type: ignore[union-attr]
                    lines.append(
                        f"    {prof.get('profile')} [{w.get('word')}] minutes={w.get('minutes')}"
                        f" {srow.get('label')}: fills={srow.get('fills')}"
                        f" ladder={srow.get('fee_ladder_net_usd')}"
                        f" tier={srow.get('net_usd')}"
                    )
    return "\n".join(lines)


# ---- one cron cycle ----


def run_cycle(  # noqa: PLR0913, PLR0917 — one argument per input source
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    directory: pathlib.Path,
    now_ms: int,
    report: typing.Callable[[str], None] = print,
    refresh_daily: bool = True,
    engine_url: str | None = None,
) -> Measurement | None:
    """The ``com.multivenue.regime`` 5-minute cycle: measure, append the
    history line (with the engine's ``/state`` regime sample when it
    answers — RG7), and once per UTC day refresh the percentile lines.
    No declaration — that is the AI's / operator's call (``declare``)."""
    if not regime_path.is_file():
        report(f"regime cycle: {regime_path} absent — nothing to measure")
        return None
    if not db_path.is_file():
        report(f"regime cycle: {db_path} absent — nothing to measure")
        return None
    m = measure(regime_path, db_path, now_ms)
    state = engine_state(state_url() if engine_url is None else engine_url)
    sample = engine_regime_sample(state) if state is not None else None
    append_history(directory, history_entry(m, now_ms, sample))
    report(
        f"regime cycle: minute={m.minute} (lag {m.age_min} min) rows={m.rows}"
        f" fast={describe(m.evaluator.measured[0])} | slow={describe(m.evaluator.measured[1])}"
        + (
            f" engine pid={sample['pid']} judged={sample['minutes_judged']}"
            if sample is not None
            else " engine=unreachable"
        )
    )
    if refresh_daily:
        stamp = directory / "params-refreshed-utc-day"
        today = datetime.datetime.fromtimestamp(now_ms / 1000, tz=datetime.timezone.utc).strftime(
            "%Y%m%d"
        )
        last = stamp.read_text(encoding="utf-8").strip() if stamp.is_file() else ""
        if last != today:
            values = refresh_params(regime_path, db_path, now_ms)
            stamp.write_text(today + "\n", encoding="utf-8")
            report(f"regime cycle: percentiles refreshed {json.dumps(values, sort_keys=True)}")
    return m


# ---- the serve lane (plan §5.4: the _REGIME phase + the strategist verdict) ----

#: Override for the artifact path (tests / alternate installs).
REGIME_TOML_ENV: str = "CLAUDE_WORKER_REGIME_TOML"
#: ``declared.json`` source of the serve phase's auto-confirm.
SOURCE_SERVE: str = "serve-measured"
#: Verdict value meaning "confirm the worker-measured word".
VERDICT_MEASURED: str = "measured"


def regime_inputs(env: typing.Mapping[str, str] | None = None) -> tuple[pathlib.Path, pathlib.Path]:
    """``(regime.toml, candles.db)`` — the artifact from ``$CLAUDE_WORKER_REGIME_TOML``
    (default ``~/multivenue/regime.toml``), the store from
    ``$CLAUDE_WORKER_CANDLES_DB`` (the candles lane's own default)."""
    source = os.environ if env is None else env
    return (
        pathlib.Path(source.get(REGIME_TOML_ENV, "") or REGIME_PATH_DEFAULT).expanduser(),
        pathlib.Path(
            source.get(claude_worker.candles.CANDLES_DB_ENV, "") or claude_worker.candles.DEFAULT_DB_PATH
        ).expanduser(),
    )


def measured_words(m: Measurement) -> dict[str, int]:
    """Each profile's worker-measured word as judged (SOURCE = measured) —
    the audit word a declaration frame carries in ``qty``."""
    return {name: m.evaluator.measured[p] for p, name in enumerate(PROFILE_NAMES)}


def measured_declaration_words(m: Measurement) -> dict[str, int]:
    """The "AI confirms the measurement" words: each profile's measured
    word minus its SOURCE byte (the engine stamps DECLARED)."""
    return {name: w & ~(0xFF << (8 * DIM_SOURCE)) for name, w in measured_words(m).items()}


def parse_verdict(verdict: typing.Mapping[str, object]) -> dict[str, str]:
    """The strategist's regime verdict ``{"fast": "<decl>|measured",
    "slow": …}`` (profiles optional, at least one): validated
    structurally — declaration grammar or the ``measured`` token.
    ``ValueError`` on anything else (the caller archives the proposal)."""
    if not verdict or any(k not in PROFILE_NAMES for k in verdict):
        raise ValueError(f"regime verdict: profiles must be among {PROFILE_NAMES}: {sorted(verdict)}")
    out: dict[str, str] = {}
    for name in PROFILE_NAMES:
        spec = verdict.get(name)
        if spec is None:
            continue
        if not isinstance(spec, str):
            raise ValueError(f"regime verdict: {name} must be a string")
        if spec != VERDICT_MEASURED:
            parse_declaration(spec)  # raises on a bad term
        out[name] = spec
    return out


def verdict_words(verdict: typing.Mapping[str, str], measured: dict[str, int]) -> dict[str, int]:
    """Resolve a parsed verdict to wire words (``measured`` = the
    confirm-the-measurement word)."""
    return {
        name: measured[name] if spec == VERDICT_MEASURED else declaration_word(parse_declaration(spec))
        for name, spec in verdict.items()
    }


def declare_words(  # noqa: PLR0913, PLR0917 — one argument per input source
    client: typing.Any,
    directory: pathlib.Path,
    words: dict[str, int],
    now_ms: int,
    ttl_s: int,
    source: str,
    measured: dict[str, int] | None = None,
) -> list[int]:
    """Persist ``declared.json`` then send the frames over an already
    CONNECTED + heartbeated client (``None`` = persist only — the
    post-boot repush re-sends while fresh). Transport errors propagate
    after the persist (the caller decides; the file is already true)."""
    persist_declared(directory, words, now_ms, ttl_s, source)
    if client is None:
        return []
    return send_declarations(client, words, ttl_s * 1_000_000_000, measured)


@dataclasses.dataclass(frozen=True)
class ServeRegimeOutcome:
    """What the serve ``_REGIME`` phase did — for the event ledger and the
    digest. ``measurement`` is ``None`` when nothing could be measured."""

    measurement: Measurement | None
    declared: dict[str, int]
    seqs: list[int]
    skipped: str
    digest: str


def serve_regime_step(  # noqa: PLR0913, PLR0917 — one argument per input source
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    directory: pathlib.Path,
    now_ms: int,
    client: typing.Any,
    ttl_s: int,
    pnl_regimes: list[dict[str, object]] | None = None,
    url: str | None = None,
) -> ServeRegimeOutcome:
    """Plan §5.4, the ``_REGIME`` phase before fetch: measure over
    candles.db, append the history line, and AUTO-CONFIRM the measurement
    (declare it with source ``serve-measured``) for every profile whose
    declaration in force is NOT a fresher ruling by someone else (the
    strategist's verdict or the operator's ``declare`` win while fresh).
    Never raises: absent inputs / a failed measurement / a transport
    error are reported in ``skipped`` and the digest says so."""
    if not regime_path.is_file() or not db_path.is_file():
        return ServeRegimeOutcome(None, {}, [], "no regime artifact / candles.db", REGIME_UNMEASURED_TEXT)
    try:
        m = measure(regime_path, db_path, now_ms)
    except (ValueError, sqlite3.Error) as exc:
        return ServeRegimeOutcome(None, {}, [], f"measure failed: {exc}", f"  (regime measurement failed: {exc})")
    append_history(directory, history_entry(m, now_ms))
    entries = load_declared(directory)
    words = {
        name: w
        for name, w in measured_declaration_words(m).items()
        if not (
            name in entries
            and declared_is_fresh(entries[name], now_ms)
            and entries[name].get("source") != SOURCE_SERVE
        )
    }
    seqs: list[int] = []
    skipped = "" if words else "fresher ruling in force on every profile"
    if words:
        try:
            seqs = declare_words(client, directory, words, now_ms, ttl_s, SOURCE_SERVE, measured_words(m))
        except claude_worker.uds.UdsError as exc:
            skipped = f"transport: {exc} (persisted; repush re-sends while fresh)"
    digest = regime_digest_from(m, directory, now_ms, pnl_regimes, url)
    return ServeRegimeOutcome(m, words, seqs, skipped, digest)


def _default_db() -> str:
    return (
        os.environ.get(claude_worker.candles.CANDLES_DB_ENV, "")
        or claude_worker.candles.DEFAULT_DB_PATH
    )


def _add_common(p: argparse.ArgumentParser) -> None:
    p.add_argument("--regime", default=REGIME_PATH_DEFAULT, help="path to regime.toml")
    p.add_argument("--db", default=_default_db(), help="candles.db")
    p.add_argument(
        "--dir",
        default=None,
        help=f"regime state dir (default ${REGIME_DIR_ENV} or {DEFAULT_REGIME_DIR})",
    )
    p.add_argument("--now-ms", type=int, default=None, help="tests only")


def main(argv: list[str] | None = None) -> int:  # noqa: PLR0911, PLR0915 — one dispatcher per lane
    parser = argparse.ArgumentParser(prog="claude_worker.regime")
    sub = parser.add_subparsers(dest="lane", required=True)
    seed = sub.add_parser("seed-out", help="export the engine's warm-up seed from candles.db")
    seed.add_argument("--regime", required=True, help="path to regime.toml")
    seed.add_argument("--out", required=True, help="path to write regime-seed.tsv")
    seed.add_argument(
        "--db",
        default=_default_db(),
        help="candles.db (default: $CLAUDE_WORKER_CANDLES_DB or ~/multivenue/worker/candles.db)",
    )
    seed.add_argument("--minutes", type=int, default=SEED_MINUTES_DEFAULT)
    seed.add_argument(
        "--refresh-tail",
        action="store_true",
        help="RG7: gap-fill the artifact's own 1 m candles to now before exporting",
    )
    seed.add_argument(
        "--universe",
        default=None,
        help="universe.toml for --refresh-tail (default: the candles lane's)",
    )
    seed.add_argument("--now-ms", type=int, default=None, help="tests only")
    rep = sub.add_parser(
        "report",
        help="the worker-measured words + raw values, declaration, engine words, 24 h history",
    )
    _add_common(rep)
    hist = sub.add_parser("history", help="the last 24 h of worker words")
    _add_common(hist)
    hist.add_argument("--hours", type=int, default=HISTORY_HOURS)
    ref = sub.add_parser(
        "refresh-params",
        help="rewrite the RV/funding percentile lines of regime.toml from candles.db",
    )
    _add_common(ref)
    ref.add_argument("--dry-run", action="store_true")
    dec = sub.add_parser(
        "declare", help="declare a regime word to the engine (SetRegime) and persist declared.json"
    )
    _add_common(dec)
    dec.add_argument("--fast", default=None, help='"trend:bull,shape:trend" or "measured"')
    dec.add_argument("--slow", default=None, help='"trend:neutral" or "measured"')
    dec.add_argument(
        "--ttl",
        type=int,
        default=None,
        help=f"seconds (default ${DECLARE_TTL_S_ENV} or {DECLARE_TTL_S_DEFAULT})",
    )
    dec.add_argument(
        "--source", default=SOURCE_OPERATOR, choices=(SOURCE_OPERATOR, SOURCE_STRATEGIST)
    )
    dec.add_argument("--no-send", action="store_true", help="persist only (no frame)")
    cyc = sub.add_parser(
        "cycle", help="the 5-minute cycle: measure + history (+ daily percentile refresh)"
    )
    _add_common(cyc)
    cyc.add_argument("--no-refresh", action="store_true")
    rep2 = sub.add_parser(
        "repush", help="post-boot: re-send the persisted declaration with its remaining TTL"
    )
    _add_common(rep2)
    soak = sub.add_parser("soak", help="RG7: judge the ≤ 2 h windows (plan §7.1)")
    _add_common(soak)
    soak.add_argument(
        "--pool",
        default=None,
        help="judge the standing pool's cuts instead of every complete window of the runs",
    )
    soak.add_argument(
        "--replay-dir", default=None, help="runs root (default $CLAUDE_WORKER_REPLAY_DIR)"
    )
    soak.add_argument(
        "--reports-dir",
        default=None,
        help="nightly reports dir (default $CLAUDE_WORKER_REPORTS_DIR)",
    )
    soak.add_argument("--hours", type=int, default=HISTORY_HOURS)
    soak.add_argument("--min-windows", type=int, default=SOAK_MIN_WINDOWS)
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
        seed_now = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
        if args.refresh_tail:
            universe = pathlib.Path(
                args.universe
                or os.environ.get(claude_worker.fetchers.UNIVERSE_FILE_ENV, "")
                or claude_worker.candles.DEFAULT_UNIVERSE_PATH
            ).expanduser()
            touched = refresh_tail(
                regime_path,
                db_path,
                universe,
                seed_now,
                os.environ,
                lambda line: print(line, file=sys.stderr),
            )
            print(f"regime seed-out: tail refreshed for {touched} descriptors", file=sys.stderr)
        n_desc, n_rows = seed_out(regime_path, db_path, out_path, args.minutes, seed_now)
        print(f"regime seed-out: {n_rows} rows for {n_desc} descriptors -> {out_path}")
        return 0

    regime_path = pathlib.Path(args.regime).expanduser()
    db_path = pathlib.Path(args.db).expanduser()
    directory = pathlib.Path(args.dir).expanduser() if args.dir else regime_dir()
    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)

    if args.lane == "history":
        for e in history_tail(directory, now_ms, args.hours):
            stamp = datetime.datetime.fromtimestamp(
                int(e.get("ts_ms", 0)) / 1000, tz=datetime.timezone.utc
            )
            print(
                f"{stamp.strftime('%Y-%m-%dT%H:%M:%SZ')}"
                f" fast={describe(int(str(e.get('fast', '0')), 16))}"
                f" | slow={describe(int(str(e.get('slow', '0')), 16))}"
            )
        return 0
    if args.lane == "repush":
        return _repush_main(directory, now_ms)
    if args.lane == "soak":
        if args.pool:
            windows = soak_windows_from_pool(pathlib.Path(args.pool).expanduser())
            source = "pool"
        else:
            replay = pathlib.Path(
                args.replay_dir
                or os.environ.get("CLAUDE_WORKER_REPLAY_DIR", "")
                or "~/multivenue/logs"
            ).expanduser()
            windows = soak_windows_from_runs(replay, now_ms - args.hours * 3_600_000)
            source = "runs"
        reports_dir = (
            pathlib.Path(args.reports_dir).expanduser()
            if args.reports_dir
            else pathlib.Path(
                os.environ.get("CLAUDE_WORKER_REPORTS_DIR", "") or "~/multivenue/worker/reports"
            ).expanduser()
        )
        result = run_soak(
            directory,
            windows,
            reports_dir,
            now_ms,
            hours=args.hours,
            min_windows=args.min_windows,
            source=source,
        )
        return 0 if result["verdict"] == "PASS" else 3
    if not regime_path.is_file():
        print(f"regime {args.lane}: {regime_path} absent", file=sys.stderr)
        return 2
    if not db_path.is_file():
        print(f"regime {args.lane}: {db_path} absent", file=sys.stderr)
        return 2
    if args.lane == "report":
        m = measure(regime_path, db_path, now_ms)
        sys.stdout.write(
            render_report(
                m,
                load_declared(directory),
                engine_words(metrics_url()),
                history_tail(directory, now_ms),
                now_ms,
            )
        )
        return 0
    if args.lane == "refresh-params":
        values = (
            compute_percentiles(regime_path, db_path, now_ms)
            if args.dry_run
            else refresh_params(regime_path, db_path, now_ms)
        )
        print(
            f"regime refresh-params{' (dry-run)' if args.dry_run else ''}:"
            f" {json.dumps(values, sort_keys=True)}"
        )
        return 0
    if args.lane == "cycle":
        return (
            0
            if run_cycle(regime_path, db_path, directory, now_ms, refresh_daily=not args.no_refresh)
            is not None
            else 2
        )
    if args.lane == "declare":
        return _declare_main(args, regime_path, db_path, directory, now_ms)
    return 2


def _declare_main(  # noqa: PLR0911 — one exit code per refusal
    args: argparse.Namespace,
    regime_path: pathlib.Path,
    db_path: pathlib.Path,
    directory: pathlib.Path,
    now_ms: int,
) -> int:
    if args.fast is None and args.slow is None:
        print("regime declare: give --fast and/or --slow", file=sys.stderr)
        return 2
    ttl_s = args.ttl if args.ttl is not None else declare_ttl_s()
    if ttl_s < 1:
        print("regime declare: --ttl must be >= 1", file=sys.stderr)
        return 2
    m = measure(regime_path, db_path, now_ms)
    try:
        verdict = parse_verdict({k: v for k, v in (("fast", args.fast), ("slow", args.slow)) if v is not None})
        words = verdict_words(verdict, measured_declaration_words(m))
    except ValueError as exc:
        print(f"regime declare: {exc}", file=sys.stderr)
        return 2
    # Persist FIRST: a transport failure below still leaves the file true
    # (the post-boot repush re-sends it while fresh).
    declare_words(None, directory, words, now_ms, ttl_s, args.source)
    for name, w in words.items():
        print(f"regime declare: {name}={describe(w)} ttl={ttl_s}s source={args.source}")
    if args.no_send:
        print(f"regime declare: persisted to {declared_path(directory)} (no frame sent)")
        return 0
    try:
        cfg = claude_worker.config.load_base_from_env()
    except ValueError as exc:
        print(f"regime declare: config: {exc}", file=sys.stderr)
        return 2
    state = claude_worker.state.State(cfg.db_path)
    try:
        client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
        try:
            client.connect()
            client.send_heartbeat()
            seqs = send_declarations(client, words, ttl_s * 1_000_000_000, measured_words(m))
        finally:
            client.close()
    except claude_worker.uds.UdsError as exc:
        print(
            f"regime declare: transport: {exc}"
            " (persisted; the post-boot repush will re-send while fresh)",
            file=sys.stderr,
        )
        return 4
    finally:
        state.close()
    print(f"regime declare: sent {len(seqs)} SetRegime frame(s) seqs={seqs}")
    return 0


def _repush_main(directory: pathlib.Path, now_ms: int) -> int:
    try:
        cfg = claude_worker.config.load_base_from_env()
    except ValueError as exc:
        print(f"regime repush: config: {exc}", file=sys.stderr)
        return 2
    state = claude_worker.state.State(cfg.db_path)
    try:
        client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
        try:
            client.connect()
            client.send_heartbeat()
            repush_declared(client, directory, now_ms)
        finally:
            client.close()
    except claude_worker.uds.UdsError as exc:
        print(f"regime repush: transport: {exc}", file=sys.stderr)
        return 4
    finally:
        state.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
