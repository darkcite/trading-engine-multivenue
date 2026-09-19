# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §14 — the pre-Stage-3 brain: prompts out, answers in.

No client is constructed and nothing is fetched. The whole point of this
file is that a SESSION's answer is held to exactly the standard a model's
answer is held to, so most of these tests are about what happens when the
answer is wrong, stale, or missing:

* a malformed answer is counted `triage_malformed` / `label_malformed` /
  `assessment_malformed`, in the same counters `serve` writes, and the item
  is NOT consumed — a session can answer it properly on the next pass;
* an answer for an id the store no longer has anything to do with is
  `unmatched`, not an error and not a silent drop;
* a prompt with no answer behind it must NEVER reach `complete_fn`, because
  `state.cached_complete` stores whatever that returns and a sentinel would
  be cached under ``model = "session"`` for good. That is what
  [`MissingAnswer`] and the watcher's ``ids`` restriction are for, and the
  test that proves the cache stays clean is the most valuable one here;
* the `session` tag is on every row, so the scorecard's `by_model` never
  pools a human pass with an automated one.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import pytest

import claude_worker.config
import claude_worker.labeling
import claude_worker.news.cascade
import claude_worker.news.filter
import claude_worker.news.resolve
import claude_worker.news.session
import claude_worker.news.sources
import claude_worker.news.store
import claude_worker.state

NOW: int = 1_789_826_400
MARKETS: dict[str, int] = {"BTC-UP": 7, "ETH-UP": 8}
VOCAB: claude_worker.news.filter.Vocabulary = claude_worker.news.filter.build_vocabulary(
    ("BTC-UP", "ETH-UP"), ("okx:BTC-USDT-SWAP",), ("delist",)
)


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


def _state(tmp_path: pathlib.Path) -> claude_worker.state.State:
    return claude_worker.state.State(tmp_path / "worker" / "state.db")


def _registry(*sources: claude_worker.news.sources.Source) -> claude_worker.news.sources.Registry:
    return claude_worker.news.sources.Registry(
        settings=claude_worker.news.sources.NewsSettings(),
        keywords=("delist",),
        calendar=claude_worker.news.sources.Calendar(),
        sources=sources
        or (
            claude_worker.news.sources.Source(
                name="press", kind="rss", url="https://p.example/f",
                origin="p.example", class_="C",
            ),
        ),
    )


def _prose_item(store: claude_worker.news.store.Store, guid: str, title: str) -> None:
    store.upsert_item(
        source="press", guid=guid, ts=NOW, fetched_ts=NOW, title=title,
        link="https://p.example/" + guid, text="Body text about the delisting.",
        origin="p.example", class_="C", weight=1.0,
    )


def _triage_json(**over: object) -> str:
    doc: dict[str, object] = {
        "family": "crypto",
        "impact": "high",
        "reason": "OKX delists a perp",
        "event_type": "delisting",
        "entities": {"venues": ["okx"], "assets": ["BTC"]},
    }
    doc.update(over)
    return json.dumps(doc)


def _label_json(**over: object) -> str:
    doc: dict[str, object] = {
        "market": "BTC-UP",
        "direction": "down",
        "confidence": 0.8,
        "half_life_s": 3600,
        "vol": "up",
        "liquidity": "down",
    }
    doc.update(over)
    return json.dumps(doc)


def _answers(path: pathlib.Path, pairs: typing.Sequence[tuple[str, str]]) -> pathlib.Path:
    lines: list[str] = []
    for ident, response in pairs:
        lines.append(json.dumps({"id": ident, "response": response}))
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return path


def _common(
    state: claude_worker.state.State, store: claude_worker.news.store.Store
) -> dict[str, object]:
    return {
        "state": state,
        "store": store,
        "registry": _registry(),
        "markets": MARKETS,
        "vocab": VOCAB,
        "now_ts": NOW,
    }


# ---- the file shapes ------------------------------------------------------


