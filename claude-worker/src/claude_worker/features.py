"""data_fetcher core (design §5.1): replay-log features + positions/P&L.

Primary input is the engine's PMLR capture (``CLAUDE_WORKER_REPLAY_DIR``
→ the engine's ``MULTIVENUE_LOG_DIR``): per-venue ``<venue>-ticks.pmlr``
plus the 8f engine-thread ``engine-fills.pmlr``. Venue REST is strictly
secondary and rate-budgeted ([`RestBudget`]); this module never imports an
HTTP client — callers inject a ``get_fn`` (the real consumers arrive in
8h, the budget mechanics are pinned now).

Positions/P&L are reconstructed from fills + latest tick marks
(design §2/§5.1): signed net quantity, integer cost-basis accounting
(scale notes below), realized + unrealized P&L, exposure, and the HIP-4
``|yes - no|`` netting mirror per docs/risk-policy.md — authoritative
enforcement stays engine-side (8i); this is the research-plane view.

Scales: prices and quantities are fixed-point 1e6 (wire format). Products
``px * qty`` are therefore 1e12-scaled "USD units" — all cost/P&L
arithmetic stays in integers at 1e12 scale (exact conservation: closing a
position removes basis pro-rata with the floor-division remainder left in
the open cost, so realized + open basis always sums to the true total);
floats appear only at the JSON edge ([`to_usd`]).

HIP-4 pairing note (interpretation call, recorded in the progress log):
SymbolId ordinals are boot-allocation order, not HIP-4 encodings, so
yes/no pairing is NOT derivable from the fill log alone — callers pass
explicit ``(yes_sym, no_sym)`` pairs (config-driven; the verb layer wires
them in item 12+).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import json
import pathlib
import time
import typing

import claude_worker.pmlr

# Engine-thread fills capture file name (Rust: `engine::ENGINE_FILLS_FILE`).
ENGINE_FILLS_FILE: str = "engine-fills.pmlr"
_RUN_DIR_PREFIX: str = "run-"
_TICKS_SUFFIX: str = "-ticks.pmlr"

SIDE_BID: int = 0
SIDE_ASK: int = 1

USD_SCALE: int = 1_000_000_000_000  # px(1e6) * qty(1e6)


def to_usd(value_1e12: int) -> float:
    """1e12-scaled integer USD units -> float USD (JSON edge only)."""
    return value_1e12 / USD_SCALE


# ---- run-dir discovery -------------------------------------------------


def run_dirs(replay_dir: pathlib.Path) -> list[pathlib.Path]:
    """All ``run-<epoch_ns>`` capture dirs under ``replay_dir``, oldest
    first. Non-conforming names are ignored."""
    out: list[tuple[int, pathlib.Path]] = []
    if not replay_dir.is_dir():
        return []
    for child in replay_dir.iterdir():
        if not child.is_dir() or not child.name.startswith(_RUN_DIR_PREFIX):
            continue
        suffix = child.name[len(_RUN_DIR_PREFIX) :]
        if suffix.isdigit():
            out.append((int(suffix), child))
    out.sort(key=lambda pair: pair[0])
    return [pair[1] for pair in out]


def latest_run_dir(replay_dir: pathlib.Path) -> pathlib.Path | None:
    """The newest run dir, or None when none exist."""
    dirs = run_dirs(replay_dir)
    return dirs[-1] if dirs else None


# ---- tick features -----------------------------------------------------


@dataclasses.dataclass(slots=True)
class SymbolFeatures:
    """Single-pass per-symbol aggregates over one run's tick capture."""

    sym: int
    venue: int
    ticks: int = 0
    first_ts_ns: int = 0
    last_ts_ns: int = 0
    last_bid_px: int = 0
    last_ask_px: int = 0
    last_mid_px: int = 0
    spread_sum: int = 0
    spread_min: int = 0
    spread_max: int = 0

    def update(self, tick: claude_worker.pmlr.TickRec) -> None:
        spread = tick.ask_px - tick.bid_px
        if self.ticks == 0:
            self.first_ts_ns = tick.ts_ns
            self.spread_min = spread
            self.spread_max = spread
        else:
            self.spread_min = min(self.spread_min, spread)
            self.spread_max = max(self.spread_max, spread)
        self.ticks += 1
        self.last_ts_ns = tick.ts_ns
        self.last_bid_px = tick.bid_px
        self.last_ask_px = tick.ask_px
        self.last_mid_px = tick.mid()
        self.spread_sum += spread

    def as_json_obj(self) -> dict[str, int | float]:
        """Compact feature record. Prices stay 1e6 integers (lossless);
        floats only for derived means/rates."""
        span_ns = self.last_ts_ns - self.first_ts_ns
        rate_hz = (self.ticks - 1) / (span_ns / 1e9) if span_ns > 0 else 0.0
        return {
            "sym": self.sym,
            "venue": self.venue,
            "ticks": self.ticks,
            "first_ts_ns": self.first_ts_ns,
            "last_ts_ns": self.last_ts_ns,
            "last_bid_px": self.last_bid_px,
            "last_ask_px": self.last_ask_px,
            "last_mid_px": self.last_mid_px,
            "mean_spread": self.spread_sum / self.ticks if self.ticks else 0.0,
            "min_spread": self.spread_min,
            "max_spread": self.spread_max,
            "tick_rate_hz": rate_hz,
        }


