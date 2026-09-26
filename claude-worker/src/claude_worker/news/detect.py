# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Class-A structural detectors — instrument sets, venue status, calendars (spec §8).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

A class-A source publishes STATE, not prose: the set of instruments a venue
lists, the maintenance windows it has scheduled, the dates a central bank
has published. This module turns two consecutive snapshots of that state
into the small number of facts a trader would act on — a listing, a
delisting, an expiry, a maintenance window — and records each one exactly
once.

Three properties hold everything together:

* **A transition needs a baseline.** ``prev is None`` emits NOTHING (spec
  §8.1). The first poll after an install, or after ``prune`` has dropped a
  source's history, must not read the whole instrument set as 400 new
  listings.
* **The same fact is recorded once.** ``events.dedupe_key`` is
  ``kind|venue|instrument|at_ts``, so a detector is safe to re-run over one
  snapshot pair: a second insert is a no-op that returns ``None``. Status
  windows are keyed on their ``begin``, which is what makes a window ONE
  event however many times it is polled (spec §8.2). An open-ended outage
  (Deribit ``locked``, Kraken not ``online``) carries no end to key on, so
  it is instead STICKY: it opens once and is closed by an UPDATE when the
  venue reports clear, never by a second row.
* **Nothing here decides anything.** The consumers are a proposals file the
  operator applies by hand (Q10: this lane never rewrites ``universe.toml``
  or ``xsd-table.tsv``), a calendar file nothing trades on, and the
  dashboard. The one urgent output is a red alert, which is text.

Failure doctrine, as everywhere in this lane: a reshaped wire makes the
detectors QUIET, not broken. A body that will not decode, a row whose
fields have moved, a file that cannot be written — each is a counted no-op
and the cycle continues. The engine is never reached from here.
"""

import dataclasses
import datetime
import json
import pathlib
import tomllib
import typing
import urllib.request
import zoneinfo

import claude_worker.features
import claude_worker.iv_digest
import claude_worker.news
import claude_worker.news.sources
import claude_worker.news.store

# ---------------------------------------------------------------- constants

#: The closed event vocabulary of this module (``events.kind``).
EVENT_LISTING_PENDING: str = "listing_pending"
EVENT_LISTING_LIVE: str = "listing_live"
EVENT_DELISTING: str = "delisting"
EVENT_EXPIRY: str = "expiry"
EVENT_MAINTENANCE_PLANNED: str = "maintenance_planned"
EVENT_MAINTENANCE_LIVE: str = "maintenance_live"
EVENT_KINDS: tuple[str, ...] = (
    EVENT_LISTING_PENDING,
    EVENT_LISTING_LIVE,
    EVENT_DELISTING,
    EVENT_EXPIRY,
    EVENT_MAINTENANCE_PLANNED,
    EVENT_MAINTENANCE_LIVE,
)

#: An expiry is announced this far ahead (spec §8.1) — far enough for the
#: operator to roll a table, near enough that a perpetual's year-2100
#: sentinel never qualifies.
EXPIRY_HORIZON_S: int = 72 * 3_600
#: Binance dates every PERPETUAL ``4133404800000`` (year 2100) and Deribit
#: ``32503708800000`` (year 3000). A delivery further out than half a year
#: is such a sentinel, not a schedule, so a delisting is dated NOW rather
#: than at a date no one will see.
SENTINEL_LEAD_S: int = 180 * 86_400
#: ``calendar.json`` covers the next 7 days (spec §8.3).
CALENDAR_HORIZON_S: int = 7 * 86_400
#: The ALERT file is removed when no red alert has been raised for this long
#: (spec §4.4).
ALERT_TTL_S: int = 6 * 3_600
MS_PER_S: int = 1_000
SECONDS_PER_DAY: int = 86_400

#: Which slots trade on which venues — the dashboard red rule and the (off)
#: maintenance action read it (spec §8.2). Slot 4 (ai-exec) and slot 5 (the
#: ruleset VM) carry no venue of their own: they emit into whatever the
#: operator's tables name, so a venue outage is not automatically theirs.
MEMBER_VENUES: dict[int, tuple[str, ...]] = {
    0: ("polymarket",),
    1: ("deribit",),
    2: ("binance",),
    3: ("hyperliquid",),
    4: (),
    5: (),
    6: ("binance",),
}

#: The engine gauge that says which slots are live (``/metrics``).
ENABLED_MASK_GAUGE: str = "engine_strategy_enabled_mask"
METRICS_TIMEOUT_S: float = 2.0

#: ``~/multivenue/xsd-table.tsv`` — read, never written, by this lane.
XSD_TABLE_FILE: str = "xsd-table.tsv"

#: ``calendar.json`` event kinds (spec §4.4). The three schedule kinds come
#: from the operator's ``[calendar] fixed_daily`` and are passed through.
CAL_FOMC_STATEMENT: str = "fomc_statement"
CAL_FOMC_MINUTES: str = "fomc_minutes"
CAL_FED_SPEECH: str = "fed_speech"
CAL_BLS_RELEASE: str = "bls_release"
CAL_DERIBIT_EXPIRY: str = "deribit_expiry"
CAL_VENUE_MAINTENANCE: str = "venue_maintenance"

#: Alert kinds this module raises (the ALERT file's second field).
ALERT_XSD_DROP: str = "xsd_drop"
ALERT_VENUE_MAINTENANCE: str = "venue_maintenance"

#: The Federal Reserve publishes its calendar in US Eastern wall-clock time.
FED_TZ: str = "America/New_York"

#: Source kinds each detector owns.
INSTRUMENT_KINDS: tuple[str, ...] = (
    "instruments-bn-usdm",
    "instruments-okx",
    "instruments-deribit",
    "instruments-coinbase",
)
STATUS_KINDS: tuple[str, ...] = (
    "status-okx",
    "status-deribit",
    "status-bybit",
    "status-kraken",
)
CALENDAR_KIND: str = "calendar-fed"

#: The venue a class-A kind speaks for, when the stanza does not say.
_VENUE_OF_KIND: dict[str, str] = {
    "instruments-bn-usdm": "binance",
    "instruments-okx": "okx",
    "instruments-deribit": "deribit",
    "instruments-coinbase": "coinbase",
    "status-okx": "okx",
    "status-deribit": "deribit",
    "status-bybit": "bybit",
    "status-kraken": "kraken",
}

#: Field ORDER inside a keyed snapshot body, mirroring the tuples
#: ``sources.py`` builds each row with. A drift here is silent, so
#: ``test_news_detect`` pins every one of these against that module.
_BN_STATUS: int = 0
_BN_CONTRACT: int = 1
_BN_ONBOARD: int = 2
_BN_DELIVERY: int = 3
_OKX_TYPE: int = 0
_OKX_STATE: int = 1
_OKX_LIST: int = 2
_OKX_EXP: int = 3
_DERIBIT_KIND: int = 0
_DERIBIT_TYPE: int = 1
_DERIBIT_ACTIVE: int = 2
_DERIBIT_CREATED: int = 3
_DERIBIT_EXPIRES: int = 4
_CB_STATUS: int = 0
_CB_DISABLED: int = 1
_WINDOW_STATE: int = 0
_WINDOW_BEGIN: int = 1
_WINDOW_END: int = 2

#: Venue state vocabularies. Anything OUTSIDE a venue's known words is
#: neither trading nor dead, so an unrecognised state is a no-op rather
#: than a fabricated delisting.
_BN_TRADING: str = "TRADING"
_BN_PENDING: str = "PENDING_TRADING"
_BN_DEAD: tuple[str, ...] = (
    "PRE_SETTLE",
    "SETTLING",
    "CLOSE",
    "PRE_DELIVERING",
    "DELIVERING",
    "DELIVERED",
)
_BN_PERPETUAL: str = "PERPETUAL"
_BN_SECTION_PERP: str = "usdm"
_BN_SECTION_DATED: str = "usdm_dated"
_OKX_LIVE: str = "live"
_OKX_PREOPEN: str = "preopen"
_OKX_DEAD: tuple[str, ...] = ("suspend", "expired")
_CB_ONLINE: str = "online"
_CB_DEAD: tuple[str, ...] = ("delisted", "offline")
_SECTION_INSTRUMENTS: str = "instruments"
#: Deribit's `kind` for an option, and the shape of an option NAME
#: (`BTC-23SEP26-84500-C`). An option reaches the engine through
#: `[deribit] options_underlyings` and a DISCOVERED chain, never by
#: naming a strike — and Deribit adds strikes continuously as spot moves,
#: so proposing each one would hand the operator a file to apply by hand
#: that grows every hour. The listing EVENT is still recorded: the venue
#: really did list something, and the expiry detector needs the row.
_DERIBIT_OPTION_KIND: str = "option"
_OPTION_NAME_PARTS: int = 4
_OPTION_SUFFIXES: tuple[str, ...] = ("C", "P")

#: OKX publishes a maintenance state per window; Bybit publishes one that
#: is ``completed`` once the window is over (spec §8.2).
_OKX_PLANNED_STATES: tuple[str, ...] = ("scheduled", "ongoing", "pre_open")
_BYBIT_DONE_STATE: str = "completed"
_DERIBIT_CLEAR: str = "false"
_KRAKEN_ONLINE: str = "online"

_HHMM_PARTS: int = 2
_TITLE_CAP: int = 160


# -------------------------------------------------------------------- types


class Event(typing.NamedTuple):
    """One structural fact, ready for ``store.insert_event``.

    ``section`` and ``value`` are the ``universe.toml`` coordinates the
    instrument would occupy — both empty for a venue with no ingress
    (coinbase) and for every status event, which is how the proposal writer
    knows an event is not a universe candidate without re-deriving it.

    ``close`` marks the one event that is not an insert: a venue reporting
    itself clear, which CLOSES the open outage row by setting ``until_ts``
    (spec §8.2, "never a second row").
    """

    kind: str
    venue: str
    at_ts: int
    source: str
    detail: str
    instrument: str = ""
    descriptor: str = ""
    until_ts: int = 0
    section: str = ""
    value: str = ""
    close: bool = False


class Alert(typing.NamedTuple):
    """A red alert: text, not an action. ``actions.py`` (N3) owns every
    path that reaches a venue; this lane's urgent output is one line."""

    kind: str
    text: str
    at_ts: int


