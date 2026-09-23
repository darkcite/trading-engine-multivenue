# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)

"""HIP-4 outcome markets, offline (BIN15 O2).

Two things every offline consumer of a rolling family needs, and one
place for both.

**The grammar.** A HIP-4 market carries its whole economics --
underlying, strike, expiry, settlement window -- in one ``|``-delimited
``key:value`` description string. This module is the PYTHON MIRROR of
``ingress_hyperliquid::discovery::parse_outcome_spec``: the same key
sets, the same integer arithmetic, the same refusals. Where the two
could drift they are pinned by one shared fixture
(``tests/fixtures/hip4/descriptions.tsv``), read by both sides.

**The roll sidecar.** A family's ``SymbolId`` slot is stable for the
life of the process while the venue instrument under it changes --
96 times a day for the BTC 15-minute family. ``instrument-manifest.tsv``
is two columns by law and names only the slot, so the only record of
WHICH instance a slot meant at a time is the
``ChannelId::InstrumentRoll`` event in the run's own capture.
:func:`read_rolls` is how every offline consumer reads it; without it a
captured slot sym is uninterpretable after the fact.

**The settlement law** (BIN15 S1, LAW E-11 -- ``docs/risk-policy.md``
"E8"). A HIP-4 binary settles on the TIME-weighted TWAP of the
underlying's mark over ``[expiry - twap, expiry]``, ``>=`` the strike,
under strict evidence. :func:`settle_reference_1e6` / :func:`payout_1e6`
/ :func:`link_successors` / :func:`y_next_strike` are the PYTHON MIRROR
of ``cli::backtest::binary``'s -- the first two pinned against it by the
shared fixture ``tests/fixtures/bin15/settle-1.{input,expected}.tsv``,
the last two by the worker's own tests; the accrual's
``relabel`` lane (BIN15 S4) and every research reader label instances
through them, so no reader restates the window.

Offline only -- nothing here runs in the engine.
"""

import bisect
import dataclasses
import itertools
import pathlib
import statistics
import typing

import claude_worker.bin15_ref
import claude_worker.pmlr

#: Ticks sampled when fitting the venue-wall offset (BIN15 O10).
_OFFSET_SAMPLES: int = 4000

#: The HARNESS's sample: the first N stamped Hyperliquid ticks
#: (``cli::backtest::clock::VENUE_OFFSET_SAMPLES``, BIN15 S2).
HARNESS_OFFSET_SAMPLES: int = 64

#: Grammar of a parsed description -- mirrors ``HlOutcomeGrammar``.
GRAMMAR_UNKNOWN: int = 0
GRAMMAR_OUT_BINARY_PRICE: int = 1
GRAMMAR_OUT_PRICE_TOUCH: int = 2
GRAMMAR_NATIVE_PRICE_BINARY: int = 3

#: Capacity of the Rust struct's underlying-coin field. A wider coin is
#: a venue contract change, not a truncation candidate -- the Rust side
#: refuses it, so the mirror must too.
UNDERLYING_MAX: int = 15

#: ``period:`` suffixes, in seconds.
_PERIOD_UNITS: dict[str, int] = {"s": 1, "m": 60, "h": 3600, "d": 86400}

_DAYS_IN_MONTH: tuple[int, ...] = (31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31)


def _is_leap(year: int) -> bool:
    return (year % 4 == 0 and year % 100 != 0) or year % 400 == 0


def _days_in_month(year: int, month: int) -> int:
    if month == 2:
        return 29 if _is_leap(year) else 28
    return _DAYS_IN_MONTH[month - 1]


def _days_from_civil(year: int, month: int, day: int) -> int:
    """Hinnant's ``days_from_civil`` -- the Rust twin, same arithmetic."""
    y = year - 1 if month <= 2 else year
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    mp = (month + 9) % 12
    doy = (153 * mp + 2) // 5 + day - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def parse_hl_time_ns(text: str) -> int | None:
    """``YYYYMMDD-HHMM`` (UTC) -> ns since the epoch, else ``None``.

    Integer arithmetic, no ``datetime`` -- the point is to be the same
    function as the Rust one, including which inputs it refuses (an
    impossible civil date, a year before 1970).
    """
    if len(text) != 13 or text[8] != "-":
        return None
    digits = text[:8] + text[9:]
    if not digits.isdigit() or not digits.isascii():
        return None
    year = int(text[0:4])
    month = int(text[4:6])
    day = int(text[6:8])
    hour = int(text[9:11])
    minute = int(text[11:13])
    if year < 1970 or not 1 <= month <= 12 or day < 1 or hour > 23 or minute > 59:
        return None
    if day > _days_in_month(year, month):
        return None
    secs = _days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60
    if secs < 0 or secs > (2**64 - 1) // 1_000_000_000:
        return None
    return secs * 1_000_000_000


