# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The pre-Stage-3 brain (spec §14, ruling Q3) — prompts out, answers in.

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

No API key exists yet, so no ``complete_fn`` exists either. The exchange is
two NDJSON files and a human-or-session tagger in between:

* ``prompts --tier N --out FILE`` writes one line per pending question,
  ``{"id", "tier", "prompt"}`` (plus ``"system"`` at tier 3, where the
  grammar the answer must obey lives in a cached system block).
* ``ingest --tier N --answers FILE`` reads ``{"id", "response"}`` back and
  drives the SAME [`claude_worker.news.cascade.NewsQueueWatcher`] over
  exactly those ids, with a ``complete_fn`` that resolves the prompt to the
  answer.

That last sentence is the whole design. The answers are not parsed by a
lookalike here: they go through `parse_triage_v2` / `parse_label_v2` /
`parse_assessment`, count ``triage_malformed`` / ``label_malformed`` /
``assessment_malformed`` in the same counters, land in ``prompt_cache``
under ``model = "session"``, spend the same daily ceilings, open the same
resolutions and cluster into the same stories. A session's answer is
therefore held to exactly the standard a model's answer is held to, and the
``model`` column is the only thing that separates them — which is what lets
the N4 replay score both without pooling them (`store.resolutions_resolved`
joins that column on purpose).

Two rules that are not obvious:

* **A pass touches only the ids its file answers.** `state.cached_complete`
  stores whatever ``complete_fn`` returns, so a prompt with no answer must
  never reach it — a sentinel would be cached under ``model = "session"``
  for good. Hence the watcher's ``ids`` restriction and [`MissingAnswer`],
  which fails the lane loudly instead of poisoning the cache.
* **A typed class-B item is never asked.** The venue already said what it
  was doing in its own announcement feed (`cascade.typed_triage`), so
  `prompts --tier 1` skips it: asking a human to re-tag it would buy a
  worse answer at a real cost in attention.
