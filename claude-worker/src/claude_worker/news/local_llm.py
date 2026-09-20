# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The local-model sidecar seam (doc 03 §4) — one more `complete_fn`.

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

`llama-server` on `127.0.0.1:9393`, one resident GGUF, one slot. The cascade
already routes every model call through `complete_fn(model, prompt) -> str`
inside `cascade.complete_cached`, with `prompt_cache` keyed by model, so this
module adds an implementation of that seam and nothing else. No engine change,
no Cargo change, **no Anthropic API call, no key of any kind** — a local
server has none.

Why this is worth a module rather than a `requests` call inline: the whole
safety story of tiers 1 and 2 is "closed vocabularies, exact key sets, strict
parsers". `llama-server` can be told to emit ONLY tokens a JSON schema
admits, which turns `parse_triage_v2` / `parse_label_v2` from a filter into an
assertion — the malformed rate is ~0 by construction instead of counted after
the fact. The schemas here are therefore derived FROM `labeling.py`'s tuples
at import, never retyped: a test asserts each schema's enum IS the tuple, so
a vocabulary edit cannot leave the grammar behind.

Provenance: the model string is ``local:<gguf-stem>``. The `prompt_cache` PK
and the `triage.model` / `labels.model` columns already separate it from
`claude-haiku-4-5`, from `session`, and from every other local model, so the
§9.6 scorecard's `by_model` split shows each brain's own hit rate with no
change to the scorecard.