class _Inst(typing.NamedTuple):
    """One instrument's state, normalised across four venue vocabularies.

    ``trading``/``pending``/``dead`` are mutually exclusive and may ALL be
    false — an unrecognised state, which produces no event by design.
    """

    trading: bool
    pending: bool
    dead: bool
    start_ts: int
    end_ts: int
    descriptor: str
    section: str
    value: str
    detail: str


class _Window(typing.NamedTuple):
    """One published maintenance window."""

    state: str
    begin_ts: int
    end_ts: int
    detail: str


@dataclasses.dataclass(frozen=True, slots=True)
class Context:
    """Everything the detectors write to, resolved once per lane run.

    Absent (``None`` at the call site) the detectors still run and still
    record events — only the FILE outputs are skipped. That is what lets a
    test exercise a transition without an operator's directory, and what
    keeps `serve` (§13) free to consume events without owning files.
    """

    news_dir: pathlib.Path
    xsd_table_path: pathlib.Path
    manifest: frozenset[str] = frozenset()
    metrics_url: str = ""
    gauge: typing.Callable[[str, str], int | None] | None = None

    def file(self, name: str) -> pathlib.Path:
        return self.news_dir / name


@dataclasses.dataclass(slots=True)
class Outcome:
    """What one class-A observation did."""

    snapshot_stored: bool = False
    events: int = 0
    closed: int = 0
    proposals: int = 0
    xsd_rows: int = 0
    alerts: list[Alert] = dataclasses.field(default_factory=list)


@dataclasses.dataclass(slots=True)
class FinalOutcome:
    """What the once-per-cycle tail did."""

    calendar_events: int = 0
    alerts: int = 0


# ------------------------------------------------------------------ helpers