def _scan_px_1e6(text: str) -> int | None:
    """``[-]int[.frac]`` -> x1e6, truncating past six decimals."""
    body = text[1:] if text.startswith("-") else text
    neg = text.startswith("-")
    if "." in body:
        whole, _, frac = body.partition(".")
        if not frac:
            return None
    else:
        whole, frac = body, ""
    if not whole.isdigit() or not whole.isascii():
        return None
    if frac and (not frac.isdigit() or not frac.isascii()):
        return None
    units = int(whole) * 1_000_000 + int((frac + "000000")[:6] or 0)
    return -units if neg else units


def _scan_period_s(text: str) -> int:
    if len(text) < 2:
        return 0
    mult = _PERIOD_UNITS.get(text[-1])
    head = text[:-1]
    if mult is None or not head.isdigit() or not head.isascii():
        return 0
    return int(head) * mult


@dataclasses.dataclass(frozen=True)
class OutcomeSpec:
    """A parsed HIP-4 description. Absent fields are 0, exactly as in
    the Rust ``HlOutcomeSpec``."""

    outcome: int
    grammar: int
    underlying: str
    strike_1e6: int
    expiry_ns: int
    twap_s: int
    period_s: int


def parse_outcome_spec(outcome: int, description: str) -> OutcomeSpec:
    """Parse a description VALUE. Never raises.

    Tokenised on ``|`` rather than searched key by key:
    ``priceDescription`` is free text, and prose containing
    ``threshold:`` must not become the strike. A required key that is
    present but UNPARSEABLE leaves its field 0 and demotes the grammar
    to :data:`GRAMMAR_UNKNOWN`, so a venue contract change reads as
    "unknown shape" and never as a confident wrong number.
    """
    underlying = ""
    strike_1e6 = 0
    expiry_ns = 0
    twap_s = 0
    period_s = 0
    seen: set[str] = set()

    for token in description.split("|"):
        key, sep, val = token.partition(":")
        if not sep:
            continue
        if key in ("perp", "underlying"):
            if val and len(val) <= UNDERLYING_MAX:
                underlying = val
                seen.add(key)
        elif key in ("threshold", "target", "targetPrice"):
            px = _scan_px_1e6(val)
            if px is not None:
                strike_1e6 = px
                seen.add(key)
        elif key == "seconds":
            if val.isdigit() and val.isascii() and len(val) <= 10:
                twap_s = int(val)
                seen.add(key)
        elif key == "period":
            period_s = _scan_period_s(val)
        elif key in ("time", "expiry"):
            ns = parse_hl_time_ns(val)
            if ns is not None:
                expiry_ns = ns
                seen.add(key)
        elif key == "class" and val == "priceBinary":
            seen.add("class")

    if {"perp", "threshold", "seconds", "time"} <= seen:
        grammar = GRAMMAR_OUT_BINARY_PRICE
    elif {"perp", "target"} <= seen:
        grammar = GRAMMAR_OUT_PRICE_TOUCH
    elif {"class", "underlying", "expiry", "targetPrice"} <= seen:
        grammar = GRAMMAR_NATIVE_PRICE_BINARY
    else:
        grammar = GRAMMAR_UNKNOWN

    return OutcomeSpec(
        outcome=outcome,
        grammar=grammar,
        underlying=underlying,
        strike_1e6=strike_1e6,
        expiry_ns=expiry_ns,
        twap_s=twap_s,
        period_s=period_s,
    )