Degraded modes are honest and counted, never raises: a server that is down,
slow, or answering something other than JSON yields ``""``, which the strict
parser refuses and the cascade counts exactly as it counts a frontier model's
malformed answer.
"""

import dataclasses
import json
import pathlib
import tomllib
import typing

import httpx

import claude_worker.labeling
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.filter
import claude_worker.news.sources
import claude_worker.news.store
import claude_worker.state

#: Where the sidecar listens. Loopback only — nothing about this is reachable
#: off the box (doc 03 §7's law).
DEFAULT_HOST: str = "127.0.0.1"
DEFAULT_PORT: int = 9393

#: One structured answer: a triage row is ~60 tokens, a label ~50. 256 leaves
#: room for a long `reason` and stops a runaway generation.
MAX_TOKENS: int = 256
#: Classification, not chat. Temperature 0 makes the same item give the same
#: answer, which is what lets the prompt cache mean anything.
TEMPERATURE: float = 0.0
#: A 9B on Metal answers a ~1 k-token prompt in 1-2 s with the static prefix
#: in KV. 60 s is the "something is badly wrong" bound, not a target.
DEFAULT_TIMEOUT_S: float = 60.0
HEALTH_TIMEOUT_S: float = 2.0

#: Qwen3-family models are hybrid "thinking" models. Thinking must be OFF for
#: classification: it burns the token budget before the JSON starts and the
#: schema then truncates. Verified against the served model's chat template on
#: install (doc 03 §2).
THINKING_KWARGS: dict[str, object] = {"enable_thinking": False}

#: The only status this module treats as an answer. Anything else — 503 while
#: the model loads, 400 on a schema the server will not compile, 500 — is a
#: counted empty, never a raise.
HTTP_OK: int = 200
#: Fields in a Prometheus text line: `name{labels} value`.
_METRIC_FIELDS: int = 2

CHAT_PATH: str = "/v1/chat/completions"
HEALTH_PATH: str = "/health"
PROPS_PATH: str = "/props"
METRICS_PATH: str = "/metrics"

#: `llm.toml` keys, rejected strictly like every other artifact this lane
#: reads (an unknown key is an operator typo, and a half-read config is worse
#: than none).
_SERVER_KEYS: frozenset[str] = frozenset(
    (
        "port",
        "host",
        "model",
        "sha256",
        "ctx_size",
        "threads",
        "gpu_layers",
        "parallel",
        "cache_reuse",
    )
)
_TAG_KEYS: frozenset[str] = frozenset(("model_tag",))
_FALLBACK_KEYS: frozenset[str] = frozenset(("model", "sha256", "model_tag"))
_TOP_KEYS: frozenset[str] = frozenset(("server", "tags", "fallback"))

#: The provenance prefix. `cascade.complete_cached` takes the model string
#: opaquely, so this is the only place the shape is decided.
TAG_PREFIX: str = "local:"


# ------------------------------------------------------------------ schemas


def _enum(values: typing.Sequence[str]) -> dict[str, object]:
    return {"type": "string", "enum": list(values)}


def _closed_array(values: typing.Sequence[str]) -> dict[str, object]:
    """A JSON-schema array over a closed vocabulary — `uniqueItems` so the
    model cannot pad, and the enum is the tuple itself."""
    return {
        "type": "array",
        "items": _enum(values),
        "uniqueItems": True,
    }


def triage_schema(assets: typing.Sequence[str]) -> dict[str, object]:
    """The tier-1 answer's schema, derived from `labeling.py`.

    ``assets`` is per-call: it is the vocabulary the PROMPT offered, and the
    grammar must admit exactly that list or the model can name an asset
    `parse_triage_v2` will then reject — which would be a malformed answer
    manufactured by our own schema.
    """
    return {
        "type": "object",
        "additionalProperties": False,
        "required": ["family", "impact", "reason", "event_type", "entities"],
        "properties": {
            "family": _enum(claude_worker.labeling.FAMILIES),
            "impact": _enum(claude_worker.labeling.IMPACTS),
            "reason": {"type": "string", "maxLength": claude_worker.labeling.REASON_MAX},
            "event_type": _enum(claude_worker.labeling.EVENT_TYPES),
            "entities": {
                "type": "object",
                "additionalProperties": False,
                "required": ["venues", "assets"],
                "properties": {
                    "venues": _closed_array(claude_worker.labeling.VENUE_NAMES),
                    "assets": _closed_array(list(assets)),
                },
            },
        },
    }


def label_schema(markets: typing.Sequence[str]) -> dict[str, object]:
    """The tier-2 answer's schema.

    Two shapes are legal and the spec's parser accepts both: a null market
    with the other keys OMITTED (the explicit pass), or all six keys. A
    `oneOf` expresses that exactly, so the grammar cannot produce the
    half-filled third shape a prose prompt invites.
    """
    full = {
        "type": "object",
        "additionalProperties": False,
        "required": ["market", "direction", "confidence", "half_life_s", "vol", "liquidity"],
        "properties": {
            "market": _enum(list(markets)),
            "direction": _enum(claude_worker.labeling.DIRECTIONS_V2),
            "confidence": {"type": "number", "minimum": 0.0, "maximum": 1.0},
            "half_life_s": {"type": "number", "exclusiveMinimum": 0},
            "vol": _enum(claude_worker.labeling.VOL_LEVELS),
            "liquidity": _enum(claude_worker.labeling.LIQUIDITY_LEVELS),
        },
    }
    passing = {
        "type": "object",
        "additionalProperties": False,
        "required": ["market"],
        "properties": {"market": {"type": "null"}},
    }
    return {"oneOf": [full, passing]}


# ------------------------------------------------------------------- config


@dataclasses.dataclass(frozen=True, slots=True)
class LlmConfig:
    """`llm.toml`, validated. The DEFAULT instance is the absent one:
    ``present = False`` means every local tier is skipped and counted, which
    is the same degraded shape an absent `news.toml` gives the whole lane."""

    present: bool = False
    valid: bool = True
    host: str = DEFAULT_HOST
    port: int = DEFAULT_PORT
    model_path: pathlib.Path = pathlib.Path()
    sha256: str = ""
    model_tag: str = ""
    ctx_size: int = 4096
    threads: int = 4
    gpu_layers: int = 99
    parallel: int = 1
    cache_reuse: int = 256
    fallback_model_path: pathlib.Path = pathlib.Path()
    fallback_sha256: str = ""
    fallback_model_tag: str = ""

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self.port}"


def _reject_unknown(where: str, table: typing.Mapping[str, object], known: frozenset[str]) -> None:
    for key in table:
        if key not in known:
            raise ValueError(f"{where}: unknown key {key!r}")


def tag_for(model_path: pathlib.Path) -> str:
    """``local:<gguf-stem>``, lowercased. Derived from the FILE so a tag can
    never claim a model the server is not serving."""
    return TAG_PREFIX + model_path.name.removesuffix(".gguf").lower()


def load_config(path: pathlib.Path) -> LlmConfig:
    """Parse `llm.toml`. NEVER raises.

    An absent file is `present = False` and every local tier is skipped. An
    unreadable or invalid one is `valid = False` — the cycle counts
    `llm_config_invalid` and the dashboard says so — because half a sidecar
    config is more dangerous than none, exactly as with `news-policy.toml`.
    """
    try:
        raw = path.read_bytes()
    except OSError:
        return LlmConfig()
    try:
        doc = tomllib.loads(raw.decode("utf-8"))
        return _config_from(doc)
    except (ValueError, TypeError, UnicodeDecodeError, tomllib.TOMLDecodeError):
        return LlmConfig(present=True, valid=False)


def _config_from(doc: typing.Mapping[str, object]) -> LlmConfig:
    _reject_unknown("llm.toml", doc, _TOP_KEYS)
    server = doc.get("server")
    if not isinstance(server, dict):
        raise ValueError("llm.toml: [server] is required")
    _reject_unknown("llm.toml [server]", server, _SERVER_KEYS)
    tags = doc.get("tags") or {}
    if not isinstance(tags, dict):
        raise ValueError("llm.toml: [tags] must be a table")
    _reject_unknown("llm.toml [tags]", tags, _TAG_KEYS)
    fallback = doc.get("fallback") or {}
    if not isinstance(fallback, dict):
        raise ValueError("llm.toml: [fallback] must be a table")
    _reject_unknown("llm.toml [fallback]", fallback, _FALLBACK_KEYS)
    model = pathlib.Path(str(server.get("model", ""))).expanduser()
    if not str(model):
        raise ValueError("llm.toml [server]: model is required")
    fallback_model = pathlib.Path(str(fallback.get("model", ""))).expanduser()
    return LlmConfig(
        present=True,
        valid=True,
        host=str(server.get("host", DEFAULT_HOST)),
        port=int(typing.cast(int, server.get("port", DEFAULT_PORT))),
        model_path=model,
        sha256=str(server.get("sha256", "")).lower(),
        model_tag=str(tags.get("model_tag", "")) or tag_for(model),
        ctx_size=int(typing.cast(int, server.get("ctx_size", 4096))),
        threads=int(typing.cast(int, server.get("threads", 4))),
        gpu_layers=int(typing.cast(int, server.get("gpu_layers", 99))),
        parallel=int(typing.cast(int, server.get("parallel", 1))),
        cache_reuse=int(typing.cast(int, server.get("cache_reuse", 256))),
        fallback_model_path=fallback_model,
        fallback_sha256=str(fallback.get("sha256", "")).lower(),
        fallback_model_tag=(
            str(fallback.get("model_tag", ""))
            or (tag_for(fallback_model) if str(fallback_model) else "")
        ),
    )


def server_argv(cfg: LlmConfig) -> list[str]:
    """The `llama-server` argv (doc 03 §7).

    Built here rather than in the shell script so the script carries no model
    path and no tuning number — the operator's artifact is the only source,
    and `llm-args` prints this after verifying the sha256.

    CMDLINE LAW: nothing in this argv is `claude_worker`, so the sidecar is
    invisible to the global worker guard and can be resident forever.
    """
    return [
        "llama-server",
        "--host",
        cfg.host,
        "--port",
        str(cfg.port),
        "-m",
        str(cfg.model_path),
        "-c",
        str(cfg.ctx_size),
        "-t",
        str(cfg.threads),
        "-ngl",
        str(cfg.gpu_layers),
        "--parallel",
        str(cfg.parallel),
        "--cache-reuse",
        str(cfg.cache_reuse),
        "--metrics",
        "--no-webui",
    ]


# ------------------------------------------------------------------- client


@dataclasses.dataclass(slots=True)
class LocalStats:
    """What the sidecar did this pass. `unhealthy` and `empty` are the two
    the operator acts on."""

    calls: int = 0
    empty: int = 0
    unhealthy: int = 0
    total_ms: int = 0


class LocalClient:
    """`llama-server`'s OpenAI-compatible endpoint, one request at a time.

    Never raises. Every failure — transport, non-200, a body that is not the
    shape we asked for — is ``""``, counted, and handed to the strict parser,
    which refuses it and counts it the same way it counts a frontier model's
    malformed answer. A sidecar that is down must degrade the cascade to "no
    local tiers this cycle", never break a cycle that has real work to do.
    """

    def __init__(
        self,
        base_url: str,
        http: httpx.Client | None = None,
        timeout_s: float = DEFAULT_TIMEOUT_S,
    ) -> None:
        self._base = base_url.rstrip("/")
        self._http = httpx.Client(timeout=timeout_s) if http is None else http
        self._owns = http is None
        self._timeout_s = timeout_s
        self.stats: LocalStats = LocalStats()

    def close(self) -> None:
        if self._owns:
            self._http.close()

    def __enter__(self) -> "LocalClient":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def health(self) -> bool:
        """`GET /health`. A sidecar still loading its model answers 503, which
        is NOT healthy and NOT an error — the next cycle asks again."""
        try:
            response = self._http.get(
                self._base + HEALTH_PATH, timeout=HEALTH_TIMEOUT_S
            )
        except httpx.HTTPError:
            self.stats.unhealthy += 1
            return False
        if response.status_code != HTTP_OK:
            self.stats.unhealthy += 1
            return False
        return True

    def props(self) -> dict[str, object]:
        """`GET /props` — what the server is ACTUALLY serving. The cycle
        compares this against `llm.toml` so a model file swapped under us is
        a refusal, not a silent change of brain."""
        try:
            response = self._http.get(
                self._base + PROPS_PATH, timeout=HEALTH_TIMEOUT_S
            )
            if response.status_code != HTTP_OK:
                return {}
            doc = response.json()
        except (httpx.HTTPError, ValueError):
            return {}
        return doc if isinstance(doc, dict) else {}

    def metrics(self) -> dict[str, float]:
        """`GET /metrics`, the Prometheus text form, as a flat dict. Best
        effort: the dashboard's `llm` block is a nicety, not a gate."""
        out: dict[str, float] = {}
        try:
            response = self._http.get(
                self._base + METRICS_PATH, timeout=HEALTH_TIMEOUT_S
            )
            if response.status_code != HTTP_OK:
                return out
            body = response.text
        except httpx.HTTPError:
            return out
        lines = body.splitlines()
        for i in range(len(lines)):
            line = lines[i].strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) < _METRIC_FIELDS:
                continue
            name = parts[0].split("{", 1)[0]
            try:
                out[name] = float(parts[len(parts) - 1])
            except ValueError:
                continue
        return out

    def complete(
        self, prompt: str, schema: dict[str, object], name: str = "answer"
    ) -> str:
        """One schema-constrained completion, or ``""``.

        `response_format: json_schema` with `strict: true` is the whole point:
        the sampler may only emit tokens the schema admits, so the answer is
        valid JSON of the right shape or the server refused the request —
        there is no third outcome to parse defensively around.
        """
        payload: dict[str, object] = {
            "messages": [{"role": "user", "content": prompt}],
            "temperature": TEMPERATURE,
            "max_tokens": MAX_TOKENS,
            "stream": False,
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": name, "schema": schema, "strict": True},
            },
            "chat_template_kwargs": dict(THINKING_KWARGS),
        }
        self.stats.calls += 1
        try:
            response = self._http.post(
                self._base + CHAT_PATH, json=payload, timeout=self._timeout_s
            )
        except httpx.HTTPError:
            self.stats.empty += 1
            return ""
        if response.status_code != HTTP_OK:
            self.stats.empty += 1
            return ""
        try:
            doc = response.json()
        except ValueError:
            self.stats.empty += 1
            return ""
        text = _first_message(doc)
        if not text:
            self.stats.empty += 1
        return text