def iso(ts: int) -> str:
    """UTC seconds as the ISO stamp every file output of this lane writes."""
    return datetime.datetime.fromtimestamp(ts, tz=datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def _epoch_s(value: object) -> int:
    """A venue's millisecond stamp as UTC seconds; 0 when absent or junk.

    Every venue here publishes milliseconds, three of them as STRINGS and
    one of those (OKX) as an empty string for "no such time".
    """
    if value is None or isinstance(value, bool):
        return 0
    if isinstance(value, (int, float)):
        ms = int(value)
    else:
        raw = str(value).strip()
        if not raw or not raw.isdigit():
            return 0
        ms = int(raw)
    return ms // MS_PER_S if ms > 0 else 0


def _truthy(value: object) -> bool:
    """JSON ``true`` and the string forms venues send instead of it."""
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("true", "1", "yes")


def _text(value: object) -> str:
    return "" if value is None else str(value).strip()


def _field(values: object, index: int) -> object:
    """``values[index]`` when the row still has that field. A venue that
    drops or reorders a field makes this ``None``, which every adapter
    reads as "unknown" — never as a transition."""
    if not isinstance(values, list) or index >= len(values):
        return None
    return values[index]


def _body(snapshot: claude_worker.news.sources.Snapshot | None) -> dict[str, object]:
    """A snapshot's keyed body. Unreadable JSON is an EMPTY body, which the
    detectors read as "no information", never as "everything vanished" —
    the absent-side rules only fire against a body that decoded."""
    if snapshot is None:
        return {}
    try:
        doc = json.loads(snapshot.body)
    except ValueError:
        return {}
    return doc if isinstance(doc, dict) else {}


def snapshot_from_row(
    row: typing.Mapping[str, object] | None,
) -> claude_worker.news.sources.Snapshot | None:
    """A ``snapshots`` row as the [`sources.Snapshot`] the detectors take."""
    if row is None:
        return None
    return claude_worker.news.sources.Snapshot(
        source=str(row["source"]),
        taken_ts=int(typing.cast(int, row["taken_ts"])),
        sha256=str(row["sha256"]),
        count=int(typing.cast(int, row["count"])),
        body=str(row["body"]),
    )


def venue_of(source: claude_worker.news.sources.Source) -> str:
    """The venue an event belongs to: the stanza's own ``venue`` first (the
    registry validates it against ``sources.VENUES``, which is what
    [`MEMBER_VENUES`] is keyed on), the kind's venue as the fallback."""
    return source.venue or _VENUE_OF_KIND.get(source.kind, "")


# ------------------------------------------------- §8.1 instrument-set diff


def _inst_bn(ident: str, values: object, now_ts: int) -> _Inst:
    status = _text(_field(values, _BN_STATUS))
    contract = _text(_field(values, _BN_CONTRACT))
    start = _epoch_s(_field(values, _BN_ONBOARD))
    trading = status == _BN_TRADING
    pending = not trading and (status == _BN_PENDING or start > now_ts)
    dead = not trading and not pending and status in _BN_DEAD
    value = ident.lower()
    section = _BN_SECTION_PERP if contract == _BN_PERPETUAL else _BN_SECTION_DATED
    return _Inst(
        trading=trading,
        pending=pending,
        dead=dead,
        start_ts=start,
        end_ts=_epoch_s(_field(values, _BN_DELIVERY)),
        descriptor=f"binance-usdm:{value}",
        section=section,
        value=value,
        detail=f"{status} {contract}".strip(),
    )


def _inst_okx(ident: str, values: object, now_ts: int) -> _Inst:
    state = _text(_field(values, _OKX_STATE))
    inst_type = _text(_field(values, _OKX_TYPE))
    start = _epoch_s(_field(values, _OKX_LIST))
    trading = state == _OKX_LIVE
    pending = not trading and (state == _OKX_PREOPEN or start > now_ts)
    dead = not trading and not pending and state in _OKX_DEAD
    return _Inst(
        trading=trading,
        pending=pending,
        dead=dead,
        start_ts=start,
        end_ts=_epoch_s(_field(values, _OKX_EXP)),
        descriptor=f"okx:{ident}",
        section=_SECTION_INSTRUMENTS,
        value=ident,
        detail=f"{state} {inst_type}".strip(),
    )


def _inst_deribit(ident: str, values: object, now_ts: int) -> _Inst:
    active = _truthy(_field(values, _DERIBIT_ACTIVE))
    start = _epoch_s(_field(values, _DERIBIT_CREATED))
    pending = not active and start > now_ts
    kind = _text(_field(values, _DERIBIT_KIND))
    inst_type = _text(_field(values, _DERIBIT_TYPE))
    option = kind == _DERIBIT_OPTION_KIND
    return _Inst(
        trading=active,
        pending=pending,
        dead=not active and not pending,
        start_ts=start,
        end_ts=_epoch_s(_field(values, _DERIBIT_EXPIRES)),
        descriptor=f"deribit:{ident}",
        # An option carries no universe coordinates: the event is real and
        # is recorded, but a strike is never proposed for `instruments`.
        section="" if option else _SECTION_INSTRUMENTS,
        value="" if option else ident,
        detail=f"{'active' if active else 'inactive'} {kind} {inst_type}".strip(),
    )


def _inst_coinbase(ident: str, values: object, now_ts: int) -> _Inst:
    del now_ts
    status = _text(_field(values, _CB_STATUS))
    disabled = _truthy(_field(values, _CB_DISABLED))
    trading = status == _CB_ONLINE and not disabled
    # Coinbase publishes no listing time and no pending state, so an
    # appearing row that is not trading is a dead row the venue happens to
    # keep listing — never a pending listing (measured: the live products
    # payload carries hundreds of `delisted` rows).
    return _Inst(
        trading=trading,
        pending=False,
        dead=not trading and (disabled or status in _CB_DEAD),
        start_ts=0,
        end_ts=0,
        descriptor="",
        section="",
        value="",
        detail=f"{status}{' disabled' if disabled else ''}",
    )


_INST_ADAPTERS: dict[str, typing.Callable[[str, object, int], _Inst]] = {
    "instruments-bn-usdm": _inst_bn,
    "instruments-okx": _inst_okx,
    "instruments-deribit": _inst_deribit,
    "instruments-coinbase": _inst_coinbase,
}


def _instruments(kind: str, body: dict[str, object], now_ts: int) -> dict[str, _Inst]:
    adapter = _INST_ADAPTERS.get(kind)
    if adapter is None:
        return {}
    out: dict[str, _Inst] = {}
    keys = sorted(body)
    for i in range(len(keys)):
        out[keys[i]] = adapter(keys[i], body[keys[i]], now_ts)
    return out


def _candidate(venue: str, source: str, ident: str, row: _Inst) -> Event:
    """The event ANY transition on this instrument would be, minus the kind
    and the instant — the caller fills those two with ``_replace``, so the
    nine fields that describe the instrument are written once."""
    return Event(
        kind="",
        venue=venue,
        at_ts=0,
        source=source,
        detail=row.detail,
        instrument=ident,
        descriptor=row.descriptor,
        section=row.section,
        value=row.value,
    )


def diff_instruments(
    source: claude_worker.news.sources.Source,
    prev: claude_worker.news.sources.Snapshot | None,
    cur: claude_worker.news.sources.Snapshot,
    now_ts: int,
) -> list[Event]:
    """The §8.1 transition table for one venue's instrument set.

    ``prev is None`` emits NOTHING: with no baseline every instrument would
    read as a new listing. Ordering is deterministic (identifiers sorted)
    so a test and a live cycle see the same list.
    """
    if prev is None or source.kind not in _INST_ADAPTERS:
        return []
    before = _instruments(source.kind, _body(prev), now_ts)
    after = _instruments(source.kind, _body(cur), now_ts)
    if not after:
        return []
    venue = venue_of(source)
    out: list[Event] = []
    keys = sorted(after)
    for i in range(len(keys)):
        ident = keys[i]
        row = after[ident]
        was = before.get(ident)
        base = _candidate(venue, source.name, ident, row)
        if was is None:
            if row.pending:
                out.append(
                    base._replace(
                        kind=EVENT_LISTING_PENDING, at_ts=row.start_ts or now_ts
                    )
                )
            elif row.trading:
                out.append(base._replace(kind=EVENT_LISTING_LIVE, at_ts=now_ts))
        elif row.trading and not was.trading:
            out.append(base._replace(kind=EVENT_LISTING_LIVE, at_ts=now_ts))
        elif was.trading and row.dead:
            scheduled = now_ts < row.end_ts <= now_ts + SENTINEL_LEAD_S
            out.append(
                base._replace(
                    kind=EVENT_DELISTING, at_ts=row.end_ts if scheduled else now_ts
                )
            )
        if now_ts <= row.end_ts <= now_ts + EXPIRY_HORIZON_S:
            out.append(base._replace(kind=EVENT_EXPIRY, at_ts=row.end_ts))
    out.extend(_gone(before, after, venue, source.name, now_ts))
    return out


def _gone(
    before: dict[str, _Inst], after: dict[str, _Inst], venue: str, source: str, now_ts: int
) -> list[Event]:
    """Instruments that were trading and are no longer listed at all."""
    out: list[Event] = []
    keys = sorted(before)
    for i in range(len(keys)):
        ident = keys[i]
        if ident in after or not before[ident].trading:
            continue
        row = before[ident]
        out.append(
            Event(
                kind=EVENT_DELISTING,
                venue=venue,
                at_ts=now_ts,
                source=source,
                detail="absent from the instrument set",
                instrument=ident,
                descriptor=row.descriptor,
                section=row.section,
                value=row.value,
            )
        )
    return out


# ------------------------------------------------------ §8.2 venue status


def _windows(body: dict[str, object]) -> list[_Window]:
    """The published maintenance windows of an OKX/Bybit status payload.

    The snapshot is already keyed on ``title|begin`` (``sources.py``), which
    is why a window survives a venue re-ordering its list.
    """
    out: list[_Window] = []
    keys = sorted(body)
    for i in range(len(keys)):
        values = body[keys[i]]
        begin = _epoch_s(_field(values, _WINDOW_BEGIN))
        end = _epoch_s(_field(values, _WINDOW_END))
        if begin <= 0 or end <= 0:
            continue
        title = keys[i].split("|", 1)[0][:_TITLE_CAP]
        state = _text(_field(values, _WINDOW_STATE)).lower()
        out.append(_Window(state=state, begin_ts=begin, end_ts=end, detail=title))
    return out


def _window_is_planned(kind: str, state: str) -> bool:
    if kind == "status-bybit":
        return state != _BYBIT_DONE_STATE
    return state in _OKX_PLANNED_STATES


def _window_events(
    source: claude_worker.news.sources.Source,
    cur: claude_worker.news.sources.Snapshot,
    now_ts: int,
) -> list[Event]:
    venue = venue_of(source)
    out: list[Event] = []
    windows = _windows(_body(cur))
    for i in range(len(windows)):
        window = windows[i]
        if not _window_is_planned(source.kind, window.state):
            continue
        detail = f"{window.detail} [{window.state}]" if window.state else window.detail
        # at_ts = begin re-keys the window: `kind|venue||begin` is the
        # dedupe key, so one window is one row however often it is polled.
        out.append(
            Event(
                kind=EVENT_MAINTENANCE_PLANNED,
                venue=venue,
                at_ts=window.begin_ts,
                source=source.name,
                detail=detail,
                until_ts=window.end_ts,
            )
        )
        if window.begin_ts <= now_ts <= window.end_ts:
            out.append(
                Event(
                    kind=EVENT_MAINTENANCE_LIVE,
                    venue=venue,
                    at_ts=window.begin_ts,
                    source=source.name,
                    detail=detail,
                    until_ts=window.end_ts,
                )
            )
    return out


def _outage_flag(kind: str, body: dict[str, object]) -> tuple[bool, str]:
    """``(in maintenance, detail)`` for the two venues that publish a FLAG
    instead of a window. An empty body (an unreadable snapshot) is not an
    outage — silence must never invent one."""
    if not body:
        return False, ""
    if kind == "status-deribit":
        locked = _text(body.get("locked")).lower()
        if not locked or locked == _DERIBIT_CLEAR:
            return False, ""
        indices = body.get("locked_indices") or []
        currencies = body.get("locked_currencies") or []
        return True, f"locked={locked} indices={indices} currencies={currencies}"[:_TITLE_CAP]
    status = _text(body.get("status")).lower()
    if not status or status == _KRAKEN_ONLINE:
        return False, ""
    return True, f"status={status}"


def _outage_events(
    source: claude_worker.news.sources.Source,
    cur: claude_worker.news.sources.Snapshot,
    now_ts: int,
) -> list[Event]:
    """One open-ended outage, opened once and closed by the venue's own
    all-clear.

    Both edges are IDEMPOTENT on purpose: the open is skipped when a row is
    already open (there is no ``begin`` to key on, so the dedupe key cannot
    do it), and the clear closes whatever is open and is a no-op otherwise.
    A missed poll, a crash mid-outage or a restart therefore cannot leave
    an outage open forever, which is the failure that would matter.
    """
    locked, detail = _outage_flag(source.kind, _body(cur))
    venue = venue_of(source)
    if locked:
        return [
            Event(
                kind=EVENT_MAINTENANCE_LIVE,
                venue=venue,
                at_ts=now_ts,
                source=source.name,
                detail=detail,
            )
        ]
    return [
        Event(
            kind=EVENT_MAINTENANCE_LIVE,
            venue=venue,
            at_ts=now_ts,
            source=source.name,
            detail="clear",
            close=True,
        )
    ]


def detect_status(
    source: claude_worker.news.sources.Source,
    prev: claude_worker.news.sources.Snapshot | None,
    cur: claude_worker.news.sources.Snapshot,
    now_ts: int,
) -> list[Event]:
    """The §8.2 venue-status detectors.

    Unlike §8.1 these do not need a baseline: a published window and a
    ``locked`` flag are STATES, not transitions, and a first poll that finds
    the venue down should say so. ``prev`` is accepted (and unused for the
    window venues) to keep one detector signature across the module.
    """
    del prev
    if source.kind in ("status-okx", "status-bybit"):
        return _window_events(source, cur, now_ts)
    if source.kind in ("status-deribit", "status-kraken"):
        return _outage_events(source, cur, now_ts)
    return []


# ----------------------------------------------------------- applying them


def _is_sticky(event: Event) -> bool:
    """An open-ended live outage: the one event whose repeat cannot be
    caught by the dedupe key, because its ``at_ts`` is "now"."""
    return event.kind == EVENT_MAINTENANCE_LIVE and event.until_ts == 0


def apply_events(
    store: claude_worker.news.store.Store, events: list[Event], now_ts: int
) -> tuple[list[tuple[int, Event]], int]:
    """Record every event once. Returns ``(stored, closed)`` — ``stored``
    carries the new row ids, which the proposals files cite."""
    stored: list[tuple[int, Event]] = []
    closed = 0
    for i in range(len(events)):
        event = events[i]
        if event.close:
            closed += _close_open(store, event, now_ts)
            continue
        if _is_sticky(event) and store.open_events(event.kind, event.venue, event.instrument):
            continue
        event_id = store.insert_event(
            kind=event.kind,
            venue=event.venue,
            at_ts=event.at_ts,
            source=event.source,
            detail=event.detail,
            created_ts=now_ts,
            descriptor=event.descriptor,
            instrument=event.instrument,
            until_ts=event.until_ts,
        )
        if event_id is not None:
            stored.append((event_id, event))
    return stored, closed


def _close_open(store: claude_worker.news.store.Store, event: Event, now_ts: int) -> int:
    rows = store.open_events(event.kind, event.venue, event.instrument)
    closed = 0
    for i in range(len(rows)):
        if store.close_event(int(typing.cast(int, rows[i]["id"])), now_ts):
            closed += 1
    return closed


# ------------------------------------------------- §4.4 proposals (Q10)


_UNIVERSE_HEADER: str = (
    "# universe-proposals.toml — NEWS lane (spec §4.4). APPEND-ONLY.\n"
    "# The operator applies a stanza to ~/multivenue/universe.toml BY HAND\n"
    "# and flips `applied`; this lane never rewrites universe.toml (Q10),\n"
    "# because SymbolIds are file-order ordinals and a reorder renumbers\n"
    "# every symbol the worker map and the engine agreed on.\n"
)
_XSD_HEADER: str = "# ts\taction\ttarget\tbefore_ts\treason\n"
_XSD_DROP: str = "drop"
#: The two events that can retire an xsd row.
_XSD_TRIGGERS: tuple[str, ...] = (EVENT_DELISTING, EVENT_EXPIRY)
_XSD_VERB: dict[str, str] = {EVENT_DELISTING: "delists", EVENT_EXPIRY: "expires"}
#: The two events that can grow the universe.
_UNIVERSE_TRIGGERS: tuple[str, ...] = (EVENT_LISTING_PENDING, EVENT_LISTING_LIVE)
_TSV_FIELDS: int = 5


def _read_text(path: pathlib.Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        return ""


def _safe_write(path: pathlib.Path, text: str) -> bool:
    """Atomic write, never fatal: a full or read-only disk must not stop a
    cycle that has already recorded its events."""
    try:
        claude_worker.news.write_atomic(path, text)
    except OSError:
        return False
    return True


def proposed_descriptors(text: str) -> frozenset[str]:
    """Descriptors already carried by ``universe-proposals.toml``.

    TOML first; a hand-edited file that no longer parses falls back to a
    line scan, because the one thing worse than a malformed proposals file
    is appending a duplicate to it every 60 s.
    """
    out: set[str] = set()
    try:
        doc = tomllib.loads(text)
    except (ValueError, TypeError):
        doc = {}
    rows = doc.get("proposal")
    if isinstance(rows, list):
        for i in range(len(rows)):
            row = rows[i]
            if isinstance(row, dict) and row.get("descriptor"):
                out.add(str(row["descriptor"]))
        return frozenset(out)
    lines = text.splitlines()
    for i in range(len(lines)):
        line = lines[i].strip()
        if line.startswith("descriptor"):
            _, _, value = line.partition("=")
            out.add(value.strip().strip('"'))
    return frozenset(out)


def _stanza(event: Event, event_id: int, now_ts: int) -> str:
    return (
        "\n[[proposal]]\n"
        f'ts = "{iso(now_ts)}"\n'
        f'venue = "{event.venue}"\n'
        f'section = "{event.section}"\n'
        f'value = "{event.value}"\n'
        f'descriptor = "{event.descriptor}"\n'
        f"event_id = {event_id}\n"
        "applied = 0\n"
    )


def write_universe_proposals(
    ctx: Context, stored: list[tuple[int, Event]], now_ts: int
) -> int:
    """Append one stanza per newly listed instrument the engine does not
    already carry (spec §4.4).

    Skipped: a venue with no ingress (no ``section``), a descriptor already
    in the newest run's instrument manifest, and one already proposed.
    """
    path = ctx.file(claude_worker.news.UNIVERSE_PROPOSALS_FILE)
    text = _read_text(path)
    seen = set(proposed_descriptors(text))
    additions: list[str] = []
    for i in range(len(stored)):
        event_id, event = stored[i]
        if event.kind not in _UNIVERSE_TRIGGERS or not event.section or not event.descriptor:
            continue
        if event.descriptor in ctx.manifest or event.descriptor in seen:
            continue
        seen.add(event.descriptor)
        additions.append(_stanza(event, event_id, now_ts))
    if not additions:
        return 0
    body = text if text else _UNIVERSE_HEADER
    if not _safe_write(path, body + "".join(additions)):
        return 0
    return len(additions)


def is_option_name(value: str) -> bool:
    """A Deribit option, by the shape of its name: four dash-separated
    parts ending in C or P (`BTC-23SEP26-84500-C`). A future is two
    (`BTC-20SEP26`) and a perpetual is `BTC-PERPETUAL`.

    Used where only the descriptor is in hand — the instrument-set
    detector reads the venue's own `kind` field instead, which is better
    evidence when it is available.
    """
    parts = value.split("-")
    return len(parts) == _OPTION_NAME_PARTS and parts[-1].upper() in _OPTION_SUFFIXES


def proposal_from_descriptor(descriptor: str, at_ts: int = 0) -> Event | None:
    """The `universe.toml` coordinates a descriptor names, or ``None``.

    The structural detectors carry section and value on the event because
    they read them off the venue's own row; an ANALYST only ever names a
    descriptor, so this recovers the rest from its shape. A prefix with no
    ingress (coinbase, anything unknown) is ``None`` — a proposal nobody
    could apply is worse than none.
    """
    prefix, _, value = descriptor.partition(":")
    if not prefix or not value:
        return None
    if prefix == "binance-usdm":
        venue = "binance"
        # A dated USDM contract is `<base>_<yymmdd>`; the underscore is
        # exactly what separates the class from a perpetual.
        section = _BN_SECTION_DATED if "_" in value else _BN_SECTION_PERP
    elif prefix in ("okx", "deribit"):
        if prefix == "deribit" and is_option_name(value):
            return None
        venue = prefix
        section = _SECTION_INSTRUMENTS
    else:
        return None
    return Event(
        kind=EVENT_LISTING_LIVE,
        venue=venue,
        at_ts=at_ts,
        source="assessment",
        detail=f"proposed: {descriptor}",
        instrument=value,
        descriptor=descriptor,
        section=section,
        value=value,
    )


def xsd_table_descriptors(path: pathlib.Path) -> frozenset[str]:
    """Targets and partners of ``~/multivenue/xsd-table.tsv``.

    The grammar's owner parses it (``claude_worker.xsd_author``), imported
    here rather than at module scope because it pulls numpy and this lane
    runs every 60 s under launchd.
    """
    text = _read_text(path)
    if not text:
        return frozenset()
    import claude_worker.xsd_author  # noqa: PLC0415 — numpy-heavy, rarely needed

    return frozenset(claude_worker.xsd_author.table_descriptors(text))


def _xsd_rows_present(text: str) -> frozenset[tuple[str, str, str]]:
    out: set[tuple[str, str, str]] = set()
    lines = text.splitlines()
    for i in range(len(lines)):
        line = lines[i].strip()
        if not line or line.startswith("#"):
            continue
        fields = line.split("\t")
        if len(fields) < _TSV_FIELDS:
            continue
        out.add((fields[1].strip(), fields[2].strip(), fields[3].strip()))
    return frozenset(out)


def write_xsd_proposals(
    ctx: Context, stored: list[tuple[int, Event]], now_ts: int
) -> tuple[int, list[Alert]]:
    """A ``drop`` row (and a red alert) for every delisting or expiry that
    hits a descriptor the live xsd table trades (spec §8.1).

    Only members of the table are proposed: a delisting on some instrument
    the strategy never names is news, not a table change.
    """
    members = xsd_table_descriptors(ctx.xsd_table_path)
    if not members:
        return 0, []
    path = ctx.file(claude_worker.news.XSD_PROPOSALS_FILE)
    text = _read_text(path)
    present = set(_xsd_rows_present(text))
    rows: list[str] = []
    alerts: list[Alert] = []
    for i in range(len(stored)):
        event = stored[i][1]
        if event.kind not in _XSD_TRIGGERS or event.descriptor not in members:
            continue
        key = (_XSD_DROP, event.descriptor, str(event.at_ts))
        if key in present:
            continue
        present.add(key)
        reason = f"{event.kind} on {event.venue} ({event.source})"
        rows.append(
            f"{iso(now_ts)}\t{_XSD_DROP}\t{event.descriptor}\t{event.at_ts}\t{reason}\n"
        )
        verb = _XSD_VERB.get(event.kind, event.kind)
        alerts.append(
            Alert(
                kind=ALERT_XSD_DROP,
                text=f"xsd target {event.descriptor} {verb} at {iso(event.at_ts)}",
                at_ts=now_ts,
            )
        )
    if not rows:
        return 0, []
    body = text if text else _XSD_HEADER
    if not _safe_write(path, body + "".join(rows)):
        return 0, []
    return len(rows), alerts


# ------------------------------------------------------- §8.3 calendars


_YM_PARTS: int = 2
_MONTHS_PER_YEAR: int = 12
_HOURS_HALF_DAY: int = 12
_HOURS_PER_DAY: int = 24
_MINUTES_PER_HOUR: int = 60
_CALENDAR_DAYS: int = 7
_FED_MINUTES: str = "minutes"
_FED_FOMC: str = "fomc"
#: MEASURED 2026-09-19 against the live document (2585 events): the `type`
#: vocabulary is `Stat` 1059, `Speeches` 571, `events` 569, `FOMC` 135,
#: `Other` 104, `Testimony` 74, `Beige` 56, `Conferences` 10, `Board` 6.
#: Spec §8.3 enumerates what belongs on this calendar — FOMC statement,
#: minutes, press conference, speeches — so the statistical releases, the
#: Beige Book, the board meetings and the federal holidays are NOT entries
#: here. Calling them `fed_speech` would be a label a later tier reasons
#: over, and the recorded fixture (12 rows, all Speeches/Testimony) could
#: not have shown it.
_FED_TYPE_FOMC: str = "fomc"
_FED_SPEECH_TYPES: tuple[str, ...] = ("speeches", "testimony")


def _fed_zone() -> datetime.tzinfo:
    """US Eastern, or UTC when the host carries no tz database — a shifted
    hour is a far smaller lie than a missing calendar."""
    try:
        return zoneinfo.ZoneInfo(FED_TZ)
    except (KeyError, ValueError, OSError):
        return datetime.UTC


def _fed_clock(raw: str) -> tuple[int, int]:
    """``"10:05 a.m."`` -> ``(10, 5)``; an absent or odd time -> midnight,
    because the DATE is the information a 7-day calendar carries."""
    text = raw.lower().replace(".", "").replace(" ", "")
    pm = text.endswith("pm")
    text = text.removesuffix("am").removesuffix("pm")
    parts = text.split(":")
    if len(parts) != _YM_PARTS or not parts[0].isdigit() or not parts[1].isdigit():
        return 0, 0
    hour = int(parts[0]) % _HOURS_HALF_DAY
    if pm:
        hour += _HOURS_HALF_DAY
    minute = int(parts[1])
    if hour >= _HOURS_PER_DAY or minute >= _MINUTES_PER_HOUR:
        return 0, 0
    return hour, minute


def _fed_at(row: typing.Mapping[str, object]) -> int:
    """The instant a Fed calendar row happens.

    ``days`` is a RANGE for a two-day FOMC meeting (``"16-17"``) and the
    decision lands on its second day, so the last day is the one that
    matters to anything trading around it. A range that crosses a month
    end (``"31-1"``) ends in the NEXT month — the scheduled-events feed
    places a macro jump by this instant, so a month's error is not
    display-only.
    """
    month = _text(row.get("month"))
    days = _text(row.get("days"))
    if not month or not days:
        return 0
    first = days.split("-")[0].strip()
    day = days.split("-")[-1].strip()
    parts = month.split("-")
    if len(parts) != _YM_PARTS or not day.isdigit() or not first.isdigit():
        return 0
    if not parts[0].isdigit() or not parts[1].isdigit():
        return 0
    year, mon = int(parts[0]), int(parts[1])
    if int(day) < int(first):
        year, mon = (year + 1, 1) if mon == _MONTHS_PER_YEAR else (year, mon + 1)
    hour, minute = _fed_clock(_text(row.get("time")))
    try:
        local = datetime.datetime(year, mon, int(day), hour, minute, tzinfo=_fed_zone())
    except ValueError:
        return 0
    return int(local.timestamp())


def fed_timed(row: typing.Mapping[str, object]) -> bool:
    """Whether a Fed row states a time of day (a row without one is placed
    at midnight ET, which is a DATE, not an instant)."""
    return _fed_clock(_text(row.get("time"))) != (0, 0)


def fed_kind(row: typing.Mapping[str, object]) -> str:
    """Which §4.4 calendar kind a Fed row is, or ``""`` for a row that is
    not one of the four §8.3 enumerates.

    ``type`` decides, with the text as the fallback: the Fed has moved this
    document's shape before (the envelope key, measured the same day), so a
    vocabulary change must degrade to the old reading rather than to
    silence.
    """
    kind = _text(row.get("type")).lower()
    text = f"{kind} {_text(row.get('title'))}".lower()
    if _FED_MINUTES in text:
        return CAL_FOMC_MINUTES
    if kind == _FED_TYPE_FOMC:
        return CAL_FOMC_STATEMENT
    if kind in _FED_SPEECH_TYPES:
        return CAL_FED_SPEECH
    if _FED_FOMC in text:
        return CAL_FOMC_STATEMENT
    return ""


def _entry(kind: str, at_ts: int, source: str, detail: str, until_ts: int = 0) -> dict[str, object]:
    return {
        "kind": kind,
        "at_ts": at_ts,
        "until_ts": until_ts,
        "source": source,
        "detail": detail[:_TITLE_CAP],
    }


#: Carried however far out they are (operator ruling 2026-09-20). MEASURED
#: 2026-09-19: the next FOMC minutes are 18 days away and the next meeting
#: 39, both outside §8.3's 7-day horizon — so a calendar that showed only
#: the week ahead would never show the one scheduled event that moves vol,
#: and half the N2 gate asks for exactly that. Everything else still obeys
#: the horizon.
_PINNED_KINDS: tuple[str, ...] = (CAL_FOMC_MINUTES, CAL_FOMC_STATEMENT)


def _keep_earliest(
    pinned: dict[str, dict[str, object]], kind: str, entry: dict[str, object]
) -> None:
    best = pinned.get(kind)
    if best is None or int(typing.cast(int, entry["at_ts"])) < int(
        typing.cast(int, best["at_ts"])
    ):
        pinned[kind] = entry


def _pinned_entries(
    pinned: dict[str, dict[str, object]], in_window: set[str]
) -> list[dict[str, object]]:
    """The next of each pinned kind, unless the horizon already carries one
    — a duplicate FOMC row would read as two meetings."""
    out: list[dict[str, object]] = []
    kinds = sorted(pinned)
    for i in range(len(kinds)):
        if kinds[i] not in in_window:
            out.append(pinned[kinds[i]])
    return out


class FedRow(typing.NamedTuple):
    """One §8.3 row of a ``calendar-fed`` snapshot. ``timed`` is whether
    the row stated its time of day ([`fed_timed`])."""

    kind: str
    at_ts: int
    source: str
    title: str
    timed: bool


def fed_rows(
    registry: claude_worker.news.sources.Registry,
    store: claude_worker.news.store.Store,
) -> list[FedRow]:
    """Every row the newest ``calendar-fed`` snapshots carry that §8.3
    names (FOMC statement, minutes, speeches), past and future, in source
    then key order — the one reading both the 7-day calendar and the
    scheduled-events feed (O-HC8) take."""
    out: list[FedRow] = []
    sources = registry.sources
    for i in range(len(sources)):
        if sources[i].kind != CALENDAR_KIND:
            continue
        body = _body(snapshot_from_row(store.latest_snapshot(sources[i].name)))
        keys = sorted(body)
        for j in range(len(keys)):
            row = body[keys[j]]
            if not isinstance(row, dict):
                continue
            kind = fed_kind(row)
            at_ts = _fed_at(row)
            if kind and at_ts > 0:
                title = _text(row.get("title"))
                out.append(FedRow(kind, at_ts, sources[i].name, title, fed_timed(row)))
    return out


def _fed_entries(
    registry: claude_worker.news.sources.Registry,
    store: claude_worker.news.store.Store,
    now_ts: int,
    horizon: int,
) -> list[dict[str, object]]:
    out: list[dict[str, object]] = []
    pinned: dict[str, dict[str, object]] = {}
    in_window: set[str] = set()
    rows = fed_rows(registry, store)
    for i in range(len(rows)):
        row = rows[i]
        if row.at_ts < now_ts:
            continue
        entry = _entry(row.kind, row.at_ts, row.source, row.title)
        if row.at_ts <= horizon:
            out.append(entry)
            in_window.add(row.kind)
        elif row.kind in _PINNED_KINDS:
            _keep_earliest(pinned, row.kind, entry)
    out.extend(_pinned_entries(pinned, in_window))
    return out


def bls_rows(registry: claude_worker.news.sources.Registry) -> list[tuple[int, str]]:
    """``[calendar] bls_releases`` as ``(at_ts, detail)``, in file order —
    ISO stamps the operator maintains, because no keyless JSON publishes
    the BLS schedule.

    An entry is a stamp, optionally followed by a space and a label:
    ``"2026-10-14T12:30:00Z CPI (September)"``. The label becomes the
    ``detail``, so an entry reads as something rather than as a bare
    number. A stamp alone still works and is its own detail; a bare date
    lands at midnight UTC, so write the time to place it. An entry that is
    not a stamp is skipped.
    """
    out: list[tuple[int, str]] = []
    releases = registry.calendar.bls_releases
    for i in range(len(releases)):
        stamp, _, label = releases[i].strip().partition(" ")
        at_ts = parse_iso(stamp)
        if at_ts > 0:
            out.append((at_ts, label.strip() or stamp))
    return out


def _bls_entries(
    registry: claude_worker.news.sources.Registry, now_ts: int, horizon: int
) -> list[dict[str, object]]:
    out: list[dict[str, object]] = []
    rows = bls_rows(registry)
    for i in range(len(rows)):
        at_ts, detail = rows[i]
        if now_ts <= at_ts <= horizon:
            out.append(_entry(CAL_BLS_RELEASE, at_ts, "news.toml [calendar]", detail))
    return out


def _hhmm(raw: str) -> tuple[int, int]:
    parts = raw.strip().split(":")
    if len(parts) != _YM_PARTS or not parts[0].isdigit() or not parts[1].isdigit():
        return -1, -1
    hour = int(parts[0])
    minute = int(parts[1])
    if hour >= _HOURS_PER_DAY or minute >= _MINUTES_PER_HOUR:
        return -1, -1
    return hour, minute


def _fixed_daily_entries(
    registry: claude_worker.news.sources.Registry, now_ts: int, horizon: int
) -> list[dict[str, object]]:
    """``[calendar] fixed_daily`` expanded over the window — the engine's
    own schedule (restarts, the PM daily resolve, the HIP-4 settle) in UTC
    wall-clock, which is how ``daily-restart`` states it."""
    out: list[dict[str, object]] = []
    midnight = datetime.datetime.fromtimestamp(now_ts, tz=datetime.UTC).replace(
        hour=0, minute=0, second=0, microsecond=0
    )
    entries = registry.calendar.fixed_daily
    for i in range(len(entries)):
        hour, minute = _hhmm(entries[i].at)
        if hour < 0:
            continue
        for day in range(_CALENDAR_DAYS + 1):
            moment = (midnight + datetime.timedelta(days=day)).replace(hour=hour, minute=minute)
            at_ts = int(moment.timestamp())
            if now_ts <= at_ts <= horizon:
                out.append(
                    _entry(
                        entries[i].kind,
                        at_ts,
                        "news.toml [calendar]",
                        f"daily {entries[i].at}Z",
                    )
                )
    return out


#: MEASURED 2026-09-19: one poll of `deribit-instruments-btc-option` (972
#: live instruments) yields 190 expiry events in THREE instants — 62, 66
#: and 62 strikes sharing one expiry. Each is a true fact and each is its
#: own `events` row (the xsd-proposal path keys on the instrument), but a
#: 7-day calendar listing 190 strikes is a calendar no one reads. Same-
#: instant expiries therefore collapse into ONE entry that names the count.
_EXPIRY_SAMPLE: int = 3


def _expiry_entry(at_ts: int, source: str, names: list[str]) -> dict[str, object]:
    names.sort()
    if len(names) <= _EXPIRY_SAMPLE:
        return _entry(CAL_DERIBIT_EXPIRY, at_ts, source, ", ".join(names))
    shown = ", ".join(names[:_EXPIRY_SAMPLE])
    detail = f"{len(names)} instruments ({shown}, +{len(names) - _EXPIRY_SAMPLE} more)"
    return _entry(CAL_DERIBIT_EXPIRY, at_ts, source, detail)


def _event_entries(
    store: claude_worker.news.store.Store, now_ts: int, horizon: int
) -> list[dict[str, object]]:
    """Deribit expiries and open maintenance windows, from the events this
    lane already recorded — one query, so the calendar can never disagree
    with the dashboard's events tail."""
    out: list[dict[str, object]] = []
    expiries: dict[int, list[str]] = {}
    sources: dict[int, str] = {}
    rows = store.events_since(now_ts - CALENDAR_HORIZON_S)
    for i in range(len(rows)):
        row = rows[i]
        kind = str(row["kind"])
        at_ts = int(typing.cast(int, row["at_ts"]))
        until_ts = int(typing.cast(int, row["until_ts"]))
        if at_ts > horizon:
            continue
        if kind == EVENT_EXPIRY and str(row["venue"]) == "deribit" and at_ts >= now_ts:
            expiries.setdefault(at_ts, []).append(str(row["instrument"]))
            sources.setdefault(at_ts, str(row["source"]))
        elif kind == EVENT_MAINTENANCE_PLANNED and (until_ts == 0 or until_ts > now_ts):
            out.append(
                _entry(
                    CAL_VENUE_MAINTENANCE,
                    at_ts,
                    str(row["source"]),
                    f"{row['venue']}: {row['detail']}",
                    until_ts,
                )
            )
    instants = sorted(expiries)
    for i in range(len(instants)):
        out.append(_expiry_entry(instants[i], sources[instants[i]], expiries[instants[i]]))
    return out


def parse_iso(raw: str) -> int:
    """An ISO-8601 stamp (``Z`` or offset, or a bare date) as UTC seconds;
    0 when it is not one."""
    text = raw.strip()
    if not text:
        return 0
    try:
        stamp = datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))
    except ValueError:
        return 0
    if stamp.tzinfo is None:
        stamp = stamp.replace(tzinfo=datetime.UTC)
    return int(stamp.timestamp())


