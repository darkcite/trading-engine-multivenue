# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The scheduled-events feed (O-HC8): ``news.toml [events]``, the feed the
cycle writes, and the reader R1 and any future member take it through.

Pins what the S1 event law depends on: every dated event lands tagged with
the underlyings it moves, the macro rows are counted ONCE per event, the
window is ``[now - lookback, now + horizon]``, and a consumer reads
``(t, T]`` exactly.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib

import pytest

import claude_worker.news
import claude_worker.news.detect
import claude_worker.news.scheduled
import claude_worker.news.sources
import claude_worker.news.store

#: 2026-09-26T00:00:00Z.
NOW: int = 1_790_380_800
DAY: int = 86_400
_FED = claude_worker.news.sources.Source(
    name="fed-calendar",
    kind="calendar-fed",
    url="https://www.federalreserve.gov/x",
    origin="www.federalreserve.gov",
    class_="A",
)
_TOML_HEAD = """
[news]
user_agent = "t/1 (+contact: t@example.invalid)"
[registry]
sources = []
"""


def _registry(
    events: claude_worker.news.sources.EventsConfig,
    bls: tuple[str, ...] = (),
) -> claude_worker.news.sources.Registry:
    return claude_worker.news.sources.Registry(
        settings=claude_worker.news.sources.NewsSettings(),
        keywords=(),
        calendar=claude_worker.news.sources.Calendar(bls_releases=bls),
        sources=(_FED,),
        events=events,
    )


def _entry(at: str, kind: str, *unds: str, label: str = "", confirmed: bool = True):
    return claude_worker.news.sources.ScheduledEntry(at, kind, unds, label, confirmed)


def _fed_rows(store: claude_worker.news.store.Store, rows: dict[str, dict[str, str]]) -> None:
    body = json.dumps(rows, separators=(",", ":"), sort_keys=True)
    store.insert_snapshot(
        *claude_worker.news.sources.Snapshot(
            source="fed-calendar",
            taken_ts=NOW,
            sha256=hashlib.sha256(body.encode("utf-8")).hexdigest(),
            count=len(rows),
            body=body,
        )
    )


def _load(tmp_path: pathlib.Path, events_toml: str) -> claude_worker.news.sources.Registry:
    path = tmp_path / "news.toml"
    path.write_text(_TOML_HEAD + events_toml, encoding="utf-8")
    return claude_worker.news.sources.load_registry(path)


# ---- the [events] table ------------------------------------------------------


def test_the_events_table_parses_and_defaults(tmp_path: pathlib.Path) -> None:
    reg = _load(
        tmp_path,
        """
[events]
horizon_days = 50
macro = ["SP500", "BTC"]
scheduled = [
  { at = "2026-10-28T20:05:00Z", kind = "earnings", underlyings = ["MSFT"], label = "FQ1 FY27" },
  { at = "2026-12-09T13:30:00Z", kind = "lockup", underlyings = ["SPCX"], confirmed = 0 },
]
""",
    )
    ev = reg.events
    assert (ev.horizon_days, ev.lookback_days, ev.macro) == (50, 120, ("SP500", "BTC"))
    assert ev.scheduled[0] == _entry("2026-10-28T20:05:00Z", "earnings", "MSFT", label="FQ1 FY27")
    assert ev.scheduled[1].confirmed is False
    # Absent, the feed has nothing to carry and the lane does the rest.
    assert _load(tmp_path, "").events == claude_worker.news.sources.EventsConfig()


_AT: str = 'at = "2026-10-28T20:05:00Z"'
_EARN: str = 'kind = "earnings"'
_MU: str = 'underlyings = ["MU"]'


def _one(*fields: str) -> str:
    """An ``[events]`` table with one ``scheduled`` entry of ``fields``."""
    return "[events]\nscheduled = [{ " + ", ".join(fields) + " }]\n"


def test_a_toml_datetime_literal_is_a_stamp(tmp_path: pathlib.Path) -> None:
    reg = _load(tmp_path, _one("at = 2026-10-28T20:05:00Z", _EARN, _MU))
    at = reg.events.scheduled[0].at
    assert claude_worker.news.detect.parse_iso(at) == 1_793_217_900


