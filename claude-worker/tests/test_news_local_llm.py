# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""doc 03 — the local model sidecar, against a fake transport.

Nothing here opens a socket and nothing here needs `llama-server`: every test
drives `httpx.MockTransport`. That is deliberate — the sidecar is an
OPTIMISATION, and the properties worth defending are all about what happens
when it is absent, down, slow, or lying:

* the schemas are DERIVED from `labeling.py`, so a vocabulary edit cannot
  leave the grammar behind and let the model name something the parser will
  then refuse;
* a tier the policy has not routed local is never asked of the sidecar,
  whatever is resident;
* a server serving a model `llm.toml` does not pin is a REFUSAL, because a
  swapped weight file would silently change the brain behind a tag the
  scorecard splits on;
* local spend lands in its OWN `budget` rows — reusing `tier1` would add free
  calls to the counter that means money;
* and every failure is `""` plus a counter, never a raise, because this runs
  inside a 120 s lane that has real work to do without it.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import httpx
import pytest

import claude_worker.labeling
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.filter
import claude_worker.news.local_llm
import claude_worker.news.sources
import claude_worker.news.store
import claude_worker.state

NOW: int = 1_789_826_400
MARKETS: dict[str, int] = {"BTC-UP": 7, "ETH-UP": 8}
VOCAB: claude_worker.news.filter.Vocabulary = claude_worker.news.filter.build_vocabulary(
    ("BTC-UP", "ETH-UP"), ("okx:BTC-USDT-SWAP",), ("delist",)
)
MODEL_TAG: str = "local:qwen3.5-9b-q4_k_m"
GGUF: str = "/m/Qwen3.5-9B-Q4_K_M.gguf"


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


def _state(tmp_path: pathlib.Path) -> claude_worker.state.State:
    return claude_worker.state.State(tmp_path / "worker" / "state.db")


def _registry() -> claude_worker.news.sources.Registry:
    return claude_worker.news.sources.Registry(
        settings=claude_worker.news.sources.NewsSettings(),
        keywords=("delist",),
        calendar=claude_worker.news.sources.Calendar(),
        sources=(
            claude_worker.news.sources.Source(
                name="press", kind="rss", url="https://p.example/f",
                origin="p.example", class_="C",
            ),
        ),
    )


def _cfg(**over: object) -> claude_worker.news.local_llm.LlmConfig:
    fields: dict[str, object] = {
        "present": True,
        "valid": True,
        "model_path": pathlib.Path(GGUF),
        "model_tag": MODEL_TAG,
    }
    fields.update(over)
    return claude_worker.news.local_llm.LlmConfig(**fields)  # type: ignore[arg-type]


def _policy(**over: object) -> claude_worker.news.actions.NewsPolicy:
    fields: dict[str, object] = {
        "models": {"tier1": MODEL_TAG, "tier2": MODEL_TAG},
        "valid": True,
    }
    fields.update(over)
    return claude_worker.news.actions.NewsPolicy(**fields)  # type: ignore[arg-type]


def _triage_answer(**over: object) -> str:
    doc: dict[str, object] = {
        "family": "crypto",
        "impact": "high",
        "reason": "the venue is delisting a perp",
        "event_type": "delisting",
        "entities": {"venues": ["okx"], "assets": []},
    }
    doc.update(over)
    return json.dumps(doc)


def _label_answer(**over: object) -> str:
    doc: dict[str, object] = {
        "market": "BTC-UP",
        "direction": "down",
        "confidence": 0.7,
        "half_life_s": 3600,
        "vol": "up",
        "liquidity": "none",
    }
    doc.update(over)
    return json.dumps(doc)


