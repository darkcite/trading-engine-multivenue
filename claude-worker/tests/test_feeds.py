"""feeds.py: RSS/Atom mechanics, SQLite dedupe (pre-seeded per §11),
triage→escalate pipeline with an injected complete_fn, prompt-cache
behavior, jittered cadence, and the never-crash failure doctrine.

All HTTP goes through ``httpx.MockTransport`` (§11: canned feed XML,
no live feeds). No SDK anywhere — ``complete_fn`` is a local fake.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import random

import httpx

import claude_worker.config
import claude_worker.feeds
import claude_worker.labeling
import claude_worker.state

FEED_URL = "https://news.example/rss"
SYMBOL_MAP = {"BTC-DAILY": 7}

RSS_XML = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Example</title>
<item><guid>g1</guid><title>Bitcoin ETF approved</title>
<link>https://news.example/1</link>
<pubDate>Sat, 15 Aug 2026 10:00:00 GMT</pubDate>
<description>The SEC approved a spot ETF.</description></item>
<item><guid>g2</guid><title>Local sports roundup</title>
<link>https://news.example/2</link>
<pubDate>Sat, 15 Aug 2026 11:00:00 GMT</pubDate>
<description>The game went fine.</description></item>
</channel></rss>
"""

ATOM_XML = """<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>A</title>
<entry><id>a1</id><title>Fed cuts rates</title>
<link href="https://atom.example/1"/>
<updated>2026-08-15T10:00:00+00:00</updated>
<summary>50bp cut.</summary></entry>
</feed>
"""


def _state(tmp_path: pathlib.Path) -> claude_worker.state.State:
    return claude_worker.state.State(tmp_path / "state.db")


def _client(payload: str = RSS_XML, status: int = 200) -> httpx.Client:
    def handler(_request: httpx.Request) -> httpx.Response:
        return httpx.Response(status, text=payload)

    return httpx.Client(transport=httpx.MockTransport(handler))


class ScriptedCompleter:
    """complete_fn double: routes on prompt content, counts calls."""

    def __init__(self) -> None:
        self.calls: list[str] = []
        self.label_response: str = json.dumps(
            {"market": "BTC-DAILY", "direction": "up", "confidence": 0.9, "half_life_s": 600}
        )
        self.triage_high_when: str = "Bitcoin"

    def __call__(self, model: str, prompt: str) -> str:
        self.calls.append(model)
        if "triage tagger" in prompt:
            impact = "high" if self.triage_high_when in prompt else "low"
            family = "crypto" if impact == "high" else "sports"
            return json.dumps({"family": family, "impact": impact, "reason": "r"})
        return self.label_response


def _watcher(
    state: claude_worker.state.State,
    client: httpx.Client,
    completer: ScriptedCompleter,
) -> claude_worker.feeds.NewsWatcher:
    return claude_worker.feeds.NewsWatcher(
        state=state,
        feeds=(FEED_URL,),
        symbol_map=SYMBOL_MAP,
        complete_fn=completer,
        http_client=client,
        rng=random.Random(42),
        clock_ns=lambda: 0,
    )


# ---- mechanical parsing ------------------------------------------------------


def test_parse_rss() -> None:
    items = claude_worker.feeds.parse_feed_xml(FEED_URL, RSS_XML)
    assert len(items) == 2
    first = items[0]
    assert first.feed == FEED_URL
    assert first.guid == "g1"
    assert first.title == "Bitcoin ETF approved"
    assert first.link == "https://news.example/1"
    assert first.ts > 0
    assert "SEC approved" in first.text


def test_parse_atom() -> None:
    items = claude_worker.feeds.parse_feed_xml("https://atom.example/feed", ATOM_XML)
    assert len(items) == 1
    entry = items[0]
    assert entry.guid == "a1"
    assert entry.link == "https://atom.example/1"
    assert entry.ts == 1_786_788_000  # 2026-08-15T10:00:00+00:00
    assert entry.text == "50bp cut."


def test_parse_garbage_and_guidless() -> None:
    assert claude_worker.feeds.parse_feed_xml(FEED_URL, "<not-xml") == []
    guidless = (
        '<rss version="2.0"><channel>'
        "<item><title>no id at all</title></item>"
        "<item><link>https://x/1</link><title>link fallback</title></item>"
        "</channel></rss>"
    )
    items = claude_worker.feeds.parse_feed_xml(FEED_URL, guidless)
    assert len(items) == 1
    assert items[0].guid == "https://x/1"


def test_text_capped() -> None:
    big = (
        '<rss version="2.0"><channel><item><guid>g</guid>'
        f"<description>{'x' * 3000}</description></item></channel></rss>"
    )
    items = claude_worker.feeds.parse_feed_xml(FEED_URL, big)
    assert len(items[0].text) == claude_worker.feeds.TEXT_CAP