def _first_message(doc: object) -> str:
    """The assistant text out of an OpenAI-shaped body, defensively: a double
    or a future server version that reshapes this yields ``""`` rather than a
    traceback in a 120 s lane."""
    if not isinstance(doc, dict):
        return ""
    choices = doc.get("choices")
    if not isinstance(choices, list) or not choices:
        return ""
    first = choices[0]
    if not isinstance(first, dict):
        return ""
    message = first.get("message")
    if not isinstance(message, dict):
        return ""
    content = message.get("content")
    return content if isinstance(content, str) else ""


def complete_fn_for(
    client: LocalClient, schema: dict[str, object], name: str = "answer"
) -> typing.Callable[[str, str], str]:
    """The `(model, prompt) -> str` seam `cascade.complete_cached` takes.

    The model string is ignored here — the server has exactly one model
    resident, and which one it is belongs in the TAG the caller passes to
    `complete_cached`, not in a routing decision made this far down.
    """

    def complete(model: str, prompt: str) -> str:
        del model
        return client.complete(prompt, schema, name)

    return complete


def props_model_stem(props: typing.Mapping[str, object]) -> str:
    """The gguf stem `/props` reports, lowercased — comparable to
    `tag_for(...)` minus its prefix. `llama-server` has moved this field
    between versions, so several spellings are tried and an unknown shape
    yields ``""`` (the caller then trusts `llm.toml`, and says so)."""
    for key in ("model_path", "model", "default_generation_settings"):
        value = props.get(key)
        if isinstance(value, str) and value:
            return pathlib.Path(value).name.removesuffix(".gguf").lower()
        if isinstance(value, dict):
            nested = value.get("model") or value.get("model_path")
            if isinstance(nested, str) and nested:
                return pathlib.Path(nested).name.removesuffix(".gguf").lower()
    return ""