class _Server:
    """A `llama-server` double that records what it was asked."""

    def __init__(
        self,
        answer: str = "{}",
        healthy: bool = True,
        model: str = GGUF,
        status: int = 200,
    ) -> None:
        self.answer = answer
        self.healthy = healthy
        self.model = model
        self.status = status
        self.requests: list[dict[str, object]] = []

    def handler(self, request: httpx.Request) -> httpx.Response:
        path = request.url.path
        if path == claude_worker.news.local_llm.HEALTH_PATH:
            return httpx.Response(200 if self.healthy else 503, json={"status": "ok"})
        if path == claude_worker.news.local_llm.PROPS_PATH:
            return httpx.Response(200, json={"model_path": self.model})
        if path == claude_worker.news.local_llm.METRICS_PATH:
            return httpx.Response(
                200,
                text=(
                    "# HELP llamacpp:prompt_tokens_total n\n"
                    "llamacpp:prompt_tokens_total 1234\n"
                    "llamacpp:tokens_predicted_total 567\n"
                    "malformed_line_without_value\n"
                ),
            )
        if path == claude_worker.news.local_llm.CHAT_PATH:
            self.requests.append(json.loads(request.content.decode()))
            if self.status != 200:
                return httpx.Response(self.status, text="nope")
            return httpx.Response(
                200, json={"choices": [{"message": {"content": self.answer}}]}
            )
        return httpx.Response(404, text="no route")

    def client(self) -> claude_worker.news.local_llm.LocalClient:
        return claude_worker.news.local_llm.LocalClient(
            "http://127.0.0.1:9393", http=httpx.Client(transport=httpx.MockTransport(self.handler))
        )


# ---- the schemas are derived, not retyped -------------------------------


def test_the_schemas_are_the_closed_vocabularies_themselves() -> None:
    """If a vocabulary is edited and the grammar is not, the model may name
    something `parse_triage_v2` then refuses — a malformed answer manufactured
    by our own schema. So the enums ARE the tuples, and this asserts it."""
    schema = claude_worker.news.local_llm.triage_schema(("BTC", "ETH"))
    props = typing.cast(dict[str, object], schema["properties"])
    assert typing.cast(dict[str, object], props["family"])["enum"] == list(
        claude_worker.labeling.FAMILIES
    )
    assert typing.cast(dict[str, object], props["impact"])["enum"] == list(
        claude_worker.labeling.IMPACTS
    )
    assert typing.cast(dict[str, object], props["event_type"])["enum"] == list(
        claude_worker.labeling.EVENT_TYPES
    )
    entities = typing.cast(dict[str, object], props["entities"])
    ent_props = typing.cast(dict[str, object], entities["properties"])
    venues = typing.cast(dict[str, object], ent_props["venues"])
    assert typing.cast(dict[str, object], venues["items"])["enum"] == list(
        claude_worker.labeling.VENUE_NAMES
    )
    assets = typing.cast(dict[str, object], ent_props["assets"])
    assert typing.cast(dict[str, object], assets["items"])["enum"] == ["BTC", "ETH"]
    assert typing.cast(dict[str, object], props["reason"])["maxLength"] == (
        claude_worker.labeling.REASON_MAX
    )
    # The key set the parser demands, exactly.
    assert set(typing.cast(list[str], schema["required"])) == {
        "family", "impact", "reason", "event_type", "entities",
    }
    assert schema["additionalProperties"] is False


def test_the_label_schema_admits_both_legal_shapes_and_no_third() -> None:
    """A null market with the other keys OMITTED is the explicit pass; all six
    keys is a label. `parse_label_v2` accepts exactly those two, so the grammar
    must too — a `oneOf`, not an optional-everything object."""
    schema = claude_worker.news.local_llm.label_schema(("BTC-UP",))
    branches = typing.cast(list[dict[str, object]], schema["oneOf"])
    assert len(branches) == 2
    full, passing = branches[0], branches[1]
    assert set(typing.cast(list[str], full["required"])) == {
        "market", "direction", "confidence", "half_life_s", "vol", "liquidity",
    }
    full_props = typing.cast(dict[str, object], full["properties"])
    assert typing.cast(dict[str, object], full_props["direction"])["enum"] == list(
        claude_worker.labeling.DIRECTIONS_V2
    )
    assert typing.cast(dict[str, object], full_props["vol"])["enum"] == list(
        claude_worker.labeling.VOL_LEVELS
    )
    assert typing.cast(list[str], passing["required"]) == ["market"]
    pass_props = typing.cast(dict[str, object], passing["properties"])
    assert typing.cast(dict[str, object], pass_props["market"])["type"] == "null"


