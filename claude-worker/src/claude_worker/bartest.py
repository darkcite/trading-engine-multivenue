# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Bar-level backtest runner for multi-hour holding-period strategies.

**Why this exists.** ``crates/cli`` ``backtest`` and ``audit-pnl`` consume
only PMLR tick capture from ``run-<epoch_ns>`` directories; ``crates/cli``
has no SQLite dependency at all. A strategy that opens a position, holds
it 1-24 hours and closes it has no runner in this repo. This is that
runner, and it is deliberately the SMALLEST bridge that keeps the result
inside our existing automation:

* it emits the **frozen audit-pnl contract** (``audit_pnl_version`` 1,
  the same ``strategies`` row shape), so ``pnl_report.merge_reports``
  folds it unchanged and the P&L review lane is the existing one;
* fees come from ``~/multivenue/fees.toml`` -- the same D2-AMEND tiers
  and the same one-slot-per-``VenueId`` law -- never from a constant
  here;
* the fill law is conservative and stated, not inferred.

**The fill law, in full, because a backtest is only as honest as this.**

* A rebalance decided at bar ``t`` may use data up to and including bar
  ``t``'s CLOSE. It executes at bar ``t+1``'s OPEN. There is no path by
  which a decision sees its own execution price.
* A position opened at ``t+1``'s open is closed at the open of bar
  ``t+1+hold``. Rebalance stride equals the hold, so positions never
  overlap and every trade is an independent observation -- the effective
  n is the trade count, and it is reported.
* Both legs pay the venue's TAKER fee plus an explicit slippage
  assumption, per side. **No historical L2 exists at any venue** (probe,
  2026-09-08: ``depth_digest`` starts 2026-08-29, 1 h only), so slippage
  is ASSUMED and every result is reported at three assumptions. It is
  never claimed as measured.
* A maker variant is available and is labelled an UPPER BOUND: it assumes
  a passive fill that this data cannot confirm ever happened.

Offline path: allocation is permitted here (standing doctrine). Nothing
in this module runs in, or is reachable from, the engine.

Convention: full ``import x`` only.
"""

import collections
import dataclasses
import datetime
import math
import pathlib
import random
import sqlite3
import typing

MS_1M: int = 60_000
MS_1H: int = 3_600_000

#: The audit-pnl stdout contract this module emits (pnl_report accepts 1).
AUDIT_PNL_VERSION: int = 1

DEFAULT_DB: str = "~/multivenue/worker/candles.db"
DEFAULT_FEES: str = "~/multivenue/fees.toml"

#: descriptor prefix -> the `VenueId` fee slot in fees.toml. One slot per
#: venue is the D2-AMEND L1 law: a venue whose strategies take both spot
#: and perp legs carries the DEARER number.
VENUE_OF_PREFIX: dict[str, str] = {
    "binance": "bn",
    "binance-usdm": "bn",
    "bybit": "bybit",
    "bybit-linear": "bybit",
    "deribit": "deribit",
    "hyperliquid": "hl",
    "okx": "okx",
    "polymarket": "pm",
}

#: Reported side by side; a single number would be a claim we cannot make.
SLIPPAGE_LADDER_BPS: tuple[float, ...] = (0.0, 1.0, 3.0)

#: Execution modes.
#:  taker       -- both legs cross. The only mode this data can VERIFY.
#:  maker_entry -- rest on entry, cross on exit. The realistic hybrid: an
#:                 entry can wait for a fill, a timed exit usually cannot.
#:  maker       -- both legs rest. An UPPER BOUND: it assumes a passive
#:                 fill that no historical L2 in this repo can confirm
#:                 ever happened (depth_digest starts 2026-08-29, 1 h
#:                 only). Never report it as a result on its own.
EXECUTION_MODES: tuple[str, ...] = ("taker", "maker_entry", "maker")


class BartestError(Exception):
    """Fail-fast: a result we cannot trust must never reach a report."""


# ------------------------------------------------------------------ fees


@dataclasses.dataclass(frozen=True)
class FeeTier:
    """One venue's maker/taker bps, read from fees.toml. Integer bps: the
    file's own parser is integer-only and rounds UP (D2-AMEND L3)."""

    maker_bps: int
    taker_bps: int