def json_dumps(schema: dict[str, object]) -> str:
    """The schema as the server receives it — exposed so a test and the
    `llm-args` lane can show the operator the exact grammar in force."""
    return json.dumps(schema, sort_keys=True, separators=(",", ":"))


# --------------------------------------------------- the cycle's local tiers

#: Local spend gets its OWN `budget` rows. `complete_cached` writes
#: `(day, tier)`, so reusing `tier1` would add free calls to the counter that
#: means money and make the scorecard's `cost_24h` a lie in the one direction
#: nobody would notice. The `budget` table's `tier` column is TEXT, so this
#: needs no schema change.
LOCAL_TIER_SUFFIX: str = "_local"

#: Counters (`news.db` `counters`), so an operator reading the panel can tell
#: "the sidecar is down" from "the sidecar answered nonsense".
COUNTER_CALLS: str = "llm_calls"
COUNTER_UNHEALTHY: str = "llm_unhealthy"
COUNTER_EMPTY: str = "llm_empty"
COUNTER_MISMATCH: str = "llm_model_mismatch"
COUNTER_CONFIG_INVALID: str = "llm_config_invalid"
COUNTER_BUDGET: str = "llm_budget_spent"


def local_tier(tier: str) -> str:
    return tier + LOCAL_TIER_SUFFIX


