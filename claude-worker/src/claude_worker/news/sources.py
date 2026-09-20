# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The NEWS source registry, fetcher and payload parsers (spec §5).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Three layers, deliberately separable so each is testable with no socket:

- **registry** — ``news.toml`` parsed and VALIDATED into an immutable
  [`Registry`]. Validation is ``load_market_map``-strict: an unknown key, a
  duplicate name, an origin that is not the URL's host, or a status page
  without the operator's explicit ``allow_statuspage`` is a ``ValueError``,
  not a warning. The origin is typed twice on purpose — the URL can be
  pasted from anywhere, the origin is the operator's own resolution of it,
  and a response from any other host is refused (survey §13, the
  impersonation hazard).
- **fetcher** — one budgeted, paced, redirect-refusing GET per due source.
  Never raises: every failure is a counted [`FetchResult`] status.
- **parsers** — one function per ``kind``, all reached through [`parse`],
  which swallows every shape deviation into an empty [`Parsed`]. A feed that
  changes its wire shape degrades this lane to silence; it never crashes a
  cycle and it never reaches the engine.

Nothing here constructs an Anthropic client or imports the SDK. The model
tiers live in ``cascade.py`` and are reached only from ``serve``.
"""

import dataclasses
import datetime
import email.utils
import hashlib
import html
import json
import pathlib
import random
import re
import time
import tomllib
import typing
import urllib.parse
import xml.sax.saxutils

import httpx

import claude_worker.features
import claude_worker.feeds

# ---------------------------------------------------------------- constants

#: The closed ``kind`` table (spec §5.2). A kind outside it is a registry
#: error, so a typo never silently disables a source.
KINDS: tuple[str, ...] = (
    "rss",
    "google-news",
    "hnrss",
    "reddit-rss",
    "arxiv-api",
    "gdelt",
    "json-okx-ann",
    "json-deribit-ann",
    "json-bybit-ann",
    "json-binance-cms",
    "instruments-bn-usdm",
    "instruments-okx",
    "instruments-deribit",
    "instruments-coinbase",
    "status-okx",
    "status-deribit",
    "status-bybit",
    "status-kraken",
    "ping-binance",
    "calendar-fed",
    "json-4chan-catalog",
    "series-bn-ls",
    "series-bn-top-ls",
    "series-bn-taker",
    "series-bn-oi",
    "series-dvol",
    "series-cftc-cot",
    "series-fng",
    "series-defillama-stables",
    "series-mempool-fees",
    "series-blockchain-stats",
    "series-kalshi",
    "series-manifold",
)

#: A = structural · B = typed announcement · C = prose · D = numeric series.
CLASSES: tuple[str, ...] = ("A", "B", "C", "D")

#: Venues a source may claim as its OWN (making it a venue-origin source,
#: which the corroboration rule trusts alone). Free text is refused.
VENUES: tuple[str, ...] = (
    "okx",
    "deribit",
    "bybit",
    "binance",
    "coinbase",
    "kraken",
    "hyperliquid",
    "polymarket",
)

#: Kinds whose URL is BUILT from ``query``; ``url`` must be empty and
#: ``origin`` must be the kind's fixed host.
QUERY_KIND_HOSTS: dict[str, str] = {
    "google-news": "news.google.com",
    "gdelt": "api.gdeltproject.org",
    "hnrss": "hnrss.org",
}

FETCH_STATUS_OK: str = "ok"
FETCH_STATUS_TRANSPORT: str = "transport"
FETCH_STATUS_HTTP: str = "http"
FETCH_STATUS_REFUSED_ORIGIN: str = "refused_origin"
FETCH_STATUS_BUDGET: str = "budget"
FETCH_STATUS_PACED: str = "paced"
#: A 429 is the host pacing US, so it is reported as ``paced`` and not as a
#: generic HTTP failure — GDELT answers one with a plain-text body (§5.2),
#: and counting it as an error would fail an otherwise healthy source.
HTTP_TOO_MANY_REQUESTS: int = 429

#: Same wall as ``feeds._FETCH_TIMEOUT_S`` — one slow host may not stall a
#: cycle (the ``cycle`` p99 gate is 30 s over every due source).
FETCH_TIMEOUT_S: float = 10.0
ACCEPT_HEADER: str = (
    "application/json, application/rss+xml, application/atom+xml, text/xml;q=0.9, */*;q=0.5"
)
DEFAULT_USER_AGENT: str = "multivenue-news/1"

NAME_RE: typing.Final[re.Pattern[str]] = re.compile(r"^[a-z0-9-]+$")
NAME_MAX_LEN: int = 40
POLL_MIN_S: int = 15
WEIGHT_MIN: float = 0.0
WEIGHT_MAX: float = 1.0
ITEMS_PER_HOUR_DEFAULT: int = 60
#: A source below this weight never counts as an independent origin (Q6).
ORIGIN_WEIGHT_MIN: float = 1.0

RATE_RE: typing.Final[re.Pattern[str]] = re.compile(r"^(\d+)/(\d+)s$")
NS_PER_S: int = 1_000_000_000
NS_PER_MS: int = 1_000_000
MS_PER_S: int = 1_000
BUDGET_WINDOW_NS: int = 3_600 * NS_PER_S
BUDGET_MIN_CALLS: int = 60
SECONDS_PER_HOUR: int = 3_600
#: Per-source poll jitter, ``feeds.NewsWatcher``'s scheme restated as a
#: fraction: sources sharing a poll_s must not stampede a host in lockstep.
JITTER_FRAC: float = 0.10

#: Titles are stripped and capped too — they feed the near-dup Jaccard and a
#: model prompt, and a feed that emits a whole article as its title would
#: otherwise blow the tier-0 budget.
TITLE_CAP: int = 300
#: 4chan: threads quieter than this are noise by construction (Appendix A).
FOURCHAN_MIN_REPLIES: int = 20
FOURCHAN_TITLE_CAP: int = 80
#: A Deribit DVOL row is ``[ts_ms, open, high, low, close]``.
OHLC_ROW_LEN: int = 5
#: Index of ``close`` in that row.
OHLC_CLOSE_IDX: int = 4

#: Fixture recording (Appendix B): the reduction keeps the keyed fields only
#: and refuses to write more than this.
FIXTURE_MAX_BYTES: int = 32 * 1024
FIXTURE_MAX_ROWS: int = 12

_TAG_RE: typing.Final[re.Pattern[str]] = re.compile(r"<[^>]+>")
_URL_RE: typing.Final[re.Pattern[str]] = re.compile(r"https?://\S+")
_WS_RE: typing.Final[re.Pattern[str]] = re.compile(r"\s+")
_GDELT_TS_FMT: str = "%Y%m%dT%H%M%SZ"
#: Some publishers serve a UTF-8 BOM ahead of the document (measured
#: 2026-09-19 on www.federalreserve.gov/json/calendar.json). ``json.loads``
#: refuses it outright and ElementTree chokes on it, so it is stripped once
#: at the door rather than in every parser.
_BOM: str = "\ufeff"


# ------------------------------------------------------------------ records


@dataclasses.dataclass(frozen=True, slots=True)
class Source:
    """One ``[registry].sources`` inline table, validated.

    A pure mirror of the TOML — nothing derived is stored, so the operator's
    file and this record are the same object in two forms.
    """

    name: str
    kind: str
    url: str
    origin: str
    class_: str
    venue: str = ""
    poll_s: int = 60
    weight: float = 1.0
    rate: str = ""
    query: str = ""
    allow_statuspage: int = 0
    items_per_hour: int = ITEMS_PER_HOUR_DEFAULT
    enabled: int = 1

    @property
    def is_origin(self) -> bool:
        """Whether this source may count as an INDEPENDENT origin for the
        class-C corroboration rule (Q6: the mills never do)."""
        return self.weight >= ORIGIN_WEIGHT_MIN


@dataclasses.dataclass(frozen=True, slots=True)
class NewsSettings:
    """The ``[news]`` table."""

    poll_default_s: int = 60
    text_cap: int = claude_worker.feeds.TEXT_CAP
    story_window_s: int = 21_600
    near_dup_jaccard: float = 0.6
    near_dup_window_s: int = 21_600
    #: An item still untriaged this long after it was published is retired
    #: without a model call (§9.1). The queue is OLDEST first by law, so a
    #: backlog the lane can never afford to triage starves every fresh item
    #: behind it: on 2026-09-20 the live store held 844 untriaged items, the
    #: newest TRIAGED one was 37.8 h old, and 214 items younger than 6 h had
    #: never been looked at. A day-old headline cannot move a position, so
    #: paying for it twice over — once in calls, once in the delay it
    #: imposes on today's news — buys nothing. 0 disables the ceiling.
    triage_max_age_s: int = 86_400
    user_agent: str = DEFAULT_USER_AGENT
    items_retention_days: int = 14
    snapshots_retention_days: int = 30


@dataclasses.dataclass(frozen=True, slots=True)
class FixedDaily:
    """One ``[calendar].fixed_daily`` entry: a recurring UTC wall-clock event
    the engine's own schedule creates (restarts, PM resolve, HIP-4 settle)."""

    kind: str
    at: str