def tick_features(
    reader: claude_worker.pmlr.Reader,
    into: dict[int, SymbolFeatures] | None = None,
) -> dict[int, SymbolFeatures]:
    """Aggregate one tick file into per-symbol features. v1 files carry no
    venue byte — Phase-1 capture was Polymarket-only, so venue 0 is pinned
    for them (documented in docs/migration.md)."""
    feats: dict[int, SymbolFeatures] = {} if into is None else into
    v2 = reader.version >= claude_worker.pmlr.VENUE_BYTE_MIN_VERSION
    for tick in reader.ticks():
        entry = feats.get(tick.sym)
        if entry is None:
            entry = SymbolFeatures(sym=tick.sym, venue=tick.venue if v2 else 0)
            feats[tick.sym] = entry
        entry.update(tick)
    return feats


def write_feature_files(
    features_dir: pathlib.Path,
    run_name: str,
    feats: dict[int, SymbolFeatures],
) -> list[pathlib.Path]:
    """One compact JSON file per symbol under ``features_dir/<run_name>/``
    (design §5.1 "compact per-symbol feature files"). Returns the paths,
    symbol-sorted."""
    out_dir = features_dir / run_name
    out_dir.mkdir(parents=True, exist_ok=True)
    paths: list[pathlib.Path] = []
    for sym in sorted(feats):
        path = out_dir / f"{sym}.json"
        path.write_text(json.dumps(feats[sym].as_json_obj(), separators=(",", ":")))
        paths.append(path)
    return paths


# ---- marks + run collection --------------------------------------------


def collect_marks(reader: claude_worker.pmlr.Reader, into: dict[int, int]) -> None:
    """Fold last-mid marks per symbol from one tick file (file order —
    the last record wins, matching capture append order)."""
    for tick in reader.ticks():
        into[tick.sym] = tick.mid()


class CollectResult(typing.NamedTuple):
    """Output of [`collect_run`]: written feature files, last marks per
    symbol, and any file the engine was mid-flush on (torn tail)."""

    feature_paths: list[pathlib.Path]
    marks: dict[int, int]
    torn_files: list[str]


def collect_run(run_dir: pathlib.Path, features_dir: pathlib.Path) -> CollectResult:
    """Feature-extract every ``*-ticks.pmlr`` in one run dir and write the
    per-symbol files. The replay log is the primary data source (§5.1);
    REST never participates here."""
    feats: dict[int, SymbolFeatures] = {}
    marks: dict[int, int] = {}
    torn: list[str] = []
    for path in sorted(run_dir.glob(f"*{_TICKS_SUFFIX}")):
        with claude_worker.pmlr.Reader(path) as reader:
            tick_features(reader, into=feats)
            collect_marks(reader, into=marks)
            if reader.torn:
                torn.append(path.name)
    paths = write_feature_files(features_dir, run_dir.name, feats)
    return CollectResult(feature_paths=paths, marks=marks, torn_files=torn)