@dataclasses.dataclass(slots=True)
class LocalRunStats:
    """What one cycle's local tiers did."""

    healthy: bool = False
    tier1: int = 0
    tier2: int = 0
    calls: int = 0
    empty: int = 0
    spent_today: int = 0

    def line(self) -> str:
        return (
            f"llm: health={'up' if self.healthy else 'down'} triaged={self.tier1} "
            f"labeled={self.tier2} calls={self.calls} empty={self.empty} "
            f"spent_today={self.spent_today}"
        )


def _spent_today(
    store: "claude_worker.news.store.Store", now_ts: int
) -> int:
    total = 0
    for tier in claude_worker.news.cascade.TIERS:
        total += store.budget_today(local_tier(tier), now_ts)["calls"]
    return total


def run_local_tiers(  # noqa: PLR0913 — composition root: every collaborator named
    *,
    state: "claude_worker.state.State",
    store: "claude_worker.news.store.Store",
    registry: "claude_worker.news.sources.Registry",
    policy: "claude_worker.news.actions.NewsPolicy",
    cfg: LlmConfig,
    markets: typing.Mapping[str, int],
    vocab: "claude_worker.news.filter.Vocabulary",
    now_ts: int,
    client: LocalClient | None = None,
) -> LocalRunStats:
    """Tier 1 and tier 2 through the sidecar, bounded (doc 03 §4).

    Every guard is a REFUSAL to ask, not a failure: no `llm.toml`, a tier the
    policy has not routed local, a server that is down, a server serving a
    model `llm.toml` does not pin, or the day's local budget spent — each one
    means "no local tiers this cycle", counted, and the cycle carries on with
    the work that needs no model at all.

    The two passes are `ids`-restricted rather than batch-limited so the pass
    touches EXACTLY the rows this invocation chose. That is the same
    mechanism the session path uses, for the same reason: `complete_cached`
    caches whatever it is handed, so the set of questions asked has to be
    decided before any of them is asked.
    """
    stats = LocalRunStats()
    wants1 = policy.routes_local(claude_worker.news.cascade.TIER1)
    wants2 = policy.routes_local(claude_worker.news.cascade.TIER2)
    if not cfg.present or not cfg.valid or not (wants1 or wants2):
        if cfg.present and not cfg.valid:
            store.counter_inc(COUNTER_CONFIG_INVALID)
        return stats
    owned = client is None
    local = LocalClient(cfg.base_url) if client is None else client
    try:
        if not local.health():
            store.counter_inc(COUNTER_UNHEALTHY)
            return stats
        stats.healthy = True
        if not _model_matches(local, cfg, store):
            return stats
        stats.spent_today = _spent_today(store, now_ts)
        if stats.spent_today >= policy.local_calls_per_day:
            store.counter_inc(COUNTER_BUDGET)
            return stats
        if wants1:
            stats.tier1 = _local_triage(
                state, store, registry, policy, cfg, markets, vocab, now_ts, local
            )
        if wants2:
            stats.tier2 = _local_label(
                state, store, registry, policy, cfg, markets, vocab, now_ts, local
            )
    finally:
        stats.calls = local.stats.calls
        stats.empty = local.stats.empty
        if local.stats.calls:
            store.counter_inc(COUNTER_CALLS, local.stats.calls)
        if local.stats.empty:
            store.counter_inc(COUNTER_EMPTY, local.stats.empty)
        if owned:
            local.close()
    return stats