def build_calendar(
    registry: claude_worker.news.sources.Registry,
    store: claude_worker.news.store.Store,
    now_ts: int,
) -> dict[str, object]:
    """The next 7 days, sorted (spec §8.3). Nothing trades on it."""
    horizon = now_ts + CALENDAR_HORIZON_S
    rows: list[dict[str, object]] = []
    rows.extend(_fed_entries(registry, store, now_ts, horizon))
    rows.extend(_bls_entries(registry, now_ts, horizon))
    rows.extend(_fixed_daily_entries(registry, now_ts, horizon))
    rows.extend(_event_entries(store, now_ts, horizon))
    rows.sort(
        key=lambda row: (
            int(typing.cast(int, row["at_ts"])),
            str(row["kind"]),
            str(row["detail"]),
        )
    )
    return {
        "v": claude_worker.news.SCHEMA_VERSION,
        "generated_ts": now_ts,
        "events": rows,
    }


# ------------------------------------------------ §8.2 the red rule + ALERT


def _metrics_gauge(url: str, name: str, timeout_s: float = METRICS_TIMEOUT_S) -> int | None:
    """One gauge off the engine's ``/metrics`` page; ``None`` when the
    engine is unreachable or does not publish it (never an error).

    It lives here, not in ``regime.py``: that module is the vol channel's
    emitter and this lane adds nothing to it (spec §8.2).
    """
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as resp:  # loopback only
            text = resp.read().decode("utf-8", errors="replace")
    except (OSError, ValueError):
        return None
    lines = text.splitlines()
    for i in range(len(lines)):
        key, _, value = lines[i].partition(" ")
        if key != name:
            continue
        try:
            return int(value.strip())
        except ValueError:
            return None
    return None