def read_fills(run_dir: pathlib.Path) -> tuple[list[claude_worker.pmlr.FillRec], bool]:
    """All fills from the run's ``engine-fills.pmlr`` plus the torn flag.
    A missing file (engine predates 8f, or no fills yet) is an empty
    result, not an error."""
    path = run_dir / ENGINE_FILLS_FILE
    if not path.exists():
        return [], False
    with claude_worker.pmlr.Reader(path) as reader:
        return list(reader.fills()), reader.torn


# ---- rate-budgeted REST secondary ---------------------------------------


class RestBudget:
    """Fixed-window call budget for the venue REST secondary (§5.1).

    ``try_acquire`` is the single gate: True consumes one call, False
    means the window is exhausted (callers skip, never wait — the replay
    log is primary; REST is best-effort enrichment)."""

    def __init__(
        self,
        max_calls: int,
        window_ns: int,
        clock_ns: typing.Callable[[], int] = time.monotonic_ns,
    ) -> None:
        if max_calls < 0 or window_ns <= 0:
            raise ValueError("RestBudget: max_calls >= 0 and window_ns > 0 required")
        self._max: int = max_calls
        self._window_ns: int = window_ns
        self._clock: typing.Callable[[], int] = clock_ns
        self._window_start: int = self._clock()
        self._used: int = 0
        self.skipped_total: int = 0

    def try_acquire(self) -> bool:
        now = self._clock()
        if now - self._window_start >= self._window_ns:
            self._window_start = now
            self._used = 0
        if self._used < self._max:
            self._used += 1
            return True
        self.skipped_total += 1
        return False


def fetch_secondary(
    budget: RestBudget,
    get_fn: typing.Callable[[str], str | None],
    urls: typing.Iterable[str],
) -> tuple[list[tuple[str, str]], int]:
    """Budgeted secondary fetch: at most the budget's remaining calls are
    spent; the rest are skipped (counted, returned). ``get_fn`` is caller
    -injected (httpx lives with the consumer); a ``None`` payload means
    the fetch failed and is simply omitted — REST is best-effort."""
    fetched: list[tuple[str, str]] = []
    skipped = 0
    for url in urls:
        if not budget.try_acquire():
            skipped += 1
            continue
        payload = get_fn(url)
        if payload is not None:
            fetched.append((url, payload))
    return fetched, skipped


# ---- positions / P&L ----------------------------------------------------


@dataclasses.dataclass(slots=True)
class Position:
    """Signed-net position accumulator for one symbol (integer basis).

    ``net_qty`` 1e6-scaled signed quantity; ``open_cost`` 1e12-scaled
    signed basis of the open position; ``realized`` 1e12-scaled realized
    P&L. Basis removal on close is pro-rata floor division — the
    remainder stays in ``open_cost``, so no value is ever created or
    destroyed by rounding."""

    sym: int
    net_qty: int = 0
    open_cost: int = 0
    realized: int = 0
    fills: int = 0

    def apply(self, side: int, px: int, qty: int) -> None:
        if qty <= 0:
            raise ValueError(f"fill qty must be positive: {qty}")
        delta = qty if side == SIDE_BID else -qty
        self.fills += 1
        if self.net_qty == 0 or (self.net_qty > 0) == (delta > 0):
            self.net_qty += delta
            self.open_cost += px * delta
            return
        sign = 1 if self.net_qty > 0 else -1
        closed = min(abs(self.net_qty), abs(delta))
        removed = self.open_cost * closed // abs(self.net_qty)
        self.realized += sign * px * closed - removed
        self.open_cost -= removed
        self.net_qty -= sign * closed
        remainder = delta + sign * closed
        if remainder != 0:
            # Flip: the old side is exactly consumed (open_cost is 0 by
            # exact pro-rata when closed == |net|); open the new side.
            self.net_qty += remainder
            self.open_cost += px * remainder