# ---- the config ---------------------------------------------------------


def test_an_absent_llm_toml_is_no_sidecar_not_an_error(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.news.local_llm.load_config(tmp_path / "nope.toml")
    assert cfg.present is False and cfg.valid is True
    assert cfg.model_tag == ""


def test_an_unknown_key_invalidates_the_whole_config(tmp_path: pathlib.Path) -> None:
    """Half a sidecar config is worse than none — the same law
    `news-policy.toml` follows."""
    path = tmp_path / "llm.toml"
    path.write_text(
        f'[server]\nmodel = "{GGUF}"\nnot_a_key = 1\n', encoding="utf-8"
    )
    cfg = claude_worker.news.local_llm.load_config(path)
    assert cfg.present is True and cfg.valid is False


def test_the_tag_is_derived_from_the_file_when_omitted(tmp_path: pathlib.Path) -> None:
    """A tag may not claim a model the server is not serving, so when the
    operator gives none it comes from the gguf itself."""
    path = tmp_path / "llm.toml"
    path.write_text(f'[server]\nmodel = "{GGUF}"\n', encoding="utf-8")
    cfg = claude_worker.news.local_llm.load_config(path)
    assert cfg.model_tag == MODEL_TAG
    assert cfg.model_tag.startswith(claude_worker.news.local_llm.TAG_PREFIX)


def test_the_argv_carries_no_worker_name(tmp_path: pathlib.Path) -> None:
    """CMDLINE LAW: this process is resident forever, so it must be invisible
    to `pgrep -f 'claude[-_]worker'`."""
    del tmp_path
    argv = claude_worker.news.local_llm.server_argv(_cfg())
    joined = " ".join(argv)
    assert "claude_worker" not in joined and "claude-worker" not in joined
    assert argv[0] == "llama-server"
    for flag in ("--host", "--port", "-m", "-c", "-t", "-ngl", "--parallel", "--metrics"):
        assert flag in argv, flag
    # One slot: bounded KV, and the cycle sends one request at a time anyway.
    assert argv[argv.index("--parallel") + 1] == "1"


# ---- the client ---------------------------------------------------------


def test_the_request_is_schema_constrained_with_thinking_off() -> None:
    """The two things that make a local answer trustworthy: the sampler may
    only emit tokens the schema admits, and the model is not allowed to
    'think' first — a Qwen3-family model that does burns the token budget
    before the JSON starts and the answer truncates."""
    server = _Server(_triage_answer())
    schema = claude_worker.news.local_llm.triage_schema(("BTC",))
    with server.client() as client:
        raw = client.complete("a prompt", schema, "triage_v2")
    assert json.loads(raw)["event_type"] == "delisting"
    sent = server.requests[0]
    fmt = typing.cast(dict[str, object], sent["response_format"])
    assert fmt["type"] == "json_schema"
    inner = typing.cast(dict[str, object], fmt["json_schema"])
    assert inner["strict"] is True and inner["name"] == "triage_v2"
    assert inner["schema"] == schema
    assert sent["chat_template_kwargs"] == {"enable_thinking": False}
    assert sent["temperature"] == 0.0, "classification, not chat"
    assert sent["stream"] is False


@pytest.mark.parametrize(
    "server",
    [
        _Server(status=503),
        _Server(status=400),
        _Server(answer=""),
    ],
)
def test_every_failure_is_an_empty_string_never_a_raise(server: _Server) -> None:
    """This runs inside a 120 s lane with real work to do. A sidecar still
    loading its model (503), one refusing the schema (400), or one answering
    an empty body must cost the lane nothing."""
    schema = claude_worker.news.local_llm.triage_schema(("BTC",))
    with server.client() as client:
        assert client.complete("p", schema) == ""
    assert client.stats.empty >= 1


def test_an_unhealthy_server_is_unhealthy_but_still_answerable() -> None:
    """`/health` and `/chat` are separate facts. The GUARD is that
    `run_local_tiers` refuses to ask an unhealthy server — not that the
    endpoint would refuse, which it would not."""
    server = _Server(_triage_answer(), healthy=False)
    with server.client() as client:
        assert client.health() is False
        assert client.stats.unhealthy == 1


def test_a_transport_error_is_counted_not_raised() -> None:
    def boom(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused", request=request)

    client = claude_worker.news.local_llm.LocalClient(
        "http://127.0.0.1:9393", http=httpx.Client(transport=httpx.MockTransport(boom))
    )
    try:
        assert client.health() is False
        assert client.complete("p", {"type": "object"}) == ""
        assert client.props() == {} and client.metrics() == {}
        assert client.stats.unhealthy == 1 and client.stats.empty == 1
    finally:
        client.close()


def test_metrics_parses_prometheus_text_and_skips_junk() -> None:
    with _Server().client() as client:
        metrics = client.metrics()
    assert metrics["llamacpp:prompt_tokens_total"] == 1234.0
    assert metrics["llamacpp:tokens_predicted_total"] == 567.0
    assert "malformed_line_without_value" not in metrics


def test_props_model_stem_survives_a_reshaped_props() -> None:
    """`llama-server` has moved this field between versions, so an unknown
    shape must yield "" (the caller then trusts `llm.toml` and says so) rather
    than a false mismatch that silently disables the sidecar."""
    stem = claude_worker.news.local_llm.props_model_stem
    assert stem({"model_path": GGUF}) == "qwen3.5-9b-q4_k_m"
    assert stem({"model": GGUF}) == "qwen3.5-9b-q4_k_m"
    assert stem({"default_generation_settings": {"model": GGUF}}) == "qwen3.5-9b-q4_k_m"
    assert stem({"something_new": GGUF}) == ""
    assert stem({}) == ""


# ---- the cycle's local tiers -------------------------------------------


def _item(store: claude_worker.news.store.Store, guid: str, title: str = "") -> None:
    """One tier-0 survivor. The title VARIES by guid unless given, because two
    identical items are one QUESTION — the prompt cache collapses them, which
    is a property worth testing on purpose rather than tripping over."""
    store.upsert_item(
        source="press", guid=guid, ts=NOW, fetched_ts=NOW,
        title=title or f"OKX will delist the BTC perpetual swap ({guid})", link="l",
        text="body", origin="p.example", class_="C", weight=1.0,
    )


def _run(
    tmp_path: pathlib.Path,
    server: _Server,
    policy: claude_worker.news.actions.NewsPolicy | None = None,
    cfg: claude_worker.news.local_llm.LlmConfig | None = None,
) -> tuple[claude_worker.news.local_llm.LocalRunStats, claude_worker.news.store.Store]:
    store = _store(tmp_path)
    state = _state(tmp_path)
    try:
        _item(store, "g1")
        _item(store, "g2")
        stats = claude_worker.news.local_llm.run_local_tiers(
            state=state,
            store=store,
            registry=_registry(),
            policy=_policy() if policy is None else policy,
            cfg=_cfg() if cfg is None else cfg,
            markets=MARKETS,
            vocab=VOCAB,
            now_ts=NOW,
            client=server.client(),
        )
    finally:
        state.close()
    return stats, store


def test_the_local_tiers_triage_and_cluster_under_their_own_tag(
    tmp_path: pathlib.Path,
) -> None:
    """The whole point: the sidecar's rows are the cascade's rows, under a tag
    that keeps them out of the frontier model's bucket in the scorecard."""
    server = _Server(_triage_answer())
    stats, store = _run(tmp_path, server)
    try:
        assert stats.healthy is True and stats.tier1 == 2
        rows = store._rows("SELECT * FROM triage", ())
        assert len(rows) == 2
        for i in range(len(rows)):
            assert rows[i]["model"] == MODEL_TAG
            assert rows[i]["event_type"] == "delisting"
        # Clustering ran, so the local tier feeds the same pipeline.
        assert len(store._rows("SELECT * FROM stories", ())) >= 1
        # ...and the spend is on its OWN budget rows.
        assert store.budget_today(
            claude_worker.news.local_llm.local_tier(claude_worker.news.cascade.TIER1), NOW
        )["calls"] == 2
        assert store.budget_today(claude_worker.news.cascade.TIER1, NOW)["calls"] == 0, (
            "a free call was charged to the paid ceiling"
        )
        # Three calls: two triages, plus the tier-2 label on the story they
        # clustered into, because `_policy()` routes BOTH tiers local.
        assert store.counters()[claude_worker.news.local_llm.COUNTER_CALLS] == 3
        assert stats.tier2 == 1
        assert store.budget_today(
            claude_worker.news.local_llm.local_tier(claude_worker.news.cascade.TIER2), NOW
        )["calls"] == 1
    finally:
        store.close()


def test_a_tier_the_policy_has_not_routed_local_is_never_asked(
    tmp_path: pathlib.Path,
) -> None:
    """Routing is the grant. A resident model the policy does not name answers
    nothing — which is what makes `llm.toml` safe to install before any
    ruling about using it."""
    server = _Server(_triage_answer())
    stats, store = _run(tmp_path, server, policy=_policy(models={}))
    try:
        assert stats.tier1 == 0 and stats.calls == 0
        assert server.requests == []
        assert store._rows("SELECT * FROM triage", ()) == []
    finally:
        store.close()


def test_an_absent_config_asks_nothing(tmp_path: pathlib.Path) -> None:
    server = _Server(_triage_answer())
    stats, store = _run(
        tmp_path, server, cfg=claude_worker.news.local_llm.LlmConfig()
    )
    try:
        assert stats.healthy is False and stats.calls == 0
        assert server.requests == []
    finally:
        store.close()


def test_an_invalid_config_is_counted(tmp_path: pathlib.Path) -> None:
    server = _Server(_triage_answer())
    stats, store = _run(
        tmp_path, server, cfg=_cfg(valid=False)
    )
    try:
        assert stats.calls == 0
        assert store.counters()[claude_worker.news.local_llm.COUNTER_CONFIG_INVALID] == 1
    finally:
        store.close()


def test_a_server_serving_another_model_is_refused(tmp_path: pathlib.Path) -> None:
    """A weight file swapped under us would change the brain behind a tag the
    scorecard splits on, pooling every row before and after as one model. So a
    `/props` that disagrees with `llm.toml` stops the pass."""
    server = _Server(_triage_answer(), model="/m/SomethingElse-Q4_K_M.gguf")
    stats, store = _run(tmp_path, server)
    try:
        assert stats.healthy is True
        assert stats.tier1 == 0 and server.requests == []
        assert store.counters()[claude_worker.news.local_llm.COUNTER_MISMATCH] == 1
        assert store._rows("SELECT * FROM triage", ()) == []
    finally:
        store.close()


def test_an_unreported_model_trusts_the_config(tmp_path: pathlib.Path) -> None:
    """The mismatch check must not become a version-skew outage: a `/props`
    this code cannot read is not evidence of the wrong model."""
    server = _Server(_triage_answer(), model="")
    stats, store = _run(tmp_path, server)
    try:
        assert stats.tier1 == 2
        assert claude_worker.news.local_llm.COUNTER_MISMATCH not in store.counters()
    finally:
        store.close()


def test_a_down_sidecar_degrades_the_cycle_and_counts_it(
    tmp_path: pathlib.Path,
) -> None:
    server = _Server(_triage_answer(), healthy=False)
    stats, store = _run(tmp_path, server)
    try:
        assert stats.healthy is False and stats.tier1 == 0
        assert server.requests == []
        assert store.counters()[claude_worker.news.local_llm.COUNTER_UNHEALTHY] == 1
    finally:
        store.close()


def test_the_local_budget_stops_a_runaway_loop(tmp_path: pathlib.Path) -> None:
    """A free call's only failure mode is doing it forever, so the bound is a
    call count and not a cost."""
    server = _Server(_triage_answer())
    store = _store(tmp_path)
    state = _state(tmp_path)
    try:
        _item(store, "g1")
        store.budget_add(
            claude_worker.news.store.day_of(NOW),
            claude_worker.news.local_llm.local_tier(claude_worker.news.cascade.TIER1),
            calls=9,
        )
        stats = claude_worker.news.local_llm.run_local_tiers(
            state=state, store=store, registry=_registry(),
            policy=_policy(local_calls_per_day=9), cfg=_cfg(),
            markets=MARKETS, vocab=VOCAB, now_ts=NOW, client=server.client(),
        )
        assert stats.spent_today == 9 and stats.tier1 == 0
        assert server.requests == []
        assert store.counters()[claude_worker.news.local_llm.COUNTER_BUDGET] == 1
    finally:
        state.close()
        store.close()


def test_the_batch_is_bounded_by_the_policy(tmp_path: pathlib.Path) -> None:
    """The cycle has a 30 s wall budget it shares with the aggregator, so the
    sidecar takes a bounded bite and the backlog drains over several slots."""
    server = _Server(_triage_answer())
    store = _store(tmp_path)
    state = _state(tmp_path)
    try:
        for i in range(6):
            _item(store, f"g{i}")
        stats = claude_worker.news.local_llm.run_local_tiers(
            state=state, store=store, registry=_registry(),
            policy=_policy(tier1_batch_max=2), cfg=_cfg(),
            markets=MARKETS, vocab=VOCAB, now_ts=NOW, client=server.client(),
        )
        assert stats.tier1 == 2
        assert len(store._rows("SELECT * FROM triage", ())) == 2
        assert len(store.items_pending_triage(100)) == 4, "the rest wait for the next slot"
    finally:
        state.close()
        store.close()


def test_two_identical_items_are_one_question(tmp_path: pathlib.Path) -> None:
    """The prompt cache is keyed on (model, version, prompt), so a story
    republished verbatim costs one call however many times it arrives. Free
    calls still deserve this: the cache is also what makes a re-run of the
    comparison cheap and deterministic."""
    server = _Server(_triage_answer())
    store = _store(tmp_path)
    state = _state(tmp_path)
    try:
        _item(store, "g1", title="The very same headline")
        _item(store, "g2", title="The very same headline")
        stats = claude_worker.news.local_llm.run_local_tiers(
            state=state, store=store, registry=_registry(),
            policy=_policy(models={"tier1": MODEL_TAG}),  # tier 1 ONLY, to isolate
            cfg=_cfg(), markets=MARKETS, vocab=VOCAB, now_ts=NOW,
            client=server.client(),
        )
        assert stats.tier1 == 2, "both items were processed"
        assert len(server.requests) == 1, "but the sidecar was asked once"
        assert store.budget_today(
            claude_worker.news.local_llm.local_tier(claude_worker.news.cascade.TIER1), NOW
        )["calls"] == 1
        assert len(store._rows("SELECT * FROM triage", ())) == 2
    finally:
        state.close()
        store.close()


def test_a_malformed_local_answer_is_counted_like_any_other(
    tmp_path: pathlib.Path,
) -> None:
    """Schema-constrained decoding makes this ~impossible in production, which
    is exactly why it has to be tested: the ONE path that bypasses the grammar
    is a server that ignored `response_format`."""
    server = _Server('{"family": "crypto"}')
    stats, store = _run(tmp_path, server)
    try:
        assert stats.tier1 == 2
        assert store._rows("SELECT * FROM triage", ()) == []
        assert store.counters()[
            claude_worker.news.cascade.COUNTER_TRIAGE_MALFORMED
        ] == 2
    finally:
        store.close()