def engine_mask(ctx: Context) -> int | None:
    """The live strategy enable mask, through the context's gauge so a test
    never reaches the operator's engine."""
    if not ctx.metrics_url:
        return None
    gauge = ctx.gauge if ctx.gauge is not None else _metrics_gauge
    return gauge(ctx.metrics_url, ENABLED_MASK_GAUGE)


def maintenance_alerts(
    store: claude_worker.news.store.Store, mask: int | None, now_ts: int
) -> list[Alert]:
    """Red when a slot the engine is RUNNING trades a venue that is in
    maintenance right now (spec §8.2).

    An unreachable engine (``mask is None``) raises nothing: an alert whose
    premise could not be read would be noise, and the maintenance event
    itself is already on the dashboard.
    """
    if mask is None or mask <= 0:
        return []
    rows = store.events_in_force(EVENT_MAINTENANCE_LIVE, now_ts)
    if not rows:
        return []
    detail_of: dict[str, str] = {}
    for i in range(len(rows)):
        detail_of[str(rows[i]["venue"])] = str(rows[i]["detail"])
    out: list[Alert] = []
    slots = sorted(MEMBER_VENUES)
    for i in range(len(slots)):
        slot = slots[i]
        if not (mask >> slot) & 1:
            continue
        venues = MEMBER_VENUES[slot]
        for j in range(len(venues)):
            detail = detail_of.get(venues[j])
            if detail is None:
                continue
            out.append(
                Alert(
                    kind=ALERT_VENUE_MAINTENANCE,
                    text=f"slot {slot} is enabled and {venues[j]} is in maintenance: {detail}",
                    at_ts=now_ts,
                )
            )
    return out