def reconstruct_positions(
    fills: typing.Iterable[claude_worker.pmlr.FillRec],
) -> dict[int, Position]:
    """Fold fills (file order = time order) into per-symbol positions."""
    positions: dict[int, Position] = {}
    for fill in fills:
        pos = positions.get(fill.sym)
        if pos is None:
            pos = Position(sym=fill.sym)
            positions[fill.sym] = pos
        pos.apply(fill.side, fill.px, fill.qty)
    return positions


class PositionView(typing.NamedTuple):
    """Marked view of one position. No mark for the symbol ⇒ carried at
    cost: ``mark_px`` falls back to the position's average price and
    unrealized is 0 (documented fail-safe, not an error)."""

    sym: int
    net_qty: int
    avg_px: float
    mark_px: int
    realized: int
    unrealized: int
    exposure: int


def position_views(
    positions: dict[int, Position],
    marks: dict[int, int],
) -> dict[int, PositionView]:
    """Mark every position: unrealized = net*mark - basis; exposure =
    |net*mark| (both 1e12 ints)."""
    views: dict[int, PositionView] = {}
    for sym in sorted(positions):
        pos = positions[sym]
        avg_px = pos.open_cost / pos.net_qty if pos.net_qty != 0 else 0.0
        mark = marks.get(sym)
        if mark is None:
            # Carried at cost (fail-safe): no phantom P&L from a
            # truncated average — unrealized is exactly 0, exposure is
            # the absolute open basis.
            unrealized = 0
            exposure = abs(pos.open_cost)
            mark_px = int(avg_px)
        else:
            unrealized = pos.net_qty * mark - pos.open_cost
            exposure = abs(pos.net_qty * mark)
            mark_px = mark
        views[sym] = PositionView(
            sym=sym,
            net_qty=pos.net_qty,
            avg_px=avg_px,
            mark_px=mark_px,
            realized=pos.realized,
            unrealized=unrealized,
            exposure=exposure,
        )
    return views


class Hip4PairView(typing.NamedTuple):
    """HIP-4 ``|yes - no|`` netting mirror (risk-policy): equal Yes+No is
    riskless collateral, so paired exposure is the NET leg marked at the
    Yes price."""

    yes_sym: int
    no_sym: int
    net_qty: int
    flattened_qty: int
    exposure: int


def hip4_pair_views(
    views: dict[int, PositionView],
    pairs: typing.Iterable[tuple[int, int]],
) -> list[Hip4PairView]:
    """Apply the netting rule to explicit (yes, no) symbol pairs. Pairs
    with no position on either leg are omitted."""
    out: list[Hip4PairView] = []
    for yes_sym, no_sym in pairs:
        yes = views.get(yes_sym)
        no = views.get(no_sym)
        if yes is None and no is None:
            continue
        yes_qty = yes.net_qty if yes is not None else 0
        no_qty = no.net_qty if no is not None else 0
        mark_yes = yes.mark_px if yes is not None else 0
        net = yes_qty - no_qty
        out.append(
            Hip4PairView(
                yes_sym=yes_sym,
                no_sym=no_sym,
                net_qty=net,
                flattened_qty=min(yes_qty, no_qty) if yes_qty > 0 and no_qty > 0 else 0,
                exposure=abs(net * mark_yes),
            )
        )
    return out


def total_exposure(
    views: dict[int, PositionView],
    pair_views: list[Hip4PairView],
) -> int:
    """Portfolio exposure with HIP-4 netting applied: paired legs are
    replaced by their net-leg exposure; every other symbol contributes
    ``|net*mark|`` (1e12 int)."""
    paired_syms: set[int] = set()
    total = 0
    for pv in pair_views:
        paired_syms.add(pv.yes_sym)
        paired_syms.add(pv.no_sym)
        total += pv.exposure
    for sym, view in views.items():
        if sym not in paired_syms:
            total += view.exposure
    return total