@pytest.mark.parametrize(
    ("events_toml", "refusal"),
    [
        ("[events]\nhorizon = 3\n", "unknown key"),
        (_one(_AT, 'kind = "rumour"', _MU), "kind 'rumour'"),
        (_one(_AT, _EARN, 'underlyings = ["nvda"]'), "underlying 'nvda'"),
        (_one(_AT, _EARN, "underlyings = []"), "no underlyings"),
        (_one('at = "2026-10-28T20:05:00"', _EARN, _MU), "names no zone"),
        (_one('at = "next tuesday"', _EARN, _MU), "not an ISO-8601 stamp"),
        (_one(_AT, _EARN, _MU, "confirmed = 2"), "confirmed must be 0 or 1"),
        (_one(_AT, _EARN, _MU, "when = 1"), "unknown key"),
        ('[events]\nmacro = ["S&P"]\n', "underlying 'S&P'"),
        ("[events]\nlookback_days = -1\n", ">= 0"),
        ('[events]\nmacro = "SP500"\n', "must be a list"),
        ("[events]\nhorizon_days = 4.9\n", "whole number"),
        ("[events]\nhorizon_days = true\n", "whole number"),
        (_one(_AT, _EARN, "underlyings = [7]"), "underlying 7"),
        ("[events]\nscheduled = { a = 1 }\n", "list of inline tables"),
        ("[events]\nscheduled = [1]\n", "inline table"),
        (_one("at = 20261028", _EARN, _MU), "not an ISO-8601 stamp"),
        (_one(_AT, _EARN, _MU, "label = 3"), "label must be a string"),
    ],
)
def test_a_malformed_events_table_is_refused_whole(
    tmp_path: pathlib.Path, events_toml: str, refusal: str
) -> None:
    with pytest.raises(ValueError, match=refusal):
        _load(tmp_path, events_toml)


# ---- the feed ----------------------------------------------------------------


def test_the_feed_carries_the_window_tagged_and_sorted(tmp_path: pathlib.Path) -> None:
    events = claude_worker.news.sources.EventsConfig(
        horizon_days=45,
        lookback_days=120,
        macro=("SP500", "BTC", "ETH"),
        scheduled=(
            _entry("2026-09-30T20:05:00Z", "earnings", "MU", label="FQ4 FY26"),
            _entry("2026-10-28T20:05:00Z", "earnings", "MSFT", "SP500", label="FQ1 FY27"),
            _entry("2027-03-01T00:00:00Z", "other", "NVDA", label="beyond the horizon"),
            _entry("2026-01-02T00:00:00Z", "other", "NVDA", label="before the lookback"),
        ),
    )
    bls = (
        "2026-10-14T12:30:00Z CPI (September)",
        "2026-10-14T12:30:00Z Real Earnings",
        "2026-10-02T12:30:00Z Employment Situation",
        "not-a-stamp Oops",
    )
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        _fed_rows(
            store,
            {
                # Statement and press conference, one decision day.
                "a": {
                    "month": "2026-10",
                    "days": "27-28",
                    "time": "2:00 p.m.",
                    "title": "FOMC Meeting",
                    "type": "FOMC",
                },
                "b": {
                    "month": "2026-10",
                    "days": "28",
                    "time": "2:30 p.m.",
                    "title": "FOMC Press Conference",
                    "type": "FOMC",
                },
                "c": {
                    "month": "2026-10",
                    "days": "8",
                    "time": "2:00 p.m.",
                    "title": "Minutes of the FOMC Meeting",
                    "type": "Minutes",
                },
                "d": {
                    "month": "2026-10",
                    "days": "6",
                    "time": "10:00 a.m.",
                    "title": "Speech",
                    "type": "Speeches",
                },
                "e": {
                    "month": "2026-07",
                    "days": "28-29",
                    "time": "2:00 p.m.",
                    "title": "FOMC Meeting",
                    "type": "FOMC",
                },
            },
        )
        doc = claude_worker.news.scheduled.build_feed(_registry(events, bls), store, NOW)
    assert (doc["from_ts"], doc["until_ts"]) == (NOW - 120 * DAY, NOW + 45 * DAY)
    rows = doc["events"]
    got = [(r["at_ts"], r["kind"], tuple(r["underlyings"]), r["detail"]) for r in rows]
    macro = ("SP500", "BTC", "ETH")
    assert got == [
        # 2026-07-29T18:00Z: the July meeting is inside the lookback — R1
        # replays captured windows that far back.
        (1_785_348_000, "macro", macro, "FOMC Meeting"),
        (1_790_798_700, "earnings", ("MU",), "FQ4 FY26"),  # 2026-09-30T20:05Z
        (1_790_944_200, "macro", macro, "Employment Situation"),  # 2026-10-02T12:30Z
        # 2026-10-14T12:30Z: two releases at one instant are ONE event.
        (1_791_981_000, "macro", macro, "CPI (September); Real Earnings"),
        # 2026-10-28T18:00Z (14:00 EDT): statement + press conference are ONE event.
        (1_793_210_400, "macro", macro, "FOMC Meeting"),
        (1_793_217_900, "earnings", ("MSFT", "SP500"), "FQ1 FY27"),  # 2026-10-28T20:05Z
    ]
    assert [r["source"] for r in rows] == [
        "fed-calendar",
        claude_worker.news.scheduled.SOURCE_EVENTS,
        claude_worker.news.scheduled.SOURCE_CALENDAR,
        claude_worker.news.scheduled.SOURCE_CALENDAR,
        "fed-calendar",
        claude_worker.news.scheduled.SOURCE_EVENTS,
    ]
    assert all(r["confirmed"] == 1 for r in rows)


