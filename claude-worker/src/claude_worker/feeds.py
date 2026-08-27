# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""news_watcher (design §5.1): httpx RSS/Atom polling, SQLite dedupe,
triage → escalate → label pipeline over an INJECTED ``complete_fn``.

Mechanics vs brains are split deliberately (§6 ``fetch --news``): the
fetch/parse/dedupe layer is pure mechanics reusable by the verb path
(where the SESSION is the triage brain and no LLM runs here), while
[`NewsWatcher.poll_once`] adds the LLM steps for serve mode. ``llm.py``
does not exist until item 11 — nothing here may import an SDK; the model
is reached only through ``complete_fn(model, prompt) -> str`` and every
call goes through the §5.3 prompt cache (``state.cached_complete``).

Failure doctrine (§5.1): a poll cycle NEVER raises out of the watcher —
transport errors, unparseable XML, and malformed model output all
degrade to counted no-ops. The daemon loop must survive any feed and any
model on any day.

Cadence: each feed is polled on its own 15-60 s schedule with jitter
(uniform draw per §5.1), off the injected monotonic clock — cheap to
test, no wall-clock coupling.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import datetime
import email.utils
import random
import time
import typing
import xml.etree.ElementTree

import httpx

import claude_worker.config
import claude_worker.labeling
import claude_worker.state

# §5.1: 15-60 s per-feed cadence with jitter.
POLL_MIN_NS: int = 15_000_000_000
POLL_MAX_NS: int = 60_000_000_000

# Impacts that escalate triaged items to Sonnet labeling.
ESCALATE_IMPACTS: tuple[str, ...] = ("med", "high")

# Prompt-budget cap on item text handed to the models.
TEXT_CAP: int = 2_000

_FETCH_TIMEOUT_S: float = 10.0

EVENT_TRIAGE_MALFORMED: str = "triage_malformed"
EVENT_LABEL_MALFORMED: str = "label_malformed"


class FeedItem(typing.NamedTuple):
    """One mechanically extracted feed entry (the §6 items-NDJSON shape:
    id=guid, feed, ts, title, link, text)."""

    feed: str
    guid: str
    ts: int
    title: str
    link: str
    text: str


def _local_name(tag: str) -> str:
    """Element tag without its XML namespace."""
    return tag.rsplit("}", 1)[-1]


def _child_text(elem: xml.etree.ElementTree.Element, name: str) -> str:
    for child in elem:
        if _local_name(child.tag) == name and child.text:
            return child.text.strip()
    return ""


def _atom_link(elem: xml.etree.ElementTree.Element) -> str:
    for child in elem:
        if _local_name(child.tag) == "link":
            href = child.get("href")
            if href:
                return href.strip()
    return ""


def _parse_ts(rss_pubdate: str, iso_stamp: str) -> int:
    """Best-effort epoch seconds from RFC-2822 (RSS) or ISO-8601 (Atom);
    0 when absent or unparseable — timestamps are advisory here, dedupe
    keys on (feed, guid)."""
    if rss_pubdate:
        try:
            return int(email.utils.parsedate_to_datetime(rss_pubdate).timestamp())
        except ValueError, TypeError:
            return 0
    if iso_stamp:
        try:
            return int(datetime.datetime.fromisoformat(iso_stamp).timestamp())
        except ValueError:
            return 0
    return 0


def parse_feed_xml(feed_url: str, payload: str) -> list[FeedItem]:
    """RSS 2.0 ``<item>`` and Atom ``<entry>`` extraction, namespace-
    tolerant. Entries with no usable id (guid → link fallback) are
    dropped. Unparseable XML is an empty list, never an exception."""
    try:
        root = xml.etree.ElementTree.fromstring(payload)
    except xml.etree.ElementTree.ParseError:
        return []
    items: list[FeedItem] = []
    for elem in root.iter():
        name = _local_name(elem.tag)
        if name == "item":  # RSS 2.0
            guid = _child_text(elem, "guid") or _child_text(elem, "link")
            if not guid:
                continue
            items.append(
                FeedItem(
                    feed=feed_url,
                    guid=guid,
                    ts=_parse_ts(_child_text(elem, "pubDate"), ""),
                    title=_child_text(elem, "title"),
                    link=_child_text(elem, "link"),
                    text=_child_text(elem, "description")[:TEXT_CAP],
                )
            )
        elif name == "entry":  # Atom
            guid = _child_text(elem, "id") or _atom_link(elem)
            if not guid:
                continue
            stamp = _child_text(elem, "updated") or _child_text(elem, "published")
            text = _child_text(elem, "summary") or _child_text(elem, "content")
            items.append(
                FeedItem(
                    feed=feed_url,
                    guid=guid,
                    ts=_parse_ts("", stamp),
                    title=_child_text(elem, "title"),
                    link=_atom_link(elem),
                    text=text[:TEXT_CAP],
                )
            )
    return items


def fetch_feed(client: httpx.Client, url: str) -> list[FeedItem] | None:
    """One mechanical feed fetch+parse. ``None`` means the transport
    failed or the server answered non-2xx (callers count it); parse
    failures are an empty list. Never raises."""
    try:
        response = client.get(url, timeout=_FETCH_TIMEOUT_S)
    except httpx.HTTPError:
        return None
    if response.status_code != httpx.codes.OK:
        return None
    return parse_feed_xml(url, response.text)


