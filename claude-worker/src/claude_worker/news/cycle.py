# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""One aggregation pass: fetch due sources, parse, tier 0, store (spec §12).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

This is the ONE function the `cycle` lane and `serve` both call — §13 names
it ``news.cycle.aggregate_once``, so it lives here rather than in
``__main__``: a daemon importing a package's ``__main__`` to reach its work
loop is how a lane and a daemon drift apart.

No model is reached from here, ever. Tier 0 is free, and everything this
function writes is the evidence a later tier is allowed to spend on.

Wall budget: a cycle stops TAKING new sources at ``deadline_ns`` and counts
what it skipped. It never abandons a fetch in flight and never waits — the
launchd slot is 60 s and the next one re-polls whatever was missed, because
every source carries its own due time.
"""

import dataclasses
import time
import typing

import claude_worker.news.filter
import claude_worker.news.sources
import claude_worker.news.store

#: The `cycle` lane's wall budget and the point at which it stops taking new
#: sources (spec §12): 30 s total, no new source after 25 s.
CYCLE_BUDGET_S: float = 30.0
CYCLE_TAKE_UNTIL_S: float = 25.0
NS_PER_S: int = 1_000_000_000
SECONDS_PER_HOUR: int = 3_600
#: An enabled source this many consecutive failures deep fails `health`.
ERR_STREAK_ALERT: int = 10


@dataclasses.dataclass(slots=True)
class CycleStats:
    """Everything one pass did, in the shape the lane prints and the
    dashboard reads."""

    sources: int = 0
    ok: int = 0
    items_new: int = 0
    items_dup: int = 0
    passed: int = 0
    dup: int = 0
    vocab: int = 0
    weight: int = 0
    cap: int = 0
    snapshots: int = 0
    series: int = 0
    events: int = 0
    resolved: int = 0
    parse_empty: int = 0
    refused_origin: int = 0
    budget: int = 0
    paced: int = 0
    http: int = 0
    transport: int = 0
    deadline_skipped: int = 0
    elapsed_ms: int = 0

    def line(self) -> str:
        return (
            f"news cycle: sources={self.sources} ok={self.ok} items_new={self.items_new} "
            f"pass={self.passed} dup={self.dup} vocab={self.vocab} weight={self.weight} "
            f"cap={self.cap} events={self.events} resolved={self.resolved} "
            f"elapsed_ms={self.elapsed_ms}"
        )

    def detail(self) -> str:
        return (
            f"  parse_empty={self.parse_empty} refused_origin={self.refused_origin} "
            f"budget={self.budget} paced={self.paced} http={self.http} "
            f"transport={self.transport} skipped_deadline={self.deadline_skipped} "
            f"snapshots={self.snapshots} series={self.series} dup_items={self.items_dup}"
        )


_STATUS_COUNTER: dict[str, str] = {
    claude_worker.news.sources.FETCH_STATUS_REFUSED_ORIGIN: "refused_origin",
    claude_worker.news.sources.FETCH_STATUS_BUDGET: "budget",
    claude_worker.news.sources.FETCH_STATUS_PACED: "paced",
    claude_worker.news.sources.FETCH_STATUS_HTTP: "http",
    claude_worker.news.sources.FETCH_STATUS_TRANSPORT: "transport",
}

_VERDICT_COUNTER: dict[str, str] = {
    claude_worker.news.filter.TIER0_PASS: "passed",
    claude_worker.news.filter.TIER0_DROP_DUP: "dup",
    claude_worker.news.filter.TIER0_DROP_VOCAB: "vocab",
    claude_worker.news.filter.TIER0_DROP_WEIGHT: "weight",
    claude_worker.news.filter.TIER0_DROP_CAP: "cap",
}


def register_sources(
    store: claude_worker.news.store.Store, registry: claude_worker.news.sources.Registry
) -> None:
    """Make sure every configured source has a row. Counters are never
    reset — a registry edit must not erase the poll history the N1 gate
    reads."""
    for i in range(len(registry.sources)):
        source = registry.sources[i]
        store.upsert_source(source.name, source.kind, source.class_, source.origin, source.enabled)


def load_recent(
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    now_ts: int,
) -> tuple[claude_worker.news.filter.RecentTitles, dict[str, int]]:
    """Seed the near-dup ring and the per-source hourly allowance from what
    the store already holds.

    One query serves both: the ring wants the recent SURVIVORS, and the
    hourly cap counts survivors too — the cap exists to bound tier-1 spend,
    and only a survivor spends.
    """
    window = max(registry.settings.near_dup_window_s, SECONDS_PER_HOUR)
    rows = store.items_since(now_ts - window, claude_worker.news.filter.TIER0_PASS)
    recent = claude_worker.news.filter.RecentTitles()
    used_this_hour: dict[str, int] = {}
    hour_start = now_ts - SECONDS_PER_HOUR
    for i in range(len(rows)):
        row = rows[i]
        ts = int(typing.cast(int, row["ts"]))
        source = str(row["source"])
        recent.add(
            ts,
            f"{source}|{row['guid']}",
            claude_worker.news.filter.tokens(str(row["title"])),
            str(row["story_id"]),
        )
        if ts >= hour_start:
            used_this_hour[source] = used_this_hour.get(source, 0) + 1
    return recent, used_this_hour


def build_caps(
    registry: claude_worker.news.sources.Registry,
    used_this_hour: dict[str, int],
    tier1_remaining: int,
) -> claude_worker.news.filter.Caps:
    remaining: dict[str, int] = {}
    for i in range(len(registry.sources)):
        source = registry.sources[i]
        remaining[source.name] = max(
            0, source.items_per_hour - used_this_hour.get(source.name, 0)
        )
    return claude_worker.news.filter.Caps(
        per_source_remaining=remaining, tier1_remaining=max(0, tier1_remaining)
    )


def _bump(stats: CycleStats, name: str) -> None:
    setattr(stats, name, getattr(stats, name) + 1)


def _store_item(
    store: claude_worker.news.store.Store,
    item: claude_worker.news.sources.Item,
    verdict: claude_worker.news.filter.Tier0Verdict,
    fetched_ts: int,
) -> bool:
    """Insert a first sighting with its verdict. ``False`` = already stored,
    which is tier 0 step 1 and is NOT a verdict."""
    inserted = store.upsert_item(
        source=item.source,
        guid=item.guid,
        ts=item.ts or fetched_ts,
        fetched_ts=fetched_ts,
        title=item.title,
        link=item.link,
        text=item.text,
        origin=item.origin,
        class_=item.class_,
        weight=item.weight,
        venue=item.venue,
        tier0=verdict.kind,
        vocab_hits=max(0, verdict.hits),
    )
    if inserted and verdict.kind == claude_worker.news.filter.TIER0_DROP_DUP:
        store.mark_tier0(item.source, item.guid, verdict.kind, verdict.dup_of)
    if inserted and verdict.kind == claude_worker.news.filter.TIER0_DROP_CAP:
        store.set_triage_state(item.source, item.guid, claude_worker.news.store.STATE_SKIPPED)
    return inserted


def _ingest(  # noqa: PLR0913 — the whole tier-0 pipeline for one source
    store: claude_worker.news.store.Store,
    parsed: claude_worker.news.sources.Parsed,
    *,
    source_name: str,
    settings: claude_worker.news.sources.NewsSettings,
    vocab: claude_worker.news.filter.Vocabulary,
    recent: claude_worker.news.filter.RecentTitles,
    caps: claude_worker.news.filter.Caps,
    stats: CycleStats,
    now_ts: int,
) -> None:
    for i in range(len(parsed.items)):
        item = parsed.items[i]
        verdict = claude_worker.news.filter.tier0(
            item,
            vocab=vocab,
            recent=recent,
            caps=caps,
            now_ts=now_ts,
            near_dup_jaccard=settings.near_dup_jaccard,
            near_dup_window_s=settings.near_dup_window_s,
        )
        if not _store_item(store, item, verdict, now_ts):
            stats.items_dup += 1
            continue
        stats.items_new += 1
        _bump(stats, _VERDICT_COUNTER[verdict.kind])
        if verdict.passed:
            caps.take(item.source)
            recent.add(
                item.ts or now_ts,
                f"{item.source}|{item.guid}",
                claude_worker.news.filter.tokens(item.title),
            )
    if parsed.snapshot is not None:
        store.insert_snapshot(*parsed.snapshot)
        stats.snapshots += 1
    for i in range(len(parsed.series)):
        key, ts, value = parsed.series[i]
        store.insert_series(source_name, key, ts, value)


def aggregate_once(  # noqa: PLR0913 — the composition root of one pass
    fetcher: claude_worker.news.sources.Fetcher,
    store: claude_worker.news.store.Store,
    *,
    registry: claude_worker.news.sources.Registry,
    vocab: claude_worker.news.filter.Vocabulary,
    recent: claude_worker.news.filter.RecentTitles,
    caps: claude_worker.news.filter.Caps,
    now_ns: int,
    now_ts: int,
    clock_ns: typing.Callable[[], int] = time.monotonic_ns,
    take_until_ns: int | None = None,
) -> CycleStats:
    """Fetch every due source once, parse it, tier-0 its items, store the lot.

    Never raises: a source that fails is a counted status on its row and the
    pass continues. That is the whole point — one venue changing its wire
    must not stop the other thirty-two.
    """
    stats = CycleStats()
    started = clock_ns()
    due = fetcher.due(now_ns)
    stats.sources = len(due)
    for i in range(len(due)):
        source = due[i]
        if take_until_ns is not None and clock_ns() >= take_until_ns:
            stats.deadline_skipped += 1
            continue
        result = fetcher.fetch(source, now_ns)
        counter = _STATUS_COUNTER.get(result.status, "")
        if counter:
            _bump(stats, counter)
        store.record_poll(
            source.name,
            now_ts,
            claude_worker.news.store.PollOutcome(
                ok=result.status == claude_worker.news.sources.FETCH_STATUS_OK,
                refused_origin=(
                    result.status == claude_worker.news.sources.FETCH_STATUS_REFUSED_ORIGIN
                ),
                budget_skip=result.status == claude_worker.news.sources.FETCH_STATUS_BUDGET,
                error="" if result.status == claude_worker.news.sources.FETCH_STATUS_OK
                else f"{result.status} {result.http_status}",
            ),
        )
        if result.status != claude_worker.news.sources.FETCH_STATUS_OK or result.payload is None:
            continue
        stats.ok += 1
        parsed = claude_worker.news.sources.parse(
            source, result.payload, now_ts, text_cap=registry.settings.text_cap
        )
        if parsed.is_empty():
            stats.parse_empty += 1
            store.counter_inc(claude_worker.news.store.COUNTER_PARSE_EMPTY)
            continue
        before = stats.items_new
        _ingest(
            store,
            parsed,
            source_name=source.name,
            settings=registry.settings,
            vocab=vocab,
            recent=recent,
            caps=caps,
            stats=stats,
            now_ts=now_ts,
        )
        store.add_items_total(source.name, stats.items_new - before)
    stats.elapsed_ms = max(0, (clock_ns() - started) // 1_000_000)
    return stats