def test_prompts_ndjson_shape(tmp_path: pathlib.Path) -> None:
    """Every line is one JSON object with id/tier/prompt; only tier 3 adds
    the system block, because only tier 3 has a grammar to obey."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC-FOO perpetual swap")
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=_registry(), markets=MARKETS,
            vocab=VOCAB, now_ts=NOW,
        )
        lines = claude_worker.news.session.tier1_prompts(
            watcher, store, _registry(), VOCAB, 10
        )
        out = tmp_path / "p1.ndjson"
        assert claude_worker.news.session.write_prompts(out, lines) == 1
        doc = json.loads(out.read_text(encoding="utf-8").strip())
        assert set(doc) == {"id", "tier", "prompt"}
        assert doc["id"] == "press|g1"
        assert doc["tier"] == 1
        # The prompt is the EXACT text the model would have been sent —
        # the prompt cache keys on it, so a difference here is a miss.
        assert doc["prompt"] == watcher.triage_prompt(
            typing.cast(dict[str, object], store.item("press", "g1"))
        )
        assert "BTC-FOO" in doc["prompt"]
        state.close()


def test_prompts_skip_a_class_b_item_the_venue_already_typed(
    tmp_path: pathlib.Path,
) -> None:
    """A venue announcing its own delisting is typed for free. Asking a
    human to re-tag it would buy a worse answer at a real cost."""
    source = claude_worker.news.sources.Source(
        name="okx-ann", kind="json-okx-ann", url="https://www.okx.com/x",
        origin="www.okx.com", class_="B", venue="okx",
    )
    registry = _registry(source)
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        store.upsert_item(
            source="okx-ann", guid="g1", ts=NOW, fetched_ts=NOW,
            title="OKX will delist BTC-FOO-SWAP", link="l", text="body",
            origin="www.okx.com", class_="B", weight=1.0, venue="okx",
            hint="delisting",
        )
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=registry, markets=MARKETS,
            vocab=VOCAB, now_ts=NOW,
        )
        assert claude_worker.news.session.tier1_prompts(
            watcher, store, registry, VOCAB, 10
        ) == []
        state.close()


def test_read_answers_counts_a_bad_line_and_keeps_the_rest(
    tmp_path: pathlib.Path,
) -> None:
    path = tmp_path / "a.ndjson"
    path.write_text(
        "\n".join(
            (
                json.dumps({"id": "press|g1", "response": "{}"}),
                "not json at all",
                json.dumps({"id": "press|g2", "response": "{}", "extra": 1}),
                json.dumps({"id": "", "response": "{}"}),
                json.dumps({"id": "press|g3", "response": "{}"}),
                "",
            )
        )
        + "\n",
        encoding="utf-8",
    )
    answers, bad = claude_worker.news.session.read_answers(path)
    assert sorted(answers) == ["press|g1", "press|g3"]
    assert bad == 3
    assert claude_worker.news.session.read_answers(tmp_path / "absent") == ({}, 0)


# ---- tier 1 ---------------------------------------------------------------


def test_ingest_validates_with_same_parsers(tmp_path: pathlib.Path) -> None:
    """A good tier-1 answer stores a triage row, escalates the item and
    opens a story — the same three effects `serve` produces."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        stats = claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.matched, stats.stored, stats.rejected) == (1, 1, 0)
        rows = store._rows("SELECT * FROM triage", ())
        assert len(rows) == 1
        assert rows[0]["event_type"] == "delisting"
        assert store.item("press", "g1")["triage_state"] == (
            claude_worker.news.store.STATE_ESCALATED
        )
        # Clustering ran, which is what §14 means by "then clustering runs".
        assert len(store.stories_open(0)) == 1
        state.close()


def test_ingest_model_is_session(tmp_path: pathlib.Path) -> None:
    """Every row the session path writes carries `session`, so the
    scorecard's `by_model` never pools it with an automated answer."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        rows = store._rows("SELECT * FROM triage", ())
        assert rows[0]["model"] == claude_worker.news.cascade.MODEL_SESSION
        assert rows[0]["model"] != claude_worker.config.MODEL_BULK
        story_id = str(store.stories_open(0)[0]["story_id"])
        stats = claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER2,
            answers={story_id: _label_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.matched, stats.stored) == (1, 1)
        label = store.label(story_id)
        assert label is not None
        assert label["model"] == claude_worker.news.cascade.MODEL_SESSION
        assert label["direction"] == "down"
        # The claim opened its own resolution, exactly as the model path does.
        opened = store.resolution(claude_worker.news.resolve.SUBJECT_LABEL, story_id)
        assert opened is not None and opened["state"] == "pending"
        state.close()


def test_ingest_malformed_counted_not_fatal(tmp_path: pathlib.Path) -> None:
    """A malformed answer is counted and the item is NOT consumed: a
    session that misread the schema can answer it again."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        stats = claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": '{"family": "crypto"}'},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.matched, stats.rejected, stats.stored) == (1, 1, 0)
        assert store.counters()[claude_worker.news.cascade.COUNTER_TRIAGE_MALFORMED] == 1
        assert store._rows("SELECT * FROM triage", ()) == []
        state.close()