def read_fee_tiers(path: str | pathlib.Path | None = None) -> dict[str, FeeTier]:
    """Parse the ``[fees]`` block of fees.toml into per-venue tiers."""
    p = pathlib.Path(path or DEFAULT_FEES).expanduser()
    if not p.is_file():
        raise BartestError(f"fees.toml not found at {p}")
    out: dict[str, FeeTier] = {}
    for line in p.read_text(encoding="utf-8").splitlines():
        stripped = line.split("#", 1)[0].strip()
        if not stripped or stripped.startswith("[") or "=" not in stripped:
            continue
        key, _sep, raw = stripped.partition("=")
        value = raw.strip().strip('"')
        if ":" not in value:
            continue
        maker, _s, taker = value.partition(":")
        try:
            out[key.strip()] = FeeTier(int(maker), int(taker))
        except ValueError as exc:
            raise BartestError(f"fees.toml: bad tier {value!r}") from exc
    if not out:
        raise BartestError(f"fees.toml at {p} carried no [fees] entries")
    return out


def venue_of(descriptor: str) -> str:
    """The fee slot a descriptor trades in."""
    prefix = descriptor.split(":", 1)[0]
    venue = VENUE_OF_PREFIX.get(prefix)
    if venue is None:
        raise BartestError(f"no fee slot known for descriptor {descriptor!r}")
    return venue


# ------------------------------------------------------------------ bars


@dataclasses.dataclass(frozen=True)
class Bars:
    """One instrument's OHLC series, indexed by bar-open timestamp."""

    descriptor: str
    tf_ms: int
    open_ts: tuple[int, ...]
    open_px: dict[int, float]
    close_px: dict[int, float]

    def open_at(self, ts: int) -> float | None:
        return self.open_px.get(ts)

    def close_at(self, ts: int) -> float | None:
        return self.close_px.get(ts)


def load_bars(
    conn: sqlite3.Connection,
    descriptor: str,
    tf: str,
    lo_ms: int,
    hi_ms: int,
) -> Bars:
    """Read one instrument's bars in [lo, hi]. Rows with a null open or
    close are dropped rather than interpolated -- a window with a hole is
    refused by the caller, never filled in."""
    tf_ms = {"1m": MS_1M, "5m": 5 * MS_1M, "1h": MS_1H, "4h": 4 * MS_1H}.get(tf)
    if tf_ms is None:
        raise BartestError(f"unsupported tf {tf!r}")
    rows = conn.execute(
        "SELECT open_ts,o,c FROM candles WHERE descriptor=? AND tf=?"
        " AND open_ts>=? AND open_ts<=? AND o IS NOT NULL AND c IS NOT NULL"
        " ORDER BY open_ts",
        (descriptor, tf, lo_ms, hi_ms),
    ).fetchall()
    stamps = tuple(int(r[0]) for r in rows)
    return Bars(
        descriptor=descriptor,
        tf_ms=tf_ms,
        open_ts=stamps,
        open_px={int(r[0]): float(r[1]) for r in rows},
        close_px={int(r[0]): float(r[2]) for r in rows},
    )


# ----------------------------------------------------------------- trades


@dataclasses.dataclass(frozen=True)
class Trade:
    """One completed round trip. `weight` is signed: + long, - short."""

    descriptor: str
    entry_ts: int
    exit_ts: int
    weight: float
    notional_usd: float
    entry_px: float
    exit_px: float
    gross_usd: float
    fees_usd: float
    slip_usd: float

    @property
    def net_usd(self) -> float:
        return self.gross_usd - self.fees_usd - self.slip_usd

    @property
    def ret_bps(self) -> float:
        if self.notional_usd <= 0.0:
            return 0.0
        return 1e4 * self.net_usd / self.notional_usd


@dataclasses.dataclass(frozen=True)
class Rebalance:
    """Target weights decided at `ts` (using data through that bar's
    close) and executed at the NEXT bar's open."""

    ts: int
    weights: dict[str, float]


# --------------------------------------------------------------- the runner


