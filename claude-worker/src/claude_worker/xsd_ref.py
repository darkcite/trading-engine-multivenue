# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""xsd_ref -- the Python reference of the xsd member's integer law.

This is a MIRROR of ``crates/strategy-xsd`` (statarb doc 08 §3.4), not of
the float research code: the same Q60 fixed-point log, the same floor-divided
spread and z-score, the same hourly roll, the same decision table and the same
paper emission law, so a decision the engine takes is a decision this module
takes -- bit for bit.  It is pinned two ways:

* ``tests/test_xsd_ref.py`` runs the shared fixture
  ``tests/fixtures/xsd/parity-1.input.tsv`` and asserts the engine-written
  ``parity-1.expected.tsv`` line for line (the ``core-regime`` parity pattern;
  the expected file is regenerated ONLY by
  ``XSD_PARITY_WRITE=1 cargo nextest run -p strategy-xsd --test parity``);
* the research vault's one-shot replays it against the research trade log on
  ``matrix_1h.npz`` (entries / exits / sides equal).

Pure integers throughout: every ``//`` here is on a positive divisor, which is
Python floor division and the Rust ``floor_div`` (``div_euclid``) alike.
"""

import math
import typing

SCALE_1E9: int = 10**9
HOUR_NS: int = 3_600_000_000_000
XSD_K: int = 3
XSD_RING_H: int = 2160
XSD_MAX_GRID: int = 8
LN_EMPTY: int = 0
ORDER_KIND_IOC: int = 1

CAP_LEG_1E6: int = 10_000_000_000
CAP_SYM_1E6: int = 20_000_000_000
CAP_TABLE_1E6: int = 100_000_000_000

ST_FLAT: int = 0
ST_PENDING_ENTER: int = 1
ST_ENTERED: int = 2
INTENT_NONE: int = 0
INTENT_ENTER: int = 1
INTENT_ADD: int = 2
INTENT_EXIT: int = 3
EXIT_REVERT: int = 1
EXIT_STOP: int = 2
EXIT_MAXHOLD: int = 3
EXIT_ROTATION: int = 4
EXIT_REGIME: int = 5
SIDE_LONG: int = 1
SIDE_SHORT: int = 2
SIDE_BID: int = 0
SIDE_ASK: int = 1

NO_HOUR: typing.Optional[int] = None

_LN2_Q60: int = 799144290325165979
_ONE_Q60: int = 1 << 60
_SERIES_LAST: int = 21

COUNTER_NAMES: tuple[str, ...] = (
    "rolls", "pairs_warm", "decisions", "entries_decided", "adds_decided", "entries",
    "adds", "exits_revert", "exits_stop", "exits_maxhold", "exits_rotation",
    "exits_regime", "intents_carried", "entries_cancelled", "caps_rejected",
    "holds_absent", "regime_blocked", "seed_rows", "seed_dropped",
)


# ---------------------------------------------------------------------------
# math (crates/strategy-xsd/src/math.rs)
# ---------------------------------------------------------------------------

def ln1e9(v: int) -> int:
    """``round(ln v * 1e9)`` within one unit, by the Q60 atanh series."""
    if v <= 0:
        raise ValueError("ln1e9 of a non-positive value")
    k: int = v.bit_length() - 1
    m_q: int = (v << (60 - k)) if k <= 60 else (v >> (k - 60))
    u: int = ((m_q - _ONE_Q60) << 60) // (m_q + _ONE_Q60)
    u2: int = (u * u) >> 60
    term: int = u
    acc: int = u
    i: int = 3
    while i <= _SERIES_LAST:
        term = (term * u2) >> 60
        acc += term // i
        i += 2
    ln_v: int = (acc << 1) + k * _LN2_Q60
    return (ln_v * SCALE_1E9 + (1 << 59)) >> 60


def spread_1e9(ln_a: int, ln_b: int, beta_1e9: int) -> int:
    return ln_a - (beta_1e9 * ln_b) // SCALE_1E9


def min_count(window_h: int) -> int:
    return max(30, window_h // 4)


def isqrt_i128(v: int) -> int:
    if v <= 0:
        return 0
    r: int = math.isqrt(v)
    return min(r, (1 << 63) - 1)


def z_1e9(total: int, total2: int, n: int, newest_present: bool, s0: int,
          window_h: int) -> typing.Optional[int]:
    if not newest_present or n < min_count(window_h):
        return None
    mean: int = total // n
    var: int = total2 // n - mean * mean
    std: int = isqrt_i128(var)
    if std <= 0:
        return None
    return ((s0 - mean) * SCALE_1E9) // std


def shift_px_1e6(px_1e6: int, bps_1e9: int, up: bool) -> int:
    d: int = (px_1e6 * bps_1e9) // 10_000_000_000_000
    return px_1e6 + d if up else max(px_1e6 - d, 1)


def qty_for_notional_1e6(notional_1e6: int, px_1e6: int) -> int:
    return (notional_1e6 * 1_000_000) // px_1e6


# ---------------------------------------------------------------------------
# artifacts
# ---------------------------------------------------------------------------

class Params(typing.NamedTuple):
    z_window_h: int
    z_enter_1e9: int
    z_exit_1e9: int
    z_stop_1e9: int
    consensus: int
    grid_n: int
    grid_step_1e9: int
    max_hold_h: int
    cooldown_h: int
    ttl_ns: int
    position_usd_1e6: int
    max_positions: int
    max_gross_usd_1e6: int
    direction: int
    slip_1e9: int


class Order(typing.NamedTuple):
    ts_ns: int
    sym: int
    side: int
    px_1e6: int
    qty_1e6: int
    ttl_ns: int
    kind: int


class _Pair:
    __slots__ = ("partner", "beta_1e9", "z_1e9", "z_valid", "n")

    def __init__(self, partner: int, beta_1e9: int) -> None:
        self.partner: int = partner
        self.beta_1e9: int = beta_1e9
        self.z_1e9: int = 0
        self.z_valid: bool = False
        self.n: int = 0


class _Target:
    __slots__ = ("sym", "pairs", "state", "side", "d", "intent", "exit_reason",
                 "grid_units", "nfin", "zbar_1e9", "zmag_1e9", "pos_qty_1e6",
                 "pos_notional_1e6", "entry_hour", "last_exit_hour", "last_add_hour")

    def __init__(self, sym: int) -> None:
        self.sym: int = sym
        self.pairs: list[_Pair] = []
        self.state: int = ST_FLAT
        self.side: int = 0
        self.d: int = 0
        self.intent: int = INTENT_NONE
        self.exit_reason: int = 0
        self.grid_units: int = 0
        self.nfin: int = 0
        self.zbar_1e9: int = 0
        self.zmag_1e9: int = 0
        self.pos_qty_1e6: int = 0
        self.pos_notional_1e6: int = 0
        self.entry_hour: typing.Optional[int] = NO_HOUR
        self.last_exit_hour: typing.Optional[int] = NO_HOUR
        self.last_add_hour: typing.Optional[int] = NO_HOUR


class _Sym:
    __slots__ = ("sym", "ring", "newest_h", "last_mid", "last_bid", "last_ask", "last_ts",
                 "target")

    def __init__(self, sym: int) -> None:
        self.sym: int = sym
        self.ring: dict[int, int] = {}
        self.newest_h: typing.Optional[int] = NO_HOUR
        self.last_mid: int = 0
        self.last_bid: int = 0
        self.last_ask: int = 0
        self.last_ts: int = 0
        self.target: typing.Optional[int] = None


def validate_params(p: Params) -> None:
    if p.z_window_h < 40 or p.z_window_h > XSD_RING_H:
        raise ValueError("xsd: z_window_h outside [40, ring]")
    if p.z_enter_1e9 <= 0 or p.z_exit_1e9 < 0 or p.z_stop_1e9 <= p.z_enter_1e9:
        raise ValueError("xsd: z thresholds malformed")
    if p.consensus < 1 or p.consensus > XSD_K:
        raise ValueError("xsd: consensus outside 1..=K")
    if p.grid_n < 1 or p.grid_n > XSD_MAX_GRID or (p.grid_n > 1 and p.grid_step_1e9 <= 0):
        raise ValueError("xsd: grid malformed")
    if p.max_hold_h <= 0 or p.ttl_ns <= 0:
        raise ValueError("xsd: max_hold / ttl must be positive")
    if p.position_usd_1e6 <= 0 or p.position_usd_1e6 > CAP_LEG_1E6:
        raise ValueError("xsd: position_usd outside (0, per-order cap]")
    if p.max_positions <= 0 or p.max_gross_usd_1e6 <= 0 or p.max_gross_usd_1e6 > CAP_TABLE_1E6:
        raise ValueError("xsd: max_positions / max_gross malformed")
    if p.direction not in (1, -1):
        raise ValueError("xsd: direction must be +1 or -1")
    if p.slip_1e9 < 0:
        raise ValueError("xsd: slip must be >= 0")


# ---------------------------------------------------------------------------
# the member
# ---------------------------------------------------------------------------

class XsdRef:
    """The mirror.  Wall time == monotonic time (the fixture's anchor law):
    hour ``h`` opens at ``h * HOUR_NS``."""

    def __init__(self, params: Params, rows: list[tuple[int, int, int]]) -> None:
        validate_params(params)
        if not rows:
            raise ValueError("xsd: empty table")
        self.p: Params = params
        self.syms: dict[int, _Sym] = {}
        self.targets: list[_Target] = []
        self.counters: dict[str, int] = {name: 0 for name in COUNTER_NAMES}
        self.open_notional_total_1e6: int = 0
        self.n_positions: int = 0
        self.state_epoch: int = 0
        self.regime_open: bool = True
        self.cur_hour: typing.Optional[int] = NO_HOUR
        self.orders_emitted: int = 0
        by_target: dict[int, _Target] = {}
        seen: set[tuple[int, int]] = set()
        for target, partner, beta in rows:
            if target == partner:
                raise ValueError("xsd: a target cannot partner itself")
            if beta == 0:
                raise ValueError("xsd: zero hedge ratio")
            if (target, partner) in seen:
                raise ValueError("xsd: duplicate pair row")
            seen.add((target, partner))
            tg: typing.Optional[_Target] = by_target.get(target)
            if tg is None:
                tg = _Target(target)
                by_target[target] = tg
                self.targets.append(tg)
            if len(tg.pairs) == XSD_K:
                raise ValueError("xsd: more than K partners for a target")
            tg.pairs.append(_Pair(partner, beta))
            for s in (target, partner):
                if s not in self.syms:
                    self.syms[s] = _Sym(s)
        for i, tg in enumerate(self.targets):
            self.syms[tg.sym].target = i

    # ---- boot ------------------------------------------------------------

    def seed_close(self, sym: int, hour: int, close_1e6: int) -> bool:
        s: typing.Optional[_Sym] = self.syms.get(sym)
        if s is None or close_1e6 <= 1:
            self.counters["seed_dropped"] += 1
            return False
        if s.newest_h is None or hour > s.newest_h:
            s.newest_h = hour
        elif s.newest_h - hour >= XSD_RING_H:
            self.counters["seed_dropped"] += 1
            return False
        if s.ring.get(hour, LN_EMPTY) != LN_EMPTY:
            self.counters["seed_dropped"] += 1
            return False
        s.ring[hour] = ln1e9(close_1e6)
        self.counters["seed_rows"] += 1
        return True

    # ---- clock -----------------------------------------------------------

    def _set_hour(self, hour: int) -> None:
        self.cur_hour = hour

    def _maybe_roll(self, now_ns: int) -> None:
        h: int = now_ns // HOUR_NS
        if self.cur_hour is None:
            self._set_hour(h)
        elif h > self.cur_hour:
            self._roll(h)

    def on_timer(self, now_ns: int) -> None:
        self._maybe_roll(now_ns)

    def on_tick(self, sym: int, ts_ns: int, bid_1e6: int, ask_1e6: int,
                stale: bool = False) -> typing.Optional[Order]:
        s: typing.Optional[_Sym] = self.syms.get(sym)
        if s is None:
            return None
        self._maybe_roll(ts_ns)
        if stale or bid_1e6 <= 0 or ask_1e6 <= 0:
            return None
        s.last_bid = bid_1e6
        s.last_ask = ask_1e6
        s.last_mid = (bid_1e6 + ask_1e6) >> 1
        s.last_ts = ts_ns
        if s.target is not None and self.targets[s.target].intent != INTENT_NONE:
            return self._act(s, ts_ns)
        return None

    # ---- the roll --------------------------------------------------------

    def _roll(self, new_hour: int) -> None:
        closed: int = new_hour - 1
        open_closed: int = closed * HOUR_NS
        open_new: int = new_hour * HOUR_NS
        for s in self.syms.values():
            if s.newest_h is None or closed > s.newest_h:
                s.newest_h = closed
            if s.last_mid > 1 and open_closed <= s.last_ts < open_new:
                s.ring[closed] = ln1e9(s.last_mid)
        self._set_hour(new_hour)
        self.counters["rolls"] += 1
        window: int = self.p.z_window_h
        warm: int = 0
        for tg in self.targets:
            ring_t: dict[int, int] = self.syms[tg.sym].ring
            for pr in tg.pairs:
                ring_p: dict[int, int] = self.syms[pr.partner].ring
                total: int = 0
                total2: int = 0
                n: int = 0
                newest_present: bool = False
                s0: int = 0
                j: int = 0
                while j < window:
                    h: int = closed - j
                    la: int = ring_t.get(h, LN_EMPTY)
                    lb: int = ring_p.get(h, LN_EMPTY)
                    if la != LN_EMPTY and lb != LN_EMPTY:
                        sp: int = spread_1e9(la, lb, pr.beta_1e9)
                        total += sp
                        total2 += sp * sp
                        n += 1
                        if j == 0:
                            newest_present = True
                            s0 = sp
                    j += 1
                pr.n = n
                z: typing.Optional[int] = z_1e9(total, total2, n, newest_present, s0, window)
                if z is None:
                    pr.z_valid = False
                    pr.z_1e9 = 0
                else:
                    pr.z_valid = True
                    pr.z_1e9 = z
                    warm += 1
        self.counters["pairs_warm"] = warm
        for tg in self.targets:
            self._decide(tg, new_hour)

    def _decide(self, tg: _Target, hour: int) -> None:
        p: Params = self.p
        nfin: int = 0
        total: int = 0
        total_abs: int = 0
        pos_hit: int = 0
        neg_hit: int = 0
        for pr in tg.pairs:
            if pr.z_valid:
                nfin += 1
                total += pr.z_1e9
                total_abs += abs(pr.z_1e9)
                if pr.z_1e9 >= p.z_enter_1e9:
                    pos_hit += 1
                if pr.z_1e9 <= -p.z_enter_1e9:
                    neg_hit += 1
        tg.nfin = nfin
        if nfin > 0:
            tg.zbar_1e9 = total // nfin
            tg.zmag_1e9 = total_abs // nfin
        else:
            tg.zbar_1e9 = 0
            tg.zmag_1e9 = 0
        self.counters["decisions"] += 1
        if tg.intent != INTENT_NONE:
            self.counters["intents_carried"] += 1
        if tg.state == ST_FLAT:
            if nfin == 0:
                self.counters["holds_absent"] += 1
                return
            cooled: bool = (tg.last_exit_hour is None
                            or hour >= tg.last_exit_hour + 1 + p.cooldown_h)
            if not cooled:
                return
            if pos_hit >= p.consensus:
                d: int = 1
            elif neg_hit >= p.consensus:
                d = -1
            else:
                return
            if not self.regime_open:
                self.counters["regime_blocked"] += 1
                return
            long: bool = d * p.direction > 0
            tg.state = ST_PENDING_ENTER
            tg.intent = INTENT_ENTER
            tg.d = d
            tg.side = SIDE_LONG if long else SIDE_SHORT
            tg.entry_hour = hour
            tg.last_add_hour = hour
            tg.grid_units = 1
            self.counters["entries_decided"] += 1
            return
        assert tg.entry_hour is not None
        held: int = hour - tg.entry_hour
        if held >= p.max_hold_h:
            reason: int = EXIT_MAXHOLD
        elif nfin > 0 and tg.zmag_1e9 >= p.z_stop_1e9:
            reason = EXIT_STOP
        elif nfin > 0 and tg.zbar_1e9 * tg.d <= p.z_exit_1e9:
            reason = EXIT_REVERT
        else:
            reason = 0
        if reason != 0:
            if tg.intent == INTENT_EXIT:
                return
            if tg.state == ST_PENDING_ENTER:
                tg.state = ST_FLAT
                tg.intent = INTENT_NONE
                tg.entry_hour = NO_HOUR
                tg.grid_units = 0
                tg.last_exit_hour = hour
                self.counters["entries_cancelled"] += 1
                return
            tg.intent = INTENT_EXIT
            tg.exit_reason = reason
            return
        if nfin == 0:
            self.counters["holds_absent"] += 1
            return
        if (tg.state == ST_ENTERED and tg.intent == INTENT_NONE
                and tg.grid_units < p.grid_n and tg.last_add_hour is not None
                and hour > tg.last_add_hour):
            rung: int = p.z_enter_1e9 + tg.grid_units * p.grid_step_1e9
            if tg.zmag_1e9 >= rung:
                tg.intent = INTENT_ADD
                tg.grid_units += 1
                tg.last_add_hour = hour
                self.counters["adds_decided"] += 1

    # ---- emission --------------------------------------------------------

    def _emit(self, s: _Sym, side: int, px: int, qty: int, now: int) -> Order:
        self.orders_emitted += 1
        return Order(now, s.sym, side, px, qty, self.p.ttl_ns, ORDER_KIND_IOC)

    def _act(self, s: _Sym, now: int) -> typing.Optional[Order]:
        assert s.target is not None
        tg: _Target = self.targets[s.target]
        intent: int = tg.intent
        p: Params = self.p
        if intent in (INTENT_ENTER, INTENT_ADD):
            long: bool = tg.side == SIDE_LONG
            if long:
                side: int = SIDE_BID
                px: int = shift_px_1e6(s.last_ask, p.slip_1e9, True)
            else:
                side = SIDE_ASK
                px = shift_px_1e6(s.last_bid, p.slip_1e9, False)
            qty: int = qty_for_notional_1e6(p.position_usd_1e6, px)
            book_cap: int = min(p.max_gross_usd_1e6, CAP_TABLE_1E6)
            cap_ok: bool = (qty > 0
                            and tg.pos_notional_1e6 + p.position_usd_1e6 <= CAP_SYM_1E6
                            and self.open_notional_total_1e6 + p.position_usd_1e6 <= book_cap
                            and (intent == INTENT_ADD or self.n_positions < p.max_positions))
            if not cap_ok:
                self.counters["caps_rejected"] += 1
                if intent == INTENT_ENTER:
                    tg.state = ST_FLAT
                    tg.entry_hour = NO_HOUR
                    tg.grid_units = 0
                else:
                    tg.grid_units -= 1
                tg.intent = INTENT_NONE
                return None
            order: Order = self._emit(s, side, px, qty, now)
            tg.intent = INTENT_NONE
            tg.pos_qty_1e6 += qty
            tg.pos_notional_1e6 += p.position_usd_1e6
            self.open_notional_total_1e6 += p.position_usd_1e6
            if intent == INTENT_ENTER:
                tg.state = ST_ENTERED
                self.n_positions += 1
                self.counters["entries"] += 1
            else:
                self.counters["adds"] += 1
            self.state_epoch += 1
            return order
        if intent == INTENT_EXIT:
            if tg.side == SIDE_LONG:
                side = SIDE_ASK
                px = shift_px_1e6(s.last_bid, p.slip_1e9, False)
            else:
                side = SIDE_BID
                px = shift_px_1e6(s.last_ask, p.slip_1e9, True)
            order = self._emit(s, side, px, tg.pos_qty_1e6, now) if tg.pos_qty_1e6 > 0 else None
            self.open_notional_total_1e6 -= tg.pos_notional_1e6
            self.n_positions -= 1
            name: str = {EXIT_REVERT: "exits_revert", EXIT_STOP: "exits_stop",
                         EXIT_MAXHOLD: "exits_maxhold",
                         EXIT_ROTATION: "exits_rotation"}.get(tg.exit_reason, "exits_regime")
            self.counters[name] += 1
            tg.state = ST_FLAT
            tg.intent = INTENT_NONE
            tg.side = 0
            tg.grid_units = 0
            tg.pos_qty_1e6 = 0
            tg.pos_notional_1e6 = 0
            tg.entry_hour = NO_HOUR
            tg.last_exit_hour = self.cur_hour   # the research's `t = x + 1`: from the FILL
            self.state_epoch += 1
            return order
        return None

    def hard_close(self) -> None:
        """The regime hard-close (and the research's fold end): every entered
        target is armed to EXIT at its next fresh tick, every unfilled entry
        is cancelled."""
        for tg in self.targets:
            if tg.state == ST_ENTERED:
                if tg.intent != INTENT_EXIT:
                    tg.intent = INTENT_EXIT
                    tg.exit_reason = EXIT_REGIME
            elif tg.state == ST_PENDING_ENTER:
                tg.state = ST_FLAT
                tg.intent = INTENT_NONE
                tg.entry_hour = NO_HOUR
                tg.grid_units = 0
                tg.last_exit_hour = self.cur_hour
                self.counters["entries_cancelled"] += 1

    # ---- views (the parity rows) ----------------------------------------

    def target_row(self, t: int, hour: int) -> str:
        tg: _Target = self.targets[t]
        parts: list[str] = [
            "R", str(hour), str(t), str(tg.state), str(tg.side), str(tg.d), str(tg.intent),
            str(tg.exit_reason), str(tg.grid_units), str(tg.nfin), str(tg.zbar_1e9),
            str(tg.zmag_1e9), str(tg.pos_qty_1e6), str(tg.pos_notional_1e6),
        ]
        for pr in tg.pairs:
            parts.append(str(pr.z_1e9) if pr.z_valid else "-")
            parts.append(str(pr.n))
        return " ".join(parts)

    def summary_row(self) -> str:
        return "E " + " ".join(str(self.counters[name]) for name in COUNTER_NAMES) \
            + " " + str(self.orders_emitted)


# ---------------------------------------------------------------------------
# the shared fixture
# ---------------------------------------------------------------------------

class Fixture(typing.NamedTuple):
    params: Params
    wall0_ns: int
    syms: list[int]
    rows: list[tuple[int, int, int]]
    seeds: list[tuple[int, int, int]]
    live: dict[int, list[tuple[int, int, int]]]


def parse_fixture(text: str) -> Fixture:
    """``parity-<n>.input.tsv`` -- ``P`` params, ``A`` wall anchor, ``S`` syms,
    ``T`` table rows, ``C`` seed closes, ``K`` live hours (open + close)."""
    params: typing.Optional[Params] = None
    wall0: int = 0
    syms: list[int] = []
    rows: list[tuple[int, int, int]] = []
    seeds: list[tuple[int, int, int]] = []
    live: dict[int, list[tuple[int, int, int]]] = {}
    for raw in text.splitlines():
        line: str = raw.strip()
        if not line or line.startswith("#"):
            continue
        f: list[str] = line.split()
        tag: str = f[0]
        if tag == "P":
            kv: dict[str, int] = {}
            for part in f[1:]:
                k, v = part.split("=")
                kv[k] = int(v)
            params = Params(**{name: kv[name] for name in Params._fields})
        elif tag == "A":
            wall0 = int(f[1])
        elif tag == "S":
            syms.append(int(f[1]))
        elif tag == "T":
            rows.append((int(f[1]), int(f[2]), int(f[3])))
        elif tag == "C":
            seeds.append((int(f[1]), int(f[2]), int(f[3])))
        elif tag == "K":
            live.setdefault(int(f[1]), []).append((int(f[2]), int(f[3]), int(f[4])))
        else:
            raise ValueError("fixture: unknown tag " + tag)
    if params is None:
        raise ValueError("fixture: missing P row")
    return Fixture(params, wall0, syms, rows, seeds, sorted_live(live))


def sorted_live(live: dict[int, list[tuple[int, int, int]]]) -> dict[int, list[tuple[int, int, int]]]:
    return {h: live[h] for h in sorted(live)}


def run_fixture(fx: Fixture) -> list[str]:
    """Replay the fixture through the mirror and render the parity lines."""
    ref: XsdRef = XsdRef(fx.params, fx.rows)
    for hour, sym, close in fx.seeds:
        ref.seed_close(sym, hour, close)
    out: list[str] = []
    first: bool = True
    for hour, ticks in fx.live.items():
        open_ns: int = hour * HOUR_NS
        ref.on_timer(open_ns + 1)
        if not first:
            for t in range(len(ref.targets)):
                out.append(ref.target_row(t, hour))
        first = False
        for sym, opn, close in ticks:
            for ts, px in ((open_ns + 1_000_000_000, opn), (open_ns + 3_599_000_000_000, close)):
                order: typing.Optional[Order] = ref.on_tick(sym, ts, px, px)
                if order is not None:
                    out.append("O %d %d %d %d %d %d %d" % (
                        hour, order.sym, order.side, order.px_1e6, order.qty_1e6,
                        order.ttl_ns, order.kind))
    out.append(ref.summary_row())
    return out