def _model_matches(
    local: LocalClient, cfg: LlmConfig, store: "claude_worker.news.store.Store"
) -> bool:
    """The server must be serving the model `llm.toml` pins.

    A model file swapped under us would silently change the brain behind a
    tag the scorecard splits on — every row before and after would be pooled
    as one model. `/props` has moved this field between `llama-server`
    versions, so an UNREADABLE answer trusts `llm.toml` (the tag is the
    operator's claim either way); a READABLE answer that disagrees refuses.
    """
    stem = props_model_stem(local.props())
    if not stem:
        return True
    if stem == cfg.model_path.name.removesuffix(".gguf").lower():
        return True
    store.counter_inc(COUNTER_MISMATCH)
    return False


def _watcher(  # noqa: PLR0913, PLR0917 — composition root: every collaborator named
    state: "claude_worker.state.State",
    store: "claude_worker.news.store.Store",
    registry: "claude_worker.news.sources.Registry",
    markets: typing.Mapping[str, int],
    vocab: "claude_worker.news.filter.Vocabulary",
    now_ts: int,
    tag: str,
    complete: typing.Callable[[str, str], str],
    ids: typing.AbstractSet[str],
    ceilings: typing.Mapping[str, int],
) -> "claude_worker.news.cascade.NewsQueueWatcher":
    return claude_worker.news.cascade.NewsQueueWatcher(
        state=state,
        store=store,
        registry=registry,
        symbol_map=dict(markets),
        vocab=vocab.assets,
        complete_fn=complete,
        ceilings=ceilings,
        models=claude_worker.news.cascade.Models(tier1=tag, tier2=tag, tier3=tag),
        now_fn=lambda: now_ts,
        ids=ids,
        budget_suffix=LOCAL_TIER_SUFFIX,
    )


