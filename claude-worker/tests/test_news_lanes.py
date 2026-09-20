# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §12 — the lanes, and the tracked artifacts they read.

No socket is opened: `httpx.Client` is monkeypatched to a `MockTransport`
for the one end-to-end `cycle`, and every other lane needs no network at all.
No model is reached by any lane here, which a test asserts directly.

The most valuable test in this file is the one that loads the SHIPPED
`news.toml.example` through the real `load_registry`: it is the tracked
contract between the operator's file and this package, and a stanza that
drifts out of the grammar should fail here rather than at 03:00 on a
launchd slot.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import tomllib
import typing

import httpx
import pytest

import claude_worker.llm
import claude_worker.news
import claude_worker.news.__main__
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.cycle
import claude_worker.news.filter
import claude_worker.news.sources
import claude_worker.news.store

REPO_ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[2]

_RSS = (
    '<?xml version="1.0"?><rss version="2.0"><channel>'
    "<item><guid>a1</guid><link>https://press.example/a1</link>"
    "<title>OKX will delist the FOO perpetual swap</title>"
    "<pubDate>Sat, 19 Sep 2026 12:00:00 +0000</pubDate>"
    "<description>The venue said withdrawals continue.</description></item>"
    "<item><guid>a2</guid><link>https://press.example/a2</link>"
    "<title>A pleasant day in the park</title>"
    "<pubDate>Sat, 19 Sep 2026 12:05:00 +0000</pubDate>"
    "<description>Nothing to see.</description></item>"
    "</channel></rss>"
)
_OKX_INSTRUMENTS = json.dumps(
    {"data": [{"instId": "BTC-USDT-SWAP", "instType": "SWAP", "state": "live"}]}
)
_FNG = json.dumps({"data": [{"value": "44", "timestamp": "1758290000"}]})

_SOURCES: tuple[str, ...] = (
    '{ name = "press", kind = "rss", url = "https://p.example/f",'
    ' origin = "p.example", class = "C" }',
    '{ name = "okx-inst", kind = "instruments-okx", url = "https://o.example/i",'
    ' origin = "o.example", class = "A", venue = "okx", poll_s = 300 }',
    '{ name = "fng", kind = "series-fng", url = "https://f.example/f",'
    ' origin = "f.example", class = "D", poll_s = 3600 }',
)

_TOML: str = (
    "[news]\npoll_default_s = 60\nnear_dup_jaccard = 0.6\n\n"
    '[keywords]\nevent = ["delist", "withdrawals"]\n\n'
    "[registry]\nsources = [\n" + ",\n".join(_SOURCES) + ",\n]\n"
)

_ROUTES: dict[str, str] = {
    "p.example": _RSS,
    "o.example": _OKX_INSTRUMENTS,
    "f.example": _FNG,
}


def _handler(request: httpx.Request) -> httpx.Response:
    body = _ROUTES.get(request.url.host)
    if body is None:
        return httpx.Response(404, text="no route")
    return httpx.Response(200, text=body)


def _install_transport(
    monkeypatch: pytest.MonkeyPatch,
    handler: typing.Callable[[httpx.Request], httpx.Response] = _handler,
) -> None:
    """Make every `httpx.Client()` this lane builds a MockTransport client."""
    real = httpx.Client

    def factory(*args: object, **kwargs: object) -> httpx.Client:
        del args, kwargs
        return real(transport=httpx.MockTransport(handler))

    monkeypatch.setattr(claude_worker.news.__main__.httpx, "Client", factory)


def _env(monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, toml: str | None) -> None:
    news_dir = tmp_path / "worker" / "news"
    monkeypatch.setenv("CLAUDE_WORKER_NEWS_DIR", str(news_dir))
    monkeypatch.setenv("CLAUDE_WORKER_DB", str(tmp_path / "worker" / "state.db"))
    monkeypatch.setenv("CLAUDE_WORKER_MARKET_MAP", str(tmp_path / "market-map.json"))
    monkeypatch.setenv("CLAUDE_WORKER_REPLAY_DIR", str(tmp_path / "logs"))
    monkeypatch.setenv("CLAUDE_WORKER_MULTIVENUE_DIR", str(tmp_path))
    path = tmp_path / "news.toml"
    if toml is not None:
        path.write_text(toml, encoding="utf-8")
    monkeypatch.setenv("NEWS_TOML", str(path))


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