def dedupe_items(
    state: claude_worker.state.State,
    items: typing.Iterable[FeedItem],
) -> tuple[list[FeedItem], int]:
    """Filter to first sightings via ``state.mark_seen`` (§5.3 dedupe).
    Returns ``(new_items, duplicates)``."""
    fresh: list[FeedItem] = []
    dups = 0
    for item in items:
        if state.mark_seen(item.feed, item.guid, item.ts):
            fresh.append(item)
        else:
            dups += 1
    return fresh, dups


@dataclasses.dataclass(slots=True)
class PollStats:
    """Counters for one [`NewsWatcher.poll_once`] cycle (§5.1 "counted").
    ``labels`` is the cycle's actionable output — the daemon hands them
    to the commander."""

    polled_feeds: int = 0
    fetch_errors: int = 0
    new_items: int = 0
    dup_items: int = 0
    triage_malformed: int = 0
    escalated: int = 0
    label_malformed: int = 0
    label_passes: int = 0
    cache_hits: int = 0
    labels: list[claude_worker.labeling.Label] = dataclasses.field(default_factory=list)


class NewsWatcher:
    """Per-feed scheduled polling + the serve-mode LLM pipeline.

    Single-threaded by design; owned and driven by ``daemon.py`` (item
    11). The verb path (item 12) uses only the mechanical layer above.
    """

    def __init__(  # noqa: PLR0913 — composition root: every collaborator injected (§11 testability)
        self,
        *,
        state: claude_worker.state.State,
        feeds: tuple[str, ...],
        symbol_map: dict[str, int],
        complete_fn: typing.Callable[[str, str], str],
        http_client: httpx.Client,
        rng: random.Random | None = None,
        clock_ns: typing.Callable[[], int] = time.monotonic_ns,
    ) -> None:
        self._state = state
        self._feeds = feeds
        self._symbol_map = symbol_map
        self._markets: tuple[str, ...] = tuple(sorted(symbol_map))
        self._complete_fn = complete_fn
        self._http = http_client
        self._rng: random.Random = random.Random() if rng is None else rng
        self._clock = clock_ns
        # First poll is due immediately; jitter starts after it.
        self._next_due_ns: dict[str, int] = {url: 0 for url in feeds}

    def _reschedule(self, url: str, now_ns: int) -> None:
        interval = self._rng.randint(POLL_MIN_NS, POLL_MAX_NS)
        self._next_due_ns[url] = now_ns + interval

    def due_feeds(self, now_ns: int) -> list[str]:
        """Feeds whose next poll is due at ``now_ns`` (config order)."""
        return [url for url in self._feeds if self._next_due_ns[url] <= now_ns]

    def next_due_ns(self, url: str) -> int:
        """Scheduled next poll for one feed (test/diagnostic surface)."""
        return self._next_due_ns[url]

    def poll_once(self, now_ns: int | None = None) -> PollStats:
        """Poll every due feed and run new items through triage →
        escalate → label. Never raises (§5.1 doctrine)."""
        now = self._clock() if now_ns is None else now_ns
        stats = PollStats()
        for url in self.due_feeds(now):
            self._reschedule(url, now)
            stats.polled_feeds += 1
            items = fetch_feed(self._http, url)
            if items is None:
                stats.fetch_errors += 1
                continue
            fresh, dups = dedupe_items(self._state, items)
            stats.new_items += len(fresh)
            stats.dup_items += dups
            for item in fresh:
                self._process_item(item, stats)
        return stats

    def _process_item(self, item: FeedItem, stats: PollStats) -> None:
        """Triage (Haiku) → escalate (Sonnet label) for one fresh item.
        Every model call runs through the §5.3 prompt cache."""
        triage_prompt = claude_worker.labeling.build_triage_prompt(item.title, item.text)
        raw, hit = self._state.cached_complete(
            claude_worker.config.MODEL_BULK,
            claude_worker.labeling.TRIAGE_PROMPT_VERSION,
            triage_prompt,
            self._complete_fn,
        )
        stats.cache_hits += 1 if hit else 0
        triage = claude_worker.labeling.parse_triage(raw)
        if triage is None:
            stats.triage_malformed += 1
            self._state.record_event(EVENT_TRIAGE_MALFORMED, f"feed={item.feed} guid={item.guid}")
            return
        if triage.impact not in ESCALATE_IMPACTS:
            return
        stats.escalated += 1
        label_prompt = claude_worker.labeling.build_label_prompt(
            item.title, item.text, self._markets
        )
        raw, hit = self._state.cached_complete(
            claude_worker.config.MODEL_REASONING,
            claude_worker.labeling.LABEL_PROMPT_VERSION,
            label_prompt,
            self._complete_fn,
        )
        stats.cache_hits += 1 if hit else 0
        label, malformed = claude_worker.labeling.parse_label(raw, self._symbol_map)
        if malformed:
            stats.label_malformed += 1
            self._state.record_event(EVENT_LABEL_MALFORMED, f"feed={item.feed} guid={item.guid}")
            return
        if label is None:
            stats.label_passes += 1
            return
        stats.labels.append(label)