def test_fetch_feed_failures() -> None:
    assert claude_worker.feeds.fetch_feed(_client(status=500), FEED_URL) is None

    def boom(_request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("refused")

    client = httpx.Client(transport=httpx.MockTransport(boom))
    assert claude_worker.feeds.fetch_feed(client, FEED_URL) is None


# ---- dedupe (pre-seeded SQLite per §11) ----------------------------------------


def test_dedupe_against_preseeded_rows(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    state.mark_seen(FEED_URL, "g1", 0)  # pre-seed: g1 already known
    completer = ScriptedCompleter()
    watcher = _watcher(state, _client(), completer)
    stats = watcher.poll_once(0)
    assert stats.new_items == 1  # only g2
    assert stats.dup_items == 1
    assert stats.escalated == 0  # g2 triages low
    state.close()


# ---- pipeline -----------------------------------------------------------------


def test_triage_escalate_label_pipeline(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    watcher = _watcher(state, _client(), completer)
    stats = watcher.poll_once(0)
    assert stats.polled_feeds == 1
    assert stats.new_items == 2
    assert stats.escalated == 1  # only the Bitcoin item
    assert stats.triage_malformed == 0
    assert stats.label_malformed == 0
    assert len(stats.labels) == 1
    label = stats.labels[0]
    assert label.sym == 7
    assert label.direction == "up"
    assert label.confidence == 0.9
    assert label.half_life_s == 600.0
    # Deterministic file order: g1 triage (Haiku) -> g1 label (Sonnet)
    # -> g2 triage (Haiku).
    assert completer.calls == [
        claude_worker.config.MODEL_BULK,
        claude_worker.config.MODEL_REASONING,
        claude_worker.config.MODEL_BULK,
    ]
    state.close()


TWIN_XML = """<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Example</title>
<item><guid>g1</guid><title>Bitcoin ETF approved</title>
<description>The SEC approved a spot ETF.</description></item>
<item><guid>g3</guid><title>Bitcoin ETF approved</title>
<description>The SEC approved a spot ETF.</description></item>
</channel></rss>
"""


def test_identical_content_hits_prompt_cache(tmp_path: pathlib.Path) -> None:
    # g3 repeats g1's title/text under a new guid: dedupe passes it, but
    # both prompts are identical -> served from prompt_cache, zero new
    # LLM calls (the §5.3 cache doing its job).
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    watcher = _watcher(state, _client(TWIN_XML), completer)
    stats = watcher.poll_once(0)
    assert stats.new_items == 2
    assert len(stats.labels) == 2
    assert stats.cache_hits == 2  # second item: triage hit + label hit
    assert len(completer.calls) == 2  # one real triage + one real label
    state.close()


def test_malformed_triage_counted_never_raises(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)

    def bad_complete(_model: str, _prompt: str) -> str:
        return "I cannot classify this."

    watcher = claude_worker.feeds.NewsWatcher(
        state=state,
        feeds=(FEED_URL,),
        symbol_map=SYMBOL_MAP,
        complete_fn=bad_complete,
        http_client=_client(),
        rng=random.Random(42),
        clock_ns=lambda: 0,
    )
    stats = watcher.poll_once(0)
    assert stats.triage_malformed == 2
    assert stats.labels == []
    events = state.events(claude_worker.feeds.EVENT_TRIAGE_MALFORMED)
    assert len(events) == 2
    state.close()


def test_malformed_label_counted(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    completer.label_response = json.dumps({"market": "UNLISTED", "direction": "up"})
    watcher = _watcher(state, _client(), completer)
    stats = watcher.poll_once(0)
    assert stats.escalated == 1
    assert stats.label_malformed == 1
    assert stats.labels == []
    assert len(state.events(claude_worker.feeds.EVENT_LABEL_MALFORMED)) == 1
    state.close()


def test_explicit_label_pass_counted(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    completer.label_response = json.dumps({"market": None})
    watcher = _watcher(state, _client(), completer)
    stats = watcher.poll_once(0)
    assert stats.label_passes == 1
    assert stats.label_malformed == 0
    assert stats.labels == []
    state.close()


def test_fetch_error_counted_and_rescheduled(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    watcher = _watcher(state, _client(status=503), completer)
    stats = watcher.poll_once(0)
    assert stats.fetch_errors == 1
    assert stats.new_items == 0
    # A dead feed must not hot-loop: it was rescheduled with jitter.
    assert watcher.next_due_ns(FEED_URL) >= claude_worker.feeds.POLL_MIN_NS
    state.close()


# ---- cadence -------------------------------------------------------------------


def test_jittered_cadence(tmp_path: pathlib.Path) -> None:
    state = _state(tmp_path)
    completer = ScriptedCompleter()
    watcher = _watcher(state, _client(), completer)
    assert watcher.due_feeds(0) == [FEED_URL]  # first poll immediate
    watcher.poll_once(0)
    due_at = watcher.next_due_ns(FEED_URL)
    assert claude_worker.feeds.POLL_MIN_NS <= due_at <= claude_worker.feeds.POLL_MAX_NS
    assert watcher.due_feeds(due_at - 1) == []
    assert watcher.due_feeds(due_at) == [FEED_URL]
    state.close()