def test_no_macro_underlyings_means_no_macro_rows(tmp_path: pathlib.Path) -> None:
    events = claude_worker.news.sources.EventsConfig(
        scheduled=(_entry("2026-10-01T00:00:00Z", "lockup", "SPCX", confirmed=False),)
    )
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        _fed_rows(
            store,
            {
                "a": {
                    "month": "2026-10",
                    "days": "28",
                    "time": "2:00 p.m.",
                    "title": "FOMC",
                    "type": "FOMC",
                }
            },
        )
        doc = claude_worker.news.scheduled.build_feed(
            _registry(events, ("2026-10-14T12:30:00Z CPI",)), store, NOW
        )
    assert [(r["kind"], r["confirmed"]) for r in doc["events"]] == [("lockup", 0)]


def test_one_instant_and_kind_is_one_event_whatever_its_sources(tmp_path: pathlib.Path) -> None:
    """A scheduled macro entry at a BLS instant, and two companies reporting
    at one instant, are each ONE event: the law counts a jump once."""
    events = claude_worker.news.sources.EventsConfig(
        macro=("SP500",),
        scheduled=(
            _entry("2026-10-14T12:30:00Z", "macro", "SP500", "BTC", label="CPI", confirmed=False),
            _entry("2026-10-28T20:05:00Z", "earnings", "MSFT", label="MSFT FQ1"),
            _entry("2026-10-28T20:05:00Z", "earnings", "META", label="META Q3"),
        ),
    )
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        doc = claude_worker.news.scheduled.build_feed(
            _registry(events, ("2026-10-14T12:30:00Z CPI (September)",)), store, NOW
        )
    rows = doc["events"]
    assert [(r["kind"], r["underlyings"], r["detail"], r["confirmed"]) for r in rows] == [
        ("macro", ["SP500", "BTC"], "CPI (September); CPI", 0),
        ("earnings", ["MSFT", "META"], "MSFT FQ1; META Q3", 1),
    ]
    assert rows[0]["source"] == "news.toml [calendar]; news.toml [events]"


def test_the_fomc_day_is_placed_at_its_timed_row_and_a_range_may_cross_a_month(
    tmp_path: pathlib.Path,
) -> None:
    events = claude_worker.news.sources.EventsConfig(macro=("SP500",))
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        _fed_rows(
            store,
            {
                # A dated-only row (midnight ET) must not beat the statement.
                "a": {
                    "month": "2026-10",
                    "days": "28",
                    "time": "",
                    "title": "FOMC",
                    "type": "FOMC",
                },
                "b": {
                    "month": "2026-10",
                    "days": "27-28",
                    "time": "2:00 p.m.",
                    "title": "FOMC Meeting",
                    "type": "FOMC",
                },
                # A meeting across a month end decides in the NEXT month.
                "c": {
                    "month": "2026-09",
                    "days": "30-1",
                    "time": "2:00 p.m.",
                    "title": "FOMC Meeting",
                    "type": "FOMC",
                },
            },
        )
        doc = claude_worker.news.scheduled.build_feed(_registry(events), store, NOW)
    assert [r["at_ts"] for r in doc["events"]] == [
        1_790_877_600,  # 2026-10-01T18:00Z, not 2026-09-01
        1_793_210_400,  # 2026-10-28T18:00Z, not 04:00Z
    ]