"""

import dataclasses
import json
import pathlib
import typing

import claude_worker.labeling
import claude_worker.news
import claude_worker.news.cascade
import claude_worker.news.filter
import claude_worker.news.sources
import claude_worker.news.store
import claude_worker.state

#: The wire names of the tiers. The NDJSON carries the NUMBER (§14's shape),
#: the store and the budget table carry the string.
TIER_NUMBERS: dict[str, int] = {
    claude_worker.news.cascade.TIER1: 1,
    claude_worker.news.cascade.TIER2: 2,
    claude_worker.news.cascade.TIER3: 3,
}
TIER_OF_NUMBER: dict[int, str] = {}
for _name, _number in TIER_NUMBERS.items():
    TIER_OF_NUMBER[_number] = _name

#: ``prompts --limit`` when the operator gives none (§14's own example).
PROMPTS_LIMIT_DEFAULT: int = 50

#: The keys a prompt line carries. ``system`` only at tier 3.
PROMPT_KEYS: tuple[str, ...] = ("id", "tier", "prompt")
#: The keys an answer line must carry — exactly these, nothing else, so a
#: file with a stray field is a file the operator has to look at.
ANSWER_KEYS: frozenset[str] = frozenset(("id", "response"))


class MissingAnswer(Exception):
    """A prompt reached ``complete_fn`` with no answer behind it.

    Unreachable by construction — the id set and the prompt map are built
    from the same rows in the same pass — so if it ever fires, something
    changed the item under us and the honest response is to stop. Raising
    beats returning a sentinel: the sentinel would be CACHED.
    """


@dataclasses.dataclass(frozen=True, slots=True)
class PromptLine:
    """One question, as the NDJSON file carries it."""

    id: str
    tier: int
    prompt: str
    system: str = ""

    def json(self) -> str:
        doc: dict[str, object] = {"id": self.id, "tier": self.tier, "prompt": self.prompt}
        if self.system:
            doc["system"] = self.system
        return json.dumps(doc, sort_keys=True, separators=(",", ":"))


@dataclasses.dataclass(slots=True)
class IngestStats:
    """What one ingest did. ``unmatched`` and ``rejected`` are the two the
    operator has to act on: the first says the file is stale, the second
    says the answers did not obey the schema they were given."""

    lines: int = 0
    bad_lines: int = 0
    matched: int = 0
    unmatched: int = 0
    stored: int = 0
    rejected: int = 0
    passes: int = 0
    typed: int = 0
    cache_hits: int = 0

    def line(self, tier: str) -> str:
        return (
            f"news ingest: tier={TIER_NUMBERS.get(tier, 0)} lines={self.lines} "
            f"bad_lines={self.bad_lines} matched={self.matched} unmatched={self.unmatched} "
            f"stored={self.stored} rejected={self.rejected} passes={self.passes} "
            f"typed={self.typed} cache_hits={self.cache_hits}"
        )


# ------------------------------------------------------------------- files


def write_prompts(path: pathlib.Path, lines: typing.Sequence[PromptLine]) -> int:
    """The prompts file, one JSON object per line. Atomic, like every other
    file this lane writes: a session reading it never sees half a batch."""
    body: list[str] = []
    for i in range(len(lines)):
        body.append(lines[i].json())
    claude_worker.news.write_atomic(path, "\n".join(body) + ("\n" if body else ""))
    return len(body)


def read_answers(path: pathlib.Path) -> tuple[dict[str, str], int]:
    """``{id: raw response}`` and the number of unusable lines.

    An unusable LINE (not JSON, wrong keys, empty id) is counted and
    skipped: one fat-fingered line must not cost the other fifty-nine.
    A line whose *response* is nonsense is a different thing entirely and
    is not this function's business — the strict parsers judge that, and
    count it as malformed exactly as they would a model's.
    """
    answers: dict[str, str] = {}
    bad = 0
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return answers, 0
    lines = text.splitlines()
    for i in range(len(lines)):
        raw = lines[i].strip()
        if not raw:
            continue
        try:
            doc = json.loads(raw)
        except ValueError:
            bad += 1
            continue
        if not isinstance(doc, dict) or set(doc) != ANSWER_KEYS:
            bad += 1
            continue
        ident = doc["id"]
        response = doc["response"]
        if not isinstance(ident, str) or not ident or not isinstance(response, str):
            bad += 1
            continue
        answers[ident] = response
    return answers, bad


# ---------------------------------------------------------------- the seam


def _refuse(model: str, prompt: str) -> str:
    """The ``complete_fn`` of a pass that must not answer anything — the
    `prompts` lane builds a watcher only to reuse its prompt builders."""
    del prompt
    raise MissingAnswer(f"the prompts lane asked {model} a question")


def answers_fn(mapping: typing.Mapping[str, str]) -> typing.Callable[[str, str], str]:
    """A ``complete_fn`` backed by the answers file, keyed on the prompt —
    which is also the prompt cache's key, so the two cannot disagree."""

    def complete(model: str, prompt: str) -> str:
        if prompt not in mapping:
            raise MissingAnswer(f"no answer for a {model} prompt of {len(prompt)} chars")
        return mapping[prompt]

    return complete


def build_watcher(  # noqa: PLR0913 — composition root: every collaborator named
    *,
    state: claude_worker.state.State,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    markets: typing.Mapping[str, int],
    vocab: claude_worker.news.filter.Vocabulary,
    now_ts: int,
    complete_fn: typing.Callable[[str, str], str] = _refuse,
    ids: typing.AbstractSet[str] | None = None,
    ceilings: typing.Mapping[str, int] | None = None,
    context_fn: typing.Callable[[], claude_worker.news.cascade.AnalystContext] | None = None,
) -> claude_worker.news.cascade.NewsQueueWatcher:
    """The session path's watcher: every tier tagged ``session``.

    ``vocab.assets`` rather than ``vocab.entries`` — the prompt offers a
    closed ASSET list, and the venue words and the operator's event
    keywords are not assets (see `filter.Vocabulary.assets`).
    """
    return claude_worker.news.cascade.NewsQueueWatcher(
        state=state,
        store=store,
        registry=registry,
        symbol_map=dict(markets),
        vocab=vocab.assets,
        complete_fn=complete_fn,
        ceilings=ceilings,
        models=claude_worker.news.cascade.Models.session(),
        context_fn=context_fn,
        now_fn=lambda: now_ts,
        ids=ids,
    )