def test_ingest_counts_an_answer_for_an_unknown_id(tmp_path: pathlib.Path) -> None:
    """A stale prompts file answers an id the queue has moved past. That is
    `unmatched` — information for the operator, not a crash and not a row."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        stats = claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|gone": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.lines, stats.matched, stats.unmatched) == (1, 0, 1)
        assert store._rows("SELECT * FROM triage", ()) == []
        state.close()


def test_an_unanswered_prompt_never_reaches_the_cache(tmp_path: pathlib.Path) -> None:
    """THE cache-integrity test.

    `state.cached_complete` stores whatever `complete_fn` returns, so a
    sentinel for "no answer" would be cached under ``model = "session"``
    forever and that item could never be triaged again. The restricted pass
    must therefore never build a prompt the map has no answer for — and if
    it somehow does, [`MissingAnswer`] stops the pass instead of writing.
    """
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        _prose_item(store, "g2", "A second delist story nobody answered")
        # Only g1 is answered; g2 must be untouched, not skipped-forever.
        claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert store.item("press", "g2")["triage_state"] == (
            claude_worker.news.store.STATE_NEW
        )
        # And the sentinel path itself raises rather than caching.
        fn = claude_worker.news.session.answers_fn({})
        with pytest.raises(claude_worker.news.session.MissingAnswer):
            fn(claude_worker.news.cascade.MODEL_SESSION, "a prompt nobody answered")
        state.close()


def test_a_prompts_run_answers_nothing(tmp_path: pathlib.Path) -> None:
    """The `prompts` lane builds a watcher only to reuse its prompt
    builders, so its `complete_fn` REFUSES. If a future edit made that lane
    run a tier instead of describing one, this raises rather than quietly
    triaging a queue the operator never saw."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=_registry(), markets=MARKETS,
            vocab=VOCAB, now_ts=NOW,
        )
        # Building the prompt is free; asking the question is not allowed.
        assert watcher.triage_prompt(
            typing.cast(dict[str, object], store.item("press", "g1"))
        )
        with pytest.raises(claude_worker.news.session.MissingAnswer):
            watcher.poll_once()
        assert store.item("press", "g1")["triage_state"] == (
            claude_worker.news.store.STATE_NEW
        )
        state.close()


# ---- tier 2 ---------------------------------------------------------------


def test_tier2_prompt_is_the_exact_label_prompt(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=_registry(), markets=MARKETS,
            vocab=VOCAB, now_ts=NOW,
        )
        lines = claude_worker.news.session.tier2_prompts(
            watcher, store, _registry(), NOW, 10
        )
        assert len(lines) == 1
        story = store.story(lines[0].id)
        assert story is not None
        assert lines[0].tier == 2
        assert lines[0].prompt == watcher.label_prompt(story)
        # The closed market list the answer will be validated against.
        assert "BTC-UP" in lines[0].prompt and "ETH-UP" in lines[0].prompt
        state.close()