@dataclasses.dataclass
class Result:
    label: str
    trades: list[Trade]
    slippage_bps: float
    maker: bool
    hold_bars: int
    notional_usd: float
    skipped: collections.Counter
    execution: str = "taker"

    # --- aggregates -----------------------------------------------------

    @property
    def n_trades(self) -> int:
        return len(self.trades)

    @property
    def net_usd(self) -> float:
        return math.fsum(t.net_usd for t in self.trades)

    @property
    def gross_usd(self) -> float:
        return math.fsum(t.gross_usd for t in self.trades)

    @property
    def fees_usd(self) -> float:
        return math.fsum(t.fees_usd for t in self.trades)

    @property
    def slip_usd(self) -> float:
        return math.fsum(t.slip_usd for t in self.trades)

    def period_returns(self) -> list[float]:
        """Net return in bps of ONE rebalance period, summed across the
        legs held in it. Periods are non-overlapping by construction, so
        these are the independent observations and their count is the
        effective n."""
        by_period: dict[int, list[Trade]] = collections.defaultdict(list)
        for t in self.trades:
            by_period[t.entry_ts].append(t)
        out = []
        for ts in sorted(by_period):
            legs = by_period[ts]
            notional = math.fsum(abs(t.notional_usd) for t in legs)
            if notional <= 0.0:
                continue
            out.append(1e4 * math.fsum(t.net_usd for t in legs) / notional)
        return out

    def stats(self, periods_per_year: float, overlap: int = 1) -> dict:
        """Every ratio with its standard error (L9). A Sharpe without an
        error bar is not a result.

        ``overlap`` > 1 means consecutive periods SHARE holding time --
        the only way a 24-168 h hold gets a usable n out of 180 days. The
        naive SE is then wrong (it treats correlated observations as
        independent), so a Newey-West correction with ``overlap - 1``
        lags is applied and the inflation factor is reported. A long-hold
        result quoted without it is not a result."""
        rets = self.period_returns()
        n = len(rets)
        if n < 2:
            return {"n_periods": n, "status": "INSUFFICIENT"}
        mean = math.fsum(rets) / n
        var = math.fsum((r - mean) ** 2 for r in rets) / (n - 1)
        sd = math.sqrt(var)
        se = sd / math.sqrt(n)
        se_naive = se
        nw_inflation = 1.0
        if overlap > 1 and n > overlap:
            dev = [r - mean for r in rets]
            gamma0 = math.fsum(d * d for d in dev) / n
            acc = gamma0
            for lag in range(1, min(overlap, n - 1)):
                gl = math.fsum(
                    dev[i] * dev[i - lag] for i in range(lag, n)
                ) / n
                acc += 2.0 * (1.0 - lag / float(overlap)) * gl
            if acc > 0.0:
                se = math.sqrt(acc / n)
                nw_inflation = se / se_naive if se_naive > 0.0 else 1.0
        t = mean / se if se > 0.0 else float("nan")
        if sd > 0.0:
            sharpe = (mean / sd) * math.sqrt(periods_per_year)
            se_sharpe = math.sqrt(
                (1.0 + 0.5 * (mean / sd) ** 2) / n
            ) * math.sqrt(periods_per_year)
        else:
            sharpe = float("nan")
            se_sharpe = float("nan")
        return {
            "n_periods": n,
            "n_trades": self.n_trades,
            "mean_bps": mean,
            "se_bps": se,
            "se_bps_naive": se_naive,
            "overlap": overlap,
            "nw_inflation": nw_inflation,
            "t": t,
            "sharpe": sharpe,
            "se_sharpe": se_sharpe,
            "net_usd": self.net_usd,
            "gross_usd": self.gross_usd,
            "fees_usd": self.fees_usd,
            "slip_usd": self.slip_usd,
            "hit_rate": sum(1 for r in rets if r > 0.0) / n,
            "status": "OK",
        }