def test_the_reader_round_trips_and_windows_by_underlying(tmp_path: pathlib.Path) -> None:
    events = claude_worker.news.sources.EventsConfig(
        macro=("SP500",),
        scheduled=(
            _entry("2026-09-30T20:05:00Z", "earnings", "MU", label="FQ4 FY26"),
            _entry("2026-10-02T20:00:00Z", "other", "MU", "NVDA", label="x", confirmed=False),
        ),
    )
    path = tmp_path / "news" / claude_worker.news.SCHEDULED_EVENTS_FILE
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        doc = claude_worker.news.scheduled.build_feed(
            _registry(events, ("2026-10-14T12:30:00Z CPI",)), store, NOW
        )
    assert claude_worker.news.scheduled.write_feed(path, doc) == 3
    feed = claude_worker.news.scheduled.load_feed(path)
    assert [e.row() for e in feed.events] == doc["events"]
    assert (feed.generated_ts, feed.from_ts, feed.until_ts) == (
        NOW,
        doc["from_ts"],
        doc["until_ts"],
    )
    events_in = claude_worker.news.scheduled.events_in
    mu_expiry = 1_790_971_200  # MU-20261002 settles 2026-10-02T20:00Z
    inside = events_in(feed, "MU", NOW, mu_expiry)
    assert [e.detail for e in inside] == ["FQ4 FY26", "x"], "(t, T]: the expiry instant is inside"
    assert events_in(feed, "MU", mu_expiry, mu_expiry + DAY) == []
    assert [e.kind for e in events_in(feed, "SP500", NOW, NOW + 30 * DAY)] == ["macro"]
    assert events_in(feed, "AAPL", NOW, NOW + 30 * DAY) == []
    # Outside the window the feed vouches for, the answer is UNKNOWN.
    assert events_in(feed, "MU", NOW, NOW + 46 * DAY) is None
    assert events_in(feed, "MU", NOW - 121 * DAY, NOW) is None


def test_a_missing_reshaped_or_foreign_feed_covers_nothing(tmp_path: pathlib.Path) -> None:
    load = claude_worker.news.scheduled.load_feed
    empty = load(tmp_path / "absent.json")
    assert empty.events == () and not empty.covers(NOW, NOW + DAY)
    assert claude_worker.news.scheduled.events_in(empty, "MU", NOW, NOW + DAY) is None
    bad = tmp_path / "bad.json"
    bad.write_text("{not json", encoding="utf-8")
    assert load(bad).events == ()
    bad.write_text('{"v": 1, "events": {"a": 1}}', encoding="utf-8")
    assert load(bad).events == ()
    rows = [
        {"at_ts": "soon", "underlyings": ["MU"]},
        7,
        {"at_ts": 5, "underlyings": ["MU"], "kind": "other", "confirmed": 1},
    ]
    doc = {"v": 1, "generated_ts": NOW, "from_ts": 0, "until_ts": NOW, "events": rows}
    bad.write_text(json.dumps(doc), encoding="utf-8")
    only = load(bad).events
    assert len(only) == 1 and only[0].at_ts == 5 and only[0].confirmed
    # Another schema version is not this reader's to interpret.
    bad.write_text(json.dumps({**doc, "v": 2}), encoding="utf-8")
    assert load(bad).events == ()


def test_a_feed_that_cannot_be_written_says_so(tmp_path: pathlib.Path) -> None:
    blocker = tmp_path / "file"
    blocker.write_text("x", encoding="utf-8")
    doc = {"v": 1, "events": [{"at_ts": 1}]}
    assert claude_worker.news.scheduled.write_feed(blocker / "under-a-file.json", doc) == -1