def write_alert(path: pathlib.Path, alerts: list[Alert], now_ts: int) -> int:
    """The newest red alert as one line (spec §4.4); the file is removed
    once no red alert has been raised for ``ALERT_TTL_S``."""
    if alerts:
        newest = alerts[len(alerts) - 1]
        _safe_write(path, f"{iso(now_ts)} {newest.kind} {newest.text}\n")
        return len(alerts)
    _expire_alert(path, now_ts)
    return 0


def _expire_alert(path: pathlib.Path, now_ts: int) -> None:
    if not path.is_file():
        return
    written = parse_iso(_read_text(path).split(" ", 1)[0])
    # A line this lane did not write (no parsable stamp) is left alone.
    if not written or now_ts - written < ALERT_TTL_S:
        return
    try:
        path.unlink()
    except OSError:
        return


# --------------------------------------------------------- the composition


def manifest_descriptors(replay_dir: pathlib.Path) -> frozenset[str]:
    """Descriptors the engine's newest run already carries. Best-effort,
    the ``filter.vocabulary_from`` contract: a missing run dir narrows the
    check, it never stops a cycle."""
    try:
        run_dir = claude_worker.features.latest_run_dir(replay_dir)
    except OSError:
        return frozenset()
    if run_dir is None:
        return frozenset()
    manifest = claude_worker.iv_digest.read_manifest(run_dir)
    if manifest is None:
        return frozenset()
    return frozenset(manifest[0].values())