def _ceilings_for(policy: "claude_worker.news.actions.NewsPolicy") -> dict[str, int]:
    """Local spend is bounded by `local_calls_per_day` on its OWN tier keys,
    never by the paid ceilings."""
    out: dict[str, int] = {}
    for tier in claude_worker.news.cascade.TIERS:
        out[local_tier(tier)] = policy.local_calls_per_day
    return out


def _local_triage(  # noqa: PLR0913, PLR0917 — one pass, every collaborator it needs
    state: "claude_worker.state.State",
    store: "claude_worker.news.store.Store",
    registry: "claude_worker.news.sources.Registry",
    policy: "claude_worker.news.actions.NewsPolicy",
    cfg: LlmConfig,
    markets: typing.Mapping[str, int],
    vocab: "claude_worker.news.filter.Vocabulary",
    now_ts: int,
    local: LocalClient,
) -> int:
    rows = store.items_pending_triage(policy.tier1_batch_max)
    if not rows:
        return 0
    ids: set[str] = set()
    for i in range(len(rows)):
        ids.add(claude_worker.news.cascade.item_id(rows[i]))
    schema = triage_schema(vocab.assets)
    watcher = _watcher(
        state, store, registry, markets, vocab, now_ts, cfg.model_tag,
        complete_fn_for(local, schema, "triage_v2"), ids, _ceilings_for(policy),
    )
    watcher.poll_once(tiers=(claude_worker.news.cascade.TIER1,))
    return len(ids)


def _local_label(  # noqa: PLR0913, PLR0917 — one pass, every collaborator it needs
    state: "claude_worker.state.State",
    store: "claude_worker.news.store.Store",
    registry: "claude_worker.news.sources.Registry",
    policy: "claude_worker.news.actions.NewsPolicy",
    cfg: LlmConfig,
    markets: typing.Mapping[str, int],
    vocab: "claude_worker.news.filter.Vocabulary",
    now_ts: int,
    local: LocalClient,
) -> int:
    horizon = claude_worker.news.cascade.label_horizon(registry.settings)
    stories = store.stories_unlabeled(now_ts - horizon)
    if not stories:
        return 0
    ids: set[str] = set()
    for i in range(min(policy.tier2_batch_max, len(stories))):
        ids.add(str(stories[i]["story_id"]))
    schema = label_schema(sorted(markets))
    watcher = _watcher(
        state, store, registry, markets, vocab, now_ts, cfg.model_tag,
        complete_fn_for(local, schema, "label_v2"), ids, _ceilings_for(policy),
    )
    watcher.poll_once(tiers=(claude_worker.news.cascade.TIER2,))
    return len(ids)