def test_tier2_explicit_pass_is_a_real_answer(tmp_path: pathlib.Path) -> None:
    """A null market — with the other keys omitted, as the prompt says —
    marks the story labeled and writes no label row. A session saying
    "nothing here" is an answer, not a malformed one."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        story_id = str(store.stories_open(0)[0]["story_id"])
        stats = claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER2,
            answers={story_id: '{"market": null}'},
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.passes, stats.stored, stats.rejected) == (1, 0, 0)
        assert store.label(story_id) is None
        assert store.story(story_id)["state"] == claude_worker.news.store.STORY_LABELED
        state.close()


# ---- tier 3 ---------------------------------------------------------------


def _assessment_json(markets: typing.Sequence[str], **over: object) -> str:
    doc: dict[str, object] = {
        "thesis": "A delisting forces an unwind into a thin book.",
        "mechanism": "delisting_unwind",
        "channels": {
            "direction": {"market": markets[0], "dir": "none", "confidence": 0.3},
            "vol": {"profile": "fast", "level": "high", "confidence": 0.7, "ttl_s": 3600},
            "venue_risk": {"venue": "okx", "severity": "degraded"},
        },
        "affected_descriptors": [],
        "actions": [{"kind": "declare_vol_high", "profile": "fast", "ttl_s": 3600}],
        "half_life_s": 3600,
        "falsifier": "The book refills within the hour.",
        "evidence": ["press|g1"],
    }
    doc.update(over)
    return json.dumps(doc)


def _escalated_story(
    state: claude_worker.state.State, store: claude_worker.news.store.Store
) -> str:
    """One high-impact story with two origins, so it earns an analyst read."""
    _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
    store.upsert_item(
        source="press2", guid="g2", ts=NOW, fetched_ts=NOW,
        title="OKX will delist the BTC perpetual swap", link="l2",
        text="Another outlet, same delisting.", origin="other.example",
        class_="C", weight=1.0,
    )
    store.upsert_source(
        name="press2", kind="rss", class_="C", origin="other.example", enabled=1
    )
    claude_worker.news.session.ingest_tier12(
        tier=claude_worker.news.cascade.TIER1,
        answers={"press|g1": _triage_json(), "press2|g2": _triage_json()},
        **typing.cast(typing.Any, _common(state, store)),
    )
    return str(store.stories_open(0)[0]["story_id"])


def test_tier3_prompt_carries_the_system_grammar(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        story_id = _escalated_story(state, store)
        ctx = claude_worker.news.cascade.AnalystContext(markets=("BTC-UP",))
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=_registry(), markets=MARKETS,
            vocab=VOCAB, now_ts=NOW, context_fn=lambda: ctx,
        )
        lines = claude_worker.news.session.tier3_prompts(watcher, 10)
        assert [line.id for line in lines] == [story_id]
        assert lines[0].tier == 3
        assert lines[0].system == claude_worker.news.cascade.ANALYST_SYSTEM
        # The grammar the answer must obey, and the law it must respect.
        assert '"kind": "none"' in lines[0].system
        assert "halt" not in lines[0].system.split("ACTION is exactly one of:")[1]
        # The system block is NOT folded into the prompt: at Stage 3 it
        # rides the SDK's own cached `system` parameter, and the cache key
        # has to be the same text either way.
        assert claude_worker.news.cascade.ANALYST_SYSTEM not in lines[0].prompt
        doc = json.loads(lines[0].json())
        assert set(doc) == {"id", "tier", "prompt", "system"}
        state.close()


def test_tier3_ingest_stores_the_assessment_and_charges_the_budget(
    tmp_path: pathlib.Path,
) -> None:
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        story_id = _escalated_story(state, store)
        ctx = claude_worker.news.cascade.AnalystContext(markets=("BTC-UP",))
        stats = claude_worker.news.session.ingest_tier3(
            answers={story_id: _assessment_json(("BTC-UP",))},
            context_fn=lambda: ctx,
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.matched, stats.stored, stats.rejected) == (1, 1, 0)
        rows = store._rows("SELECT * FROM assessments", ())
        assert len(rows) == 1
        assert rows[0]["model"] == claude_worker.news.cascade.MODEL_SESSION
        assert rows[0]["prompt_version"] == claude_worker.news.cascade.ANALYST_PROMPT_VERSION
        # A human's read costs the same budget row a model's would, or the
        # scorecard's cost column means nothing before Stage 3.
        assert store.budget_today(claude_worker.news.cascade.TIER3, NOW)["calls"] == 1
        state.close()


def test_tier3_refuses_an_action_kind_outside_the_grammar(
    tmp_path: pathlib.Path,
) -> None:
    """Ruling Q5 through the session path: `halt` does not exist, and an
    assessment proposing one is refused WHOLE."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        story_id = _escalated_story(state, store)
        ctx = claude_worker.news.cascade.AnalystContext(markets=("BTC-UP",))
        stats = claude_worker.news.session.ingest_tier3(
            answers={
                story_id: _assessment_json(
                    ("BTC-UP",), actions=[{"kind": "halt_engine"}]
                )
            },
            context_fn=lambda: ctx,
            **typing.cast(typing.Any, _common(state, store)),
        )
        assert (stats.matched, stats.stored, stats.rejected) == (1, 0, 1)
        assert store.counters()[claude_worker.news.cascade.COUNTER_ASSESSMENT_MALFORMED] == 1
        assert store._rows("SELECT * FROM assessments", ()) == []
        state.close()


def test_the_session_tag_separates_the_prompt_cache(tmp_path: pathlib.Path) -> None:
    """A session's answer and a model's answer to the SAME question are
    two cache rows. Otherwise one would silently serve the other, and the
    `model` column the scorecard splits on would be a lie."""
    with _store(tmp_path) as store:
        state = _state(tmp_path)
        _prose_item(store, "g1", "OKX will delist the BTC perpetual swap")
        claude_worker.news.session.ingest_tier12(
            tier=claude_worker.news.cascade.TIER1,
            answers={"press|g1": _triage_json()},
            **typing.cast(typing.Any, _common(state, store)),
        )
        row = store.item("press", "g1")
        watcher = claude_worker.news.session.build_watcher(
            state=state, store=store, registry=_registry(), markets=MARKETS,
            vocab=VOCAB, now_ts=NOW,
        )
        prompt = watcher.triage_prompt(typing.cast(dict[str, object], row))
        cached, hit = state.cached_complete(
            claude_worker.news.cascade.MODEL_SESSION,
            claude_worker.labeling.TRIAGE_PROMPT_VERSION_V2,
            prompt,
            lambda model, text: "never called",
        )
        assert hit is True and json.loads(cached)["event_type"] == "delisting"
        calls: list[str] = []

        def bulk(model: str, text: str) -> str:
            calls.append(model)
            return _triage_json(impact="low")

        other, other_hit = state.cached_complete(
            claude_worker.config.MODEL_BULK,
            claude_worker.labeling.TRIAGE_PROMPT_VERSION_V2,
            prompt,
            bulk,
        )
        assert other_hit is False and calls == [claude_worker.config.MODEL_BULK]
        assert json.loads(other)["impact"] == "low"
        state.close()