# --------------------------------------------------------------- prompts


def tier1_prompts(
    watcher: claude_worker.news.cascade.NewsQueueWatcher,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    vocab: claude_worker.news.filter.Vocabulary,
    limit: int,
) -> list[PromptLine]:
    """One line per pending tier-0 survivor a model would be asked about.

    A class-B item the venue already typed is SKIPPED — `cascade._typed`
    will answer it for free during ingest, and a question whose answer is
    already known is a question nobody should be asked.
    """
    out: list[PromptLine] = []
    rows = store.items_pending_triage(limit)
    for i in range(len(rows)):
        row = rows[i]
        source = registry.by_name(str(row["source"]))
        if source is not None and claude_worker.news.cascade.typed_triage(
            source, str(row["title"]), str(row.get("hint", "")), vocab.assets
        ) is not None:
            continue
        out.append(
            PromptLine(
                id=claude_worker.news.cascade.item_id(row),
                tier=1,
                prompt=watcher.triage_prompt(row),
            )
        )
    return out


def tier2_prompts(
    watcher: claude_worker.news.cascade.NewsQueueWatcher,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    now_ts: int,
    limit: int,
) -> list[PromptLine]:
    """One line per open story with no label yet — tier 2's own queue."""
    out: list[PromptLine] = []
    window = registry.settings.story_window_s
    stories = store.stories_unlabeled(now_ts - window)
    for i in range(min(limit, len(stories))):
        prompt = watcher.label_prompt(stories[i])
        if not prompt:
            continue
        out.append(PromptLine(id=str(stories[i]["story_id"]), tier=2, prompt=prompt))
    return out


def tier3_prompts(
    watcher: claude_worker.news.cascade.NewsQueueWatcher, limit: int
) -> list[PromptLine]:
    """One line per story that earned an analyst read, carrying the static
    system block the answer's grammar lives in.

    ``system`` is its own field rather than a prefix on ``prompt`` so the
    prompt — and therefore the cache key — is byte-identical to the one
    `serve` will send when Stage 3 opens, where the system block rides the
    SDK's own cached ``system`` parameter (§13).
    """
    out: list[PromptLine] = []
    pending = watcher.pending_assessments()
    for i in range(min(limit, len(pending))):
        story_id, prompt = pending[i]
        out.append(
            PromptLine(
                id=story_id,
                tier=3,
                prompt=prompt,
                system=claude_worker.news.cascade.ANALYST_SYSTEM,
            )
        )
    return out


# ---------------------------------------------------------------- ingest


def _tier1_map(
    watcher: claude_worker.news.cascade.NewsQueueWatcher,
    store: claude_worker.news.store.Store,
    answers: typing.Mapping[str, str],
) -> tuple[dict[str, str], list[str]]:
    """``{prompt: response}`` and the ids that had a row to apply it to.

    Built from the SAME rows the restricted pass will read, through the
    SAME prompt builder, so every prompt that pass produces is a key here.
    """
    mapping: dict[str, str] = {}
    matched: list[str] = []
    rows = store.items_pending_ids(sorted(answers))
    for i in range(len(rows)):
        row = rows[i]
        ident = claude_worker.news.cascade.item_id(row)
        response = answers.get(ident)
        if response is None:
            continue
        mapping[watcher.triage_prompt(row)] = response
        matched.append(ident)
    return mapping, matched


def _tier2_map(
    watcher: claude_worker.news.cascade.NewsQueueWatcher,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    answers: typing.Mapping[str, str],
    now_ts: int,
) -> tuple[dict[str, str], list[str]]:
    """The same, over the stories tier 2's restricted pass will read."""
    mapping: dict[str, str] = {}
    matched: list[str] = []
    window = registry.settings.story_window_s
    stories = store.stories_unlabeled(now_ts - window)
    for i in range(len(stories)):
        story_id = str(stories[i]["story_id"])
        response = answers.get(story_id)
        if response is None:
            continue
        prompt = watcher.label_prompt(stories[i])
        if not prompt:
            continue
        mapping[prompt] = response
        matched.append(story_id)
    return mapping, matched