@dataclasses.dataclass(frozen=True, slots=True)
class Calendar:
    """The ``[calendar]`` table — operator-maintained, because no keyless
    JSON publishes the BLS release schedule."""

    bls_releases: tuple[str, ...] = ()
    fixed_daily: tuple[FixedDaily, ...] = ()


@dataclasses.dataclass(frozen=True, slots=True)
class Registry:
    """``news.toml`` as one immutable record, read once per lane run."""

    settings: NewsSettings
    keywords: tuple[str, ...]
    calendar: Calendar
    sources: tuple[Source, ...]

    def by_name(self, name: str) -> Source | None:
        for i in range(len(self.sources)):
            if self.sources[i].name == name:
                return self.sources[i]
        return None

    def enabled_sources(self) -> tuple[Source, ...]:
        out: list[Source] = []
        for i in range(len(self.sources)):
            if self.sources[i].enabled:
                out.append(self.sources[i])
        return tuple(out)


class Item(typing.NamedTuple):
    """One prose/announcement item, ready for tier 0.

    ``origin`` is the host the CONTENT came from, which is the source's own
    origin for every kind but ``gdelt``, where the aggregator reports the
    publisher's domain and that is what corroboration must count.

    ``hint`` is a structural event-type guess a class-B payload states about
    itself (OKX ``annType``, Bybit ``type.key``) — free evidence the cascade
    would otherwise pay a model to re-derive. It is in-memory only in N1.1:
    the spec's ``items`` DDL has no column for it.
    """

    source: str
    guid: str
    ts: int
    title: str
    link: str
    text: str
    class_: str
    weight: float
    origin: str
    venue: str
    hint: str = ""


class Snapshot(typing.NamedTuple):
    """A class-A source's KEYED state at one instant — never the raw payload.

    ``body`` is canonical compact JSON (sorted keys) so ``sha256`` changes
    if and only if the keyed state changed, which is what the §8.1
    instrument-set diff keys on.
    """

    source: str
    taken_ts: int
    sha256: str
    count: int
    body: str


class Parsed(typing.NamedTuple):
    """One parser's whole output. Empty means "shape deviation" — the caller
    counts ``parse_empty`` (spec §5.2) and moves on."""

    items: list[Item]
    snapshot: Snapshot | None
    series: list[tuple[str, int, float]]

    def is_empty(self) -> bool:
        return not self.items and self.snapshot is None and not self.series


_EMPTY: Parsed = Parsed([], None, [])


class FetchResult(typing.NamedTuple):
    """One attempted GET. ``payload`` is set only on ``ok``."""

    status: str
    payload: str | None
    http_status: int
    elapsed_ms: int


# ----------------------------------------------------------------- registry


_SOURCE_KEYS: frozenset[str] = frozenset(
    (
        "name",
        "kind",
        "url",
        "origin",
        "class",
        "venue",
        "poll_s",
        "weight",
        "rate",
        "query",
        "allow_statuspage",
        "items_per_hour",
        "enabled",
    )
)
_NEWS_KEYS: frozenset[str] = frozenset(
    (
        "poll_default_s",
        "text_cap",
        "story_window_s",
        "near_dup_jaccard",
        "near_dup_window_s",
        "triage_max_age_s",
        "user_agent",
        "items_retention_days",
        "snapshots_retention_days",
    )
)
_TOP_KEYS: frozenset[str] = frozenset(("news", "keywords", "calendar", "registry"))
_CALENDAR_KEYS: frozenset[str] = frozenset(("bls_releases", "fixed_daily"))
_FIXED_DAILY_KEYS: frozenset[str] = frozenset(("kind", "at"))
_REQUIRED_SOURCE_KEYS: tuple[str, ...] = ("name", "kind", "url", "origin", "class")


def _reject_unknown(where: str, table: typing.Mapping[str, object], known: frozenset[str]) -> None:
    extra = sorted(set(table) - known)
    if extra:
        raise ValueError(f"{where}: unknown key(s) {extra}")


def _host_of(url: str) -> str:
    return (urllib.parse.urlsplit(url).hostname or "").lower()


def _is_statuspage(origin: str) -> bool:
    """A host that publishes a venue's status is impersonation-adjacent
    (survey §13): anyone can host ``status.<venue>.example``, so the
    operator must name it explicitly."""
    if origin.startswith("status."):
        return True
    return origin == "statuspage.io" or origin.endswith(".statuspage.io")


def rate_interval_ns(rate: str) -> int:
    """``"1/5s"`` -> the minimum ns between two requests; ``""`` -> 0."""
    if not rate:
        return 0
    match = RATE_RE.match(rate)
    if match is None:
        raise ValueError(f"rate must look like '1/5s': {rate!r}")
    count = int(match.group(1))
    window_s = int(match.group(2))
    if count < 1 or window_s < 1:
        raise ValueError(f"rate must be positive: {rate!r}")
    return window_s * NS_PER_S // count


def request_url(source: Source) -> str:
    """The URL actually requested: ``source.url``, or built from ``query``
    for the three query kinds (spec §5.2)."""
    if source.kind == "google-news":
        return (
            "https://news.google.com/rss/search?q="
            + urllib.parse.quote(source.query)
            + "&hl=en-US&gl=US&ceid=US:en"
        )
    if source.kind == "hnrss":
        return "https://hnrss.org/newest?q=" + urllib.parse.quote(source.query)
    if source.kind == "gdelt":
        return (
            "https://api.gdeltproject.org/api/v2/doc/doc?query="
            + urllib.parse.quote(source.query)
            + "&mode=artlist&format=json&maxrecords=50&timespan=1h"
        )
    return source.url


def _validate_identity(source: Source) -> None:
    if not NAME_RE.match(source.name) or len(source.name) > NAME_MAX_LEN:
        raise ValueError(f"source name must be [a-z0-9-]+ and <= {NAME_MAX_LEN}: {source.name!r}")
    if source.kind not in KINDS:
        raise ValueError(f"{source.name}: unknown kind {source.kind!r}")
    if source.class_ not in CLASSES:
        raise ValueError(f"{source.name}: class must be one of {CLASSES}: {source.class_!r}")
    if source.venue and source.venue not in VENUES:
        raise ValueError(f"{source.name}: unknown venue {source.venue!r}")


def _validate_numbers(source: Source) -> None:
    if not WEIGHT_MIN <= source.weight <= WEIGHT_MAX:
        raise ValueError(f"{source.name}: weight must be in [0, 1]: {source.weight}")
    if source.poll_s < POLL_MIN_S:
        raise ValueError(f"{source.name}: poll_s must be >= {POLL_MIN_S}: {source.poll_s}")
    if source.items_per_hour < 1:
        raise ValueError(f"{source.name}: items_per_hour must be >= 1: {source.items_per_hour}")
    rate_interval_ns(source.rate)