def run(
    bars: dict[str, Bars],
    rebalances: typing.Sequence[Rebalance],
    hold_bars: int,
    notional_usd: float,
    tiers: dict[str, FeeTier],
    slippage_bps: float = 1.0,
    maker: bool = False,
    label: str = "bartest",
    execution: str = "taker",
) -> Result:
    """Execute the rebalance schedule under the fill law in the module
    docstring. Returns completed round trips only -- a leg whose entry or
    exit bar is missing is SKIPPED and counted, never priced from a
    neighbouring bar."""
    if hold_bars < 1:
        raise BartestError("hold_bars must be >= 1")
    if notional_usd <= 0.0:
        raise BartestError("notional_usd must be positive")
    if execution not in EXECUTION_MODES:
        raise BartestError(f"unknown execution mode {execution!r}")
    # `maker=True` is the legacy spelling of execution="maker"; keep both
    # working rather than silently ignoring one of them.
    if maker and execution == "taker":
        execution = "maker"
    trades: list[Trade] = []
    skipped: collections.Counter = collections.Counter()
    for reb in rebalances:
        for descriptor, weight in reb.weights.items():
            if weight == 0.0:
                continue
            series = bars.get(descriptor)
            if series is None:
                skipped["no_series"] += 1
                continue
            entry_ts = reb.ts + series.tf_ms
            exit_ts = entry_ts + hold_bars * series.tf_ms
            entry_px = series.open_at(entry_ts)
            exit_px = series.open_at(exit_ts)
            if entry_px is None:
                skipped["no_entry_bar"] += 1
                continue
            if exit_px is None:
                skipped["no_exit_bar"] += 1
                continue
            if entry_px <= 0.0 or exit_px <= 0.0:
                skipped["bad_price"] += 1
                continue
            tier = tiers.get(venue_of(descriptor))
            if tier is None:
                skipped["no_fee_tier"] += 1
                continue
            leg_notional = abs(weight) * notional_usd
            gross = weight * notional_usd * (exit_px / entry_px - 1.0)
            # Per-LEG rates: a hybrid charges maker in and taker out.
            if execution == "maker":
                in_bps = out_bps = float(tier.maker_bps)
            elif execution == "maker_entry":
                in_bps, out_bps = float(tier.maker_bps), float(tier.taker_bps)
            else:
                in_bps = out_bps = float(tier.taker_bps)
            fees = leg_notional * (in_bps + out_bps) / 1e4
            # A resting leg does not pay the spread it is quoting inside.
            slip_legs = (
                0.0 if execution == "maker"
                else (1.0 if execution == "maker_entry" else 2.0)
            )
            slip = slip_legs * leg_notional * slippage_bps / 1e4
            trades.append(
                Trade(
                    descriptor=descriptor,
                    entry_ts=entry_ts,
                    exit_ts=exit_ts,
                    weight=weight,
                    notional_usd=leg_notional,
                    entry_px=entry_px,
                    exit_px=exit_px,
                    gross_usd=gross,
                    fees_usd=fees,
                    slip_usd=slip,
                )
            )
    return Result(
        label=label,
        trades=trades,
        slippage_bps=slippage_bps,
        maker=(execution != "taker"),
        hold_bars=hold_bars,
        notional_usd=notional_usd,
        skipped=skipped,
        execution=execution,
    )


# ------------------------------------------------------ controls and nulls


def null_arm(
    rebalances: typing.Sequence[Rebalance],
    seed: int = 12345,
    across: str = "instruments",
) -> list[Rebalance]:
    """A signal-free arm (L4). ``across='instruments'`` permutes the
    weights among the instruments of each rebalance -- the right null for
    a cross-sectional family, since it preserves the weight distribution
    and destroys only the mapping to instruments. ``across='time'``
    permutes whole weight vectors between rebalance instants."""
    rng = random.Random(seed)
    if across == "time":
        vectors = [dict(r.weights) for r in rebalances]
        rng.shuffle(vectors)
        return [Rebalance(ts=r.ts, weights=v) for r, v in zip(rebalances, vectors)]
    if across != "instruments":
        raise BartestError(f"unknown null axis {across!r}")
    out = []
    for reb in rebalances:
        keys = sorted(reb.weights)
        values = [reb.weights[k] for k in keys]
        rng.shuffle(values)
        out.append(Rebalance(ts=reb.ts, weights=dict(zip(keys, values))))
    return out