def ingest_tier12(  # noqa: PLR0913 — composition root: every collaborator named
    *,
    state: claude_worker.state.State,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    markets: typing.Mapping[str, int],
    vocab: claude_worker.news.filter.Vocabulary,
    tier: str,
    answers: typing.Mapping[str, str],
    now_ts: int,
    ceilings: typing.Mapping[str, int] | None = None,
) -> IngestStats:
    """Tier 1 or tier 2, through the real watcher, over the answered ids."""
    stats = IngestStats(lines=len(answers))
    probe = build_watcher(
        state=state,
        store=store,
        registry=registry,
        markets=markets,
        vocab=vocab,
        now_ts=now_ts,
    )
    if tier == claude_worker.news.cascade.TIER1:
        mapping, matched = _tier1_map(probe, store, answers)
    else:
        mapping, matched = _tier2_map(probe, store, registry, answers, now_ts)
    stats.matched = len(matched)
    stats.unmatched = len(answers) - len(matched)
    if not matched:
        return stats
    watcher = build_watcher(
        state=state,
        store=store,
        registry=registry,
        markets=markets,
        vocab=vocab,
        now_ts=now_ts,
        complete_fn=answers_fn(mapping),
        ids=frozenset(matched),
        ceilings=ceilings,
    )
    poll = watcher.poll_once(tiers=(tier,))
    stats.cache_hits = poll.cache_hits
    stats.typed = watcher.stats.typed
    if tier == claude_worker.news.cascade.TIER1:
        stats.rejected = poll.triage_malformed
        stats.stored = max(0, stats.matched - stats.rejected)
        return stats
    stats.rejected = poll.label_malformed
    stats.passes = poll.label_passes
    stats.stored = watcher.stats.labels_stored
    return stats


def ingest_tier3(  # noqa: PLR0913 — composition root: every collaborator named
    *,
    state: claude_worker.state.State,
    store: claude_worker.news.store.Store,
    registry: claude_worker.news.sources.Registry,
    markets: typing.Mapping[str, int],
    vocab: claude_worker.news.filter.Vocabulary,
    answers: typing.Mapping[str, str],
    now_ts: int,
    ceilings: typing.Mapping[str, int] | None = None,
    context_fn: typing.Callable[[], claude_worker.news.cascade.AnalystContext] | None = None,
) -> IngestStats:
    """The analyst tier. No watcher restriction is needed: `pending_assessments`
    IS the queue and `accept_assessment` takes the raw answer directly.

    The answer still goes through `complete_cached` first, so it is cached
    under ``model = "session"``, charged to the tier-3 ceiling, and counted
    in the `budget` table — a human's read of a story costs the same row a
    model's would, which is the only way the scorecard's cost column means
    anything before Stage 3.
    """
    stats = IngestStats(lines=len(answers))
    watcher = build_watcher(
        state=state,
        store=store,
        registry=registry,
        markets=markets,
        vocab=vocab,
        now_ts=now_ts,
        ceilings=ceilings,
        context_fn=context_fn,
    )
    pending = watcher.pending_assessments()
    for i in range(len(pending)):
        story_id, prompt = pending[i]
        response = answers.get(story_id)
        if response is None:
            continue
        stats.matched += 1
        cached = claude_worker.news.cascade.complete_cached(
            state,
            store,
            claude_worker.news.cascade.DEFAULT_CEILINGS if ceilings is None else ceilings,
            claude_worker.news.cascade.TIER3,
            claude_worker.news.cascade.MODEL_SESSION,
            claude_worker.news.cascade.ANALYST_PROMPT_VERSION,
            prompt,
            answers_fn({prompt: response}),
            now_ts=now_ts,
        )
        if cached is None:
            # The tier-3 ceiling is spent. Counted in `budget.skipped` by
            # `complete_cached` itself; the answer keeps for the next run.
            continue
        raw, cache_hit = cached
        if cache_hit:
            stats.cache_hits += 1
        if watcher.accept_assessment(story_id, raw, None, now_ts) is None:
            stats.rejected += 1
            continue
        stats.stored += 1
    stats.unmatched = len(answers) - stats.matched
    return stats