def _validate_query_kind(source: Source, fixed_host: str) -> None:
    if source.url:
        raise ValueError(f"{source.name}: kind {source.kind} builds its URL — url must be ''")
    if not source.query:
        raise ValueError(f"{source.name}: kind {source.kind} requires a non-empty query")
    if source.origin != fixed_host:
        raise ValueError(f"{source.name}: origin must be {fixed_host!r} for {source.kind}")


def _validate_origin(source: Source) -> None:
    fixed_host = QUERY_KIND_HOSTS.get(source.kind, "")
    if fixed_host:
        _validate_query_kind(source, fixed_host)
        return
    if not source.url:
        raise ValueError(f"{source.name}: url is required for kind {source.kind}")
    host = _host_of(source.url)
    if not host:
        raise ValueError(f"{source.name}: url has no host: {source.url!r}")
    if source.origin != host:
        raise ValueError(f"{source.name}: origin {source.origin!r} != url host {host!r}")
    if _is_statuspage(source.origin) and not source.allow_statuspage:
        raise ValueError(
            f"{source.name}: {source.origin!r} is a status page — set allow_statuspage = 1 "
            "only after resolving it by hand (survey §13)"
        )


def _source_from_table(table: typing.Mapping[str, object], default_poll_s: int) -> Source:
    _reject_unknown("[registry].sources entry", table, _SOURCE_KEYS)
    for i in range(len(_REQUIRED_SOURCE_KEYS)):
        key = _REQUIRED_SOURCE_KEYS[i]
        if key not in table:
            raise ValueError(f"[registry].sources entry is missing required key {key!r}")
    source = Source(
        name=str(table["name"]),
        kind=str(table["kind"]),
        url=str(table["url"]),
        origin=str(table["origin"]).lower(),
        class_=str(table["class"]),
        venue=str(table.get("venue", "")),
        poll_s=int(typing.cast(int, table.get("poll_s", default_poll_s))),
        weight=float(typing.cast(float, table.get("weight", 1.0))),
        rate=str(table.get("rate", "")),
        query=str(table.get("query", "")),
        allow_statuspage=int(typing.cast(int, table.get("allow_statuspage", 0))),
        items_per_hour=int(typing.cast(int, table.get("items_per_hour", ITEMS_PER_HOUR_DEFAULT))),
        enabled=int(typing.cast(int, table.get("enabled", 1))),
    )
    _validate_identity(source)
    _validate_numbers(source)
    _validate_origin(source)
    return source


def _settings_from(table: typing.Mapping[str, object]) -> NewsSettings:
    _reject_unknown("[news]", table, _NEWS_KEYS)
    base = NewsSettings()
    return NewsSettings(
        poll_default_s=int(typing.cast(int, table.get("poll_default_s", base.poll_default_s))),
        text_cap=int(typing.cast(int, table.get("text_cap", base.text_cap))),
        story_window_s=int(typing.cast(int, table.get("story_window_s", base.story_window_s))),
        near_dup_jaccard=float(
            typing.cast(float, table.get("near_dup_jaccard", base.near_dup_jaccard))
        ),
        near_dup_window_s=int(
            typing.cast(int, table.get("near_dup_window_s", base.near_dup_window_s))
        ),
        triage_max_age_s=int(
            typing.cast(int, table.get("triage_max_age_s", base.triage_max_age_s))
        ),
        user_agent=str(table.get("user_agent", base.user_agent)),
        items_retention_days=int(
            typing.cast(int, table.get("items_retention_days", base.items_retention_days))
        ),
        snapshots_retention_days=int(
            typing.cast(int, table.get("snapshots_retention_days", base.snapshots_retention_days))
        ),
    )


def _calendar_from(table: typing.Mapping[str, object]) -> Calendar:
    _reject_unknown("[calendar]", table, _CALENDAR_KEYS)
    raw_daily = typing.cast(list[dict[str, object]], table.get("fixed_daily", []))
    daily: list[FixedDaily] = []
    for i in range(len(raw_daily)):
        entry = raw_daily[i]
        _reject_unknown("[calendar].fixed_daily entry", entry, _FIXED_DAILY_KEYS)
        daily.append(FixedDaily(kind=str(entry["kind"]), at=str(entry["at"])))
    releases = typing.cast(list[str], table.get("bls_releases", []))
    out: list[str] = []
    for i in range(len(releases)):
        out.append(str(releases[i]))
    return Calendar(bls_releases=tuple(out), fixed_daily=tuple(daily))


def load_registry(path: pathlib.Path) -> Registry:
    """Parse and validate ``news.toml``.

    Raises ``FileNotFoundError`` when absent (the caller decides the lane is
    a no-op) and ``ValueError`` on anything malformed — never a partial
    registry, because a half-read source list would silently stop polling
    the sources it dropped.
    """
    doc = tomllib.loads(path.read_bytes().decode("utf-8"))
    _reject_unknown(str(path), doc, _TOP_KEYS)
    settings = _settings_from(typing.cast(dict[str, object], doc.get("news", {})))
    keywords_table = typing.cast(dict[str, object], doc.get("keywords", {}))
    _reject_unknown("[keywords]", keywords_table, frozenset(("event",)))
    raw_keywords = typing.cast(list[str], keywords_table.get("event", []))
    keywords: list[str] = []
    for i in range(len(raw_keywords)):
        keywords.append(str(raw_keywords[i]))
    calendar = _calendar_from(typing.cast(dict[str, object], doc.get("calendar", {})))

    registry_table = typing.cast(dict[str, object], doc.get("registry", {}))
    _reject_unknown("[registry]", registry_table, frozenset(("sources",)))
    raw_sources = typing.cast(list[dict[str, object]], registry_table.get("sources", []))
    sources: list[Source] = []
    seen: set[str] = set()
    for i in range(len(raw_sources)):
        source = _source_from_table(raw_sources[i], settings.poll_default_s)
        if source.name in seen:
            raise ValueError(f"duplicate source name {source.name!r}")
        seen.add(source.name)
        sources.append(source)
    return Registry(
        settings=settings,
        keywords=tuple(keywords),
        calendar=calendar,
        sources=tuple(sources),
    )


# ------------------------------------------------------------------ fetcher