def always_long(rebalances: typing.Sequence[Rebalance]) -> list[Rebalance]:
    """The beta control (L10). F-D+ died here: always-long earned +23.94
    bps per 8 h window while the 'signal' earned +7.50."""
    return [
        Rebalance(
            ts=r.ts,
            weights={k: abs(v) for k, v in r.weights.items() if v != 0.0},
        )
        for r in rebalances
    ]


def one_side(
    rebalances: typing.Sequence[Rebalance], side: int
) -> list[Rebalance]:
    """Long-only (side=+1) or short-only (side=-1) control."""
    return [
        Rebalance(
            ts=r.ts,
            weights={
                k: v for k, v in r.weights.items() if v != 0.0 and (v > 0) == (side > 0)
            },
        )
        for r in rebalances
    ]


def split_halves(
    rebalances: typing.Sequence[Rebalance],
) -> tuple[list[Rebalance], list[Rebalance]]:
    """First / second half by time -- the decay control."""
    ordered = sorted(rebalances, key=lambda r: r.ts)
    cut = len(ordered) // 2
    return ordered[:cut], ordered[cut:]


def by_quarter(
    rebalances: typing.Sequence[Rebalance],
) -> dict[str, list[Rebalance]]:
    """Split into calendar quarters of the window (the >= 3 of 4 control)."""
    out: dict[str, list[Rebalance]] = collections.defaultdict(list)
    for r in sorted(rebalances, key=lambda z: z.ts):
        day = datetime.datetime.fromtimestamp(r.ts / 1000.0, datetime.UTC)
        out[f"{day.year}Q{(day.month - 1) // 3 + 1}"].append(r)
    return dict(out)


# ------------------------------------------------------- the frozen report


def _usd(value: float) -> str:
    """The harness's 1e-6 render -- pnl_report parses these as strings."""
    return f"{value:.6f}"


def to_audit_pnl(
    results: typing.Sequence[tuple[int, str, Result]],
    window_first_ms: int,
    window_last_ms: int,
) -> dict:
    """Render as the frozen audit-pnl contract so ``pnl_report`` folds it
    unchanged. ``results`` is (strategy_id, label, Result).

    Counters that have no meaning for a bar strategy are emitted as 0
    rather than omitted: a consumer that sums them must not have to know
    which producer wrote the row."""
    strategies = []
    for strategy_id, label, res in results:
        strategies.append(
            {
                "strategy_id": int(strategy_id),
                "label": label,
                "orders": 2 * res.n_trades,
                "fills": 2 * res.n_trades,
                "trades": res.n_trades,
                "canceled_end": 0,
                "rejected_caps": 0,
                "unroutable": int(sum(res.skipped.values())),
                "ioc_fills": 0,
                "ioc_canceled": 0,
                "ttl_expired": 0,
                "net_usd": _usd(res.net_usd),
                "realized_usd": _usd(res.gross_usd),
                "fees_usd": _usd(res.fees_usd + res.slip_usd),
                "markout_usd": _usd(0.0),
                "max_drawdown_usd": _usd(_max_drawdown(res)),
                "fee_ladder_net_usd": [
                    _usd(res.gross_usd - res.slip_usd),
                    _usd(res.net_usd),
                    _usd(res.net_usd),
                ],
            }
        )
    return {
        "audit_pnl_version": AUDIT_PNL_VERSION,
        "producer": "claude_worker.bartest",
        "window": {
            "wall_first_ns": int(window_first_ms) * 1_000_000,
            "wall_last_ns": int(window_last_ms) * 1_000_000,
        },
        "paper": {"fills": 0, "net_usd": _usd(0.0)},
        "strategies": strategies,
        "vm_by_ruleset": [],
    }


def _max_drawdown(res: Result) -> float:
    """Worst peak-to-trough of the cumulative net, period by period."""
    by_period: dict[int, float] = collections.defaultdict(float)
    for t in res.trades:
        by_period[t.entry_ts] += t.net_usd
    peak = 0.0
    equity = 0.0
    worst = 0.0
    for ts in sorted(by_period):
        equity += by_period[ts]
        peak = max(peak, equity)
        worst = min(worst, equity - peak)
    return abs(worst)