# ---- the degraded mode every lane owes the operator ----


def test_every_lane_is_an_honest_no_op_without_a_registry(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, None)
    registry_lanes = (
        ["cycle"],
        ["health"],
        ["report"],
        ["prompts", "--tier", "1"],
        ["ingest", "--tier", "1", "--answers", str(tmp_path / "absent.ndjson")],
        ["actions"],
    )
    store_lanes = (
        ["resolve"],
        ["scorecard"],
        ["proposals"],
        ["replay", "--since", "2026-09-01T00:00:00Z", "--out", str(tmp_path / "r.tsv")],
    )
    for argv in registry_lanes + store_lanes:
        # Every lane, including the six §14/§12 ones, answers the absent
        # registry the same way: one printed line and exit 0. The registry
        # question comes FIRST, before a lane looks at its own arguments —
        # an `ingest` whose answers file is also missing still exits 0,
        # because there is nothing here to ingest into.
        assert claude_worker.news.__main__.main(argv) == (
            claude_worker.news.__main__.EXIT_OK
        ), argv
    out = capsys.readouterr().out
    assert "nothing to do" in out
    assert "no store" in out


# ---- cycle ----


def test_cycle_fetches_parses_filters_and_stores(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    assert claude_worker.news.__main__.main(["cycle"]) == claude_worker.news.__main__.EXIT_OK
    line = capsys.readouterr().out
    assert "news cycle: sources=3 ok=3" in line
    assert "items_new=2" in line
    # One headline carries the vocabulary, the other does not.
    assert "pass=1" in line and "vocab=1" in line

    with _store(tmp_path) as store:
        rows = store.items_since(0)
        assert {str(r["guid"]) for r in rows} == {"a1", "a2"}
        verdicts = {str(r["guid"]): str(r["tier0"]) for r in rows}
        assert verdicts["a1"] == claude_worker.news.filter.TIER0_PASS
        assert verdicts["a2"] == claude_worker.news.filter.TIER0_DROP_VOCAB
        snapshot = store.latest_snapshot("okx-inst")
        assert snapshot is not None and snapshot.get("count") == 1
        assert store.series_tail("fng", "fng", 5) == [(1_758_290_000, 44.0)]
        health = {str(r["name"]): r for r in store.source_rows()}
        assert int(typing.cast(int, health["press"]["polls_ok"])) == 1
        assert int(typing.cast(int, health["press"]["items_total"])) == 2


def test_a_second_cycle_stores_nothing_twice(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    claude_worker.news.__main__.main(["cycle"])
    capsys.readouterr()
    claude_worker.news.__main__.main(["cycle"])
    line = capsys.readouterr().out
    assert "items_new=0" in line
    assert "dup_items=2" in line
    with _store(tmp_path) as store:
        assert len(store.items_since(0)) == 2


def test_cycle_counts_an_off_origin_redirect_as_a_refusal(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    def hijacked(request: httpx.Request) -> httpx.Response:
        if request.url.host == "p.example":
            return httpx.Response(302, headers={"location": "https://evil.example/feed"})
        return _handler(request)

    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch, hijacked)
    assert claude_worker.news.__main__.main(["cycle"]) == claude_worker.news.__main__.EXIT_OK
    assert "refused_origin=1" in capsys.readouterr().out
    with _store(tmp_path) as store:
        press = {str(r["name"]): r for r in store.source_rows()}["press"]
        assert int(typing.cast(int, press["refused_origin_total"])) == 1
        assert int(typing.cast(int, press["err_streak"])) == 1
        assert store.items_since(0) == []


def test_a_reshaped_wire_is_counted_not_raised(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    def garbled(request: httpx.Request) -> httpx.Response:
        return httpx.Response(200, text="<<< not a payload >>>")

    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch, garbled)
    assert claude_worker.news.__main__.main(["cycle"]) == claude_worker.news.__main__.EXIT_OK
    assert "parse_empty=3" in capsys.readouterr().out
    with _store(tmp_path) as store:
        assert store.counters()[claude_worker.news.store.COUNTER_PARSE_EMPTY] == 3


def test_aggregate_once_stops_taking_sources_at_the_deadline(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    registry = claude_worker.news.sources.load_registry(tmp_path / "news.toml")
    with _store(tmp_path) as store, httpx.Client(
        transport=httpx.MockTransport(_handler)
    ) as http:
        claude_worker.news.cycle.register_sources(store, registry)
        fetcher = claude_worker.news.sources.Fetcher(registry, http)
        stats = claude_worker.news.cycle.aggregate_once(
            fetcher,
            store,
            registry=registry,
            vocab=claude_worker.news.filter.build_vocabulary([], [], ["delist"]),
            recent=claude_worker.news.filter.RecentTitles(),
            caps=claude_worker.news.filter.Caps(
                per_source_remaining={"press": 9}, tier1_remaining=9
            ),
            now_ns=0,
            now_ts=1_758_290_000,
            take_until_ns=-1,
        )
    assert stats.sources == 3
    assert stats.deadline_skipped == 3
    assert stats.ok == 0


# ---- health ----


def test_health_tables_the_sources_and_fails_on_a_deep_streak(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    claude_worker.news.__main__.main(["cycle"])
    capsys.readouterr()
    assert claude_worker.news.__main__.main(["health"]) == claude_worker.news.__main__.EXIT_OK
    table = capsys.readouterr().out
    assert "press" in table and "okx-inst" in table and "1/1" in table

    with _store(tmp_path) as store:
        for i in range(claude_worker.news.cycle.ERR_STREAK_ALERT):
            store.record_poll(
                "press", 100 + i, claude_worker.news.store.PollOutcome(error="http 503")
            )
    assert claude_worker.news.__main__.main(["health"]) == claude_worker.news.__main__.EXIT_REFUSED


def test_health_ignores_a_disabled_sources_streak(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    with _store(tmp_path) as store:
        store.upsert_source("off", "rss", "C", "off.example", 0)
        for i in range(claude_worker.news.cycle.ERR_STREAK_ALERT + 5):
            store.record_poll("off", 100 + i, claude_worker.news.store.PollOutcome(error="down"))
    assert claude_worker.news.__main__.main(["health"]) == claude_worker.news.__main__.EXIT_OK
    capsys.readouterr()


# ---- probe / report / migrate-feeds ----


def test_probe_refuses_a_source_that_is_not_in_the_registry(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    code = claude_worker.news.__main__.main(["probe", "--source", "nope"])
    assert code == claude_worker.news.__main__.EXIT_REFUSED
    assert "no source named" in capsys.readouterr().err


def test_probe_records_a_fixture_that_is_its_own_reduction(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    out = tmp_path / "fixtures"
    code = claude_worker.news.__main__.main(
        ["probe", "--source", "okx-inst", "--record", "--out", str(out)]
    )
    assert code == claude_worker.news.__main__.EXIT_OK
    capsys.readouterr()
    body = (out / "instruments-okx.json").read_text(encoding="utf-8")
    assert claude_worker.news.sources.reduce_payload("instruments-okx", body) == body


def test_report_prints_the_funnel(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    claude_worker.news.__main__.main(["cycle"])
    capsys.readouterr()
    assert claude_worker.news.__main__.main(["report", "--hours", "24"]) == 0
    out = capsys.readouterr().out
    assert "items=2" in out
    assert '"pass": 1' in out and '"drop_vocab": 1' in out
    assert '"press": 1' in out


def test_migrate_feeds_emits_stanzas_the_registry_accepts(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    monkeypatch.setenv(
        "RSS_FEEDS",
        "https://cointelegraph.com/rss, https://www.newsbtc.com/feed/,"
        "https://cointelegraph.com/rss,bad",
    )
    assert claude_worker.news.__main__.main(["migrate-feeds"]) == 0
    printed = capsys.readouterr().out
    stanzas = [line for line in printed.splitlines() if line.strip().startswith("{")]
    assert len(stanzas) == 2  # the repeat and the hostless entry are skipped
    # A mill is seeded down-weighted so it is never an independent origin.
    assert any("newsbtc" in s and "weight = 0.3" in s for s in stanzas)
    assert any("cointelegraph" in s and "weight = 1.0" in s for s in stanzas)

    # The real proof: what it printed round-trips through the real parser.
    doc = "[registry]\nsources = [\n" + "\n".join(stanzas) + "\n]\n"
    path = tmp_path / "migrated.toml"
    path.write_text(doc, encoding="utf-8")
    registry = claude_worker.news.sources.load_registry(path)
    assert sorted(s.name for s in registry.sources) == ["cointelegraph-com", "www-newsbtc-com"]


def test_migrate_feeds_without_the_env_key_says_so(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _env(monkeypatch, tmp_path, _TOML)
    monkeypatch.delenv("RSS_FEEDS", raising=False)
    assert claude_worker.news.__main__.main(["migrate-feeds"]) == 0
    assert "RSS_FEEDS is empty" in capsys.readouterr().out


# ---- the tracked artifacts ----


def test_the_shipped_news_example_is_a_valid_registry() -> None:
    registry = claude_worker.news.sources.load_registry(REPO_ROOT / "news.toml.example")
    assert len(registry.sources) > 40
    assert registry.keywords
    # Every kind the example names is one this package implements.
    for source in registry.sources:
        assert source.kind in claude_worker.news.sources.KINDS, source.name
    # The mills ship down-weighted (Q6) and never count as an origin.
    mills = [s for s in registry.sources if s.weight < 1.0]
    assert len(mills) >= 6
    for mill in mills:
        assert not mill.is_origin
    # Every status page the example carries was resolved by hand.
    for source in registry.sources:
        if source.origin.startswith("status."):
            assert source.allow_statuspage == 1, source.name


def test_the_shipped_policy_example_parses_and_keeps_every_mode_conservative() -> None:
    doc = tomllib.loads((REPO_ROOT / "news-policy.toml.example").read_text(encoding="utf-8"))
    assert set(doc) == {"mode", "limits", "budget", "intent", "slots", "halt"}
    # Nothing that can reach a venue ships live.
    for kind in ("set_bias", "declare_vol_high", "order_intent"):
        assert doc["mode"][kind] == "shadow", kind
    assert doc["mode"]["disable_paper_slot"] == "off"   # Q4
    assert doc["halt"]["allow"] == 0                    # Q5
    tier1 = claude_worker.news.filter.DEFAULT_TIER1_CALLS_PER_DAY
    assert doc["budget"]["tier1_calls_per_day"] == tier1


def test_the_launchd_job_is_wired_into_the_installer_and_the_cycle_script() -> None:
    plist = (REPO_ROOT / "launchd" / "com.multivenue.news.plist").read_text(encoding="utf-8")
    assert "<string>com.multivenue.news</string>" in plist
    assert "<key>StartInterval</key><integer>120</integer>" in plist
    assert "@REPO@/scripts/news-cycle.sh" in plist

    installer = (REPO_ROOT / "scripts" / "install-launchd.sh").read_text(encoding="utf-8")
    assert installer.count("com.multivenue.news") == 2  # both label lists

    script = (REPO_ROOT / "scripts" / "news-cycle.sh").read_text(encoding="utf-8")
    assert "multivenue/news.toml" in script          # absent artifact = no-op
    assert "pgrep -f 'claude[-_]worke[r]'" in script  # it yields to every lane
    assert "pgrep -f 'news-cycle-ru[n]" in script     # and to itself


def test_a_cycle_is_invisible_to_every_other_lanes_overlap_guard() -> None:
    """The CMDLINE LAW, inverted for this one lane.

    A cycle is network-bound and runs 25-31 s of its slot (40-50 sources
    fetched in series), so an argv carrying `claude_worker` would make the
    5-minute regime cycle skip about half its slots and the hourly candles
    cycle skip whenever the two met. It therefore runs
    `scripts/news-cycle-run.py` through the venv ALIASED at
    ~/multivenue/venv — `dashboard.sh`'s precedent — and the resulting
    argv, measured on the Mac 2026-09-19, is
    `~/multivenue/venv/bin/python3 <repo>/scripts/news-cycle-run.py`,
    which matches no guard.

    That is safe ONLY because this lane shares no writer with any other:
    the law exists to protect `state.db`'s single seq namespace, and the
    last assertion here is what keeps that premise true — if a later step
    opens a second database from this package, this test fails and the
    invisibility has to be reconsidered before it becomes a corruption.
    """
    script = (REPO_ROOT / "scripts" / "news-cycle.sh").read_text(encoding="utf-8")
    lines = script.splitlines()
    code: list[str] = []
    for i in range(len(lines)):
        stripped = lines[i].strip()
        if stripped and not stripped.startswith("#"):
            code.append(stripped)
    body = "\n".join(code)
    # The comments may name the module freely; what matters is what RUNS.
    assert '"$ALIAS/bin/python3" "$REPO/scripts/news-cycle-run.py"' in body
    assert "claude_worker" not in body, "the module name is back in the argv"
    # The real venv path DOES carry `claude-worker`, which is the whole
    # reason for the alias — so it may appear where the symlink is
    # resolved, and nowhere else.
    carriers: list[str] = []
    for i in range(len(code)):
        if "claude-worker" in code[i]:
            carriers.append(code[i])
    assert carriers == ['VENV="$REPO/claude-worker/.venv"'], carriers
    runner = REPO_ROOT / "scripts" / "news-cycle-run.py"
    assert runner.is_file()
    assert "SPDX-License-Identifier: Apache-2.0" in runner.read_text(encoding="utf-8")

    package = REPO_ROOT / "claude-worker" / "src" / "claude_worker" / "news"
    writers: list[str] = []
    readers: list[str] = []
    for path in sorted(package.glob("*.py")):
        text = path.read_text(encoding="utf-8")
        if "sqlite3.connect" not in text:
            continue
        if "query_only" in text:
            readers.append(path.name)
        else:
            writers.append(path.name)
    # A second WRITER would break the premise outright: the law exists to
    # protect a single seq writer, and this lane may have exactly one
    # database of its own.
    assert writers == ["store.py"], "the news lane opened a second database for writing"
    # A connection SQLite itself refuses writes on cannot corrupt
    # anything, so it is allowed — but which files take one is recorded
    # here, because "read-only" is a claim that has to keep being true.
    # `PRAGMA query_only`, never `?mode=ro`: a read-only URI cannot open a
    # WAL database with un-checkpointed content and no live -shm.
    assert readers == ["resolve.py"], "record every read-only opener deliberately"


def test_no_lane_can_reach_a_model(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """The lane path constructs no Anthropic client, ever."""

    def forbidden(*args: object, **kwargs: object) -> object:
        del args, kwargs
        raise AssertionError("a lane constructed an Anthropic client")

    monkeypatch.setattr(claude_worker.llm, "make_client", forbidden)
    monkeypatch.setattr(claude_worker.news.__main__.claude_worker.llm, "make_client", forbidden)
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    for argv in (
        ["cycle"],
        ["health"],
        ["report"],
        ["migrate-feeds"],
        ["proposals"],
        ["prompts", "--tier", "1", "--out", str(tmp_path / "p1.ndjson")],
        ["prompts", "--tier", "2", "--out", str(tmp_path / "p2.ndjson")],
        ["prompts", "--tier", "3", "--out", str(tmp_path / "p3.ndjson")],
        ["actions"],
        ["actions", "--dry-run"],
        ["resolve"],
        ["scorecard"],
    ):
        assert claude_worker.news.__main__.main(argv) == 0, argv
    capsys.readouterr()


def test_the_session_lanes_round_trip_through_the_real_store(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """§14 end to end on the lane surface: cycle, prompts, answer, ingest.

    This is the test that would catch a prompt the `ingest` lane cannot find
    an answer for — the two lanes build the text independently, minutes
    apart, and the prompt cache keys on it.
    """
    _env(monkeypatch, tmp_path, _TOML)
    _install_transport(monkeypatch)
    assert claude_worker.news.__main__.main(["cycle"]) == 0
    out_path = tmp_path / "p1.ndjson"
    assert claude_worker.news.__main__.main(
        ["prompts", "--tier", "1", "--limit", "5", "--out", str(out_path)]
    ) == 0
    lines = [
        json.loads(line)
        for line in out_path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    assert lines, "the cycle stored a tier-0 survivor, so tier 1 has a question"
    answer = json.dumps(
        {
            "family": "crypto",
            "impact": "high",
            "reason": "the venue is delisting a perp",
            "event_type": "delisting",
            "entities": {"venues": ["okx"], "assets": []},
        }
    )
    answers_path = tmp_path / "a1.ndjson"
    answers_path.write_text(
        "\n".join(json.dumps({"id": line["id"], "response": answer}) for line in lines) + "\n",
        encoding="utf-8",
    )
    assert claude_worker.news.__main__.main(
        ["ingest", "--tier", "1", "--answers", str(answers_path)]
    ) == 0
    printed = capsys.readouterr().out
    assert "news ingest: tier=1" in printed
    assert f"matched={len(lines)}" in printed
    assert "rejected=0" in printed
    with _store(tmp_path) as store:
        rows = store._rows("SELECT * FROM triage", ())
        assert len(rows) == len(lines)
        for i in range(len(rows)):
            assert rows[i]["model"] == claude_worker.news.cascade.MODEL_SESSION
        # Clustering ran: a story exists and the item is attached to it.
        # It is already CLOSED, not open — the fixture's item is dated
        # 2026-09-19 and `close_stale_stories` retires a story with no item
        # for a whole window, which is the right answer for a replayed
        # fixture and is why this asserts the table, not the open queue.
        stories = store._rows("SELECT * FROM stories", ())
        assert len(stories) == 1
        assert int(typing.cast(int, stories[0]["item_count"])) == len(lines)
        for i in range(len(lines)):
            source, guid = str(lines[i]["id"]).split("|", 1)
            item = store.item(source, guid)
            assert item is not None and item["story_id"] == stories[0]["story_id"]


def test_actions_dry_run_forces_shadow(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]
) -> None:
    """`--dry-run` downgrades LIVE to shadow and leaves OFF alone.

    Forcing an `off` kind to shadow would write a row into the `actions`
    table that no policy ever stood behind, and that table is evidence.
    """
    policy = tmp_path / "news-policy.toml"
    policy.write_text(
        "[mode]\nset_bias = \"live\"\nalert = \"live\"\ndeclare_vol_high = \"off\"\n",
        encoding="utf-8",
    )
    monkeypatch.setenv("NEWS_POLICY_TOML", str(policy))
    _env(monkeypatch, tmp_path, _TOML)
    loaded = claude_worker.news.actions.load_policy(policy)
    assert loaded.valid
    assert loaded.mode(claude_worker.news.actions.KIND_SET_BIAS) == "live"
    dry = claude_worker.news.actions.shadow_only(loaded)
    assert dry.mode(claude_worker.news.actions.KIND_SET_BIAS) == "shadow"
    assert dry.mode(claude_worker.news.actions.KIND_ALERT) == "shadow"
    assert dry.mode(claude_worker.news.actions.KIND_DECLARE_VOL_HIGH) == "off"
    # Everything else about the policy is untouched.
    assert dry.limits == loaded.limits and dry.ceilings == loaded.ceilings
    assert claude_worker.news.__main__.main(["actions", "--dry-run"]) == 0
    assert "(dry-run)" in capsys.readouterr().out


def test_an_item_can_never_be_newer_than_the_moment_we_saw_it(
    tmp_path: pathlib.Path,
) -> None:
    """Some venue feeds stamp an entry with the SCHEDULED date of what they
    are announcing. `kraken-status-rss` gave "Rain (RAIN) Delisting" a date
    five days ahead (live store, 2026-09-20).

    A future-dated item never ages out -- `triage_max_age_s` compares against
    ``now - max_age`` -- and its story never closes, because
    `close_stale_stories` needs ``last_ts < now - window``. Both were true of
    the two live items, which sat in the open set indefinitely. The scheduled
    date stays in the title and text where the analyst reads it.
    """
    fetched = 1_789_891_000
    ahead = fetched + 5 * 24 * 3600

    def _item(guid: str, ts: int) -> claude_worker.news.sources.Item:
        return claude_worker.news.sources.Item(
            source="kraken-status-rss", guid=guid, ts=ts,
            title="Rain (RAIN) Delisting", link="l", text="body",
            class_="C", weight=1.0, origin="status.kraken.com",
            venue="kraken", hint="",
        )

    verdict = claude_worker.news.filter.Tier0Verdict(
        kind=claude_worker.news.filter.TIER0_PASS, hits=1
    )
    with claude_worker.news.store.Store(tmp_path / "news.db") as store:
        for guid, ts in (("future", ahead), ("past", fetched - 600), ("absent", 0)):
            claude_worker.news.cycle._store_item(store, _item(guid, ts), verdict, fetched)
        got = {str(r["guid"]): int(typing.cast(int, r["ts"])) for r in store.items_since(0)}

    assert got["future"] == fetched, "clamped to when we saw it"
    assert got["past"] == fetched - 600, "a real publication time is kept"
    assert got["absent"] == fetched, "no timestamp at all still falls back"
