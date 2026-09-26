# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The scheduled-events feed (O-HC8) — what moves an underlying's vol on a
known date.

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

The Hypercall S1 event law removes every scheduled event inside an
option's life ``(t, T]`` from both sides before comparing implied with
forecast vol: a dated jump priced into a term-structure hump is an event,
not a premium. Operator ruling O-HC8 puts that calendar on the news/event
plane, as ONE feed that the R1 research study and any future member both
read. This module builds it each cycle and is also its reader.

Inputs, all already on the lane:

* ``news.toml [events] scheduled`` — earnings, lock-ups and anything else
  the operator dates, each with the underlyings whose vol it moves
  (operator-maintained, like ``[calendar] bls_releases``: no keyless JSON
  serves earnings dates — Nasdaq's calendar API refuses robots).
* The newest ``calendar-fed`` snapshots' FOMC statements and ``[calendar]
  bls_releases``, both tagged ``macro`` onto ``[events] macro``. The FOMC
  rows of one UTC day (the statement and the press conference) are ONE
  event, placed at the earliest row that states its time.

The event law sums a jump variance per event, so a duplicate would count
one jump twice: every source's events at one instant and of one kind are
MERGED into one (their underlyings united, their details joined, confirmed
only if every part was).

Output: ``scheduled-events.json`` under the lane's directory, written
atomically every cycle, covering ``[now - lookback_days, now +
horizon_days]`` — back far enough that R1 replaying a captured window
still finds the events inside it, forward past the longest listed expiry.
The reader keeps that window: a query outside it is UNKNOWN (``None``),
never "no events". Nothing here trades; a member that ever reads the feed
does so at boot, on its own ruling.
"""

import dataclasses
import json
import pathlib
import typing

import claude_worker.news
import claude_worker.news.detect
import claude_worker.news.sources
import claude_worker.news.store

#: The ``macro`` kind the Fed and BLS rows are tagged with.
KIND_MACRO: str = "macro"
#: Where an ``[events] scheduled`` / ``[calendar] bls_releases`` row came from.
SOURCE_EVENTS: str = "news.toml [events]"
SOURCE_CALENDAR: str = "news.toml [calendar]"
_DAY_S: int = 86_400
_JOIN: str = "; "


@dataclasses.dataclass(frozen=True, slots=True)
class ScheduledEvent:
    """One dated event and the underlyings whose vol it moves."""

    at_ts: int
    kind: str
    underlyings: tuple[str, ...]
    source: str
    detail: str
    confirmed: bool

    def row(self) -> dict[str, object]:
        """The feed's JSON row."""
        return {
            "at_ts": self.at_ts,
            "kind": self.kind,
            "underlyings": list(self.underlyings),
            "source": self.source,
            "detail": self.detail,
            "confirmed": 1 if self.confirmed else 0,
        }


@dataclasses.dataclass(frozen=True, slots=True)
class Feed:
    """A read feed: its events and the window it vouches for."""

    generated_ts: int
    from_ts: int
    until_ts: int
    events: tuple[ScheduledEvent, ...]

    def covers(self, after_ts: int, until_ts: int) -> bool:
        """Whether ``(after_ts, until_ts]`` lies inside the feed's window —
        outside it, "no event" is not known."""
        return self.generated_ts > 0 and self.from_ts <= after_ts and until_ts <= self.until_ts


def _utc_day(ts: int) -> int:
    return ts - ts % _DAY_S


def _fomc_events(
    registry: claude_worker.news.sources.Registry,
    store: claude_worker.news.store.Store,
    lo: int,
    hi: int,
) -> list[ScheduledEvent]:
    """One FOMC event per UTC day: the earliest row that states its time
    (a row without one is only a date), else the day's earliest row."""
    best: dict[int, claude_worker.news.detect.FedRow] = {}
    fed = claude_worker.news.detect.fed_rows(registry, store)
    for i in range(len(fed)):
        row = fed[i]
        if row.kind != claude_worker.news.detect.CAL_FOMC_STATEMENT or not lo <= row.at_ts <= hi:
            continue
        day = _utc_day(row.at_ts)
        held = best.get(day)
        if held is None or (row.timed, -row.at_ts) > (held.timed, -held.at_ts):
            best[day] = row
    macro = registry.events.macro
    out: list[ScheduledEvent] = []
    days = sorted(best)
    for i in range(len(days)):
        row = best[days[i]]
        detail = row.title or "FOMC statement"
        out.append(ScheduledEvent(row.at_ts, KIND_MACRO, macro, row.source, detail, True))
    return out


def _merge(events: list[ScheduledEvent]) -> list[ScheduledEvent]:
    """One event per (instant, kind), sorted: underlyings united in order
    of appearance, details and sources joined, confirmed only if every
    part was (module doc)."""
    groups: dict[tuple[int, str], list[ScheduledEvent]] = {}
    for i in range(len(events)):
        groups.setdefault((events[i].at_ts, events[i].kind), []).append(events[i])
    out: list[ScheduledEvent] = []
    keys = sorted(groups)
    for i in range(len(keys)):
        parts = groups[keys[i]]
        names: list[str] = []
        details: list[str] = []
        sources: list[str] = []
        for j in range(len(parts)):
            part = parts[j]
            names.extend(n for n in part.underlyings if n not in names)
            if part.detail and part.detail not in details:
                details.append(part.detail)
            if part.source not in sources:
                sources.append(part.source)
        out.append(
            ScheduledEvent(
                keys[i][0],
                keys[i][1],
                tuple(names),
                _JOIN.join(sources),
                _JOIN.join(details),
                all(p.confirmed for p in parts),
            )
        )
    return out


def build_feed(
    registry: claude_worker.news.sources.Registry,
    store: claude_worker.news.store.Store,
    now_ts: int,
) -> dict[str, object]:
    """The feed document for ``[now - lookback, now + horizon]`` (module
    doc)."""
    cfg = registry.events
    lo = now_ts - cfg.lookback_days * _DAY_S
    hi = now_ts + cfg.horizon_days * _DAY_S
    events: list[ScheduledEvent] = []
    if cfg.macro:
        events.extend(_fomc_events(registry, store, lo, hi))
        releases = claude_worker.news.detect.bls_rows(registry)
        for i in range(len(releases)):
            at_ts, detail = releases[i]
            if lo <= at_ts <= hi:
                events.append(
                    ScheduledEvent(at_ts, KIND_MACRO, cfg.macro, SOURCE_CALENDAR, detail, True)
                )
    for i in range(len(cfg.scheduled)):
        entry = cfg.scheduled[i]
        at_ts = claude_worker.news.detect.parse_iso(entry.at)
        if lo <= at_ts <= hi:
            events.append(
                ScheduledEvent(
                    at_ts,
                    entry.kind,
                    entry.underlyings,
                    SOURCE_EVENTS,
                    entry.label,
                    entry.confirmed,
                )
            )
    merged = _merge(events)
    return {
        "v": claude_worker.news.SCHEMA_VERSION,
        "generated_ts": now_ts,
        "from_ts": lo,
        "until_ts": hi,
        "events": [merged[i].row() for i in range(len(merged))],
    }


def write_feed(path: pathlib.Path, doc: typing.Mapping[str, object]) -> int:
    """Write the feed atomically; the number of events written, or -1 when
    the write failed (the next cycle rewrites it)."""
    try:
        claude_worker.news.write_atomic(
            path, json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
        )
    except OSError:
        return -1
    return len(typing.cast(list[object], doc["events"]))


def _event_from(row: object) -> ScheduledEvent | None:
    if not isinstance(row, dict):
        return None
    names = row.get("underlyings")
    at_ts = row.get("at_ts")
    if not isinstance(names, list) or not isinstance(at_ts, int) or isinstance(at_ts, bool):
        return None
    return ScheduledEvent(
        at_ts=at_ts,
        kind=str(row.get("kind", "")),
        underlyings=tuple(str(n) for n in names),
        source=str(row.get("source", "")),
        detail=str(row.get("detail", "")),
        confirmed=row.get("confirmed") == 1,
    )


def _int(doc: dict[str, object], key: str) -> int:
    value = doc.get(key)
    return value if isinstance(value, int) and not isinstance(value, bool) else 0


_EMPTY: Feed = Feed(0, 0, 0, ())


def load_feed(path: pathlib.Path) -> Feed:
    """The READER (R1, a future member): the feed at ``path`` and the
    window it vouches for. An absent, unreadable, reshaped or other-version
    file is an EMPTY feed that covers nothing — every query on it is
    unknown, which a consumer must treat as it treats any missing input."""
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except OSError, ValueError:
        return _EMPTY
    if not isinstance(doc, dict) or doc.get("v") != claude_worker.news.SCHEMA_VERSION:
        return _EMPTY
    rows = doc.get("events")
    if not isinstance(rows, list):
        return _EMPTY
    events: list[ScheduledEvent] = []
    for i in range(len(rows)):
        event = _event_from(rows[i])
        if event is not None:
            events.append(event)
    typed = typing.cast(dict[str, object], doc)
    return Feed(
        generated_ts=_int(typed, "generated_ts"),
        from_ts=_int(typed, "from_ts"),
        until_ts=_int(typed, "until_ts"),
        events=tuple(events),
    )


def events_in(
    feed: Feed, underlying: str, after_ts: int, until_ts: int
) -> list[ScheduledEvent] | None:
    """The events that move ``underlying`` inside ``(after_ts, until_ts]`` —
    the S1 event law's window: an option live at ``t`` expiring at ``T``
    carries exactly these. ``None`` when the feed does not cover the
    window (unknown, not empty)."""
    if not feed.covers(after_ts, until_ts):
        return None
    out: list[ScheduledEvent] = []
    for i in range(len(feed.events)):
        e = feed.events[i]
        if after_ts < e.at_ts <= until_ts and underlying in e.underlyings:
            out.append(e)
    return out