def context_from(
    paths: claude_worker.news.NewsPaths,
    gauge: typing.Callable[[str, str], int | None] | None = None,
) -> Context:
    """[`Context`] from the lane's resolved paths, manifest read once."""
    return Context(
        news_dir=paths.news_dir,
        xsd_table_path=paths.multivenue_dir / XSD_TABLE_FILE,
        manifest=manifest_descriptors(paths.replay_dir),
        metrics_url=paths.metrics_url,
        gauge=gauge,
    )


def detectors_for(kind: str) -> bool:
    """Whether this source kind has a class-A detector at all."""
    return kind in _INST_ADAPTERS or kind in STATUS_KINDS


def observe(
    store: claude_worker.news.store.Store,
    source: claude_worker.news.sources.Source,
    snapshot: claude_worker.news.sources.Snapshot,
    now_ts: int,
    ctx: Context | None = None,
) -> Outcome:
    """One class-A source's snapshot: diff against the stored baseline,
    record what changed, store the snapshot only when it actually moved.

    Order matters. The baseline is read BEFORE the new snapshot is stored,
    and an unchanged body is not stored again (spec §8.1) — the detectors
    still run over the pair, because an expiry inside 72 h is a fact about
    the CLOCK and must fire whether or not the venue's list moved.
    """
    out = Outcome()
    prev = snapshot_from_row(store.latest_snapshot(source.name))
    events: list[Event] = []
    if source.kind in _INST_ADAPTERS:
        events = diff_instruments(source, prev, snapshot, now_ts)
    elif source.kind in STATUS_KINDS:
        events = detect_status(source, prev, snapshot, now_ts)
    if prev is None or prev.sha256 != snapshot.sha256:
        store.insert_snapshot(*snapshot)
        out.snapshot_stored = True
    stored, closed = apply_events(store, events, now_ts)
    out.events = len(stored)
    out.closed = closed
    if ctx is None or not stored:
        return out
    out.proposals = write_universe_proposals(ctx, stored, now_ts)
    rows, alerts = write_xsd_proposals(ctx, stored, now_ts)
    out.xsd_rows = rows
    out.alerts.extend(alerts)
    return out


def finalize(
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    now_ts: int,
    alerts: list[Alert],
    ctx: Context | None = None,
) -> FinalOutcome:
    """The once-per-cycle tail: the calendar, the red rule, the ALERT file.

    Every path here is best-effort. A cycle that recorded its events has
    done the work that matters; a file it could not write is the next
    cycle's problem, sixty seconds away.
    """
    out = FinalOutcome()
    doc = build_calendar(registry, store, now_ts)
    out.calendar_events = len(typing.cast(list[object], doc["events"]))
    if ctx is None:
        return out
    _safe_write(
        ctx.file(claude_worker.news.CALENDAR_FILE),
        json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n",
    )
    every = list(alerts)
    every.extend(maintenance_alerts(store, engine_mask(ctx), now_ts))
    out.alerts = write_alert(ctx.file(claude_worker.news.ALERT_FILE), every, now_ts)
    return out