def build_budgets(
    sources: typing.Sequence[Source],
    clock_ns: typing.Callable[[], int] = time.monotonic_ns,
) -> dict[str, claude_worker.features.RestBudget]:
    """One hourly ``RestBudget`` per ORIGIN host (spec §5.1).

    The ceiling is twice what the configured cadence can legitimately spend
    in an hour, floored at 60 — generous enough that a normal cycle never
    trips it, tight enough that a bug which polls in a loop is stopped by
    the budget rather than by the host.
    """
    per_host: dict[str, list[Source]] = {}
    for i in range(len(sources)):
        per_host.setdefault(sources[i].origin, []).append(sources[i])
    out: dict[str, claude_worker.features.RestBudget] = {}
    for host, group in per_host.items():
        min_poll_s = group[0].poll_s
        for i in range(len(group)):
            min_poll_s = min(min_poll_s, group[i].poll_s)
        max_calls = max(BUDGET_MIN_CALLS, 2 * len(group) * SECONDS_PER_HOUR // max(min_poll_s, 1))
        out[host] = claude_worker.features.RestBudget(
            max_calls=max_calls, window_ns=BUDGET_WINDOW_NS, clock_ns=clock_ns
        )
    return out


#: A document answer starts with one of these. A rate-limited host that
#: answers prose instead is pacing us, whatever status it attached: GDELT
#: serves its "Please limit requests to one every 5 seconds" notice with a
#: 429 AND, under load, with a 200 (measured 2026-09-19). Counting that as a
#: successful poll would feed the parser prose and read as a wire reshape.
_DOCUMENT_STARTS: tuple[str, ...] = ("{", "[", "<")


def _is_document(payload: str | None) -> bool:
    return bool(payload) and str(payload).lstrip().startswith(_DOCUMENT_STARTS)


def _status_refusal(response: httpx.Response, elapsed_ms: int) -> FetchResult | None:
    """The non-OK verdicts, so ``Fetcher._get`` keeps one return per outcome."""
    if response.status_code == HTTP_TOO_MANY_REQUESTS:
        return FetchResult(FETCH_STATUS_PACED, None, response.status_code, elapsed_ms)
    if response.status_code != httpx.codes.OK:
        return FetchResult(FETCH_STATUS_HTTP, None, response.status_code, elapsed_ms)
    return None


class Fetcher:
    """Schedules and performs one GET per due source. Never raises."""

    def __init__(
        self,
        registry: Registry,
        http: httpx.Client,
        clock_ns: typing.Callable[[], int] = time.monotonic_ns,
        budgets: dict[str, claude_worker.features.RestBudget] | None = None,
        rng: random.Random | None = None,
    ) -> None:
        self._registry = registry
        self._http = http
        self._clock = clock_ns
        self._sources: tuple[Source, ...] = registry.enabled_sources()
        self._budgets: dict[str, claude_worker.features.RestBudget] = (
            build_budgets(self._sources, clock_ns) if budgets is None else budgets
        )
        self._rng: random.Random = random.Random() if rng is None else rng
        # First poll of every source is due immediately; jitter starts after.
        self._next_due_ns: dict[str, int] = {}
        self._next_allowed_ns: dict[str, int] = {}
        for i in range(len(self._sources)):
            self._next_due_ns[self._sources[i].name] = 0
            self._next_allowed_ns[self._sources[i].name] = 0

    @property
    def budgets(self) -> dict[str, claude_worker.features.RestBudget]:
        return self._budgets

    def due(self, now_ns: int) -> list[Source]:
        """Enabled sources whose next poll has come round (config order)."""
        out: list[Source] = []
        for i in range(len(self._sources)):
            source = self._sources[i]
            if self._next_due_ns[source.name] <= now_ns:
                out.append(source)
        return out

    def next_due_ns(self, name: str) -> int:
        """Scheduled next poll for one source (test/diagnostic surface)."""
        return self._next_due_ns[name]

    def reschedule(self, source: Source, now_ns: int) -> None:
        """Push the source out one jittered period."""
        base = source.poll_s * NS_PER_S
        jitter = self._rng.uniform(-JITTER_FRAC, JITTER_FRAC)
        self._next_due_ns[source.name] = now_ns + int(base * (1.0 + jitter))

    def _headers(self) -> dict[str, str]:
        return {"User-Agent": self._registry.settings.user_agent, "Accept": ACCEPT_HEADER}

    def _redirect_target(self, response: httpx.Response, source: Source) -> str | None:
        """The ONE hop we allow: a 3xx whose ``Location`` stays on the
        source's own origin. Anything else is ``refused_origin`` — a
        redirect off-origin is exactly the impersonation shape."""
        location = response.headers.get("location", "")
        if not location:
            return None
        target = str(httpx.URL(str(response.request.url)).join(location))
        return target if _host_of(target) == source.origin else None

    def fetch(self, source: Source, now_ns: int) -> FetchResult:
        """One budgeted GET. Reschedules first, so a failing source backs off
        on its own cadence instead of being retried inside the same cycle."""
        self.reschedule(source, now_ns)
        interval = rate_interval_ns(source.rate)
        if interval and now_ns < self._next_allowed_ns.get(source.name, 0):
            return FetchResult(FETCH_STATUS_PACED, None, 0, 0)
        budget = self._budgets.get(source.origin)
        if budget is not None and not budget.try_acquire():
            return FetchResult(FETCH_STATUS_BUDGET, None, 0, 0)
        if interval:
            self._next_allowed_ns[source.name] = now_ns + interval
        result = self._get(source, request_url(source), allow_hop=True)
        if result.status == FETCH_STATUS_OK and interval and not _is_document(result.payload):
            return FetchResult(FETCH_STATUS_PACED, None, result.http_status, result.elapsed_ms)
        return result

    def _get(self, source: Source, url: str, *, allow_hop: bool) -> FetchResult:
        started = self._clock()
        try:
            response = self._http.get(
                url, timeout=FETCH_TIMEOUT_S, headers=self._headers(), follow_redirects=False
            )
        except httpx.HTTPError:
            return FetchResult(FETCH_STATUS_TRANSPORT, None, 0, self._elapsed_ms(started))
        elapsed = self._elapsed_ms(started)
        if response.is_redirect:
            target = self._redirect_target(response, source) if allow_hop else None
            if target is None:
                return FetchResult(FETCH_STATUS_REFUSED_ORIGIN, None, response.status_code, elapsed)
            return self._get(source, target, allow_hop=False)
        refused = _status_refusal(response, elapsed)
        if refused is not None:
            return refused
        if _host_of(str(response.request.url)) != source.origin:
            return FetchResult(FETCH_STATUS_REFUSED_ORIGIN, None, response.status_code, elapsed)
        return FetchResult(FETCH_STATUS_OK, response.text, response.status_code, elapsed)

    def _elapsed_ms(self, started_ns: int) -> int:
        return max(0, (self._clock() - started_ns) // NS_PER_MS)


# ------------------------------------------------------------------ parsers


def strip_text(text: str, cap: int) -> str:
    """Tag-strip, unescape, drop bare URLs, collapse whitespace, cap.

    Order matters: tags go first so ``&lt;b&gt;`` in the source cannot
    become a tag after unescaping.
    """
    out = _TAG_RE.sub(" ", text)
    out = html.unescape(out)
    out = _URL_RE.sub("", out)
    return _WS_RE.sub(" ", out).strip()[:cap]


def strip_bom(payload: str) -> str:
    """Drop one leading UTF-8 BOM. Idempotent and cheap."""
    return payload[1:] if payload.startswith(_BOM) else payload


def _number(value: object) -> float | None:
    """Bool-rejecting numeric coercion (``labeling._number``'s contract)."""
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    if isinstance(value, str):
        try:
            return float(value.strip())
        except ValueError:
            return None
    return None


def _epoch_s(value: object, *, unit_ms: bool) -> int:
    number = _number(value)
    if number is None:
        return 0
    return int(number) // MS_PER_S if unit_ms else int(number)


def _mk(  # noqa: PLR0913
    source: Source,
    *,
    guid: str,
    ts: int,
    title: str,
    link: str,
    text: str,
    cap: int,
    origin: str = "",
    hint: str = "",
) -> Item:
    """Build one [`Item`], carrying the source's class/weight/venue with it.

    Wide on purpose: every field is wire data a parser has just extracted,
    and the alternative (a mutable builder) would let a parser forget one.
    """
    return Item(
        source=source.name,
        guid=guid,
        ts=ts,
        title=strip_text(title, TITLE_CAP),
        link=link,
        text=strip_text(text, cap),
        class_=source.class_,
        weight=source.weight,
        origin=origin or source.origin,
        venue=source.venue,
        hint=hint,
    )


def _snapshot(source: Source, fetched_ts: int, body_obj: dict[str, object]) -> Snapshot:
    body = json.dumps(body_obj, separators=(",", ":"), sort_keys=True)
    return Snapshot(
        source=source.name,
        taken_ts=fetched_ts,
        sha256=hashlib.sha256(body.encode("utf-8")).hexdigest(),
        count=len(body_obj),
        body=body,
    )


def _obj(payload: str) -> dict[str, object]:
    doc = json.loads(payload)
    if not isinstance(doc, dict):
        raise ValueError("expected a JSON object")
    return doc


def _arr(payload: str) -> list[object]:
    doc = json.loads(payload)
    if not isinstance(doc, list):
        raise ValueError("expected a JSON array")
    return doc


def _dicts(node: object) -> list[dict[str, object]]:
    if not isinstance(node, list):
        raise ValueError("expected a JSON array")
    out: list[dict[str, object]] = []
    for i in range(len(node)):
        entry = node[i]
        if isinstance(entry, dict):
            out.append(entry)
    return out


def _at(doc: object, *path: object) -> object:
    node = doc
    for i in range(len(path)):
        key = path[i]
        if isinstance(key, int):
            if not isinstance(node, list) or len(node) <= key:
                raise ValueError(f"missing index {key}")
            node = node[key]
            continue
        if not isinstance(node, dict) or key not in node:
            raise ValueError(f"missing key {key!r}")
        node = node[str(key)]
    return node


# -- feed-shaped kinds ------------------------------------------------------


def _parse_feedlike(
    source: Source, payload: str, fetched_ts: int, cap: int, *, prefer_link: bool
) -> Parsed:
    del fetched_ts
    entries = claude_worker.feeds.parse_feed_xml(request_url(source), payload)
    items: list[Item] = []
    for i in range(len(entries)):
        entry = entries[i]
        guid = (entry.link or entry.guid) if prefer_link else entry.guid
        if not guid:
            continue
        items.append(
            _mk(
                source,
                guid=guid,
                ts=entry.ts,
                title=entry.title,
                link=entry.link,
                text=entry.text,
                cap=cap,
            )
        )
    if not items:
        raise ValueError("feed: no usable entries")
    return Parsed(items, None, [])


def _parse_rss(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    return _parse_feedlike(source, payload, fetched_ts, cap, prefer_link=False)


def _parse_google_news(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    return _parse_feedlike(source, payload, fetched_ts, cap, prefer_link=True)


# -- gdelt ------------------------------------------------------------------


def _gdelt_ts(raw: object) -> int:
    if not isinstance(raw, str) or not raw:
        return 0
    try:
        stamp = datetime.datetime.strptime(raw, _GDELT_TS_FMT)
    except ValueError:
        return 0
    return int(stamp.replace(tzinfo=datetime.UTC).timestamp())


def _parse_gdelt(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    rows = _dicts(_at(_obj(payload), "articles"))
    items: list[Item] = []
    for i in range(len(rows)):
        row = rows[i]
        url = str(row.get("url", ""))
        if not url:
            continue
        items.append(
            _mk(
                source,
                guid=url,
                ts=_gdelt_ts(row.get("seendate")),
                title=str(row.get("title", "")),
                link=url,
                text=str(row.get("title", "")),
                cap=cap,
                # The publisher's domain, not GDELT's — corroboration counts
                # distinct publishers, and every GDELT item shares one host.
                origin=str(row.get("domain", "")).lower() or source.origin,
            )
        )
    if not items:
        raise ValueError("gdelt: no articles")
    return Parsed(items, None, [])


# -- venue announcements (class B) ------------------------------------------


HINT_OTHER: str = "other"
_OKX_ANN_HINTS: dict[str, str] = {
    "announcements-delistings": "delisting",
    "announcements-new-listings": "listing",
}


def _parse_okx_ann(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    rows = _dicts(_at(_obj(payload), "data", 0, "details"))
    items: list[Item] = []
    for i in range(len(rows)):
        row = rows[i]
        url = str(row.get("url", ""))
        title = str(row.get("title", ""))
        if not url:
            continue
        items.append(
            _mk(
                source,
                guid=url,
                ts=_epoch_s(row.get("pTime"), unit_ms=True),
                title=title,
                link=url,
                text=title,
                cap=cap,
                hint=_OKX_ANN_HINTS.get(str(row.get("annType", "")), HINT_OTHER),
            )
        )
    if not items:
        raise ValueError("okx announcements: no details")
    return Parsed(items, None, [])


def _parse_deribit_ann(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    rows = _dicts(_at(_obj(payload), "result"))
    items: list[Item] = []
    for i in range(len(rows)):
        row = rows[i]
        ident = row.get("id")
        if ident is None:
            continue
        items.append(
            _mk(
                source,
                guid=str(ident),
                ts=_epoch_s(row.get("publication_timestamp"), unit_ms=True),
                title=str(row.get("title", "")),
                link="",
                text=str(row.get("body", "")),
                cap=cap,
            )
        )
    if not items:
        raise ValueError("deribit announcements: no rows")
    return Parsed(items, None, [])


_BYBIT_HINTS: dict[str, str] = {
    "delistings": "delisting",
    "new_crypto": "listing",
    "listing": "listing",
    "maintenance_updates": "maintenance",
}


def _bybit_hint(row: typing.Mapping[str, object]) -> str:
    type_key = ""
    kind = row.get("type")
    if isinstance(kind, dict):
        type_key = str(kind.get("key", ""))
    hint = _BYBIT_HINTS.get(type_key, "")
    if hint:
        return hint
    tags = row.get("tags")
    if isinstance(tags, list):
        for i in range(len(tags)):
            hint = _BYBIT_HINTS.get(str(tags[i]), "")
            if hint:
                return hint
    return HINT_OTHER


def _parse_bybit_ann(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    rows = _dicts(_at(_obj(payload), "result", "list"))
    items: list[Item] = []
    for i in range(len(rows)):
        row = rows[i]
        url = str(row.get("url", ""))
        if not url:
            continue
        items.append(
            _mk(
                source,
                guid=url,
                ts=_epoch_s(row.get("publishTime") or row.get("dateTimestamp"), unit_ms=True),
                title=str(row.get("title", "")),
                link=url,
                text=str(row.get("description", "")),
                cap=cap,
                hint=_bybit_hint(row),
            )
        )
    if not items:
        raise ValueError("bybit announcements: no rows")
    return Parsed(items, None, [])


_BINANCE_ANN_URL: str = "https://www.binance.com/en/support/announcement/"


def _binance_cms_rows(doc: dict[str, object]) -> list[dict[str, object]]:
    """The endpoint is undocumented and has been seen in two shapes; both
    are accepted rather than letting a silent reshape stop the source."""
    data = _at(doc, "data")
    if not isinstance(data, dict):
        raise ValueError("binance cms: no data object")
    if "catalogs" in data:
        catalogs = _dicts(data["catalogs"])
        out: list[dict[str, object]] = []
        for i in range(len(catalogs)):
            articles = catalogs[i].get("articles")
            if isinstance(articles, list):
                out.extend(_dicts(articles))
        return out
    return _dicts(data.get("articles"))


def _parse_binance_cms(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    rows = _binance_cms_rows(_obj(payload))
    items: list[Item] = []
    for i in range(len(rows)):
        row = rows[i]
        code = str(row.get("code", ""))
        title = str(row.get("title", ""))
        if not code:
            continue
        items.append(
            _mk(
                source,
                guid=code,
                ts=_epoch_s(row.get("releaseDate"), unit_ms=True),
                title=title,
                link=_BINANCE_ANN_URL + code,
                text=title,
                cap=cap,
            )
        )
    if not items:
        raise ValueError("binance cms: no articles")
    return Parsed(items, None, [])


# -- class A: instrument sets -----------------------------------------------


def _keyed_snapshot(
    source: Source,
    fetched_ts: int,
    rows: list[dict[str, object]],
    id_key: str,
    field_keys: tuple[str, ...],
) -> Parsed:
    body: dict[str, object] = {}
    for i in range(len(rows)):
        row = rows[i]
        ident = row.get(id_key)
        if ident is None:
            continue
        values: list[object] = []
        for j in range(len(field_keys)):
            values.append(row.get(field_keys[j]))
        body[str(ident)] = values
    if not body:
        raise ValueError(f"no rows keyed by {id_key!r}")
    return Parsed([], _snapshot(source, fetched_ts, body), [])


_BN_USDM_KEYS: tuple[str, ...] = ("status", "contractType", "onboardDate", "deliveryDate")
_OKX_INST_KEYS: tuple[str, ...] = (
    "instType",
    "state",
    "listTime",
    "expTime",
    "preMktSwTime",
    "contTdSwTime",
)
_DERIBIT_INST_KEYS: tuple[str, ...] = (
    "kind",
    "instrument_type",
    "is_active",
    "creation_timestamp",
    "expiration_timestamp",
)
_COINBASE_KEYS: tuple[str, ...] = ("status", "trading_disabled")


def _parse_instruments_bn_usdm(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "symbols"))
    return _keyed_snapshot(source, fetched_ts, rows, "symbol", _BN_USDM_KEYS)


def _parse_instruments_okx(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "data"))
    return _keyed_snapshot(source, fetched_ts, rows, "instId", _OKX_INST_KEYS)


def _parse_instruments_deribit(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "result"))
    return _keyed_snapshot(source, fetched_ts, rows, "instrument_name", _DERIBIT_INST_KEYS)


def _parse_instruments_coinbase(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    return _keyed_snapshot(source, fetched_ts, _dicts(_arr(payload)), "id", _COINBASE_KEYS)


# -- class A: venue status --------------------------------------------------


def _window_key(row: typing.Mapping[str, object]) -> str:
    return f"{row.get('title', '')}|{row.get('begin', '')}"


def _parse_status_okx(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "data"))
    body: dict[str, object] = {}
    for i in range(len(rows)):
        row = rows[i]
        body[_window_key(row)] = [
            row.get("state"),
            row.get("begin"),
            row.get("end"),
            row.get("serviceType"),
            row.get("system"),
        ]
    # An EMPTY maintenance list is the venue's normal state and must still
    # produce a snapshot, else "all clear" would read as a parse failure.
    return Parsed([], _snapshot(source, fetched_ts, body), [])


def _parse_status_deribit(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    result = _at(_obj(payload), "result")
    if not isinstance(result, dict) or "locked" not in result:
        raise ValueError("deribit status: no locked field")
    body: dict[str, object] = {
        "locked": result.get("locked"),
        "locked_indices": result.get("locked_indices", []),
        "locked_currencies": result.get("locked_currencies", []),
    }
    return Parsed([], _snapshot(source, fetched_ts, body), [])


def _parse_status_bybit(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "result", "list"))
    body: dict[str, object] = {}
    for i in range(len(rows)):
        row = rows[i]
        body[_window_key(row)] = [
            row.get("state"),
            row.get("begin"),
            row.get("end"),
            row.get("serviceTypes"),
            row.get("product"),
            row.get("maintainType"),
        ]
    return Parsed([], _snapshot(source, fetched_ts, body), [])


def _parse_status_kraken(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    result = _at(_obj(payload), "result")
    if not isinstance(result, dict) or "status" not in result:
        raise ValueError("kraken status: no status")
    # The venue's own `timestamp` is DELIBERATELY not kept: it changes on
    # every poll, so a body carrying it has a fresh sha256 every 60 s and
    # the §8.1 "unchanged snapshot is not stored again" rule could never
    # fire for this source (measured 2026-09-19 — kraken-status was the one
    # class-A source of thirteen that stored a second row across two
    # cycles). The snapshot is the venue's STATE; when it was read is
    # already `taken_ts`.
    body: dict[str, object] = {"status": result.get("status")}
    return Parsed([], _snapshot(source, fetched_ts, body), [])


def _parse_ping_binance(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    doc = _obj(payload)
    if doc:
        # Binance's ping answers `{}`; anything else is an error envelope.
        raise ValueError("binance ping: non-empty body")
    # Liveness only — but it must be a NON-empty Parsed, because an empty
    # Parsed is this module's "shape deviation" signal (§5.2).
    return Parsed([], _snapshot(source, fetched_ts, {"ok": 1}), [])


# -- class A: calendars -----------------------------------------------------


_FED_EVENT_KEYS: tuple[str, ...] = ("month", "days", "time", "type", "title", "description")
#: Envelope keys tried in order before falling back to the longest array of
#: objects. Measured 2026-09-19: the live key is ``events`` and the document
#: ALSO carries a shorter ``announcement`` array, so "the first array wins"
#: would pick the wrong one.
_FED_ENVELOPE_KEYS: tuple[str, ...] = ("events", "mtgs")


def _longest_object_array(doc: dict[str, object]) -> list[dict[str, object]]:
    best: list[dict[str, object]] = []
    for key in sorted(doc):
        node = doc[key]
        if isinstance(node, list) and node and isinstance(node[0], dict):
            rows = _dicts(node)
            if len(rows) > len(best):
                best = rows
    return best


def _fed_events(doc: object) -> list[dict[str, object]]:
    """The Fed's event array. Its envelope key has moved before, so the known
    names are tried first and the longest array of objects is the fallback."""
    if isinstance(doc, list):
        return _dicts(doc)
    if not isinstance(doc, dict):
        raise ValueError("fed calendar: not a list or object")
    for i in range(len(_FED_ENVELOPE_KEYS)):
        node = doc.get(_FED_ENVELOPE_KEYS[i])
        if isinstance(node, list) and node:
            return _dicts(node)
    rows = _longest_object_array(doc)
    if not rows:
        raise ValueError("fed calendar: no event array")
    return rows


def _fed_key(row: typing.Mapping[str, object], index: int) -> str:
    stamp = f"{row.get('month', '')}-{row.get('days', '')}T{row.get('time', '')}"
    title = str(row.get("title", ""))
    return f"{stamp}|{title}" if title else f"{stamp}|#{index}"


def _parse_calendar_fed(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _fed_events(json.loads(payload))
    body: dict[str, object] = {}
    for i in range(len(rows)):
        row = rows[i]
        kept: dict[str, object] = {}
        for j in range(len(_FED_EVENT_KEYS)):
            key = _FED_EVENT_KEYS[j]
            if key in row:
                kept[key] = row[key]
        body[_fed_key(row, i)] = kept
    if not body:
        raise ValueError("fed calendar: empty")
    return Parsed([], _snapshot(source, fetched_ts, body), [])


# -- 4chan ------------------------------------------------------------------


def _parse_4chan(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del fetched_ts
    pages = _dicts(_arr(payload))
    items: list[Item] = []
    for i in range(len(pages)):
        threads = pages[i].get("threads")
        if not isinstance(threads, list):
            continue
        rows = _dicts(threads)
        for j in range(len(rows)):
            row = rows[j]
            replies = _number(row.get("replies")) or 0.0
            if replies < FOURCHAN_MIN_REPLIES:
                continue
            com = strip_text(str(row.get("com", "")), cap)
            items.append(
                _mk(
                    source,
                    guid=str(row.get("no", "")),
                    ts=_epoch_s(row.get("time"), unit_ms=False),
                    title=str(row.get("sub", "")) or com[:FOURCHAN_TITLE_CAP],
                    link="",
                    text=com,
                    cap=cap,
                )
            )
    if not items:
        raise ValueError("4chan: no thread above the reply floor")
    return Parsed(items, None, [])


# -- class D: numeric series ------------------------------------------------


_BINANCE_SERIES_KEYS: dict[str, str] = {
    "series-bn-ls": "longShortRatio",
    "series-bn-top-ls": "longShortRatio",
    "series-bn-taker": "buySellRatio",
    "series-bn-oi": "sumOpenInterestValue",
}


def _one(key: str, ts: int, value: float) -> list[tuple[str, int, float]]:
    return [(key, ts, value)]


def _parse_binance_series(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    key = _BINANCE_SERIES_KEYS[source.kind]
    rows = _dicts(_arr(payload))
    if not rows:
        raise ValueError("binance series: empty")
    row = rows[-1]
    value = _number(row.get(key))
    if value is None:
        raise ValueError(f"binance series: no {key}")
    ts = _epoch_s(row.get("timestamp"), unit_ms=True) or fetched_ts
    return Parsed([], None, _one(key, ts, value))


def _parse_dvol(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _at(_obj(payload), "result", "data")
    if not isinstance(rows, list) or not rows:
        raise ValueError("dvol: empty data")
    row = rows[-1]
    if not isinstance(row, list) or len(row) < OHLC_ROW_LEN:
        raise ValueError("dvol: row is not [ts, o, h, l, c]")
    close = _number(row[OHLC_CLOSE_IDX])
    if close is None:
        raise ValueError("dvol: no close")
    return Parsed([], None, _one("dvol", _epoch_s(row[0], unit_ms=True) or fetched_ts, close))


_COT_NEEDLE: str = "bitcoin"
_COT_LONG: str = "noncomm_positions_long_all"
_COT_SHORT: str = "noncomm_positions_short_all"


def _cot_net(row: typing.Mapping[str, object]) -> float | None:
    long_side = _number(row.get(_COT_LONG))
    short_side = _number(row.get(_COT_SHORT))
    return None if long_side is None or short_side is None else long_side - short_side


def _parse_cot(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_arr(payload))
    fallback: float | None = None
    for i in range(len(rows)):
        name = str(rows[i].get("market_and_exchange_names", "")).lower()
        if _COT_NEEDLE not in name:
            continue
        net = _cot_net(rows[i])
        if net is None:
            continue
        # "MICRO BITCOIN - CHICAGO MERCANTILE EXCHANGE" carries the needle too;
        # the full-size contract is the one the series means.
        if name.startswith(_COT_NEEDLE):
            return Parsed([], None, _one("noncomm_net", fetched_ts, net))
        if fallback is None:
            fallback = net
    if fallback is None:
        raise ValueError("cot: no bitcoin row")
    return Parsed([], None, _one("noncomm_net", fetched_ts, fallback))


def _parse_fng(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    rows = _dicts(_at(_obj(payload), "data"))
    if not rows:
        raise ValueError("fng: empty")
    value = _number(rows[0].get("value"))
    if value is None:
        raise ValueError("fng: no value")
    ts = _epoch_s(rows[0].get("timestamp"), unit_ms=False) or fetched_ts
    return Parsed([], None, _one("fng", ts, value))


def _parse_stables(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    assets = _dicts(_at(_obj(payload), "peggedAssets"))
    total = 0.0
    seen = False
    for i in range(len(assets)):
        circulating = assets[i].get("circulating")
        if not isinstance(circulating, dict):
            continue
        value = _number(circulating.get("peggedUSD"))
        if value is not None:
            total += value
            seen = True
    if not seen:
        raise ValueError("stables: no circulating totals")
    return Parsed([], None, _one("pegged_usd", fetched_ts, total))


def _parse_mempool_fees(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    value = _number(_obj(payload).get("fastestFee"))
    if value is None:
        raise ValueError("mempool: no fastestFee")
    return Parsed([], None, _one("fastestFee", fetched_ts, value))


def _parse_blockchain_stats(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    value = _number(_obj(payload).get("hash_rate"))
    if value is None:
        raise ValueError("blockchain stats: no hash_rate")
    return Parsed([], None, _one("hash_rate", fetched_ts, value))


def _parse_kalshi(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    markets = _at(_obj(payload), "markets")
    if not isinstance(markets, list):
        raise ValueError("kalshi: markets is not a list")
    return Parsed([], None, _one("market_count", fetched_ts, float(len(markets))))


def _parse_manifold(source: Source, payload: str, fetched_ts: int, cap: int) -> Parsed:
    del cap
    return Parsed([], None, _one("market_count", fetched_ts, float(len(_arr(payload)))))


# -- dispatch ---------------------------------------------------------------

_Parser = typing.Callable[[Source, str, int, int], Parsed]

PARSERS: dict[str, _Parser] = {
    "rss": _parse_rss,
    "google-news": _parse_google_news,
    "hnrss": _parse_rss,
    "reddit-rss": _parse_rss,
    "arxiv-api": _parse_rss,
    "gdelt": _parse_gdelt,
    "json-okx-ann": _parse_okx_ann,
    "json-deribit-ann": _parse_deribit_ann,
    "json-bybit-ann": _parse_bybit_ann,
    "json-binance-cms": _parse_binance_cms,
    "instruments-bn-usdm": _parse_instruments_bn_usdm,
    "instruments-okx": _parse_instruments_okx,
    "instruments-deribit": _parse_instruments_deribit,
    "instruments-coinbase": _parse_instruments_coinbase,
    "status-okx": _parse_status_okx,
    "status-deribit": _parse_status_deribit,
    "status-bybit": _parse_status_bybit,
    "status-kraken": _parse_status_kraken,
    "ping-binance": _parse_ping_binance,
    "calendar-fed": _parse_calendar_fed,
    "json-4chan-catalog": _parse_4chan,
    "series-bn-ls": _parse_binance_series,
    "series-bn-top-ls": _parse_binance_series,
    "series-bn-taker": _parse_binance_series,
    "series-bn-oi": _parse_binance_series,
    "series-dvol": _parse_dvol,
    "series-cftc-cot": _parse_cot,
    "series-fng": _parse_fng,
    "series-defillama-stables": _parse_stables,
    "series-mempool-fees": _parse_mempool_fees,
    "series-blockchain-stats": _parse_blockchain_stats,
    "series-kalshi": _parse_kalshi,
    "series-manifold": _parse_manifold,
}


def parse(
    source: Source,
    payload: str,
    fetched_ts: int,
    *,
    text_cap: int = claude_worker.feeds.TEXT_CAP,
) -> Parsed:
    """Parse one payload for ``source.kind``. NEVER raises.

    Any shape deviation — bad JSON, a moved key, a list where an object was
    promised — is an empty [`Parsed`], which the caller counts as
    ``parse_empty``. That is the whole failure doctrine: a venue that
    reshapes its wire makes this lane quiet, not broken.
    """
    parser = PARSERS.get(source.kind)
    if parser is None:
        return _EMPTY
    try:
        return parser(source, strip_bom(payload), fetched_ts, text_cap)
    except (ValueError, TypeError, KeyError, IndexError, AttributeError, OverflowError):
        return _EMPTY


# ------------------------------------------------ fixture reduction (App. B)


@dataclasses.dataclass(frozen=True, slots=True)
class _Reduction:
    """How to shrink one kind's payload to the keyed fields only.

    ``paths`` are candidates tried in order (an endpoint with two known
    shapes lists both); the matched node is truncated/filtered and the
    original envelope is rebuilt around it, so the reduction is still a
    payload THIS kind's parser eats.
    """

    mode: str
    paths: tuple[tuple[object, ...], ...] = ((),)
    keys: tuple[str, ...] = ()
    max_rows: int = FIXTURE_MAX_ROWS
    select: str = ""


_FEED: _Reduction = _Reduction("feed")
_GDELT_KEYS: tuple[str, ...] = ("url", "title", "seendate", "domain", "language", "sourcecountry")
_BYBIT_ANN_KEYS: tuple[str, ...] = (
    "title",
    "description",
    "type",
    "tags",
    "url",
    "dateTimestamp",
    "publishTime",
    "startDateTimestamp",
    "endDateTimestamp",
)
_STATUS_OKX_KEYS: tuple[str, ...] = ("title", "state", "begin", "end", "serviceType", "system")
_STATUS_BYBIT_KEYS: tuple[str, ...] = (
    "title",
    "state",
    "begin",
    "end",
    "serviceTypes",
    "product",
    "maintainType",
)
_COT_KEYS: tuple[str, ...] = (
    "market_and_exchange_names",
    _COT_LONG,
    _COT_SHORT,
    "report_date_as_yyyy_mm_dd",
)

_REDUCTIONS: dict[str, _Reduction] = {
    "rss": _FEED,
    "google-news": _FEED,
    "hnrss": _FEED,
    "reddit-rss": _FEED,
    "arxiv-api": _FEED,
    "gdelt": _Reduction("list", (("articles",),), _GDELT_KEYS),
    "json-okx-ann": _Reduction(
        "list", (("data", 0, "details"),), ("annType", "title", "url", "pTime")
    ),
    "json-deribit-ann": _Reduction(
        "list", (("result",),), ("id", "title", "body", "publication_timestamp")
    ),
    "json-bybit-ann": _Reduction("list", (("result", "list"),), _BYBIT_ANN_KEYS),
    "json-binance-cms": _Reduction(
        "list",
        (("data", "catalogs", 0, "articles"), ("data", "articles")),
        ("title", "code", "releaseDate"),
    ),
    "instruments-bn-usdm": _Reduction("list", (("symbols",),), ("symbol", *_BN_USDM_KEYS)),
    "instruments-okx": _Reduction("list", (("data",),), ("instId", *_OKX_INST_KEYS)),
    "instruments-deribit": _Reduction(
        "list", (("result",),), ("instrument_name", *_DERIBIT_INST_KEYS)
    ),
    "instruments-coinbase": _Reduction("list", ((),), ("id", *_COINBASE_KEYS)),
    "status-okx": _Reduction("list", (("data",),), _STATUS_OKX_KEYS),
    "status-deribit": _Reduction(
        "object", (("result",),), ("locked", "locked_indices", "locked_currencies")
    ),
    "status-bybit": _Reduction("list", (("result", "list"),), _STATUS_BYBIT_KEYS),
    "status-kraken": _Reduction("object", (("result",),), ("status", "timestamp")),
    "ping-binance": _Reduction("object", ((),), ()),
    "calendar-fed": _Reduction("list", (("events",), ("mtgs",), ()), _FED_EVENT_KEYS),
    "json-4chan-catalog": _Reduction(
        "list", ((0, "threads"),), ("no", "sub", "com", "time", "replies")
    ),
    "series-bn-ls": _Reduction("list", ((),), ("longShortRatio", "timestamp"), max_rows=1),
    "series-bn-top-ls": _Reduction("list", ((),), ("longShortRatio", "timestamp"), max_rows=1),
    "series-bn-taker": _Reduction("list", ((),), ("buySellRatio", "timestamp"), max_rows=1),
    "series-bn-oi": _Reduction("list", ((),), ("sumOpenInterestValue", "timestamp"), max_rows=1),
    "series-dvol": _Reduction("list", (("result", "data"),), (), max_rows=3),
    "series-cftc-cot": _Reduction("list", ((),), _COT_KEYS, max_rows=3, select=_COT_NEEDLE),
    "series-fng": _Reduction("list", (("data",),), ("value", "timestamp"), max_rows=1),
    "series-defillama-stables": _Reduction(
        "list", (("peggedAssets",),), ("name", "circulating"), max_rows=5
    ),
    "series-mempool-fees": _Reduction("object", ((),), ("fastestFee",)),
    "series-blockchain-stats": _Reduction("object", ((),), ("hash_rate",)),
    "series-kalshi": _Reduction("list", (("markets",),), ("ticker",)),
    "series-manifold": _Reduction("list", ((),), ("id",)),
}


def fixture_suffix(kind: str) -> str:
    """``.xml`` for the feed-shaped kinds, ``.json`` for the rest."""
    return ".xml" if _REDUCTIONS[kind].mode == "feed" else ".json"


def _reduce_feed(payload: str, cap: int) -> str:
    """Re-emit the feed as minimal RSS 2.0 carrying only the keyed elements.

    ``parse_feed_xml`` reads RSS and Atom alike, so one shape serves all five
    feed kinds and the reduced fixture yields the same items as the original.

    Text is stripped before it is escaped: ``parse_feed_xml`` cuts at
    ``TEXT_CAP`` and its reader strips, so a cut that lands on whitespace
    would make the reduction lose a trailing space on the NEXT pass — i.e.
    not a fixed point, which is what proves a fixture is fully reduced.
    """
    entries = claude_worker.feeds.parse_feed_xml("", payload)[:FIXTURE_MAX_ROWS]
    parts: list[str] = ['<?xml version="1.0" encoding="UTF-8"?>', '<rss version="2.0"><channel>']
    for i in range(len(entries)):
        entry = entries[i]
        stamp = ""
        if entry.ts:
            moment = datetime.datetime.fromtimestamp(entry.ts, tz=datetime.UTC)
            stamp = email.utils.format_datetime(moment)
        parts.append(
            "<item>"
            f"<guid>{xml.sax.saxutils.escape(entry.guid)}</guid>"
            f"<link>{xml.sax.saxutils.escape(entry.link)}</link>"
            f"<title>{xml.sax.saxutils.escape(entry.title)}</title>"
            f"<pubDate>{stamp}</pubDate>"
            f"<description>{xml.sax.saxutils.escape(entry.text[:cap].strip())}</description>"
            "</item>"
        )
    parts.append("</channel></rss>")
    return "".join(parts)


def _navigate(doc: object, path: tuple[object, ...]) -> object | None:
    node = doc
    for i in range(len(path)):
        key = path[i]
        if isinstance(key, int):
            if not isinstance(node, list) or len(node) <= key:
                return None
            node = node[key]
            continue
        if not isinstance(node, dict) or key not in node:
            return None
        node = node[str(key)]
    return node


def _rebuild(path: tuple[object, ...], node: object) -> object:
    for i in range(len(path) - 1, -1, -1):
        node = [node] if isinstance(path[i], int) else {str(path[i]): node}
    return node


def _keep(row: object, keys: tuple[str, ...]) -> object:
    if not keys or not isinstance(row, dict):
        return row
    kept: dict[str, object] = {}
    for j in range(len(keys)):
        if keys[j] in row:
            kept[keys[j]] = row[keys[j]]
    return kept


def _select_rows(rows: list[object], red: _Reduction) -> list[object]:
    ordered = list(rows)
    if red.select:
        first: list[object] = []
        rest: list[object] = []
        for i in range(len(ordered)):
            if red.select in json.dumps(ordered[i], default=str).lower():
                first.append(ordered[i])
            else:
                rest.append(ordered[i])
        ordered = first + rest
    ordered = ordered[: red.max_rows]
    out: list[object] = []
    for i in range(len(ordered)):
        out.append(_keep(ordered[i], red.keys))
    return out


def _reduce_at(node: object, path: tuple[object, ...], red: _Reduction) -> str | None:
    if red.mode == "object":
        if not isinstance(node, dict):
            return None
        kept = _keep(node, red.keys) if red.keys else node
        return json.dumps(_rebuild(path, kept), separators=(",", ":"), sort_keys=True)
    if not isinstance(node, list):
        return None
    rows = _select_rows(node, red)
    return json.dumps(_rebuild(path, rows), separators=(",", ":"), sort_keys=True)


def reduce_payload(kind: str, payload: str, cap: int = claude_worker.feeds.TEXT_CAP) -> str:
    """The recorded-fixture body: the keyed fields only, same shape as live.

    Appendix B's law — a fixture is a TEST, not research, so it carries no
    raw payload and no field the parser does not read.
    """
    red = _REDUCTIONS.get(kind)
    if red is None:
        raise ValueError(f"no reduction for kind {kind!r}")
    if red.mode == "feed":
        return _reduce_feed(strip_bom(payload), cap)
    doc = json.loads(strip_bom(payload))
    for i in range(len(red.paths)):
        path = red.paths[i]
        node = _navigate(doc, path)
        if node is None:
            continue
        out = _reduce_at(node, path, red)
        if out is not None:
            return out
    raise ValueError(f"reduce_payload: no candidate path matched for kind {kind!r}")