# ---------------------------------------------------------------------------
# The roll sidecar
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class Roll:
    """One ``ChannelId::InstrumentRoll`` event.

    ``sym`` is the family's YES slot (the No leg is the next ordinal).
    ``settled`` distinguishes the two transitions: an instance settling
    (the book empties, the slot keeps its binding) from the successor
    being created (the slot is rebound).
    """

    ts_ns: int
    sym: int
    family: int
    outcome: int
    strike_1e6: int
    expiry_ns: int
    twap_s: int
    settled: bool


def unpack_roll_seq(seq: int) -> tuple[int, int, int, bool]:
    """``venue_seq`` -> ``(outcome, twap_s, family, settled)``.

    Mirrors ``ingress_hyperliquid::family::unpack_roll_seq``; the layout
    is the ``ChannelId::InstrumentRoll`` doc's table.
    """
    return (
        seq & 0xFFFFFFFF,
        (seq >> 32) & 0xFFFF,
        (seq >> 48) & 0xFF,
        bool((seq >> 56) & 1),
    )


def venue_wall_offset_ns(run_dir: pathlib.Path) -> int | None:
    """Nanoseconds to ADD to a capture's monotonic stamp to get the
    VENUE's wall clock, or ``None`` when the run carries no venue time.

    A HIP-4 expiry is a wall instant; every stamp in a capture is
    monotonic-since-boot. The v3 tick's ``venue_time_ms`` is the venue's
    own clock against that stamp, so their median difference is the
    mapping — accurate to the feed delay (~100-300 ms on this venue).

    NOT ``pmlr.run_anchor_ns``: that anchor is the run's FIRST TICK,
    which arrives 10-18 s after the run directory's epoch, so pairing
    the two reads ~15 s early — enough to miss a 60 s TWAP window's
    edge. Measured against the settled rolls, this mapping puts every
    settlement publication 6-14 s AFTER its expiry, which is the
    venue's own lag; the anchor mapping put them BEFORE it, which is
    impossible.
    """
    f = run_dir / "hl-ticks.pmlr"
    if not f.is_file():
        return None
    try:
        with claude_worker.pmlr.Reader(f) as reader:
            if not reader.has_venue_time or len(reader) == 0:
                return None
            n = len(reader)
            step = max(1, n // _OFFSET_SAMPLES)
            return venue_offset_from_samples(
                (t.venue_time_ms, t.ts_ns)
                for t in (reader.tick(i) for i in range(0, n, step))
            )
    except (claude_worker.pmlr.PmlrError, OSError, ValueError):
        return None


def venue_offset_from_samples(pairs: typing.Iterable[tuple[int, int]]) -> int | None:
    """``int(median(venue_time_ms·1e6 − ts_ns))`` over ``(venue_time_ms,
    ts_ns)`` pairs, unstamped ones (``venue_time_ms == 0``) skipped;
    ``None`` with none -- the law :func:`venue_wall_offset_ns` applies to
    its 4 000-tick sample of a file."""
    offs = [v * 1_000_000 - ts for v, ts in pairs if v]
    return int(statistics.median(offs)) if offs else None


def venue_offset_first_n(
    pairs: typing.Iterable[tuple[int, int]], n: int = HARNESS_OFFSET_SAMPLES
) -> int | None:
    """The HARNESS's law (``cli::backtest::clock::VenueOffsetFit``, BIN15
    S2), mirrored exactly: the median of the FIRST ``n`` stamped pairs in
    file order -- on an even count the mean of the two middle ones,
    truncated toward zero as Rust's integer division does. Integer
    throughout (a float median of 1.8e18-sized numbers is off by up to
    ~100 ns)."""
    offs: list[int] = []
    for v, ts in pairs:
        if not v:
            continue
        d = v * 1_000_000 - ts
        # Rust drops a difference that leaves i64 rather than wrapping it.
        if d < -(2**63) or d > 2**63 - 1:
            continue
        offs.append(d)
        if len(offs) == n:
            break
    if not offs:
        return None
    offs.sort()
    mid = len(offs) // 2
    if len(offs) % 2 == 1:
        return offs[mid]
    s = offs[mid - 1] + offs[mid]
    return s // 2 if s >= 0 else -((-s) // 2)


def open_at_end(run_dir: pathlib.Path) -> list[Roll]:
    """Instances CREATED in ``run_dir`` whose settlement instant falls
    after the run's last record — the positions a per-run audit can
    open but never close.

    Empty when the run carries no rolls, or no venue time to place
    them on the wall clock.
    """
    off = venue_wall_offset_ns(run_dir)
    if off is None:
        return []
    last = _last_mono_ts(run_dir)
    if last is None:
        return []
    end_wall = last + off
    out: list[Roll] = []
    for roll in read_rolls(run_dir):
        if roll.settled or not roll.expiry_ns:
            continue
        if roll.expiry_ns + roll.twap_s * 1_000_000_000 > end_wall:
            out.append(roll)
    return out


def _last_mono_ts(run_dir: pathlib.Path) -> int | None:
    """The newest monotonic stamp across the run's tick files."""
    last: int | None = None
    for path in sorted(run_dir.glob("*-ticks.pmlr")):
        try:
            with claude_worker.pmlr.Reader(path) as reader:
                if len(reader) == 0:
                    continue
                ts = reader.tick(len(reader) - 1).ts_ns
        except (claude_worker.pmlr.PmlrError, OSError, ValueError):
            continue
        last = ts if last is None else max(last, ts)
    return last


def read_rolls(run_dir: pathlib.Path, venue: str = "hl") -> list[Roll]:
    """Every roll in a run's capture, in file order.

    THE sidecar: a captured slot sym means different instruments at
    different times, and this is the only record of which. An absent or
    unreadable events file yields an empty list rather than raising --
    a run with no rolling families simply has none.
    """
    path = run_dir / f"{venue}-events.pmlr"
    if not path.is_file():
        return []
    out: list[Roll] = []
    with claude_worker.pmlr.Reader(path) as reader:
        if reader.slot_kind != claude_worker.pmlr.SLOT_KIND_CHANNEL_EVENT:
            return []
        for ev in reader.channel_events():
            if ev.channel != claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL:
                continue
            outcome, twap_s, family, settled = unpack_roll_seq(ev.venue_seq)
            out.append(
                Roll(
                    ts_ns=ev.ts_ns,
                    sym=ev.sym,
                    family=family,
                    outcome=outcome,
                    strike_1e6=ev.v0,
                    expiry_ns=ev.v1,
                    twap_s=twap_s,
                    settled=settled,
                )
            )
    return out


def instance_at(rolls: typing.Iterable[Roll], sym: int, ts_ns: int) -> Roll | None:
    """Which instance slot ``sym`` held at ``ts_ns``.

    The newest CREATED roll for that slot at or before the instant. The
    settled rows are deliberately not eligible: a settled instance is
    still the one the slot holds until its successor is created, which
    is exactly what the ingress does on the wire.
    """
    best: Roll | None = None
    for r in rolls:
        if r.sym != sym or r.settled or r.ts_ns > ts_ns:
            continue
        if best is None or r.ts_ns > best.ts_ns:
            best = r
    return best


# ---------------------------------------------------------------------------
# The settlement law (BIN15 S1, LAW E-11) -- mirrored from cli::backtest::binary
# ---------------------------------------------------------------------------

#: ``cli::backtest::binary::SETTLE_MIN_MARKS`` -- marks INSIDE the window.
SETTLE_MIN_MARKS: int = 3

#: ``core_types::BINARY_SETTLE_MARK_GAP_MAX_NS`` -- the longest one mark may
#: stand for the venue's series across the window (the whole piece counts).
SETTLE_MARK_GAP_MAX_NS: int = 10_000_000_000

#: ``cli::backtest::binary::SUCCESSOR_MAX_LAG_NS``.
SUCCESSOR_MAX_LAG_NS: int = 120_000_000_000

#: ``cli::backtest::binary::Y_NEXT_STRIKE_UNKNOWN``.
Y_NEXT_STRIKE_UNKNOWN: int = -1

#: A payout ×1e6: the leg won.
PAYOUT_ITM_1E6: int = 1_000_000


def settle_reference_1e6(
    ts: typing.Sequence[int],
    px: typing.Sequence[int],
    expiry_ns: int,
    twap_ns: int,
) -> int | None:
    """``cli::backtest::binary::settle_reference_1e6`` -- the settlement
    price ×1e6, or ``None`` when the evidence is not there.

    ``ts`` / ``px`` are the underlying's marks on the VENUE clock,
    ascending in ``ts`` (a stable sort of the capture). The window is
    ``[expiry - twap, expiry]``; each mark counts for as long as it was
    the mark (:func:`claude_worker.bin15_ref.binary_twap_segment`, the
    engine's own piece), the last one before the open carries into it and
    the last one inside carries to the expiry. Strict evidence, as the
    harness: a mark in force AT the open; no piece spanning the window
    longer than :data:`SETTLE_MARK_GAP_MAX_NS` (the carry-in and the carry
    to the expiry included); at least :data:`SETTLE_MIN_MARKS` marks
    inside. One truncating divide at the end (Rust's ``i128 /``).
    ``twap_ns == 0``: the last mark at or before the expiry.
    """
    close = expiry_ns
    if twap_ns == 0:
        end = bisect.bisect_right(ts, close)
        return None if end == 0 else int(px[end - 1])
    open_ns = max(close - twap_ns, 0)
    first = bisect.bisect_right(ts, open_ns)
    if first == 0:
        return None
    k = first - 1
    since = int(ts[k])
    cur = int(px[k])
    k += 1
    total = 0
    inside = 1 if since == open_ns else 0
    n = len(ts)
    while k < n:
        t = int(ts[k])
        if t > close:
            break
        if t - since > SETTLE_MARK_GAP_MAX_NS:
            return None
        total += claude_worker.bin15_ref.binary_twap_segment(cur, since, t, open_ns, close)[0]
        inside += 1
        since = t
        cur = int(px[k])
        k += 1
    if close - since > SETTLE_MARK_GAP_MAX_NS:
        return None
    total += claude_worker.bin15_ref.binary_twap_segment(cur, since, close, open_ns, close)[0]
    if inside < SETTLE_MIN_MARKS:
        return None
    width = close - open_ns
    q = abs(total) // width
    return q if total >= 0 else -q


def payout_1e6(reference_1e6: int, strike_1e6: int) -> int:
    """``cli::backtest::binary::payout_1e6`` -- ``>=`` settles in the money."""
    return PAYOUT_ITM_1E6 if reference_1e6 >= strike_1e6 else 0


def y_next_strike(strike_1e6: int, next_strike_1e6: int) -> int:
    """``BinaryInstance::y_next_strike`` -- the VENUE-published label: the
    successor's strike is the venue's settlement price (rounded to the
    strike grid). :data:`Y_NEXT_STRIKE_UNKNOWN` when there is no successor
    or it rounded onto the strike (a tie is excluded, never guessed)."""
    if next_strike_1e6 <= 0 or next_strike_1e6 == strike_1e6:
        return Y_NEXT_STRIKE_UNKNOWN
    return PAYOUT_ITM_1E6 if next_strike_1e6 > strike_1e6 else 0


@dataclasses.dataclass(slots=True)
class Instance:
    """One created HIP-4 instance on the venue clock -- the fields
    ``cli::backtest::binary::BinaryInstance`` carries, with the slot named
    by its DESCRIPTOR (a sym means nothing across runs)."""

    outcome: int
    slot: str
    strike_1e6: int
    expiry_ns: int
    twap_ns: int
    created_ns: int
    next_strike_1e6: int = 0

    @property
    def settle_open_ns(self) -> int:
        return max(self.expiry_ns - self.twap_ns, 0)


def link_successors(insts: typing.Sequence[Instance]) -> None:
    """``cli::backtest::binary::link_successors``: set each instance's
    ``next_strike_1e6`` from the next created instance of the SAME slot
    with a later expiry, created between this one's window open and
    :data:`SUCCESSOR_MAX_LAG_NS` after its expiry (anything else is a
    capture gap). Order-independent: visited by ``(slot, created, expiry)``."""
    order = sorted(
        range(len(insts)), key=lambda k: (insts[k].slot, insts[k].created_ns, insts[k].expiry_ns)
    )
    for a, b in itertools.pairwise(order):
        prev = insts[a]
        nxt = insts[b]
        if (
            prev.slot == nxt.slot
            and nxt.expiry_ns > prev.expiry_ns
            and nxt.created_ns >= prev.settle_open_ns
            and nxt.created_ns <= prev.expiry_ns + SUCCESSOR_MAX_LAG_NS
            and nxt.strike_1e6 > 0
        ):
            prev.next_strike_1e6 = nxt.strike_1e6
