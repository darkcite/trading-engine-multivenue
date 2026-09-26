# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG6 §6.2 — the worker dashboard: the ``/api/worker`` document shape
on a tmp worker dir, and the server's routes over a real loopback
socket (``/`` html, ``/api/worker`` JSON, the engine proxies against a
closed port ⇒ 502, unknown ⇒ 404). No live engine, no operator files.

Convention: full ``import x`` only. No ``from x import y``.
"""

import http.client
import http.server
import json
import pathlib
import threading

import pytest

import claude_worker.dashboard
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.resolve
import claude_worker.news.store
import claude_worker.pnl_report
import claude_worker.state

_HEX32 = "ab" * 16
_HEX64 = "cd" * 32


def _worker_dir(tmp_path: pathlib.Path) -> claude_worker.dashboard.Inputs:
    """A worker dir with one of everything the readers touch."""
    worker = tmp_path / "worker"
    worker.mkdir()
    db = worker / "state.db"
    state = claude_worker.state.State(db)
    state.stage_ruleset(
        _HEX64,
        "/tmp/r.json",
        "/tmp/r.report.json",
        "session",
        ts=1_700_000_000,
        model="claude-fable-5",
        thesis="two disjoint vol variants of xv",
    )
    state.mark_ruleset_committed(_HEX64, ts=1_700_000_100)
    state.library_insert(
        "m1",
        "xv-okx",
        "vm-rows",
        "/tmp/m1.json",
        "validated",
        [["fast:vol:low"]],
        {"from": "test"},
        regime_off="soft",
        thesis="xv",
        ts=1_700_000_000,
    )
    state.evidence_upsert(
        "m1",
        "w1",
        "/tmp/w1",
        n_ticks=1000,
        n_fills=12,
        net_usd_0=3.5,
        net_usd_tier=-4.25,
        max_dd_usd=2.0,
        regime_word_mode="0001028080010102",
        judged=True,
        detail_version=4,
        ts=1_700_000_200,
    )
    state.composition_insert(
        _HEX64,
        _HEX32,
        ["m1"],
        {"fast": "0001028080010102", "slow": "0001028080010102"},
        "/tmp/c.json",
        None,
        ts=1_700_000_300,
    )
    state.record_event("dashboard-test", "hello", ts_ns=1_700_000_000_000_000_000)
    state.close()

    regime_dir = worker / "regime"
    regime_dir.mkdir()
    (regime_dir / "declared.json").write_text(
        json.dumps(
            {
                "profiles": {
                    "fast": {
                        "word": "0001028080010102",
                        "dims": {},
                        "ts_ms": 1,
                        "ttl_s": 900,
                        "source": "operator",
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    (regime_dir / "history.ndjson").write_text(
        json.dumps(
            {
                "ts_ms": 10**13 + 5_000_000_000_000,
                "minute": 1,
                "age_min": 0,
                "rows": 1,
                "fast": "0001028080010102",
                "slow": "0001028080010102",
            }
        )
        + "\n",
        encoding="utf-8",
    )
    reports = worker / "reports"
    reports.mkdir()
    (reports / "pnl-2026-09-04.json").write_text(
        json.dumps(
            {
                "audit_pnl_version": 2,
                "day": "2026-09-04",
                "runs": 2,
                "paper": {"fills": 7, "net_usd": "-1.500000"},
                "strategies": [
                    {
                        "strategy_id": 5,
                        "label": "vm",
                        "fills": 7,
                        "net_usd": "-1.500000",
                        "fee_ladder_net_usd": ["-1.5", "-2.5", "-9.0"],
                        "max_drawdown_usd": "3.0",
                    }
                ],
                "vm_by_ruleset": [],
                "regime": {"modes": {}, "profiles": []},
                "runs_detail": [{"run": "run-1", "report": {"big": "x" * 1000}}],
            }
        ),
        encoding="utf-8",
    )
    candidates = worker / "candidates"
    candidates.mkdir()
    (candidates / "cand-1.json").write_text("{}", encoding="utf-8")
    replay = tmp_path / "logs"
    (replay / "run-1700000000000000000").mkdir(parents=True)
    mv = tmp_path / "multivenue"
    mv.mkdir()
    (mv / "strategy.conf").write_text("STRATEGY=ai+xmm\n", encoding="utf-8")
    (mv / "fees.toml").write_text('[fees]\npm = "0:350"\n', encoding="utf-8")
    (mv / "universe.toml").write_text(
        '[binance]\nspot = ["btcusdt"]\nusdm = ["btcusdt", "ethusdt"]\n', encoding="utf-8"
    )
    (mv / "xmm.toml").write_text(
        "[xmm]\nmaker_enabled = 1\nquote_btc = 1\nquote_sol = 1\nquote_xrp = 0\n", encoding="utf-8"
    )
    (mv / ".env").write_text("ANTHROPIC_API_KEY=sk-ant-secret\n", encoding="utf-8")
    return claude_worker.dashboard.Inputs(
        db_path=db,
        reports_dir=reports,
        regime_dir=regime_dir,
        candidates_dir=candidates,
        replay_dir=replay,
        multivenue_dir=mv,
        news_dir=worker / "news",
        news_policy_path=mv / "news-policy.toml",
        news_llm_path=mv / "llm.toml",
        engine_url="http://127.0.0.1:1",  # nothing listens here — the proxy must 502
    )


def test_worker_payload_shape(tmp_path: pathlib.Path) -> None:
    inputs = _worker_dir(tmp_path)
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=10**13 + 5_000_000_000_000)
    assert doc["v"] == 1
    assert doc["db"]["present"] is True
    # rulesets registry row, committed.
    assert doc["rulesets"][0]["hash"] == _HEX64
    assert doc["rulesets"][0]["committed_ts"] == 1_700_000_100
    # library member with its evidence roll-up.
    m = doc["library"][0]
    assert m["member_id"] == "m1" and m["status"] == "validated"
    assert m["evidence_n"] == 1 and m["evidence_fills"] == 12
    assert m["evidence_net_usd_0"] == 3.5 and m["evidence_net_usd_tier"] == -4.25
    assert m["evidence"][0]["window_id"] == "w1"
    # composition link.
    assert doc["compositions"][0]["hash128"] == _HEX32
    assert doc["compositions"][0]["member_ids"] == ["m1"]
    # regime: history within 24 h, declared, the byte map for the page.
    assert len(doc["regime"]["history"]) == 1
    assert doc["regime"]["declared"]["fast"]["source"] == "operator"
    assert doc["regime"]["params"] is None  # no regime.toml in the tmp dir
    assert doc["regime"]["dims"]["source"] == 6
    assert doc["regime"]["values"]["vol"] == ("low", "normal", "high")
    # pnl: latest without the per-run detail; the day series.
    assert doc["pnl"]["latest"]["day"] == "2026-09-04"
    assert "runs_detail" not in doc["pnl"]["latest"]
    assert doc["pnl"]["series"] == [
        {
            "day": "2026-09-04",
            "runs": 2,
            "paper_fills": 7,
            "paper_net_usd": "-1.500000",
            "strategies": {
                "vm": {
                    "net_usd": "-1.500000",
                    "fee_ladder_net_usd": ["-1.5", "-2.5", "-9.0"],
                    "fills": 7,
                }
            },
        }
    ]
    assert doc["candidates"][0]["name"] == "cand-1.json"
    assert doc["events"][-1]["kind"] == "dashboard-test"
    # positions: the current run has no fills file yet.
    assert doc["positions"]["run_dir"].endswith("run-1700000000000000000")
    assert doc["positions"]["positions"] == [] and doc["positions"]["fills"] == 0
    # config snapshot — and NEVER the .env.
    assert doc["config"]["strategy_conf"] == "STRATEGY=ai+xmm\n"
    assert doc["config"]["xmm"]["quoted"] == ["BTC", "SOL"]
    assert len(doc["config"]["xmm"]["hash"]) == 64
    assert doc["config"]["universe"] == {"binance": {"spot": 1, "usdm": 2}}
    assert doc["config"]["regime_toml"] is None
    flat = json.dumps(doc)
    assert "sk-ant" not in flat and ".env" not in flat
    assert doc["disk"]["free_bytes"] > 0


def test_worker_payload_without_a_db_is_empty_not_an_error(tmp_path: pathlib.Path) -> None:
    inputs = claude_worker.dashboard.Inputs(
        db_path=tmp_path / "missing.db",
        reports_dir=tmp_path / "reports",
        regime_dir=tmp_path / "regime",
        candidates_dir=tmp_path / "candidates",
        replay_dir=tmp_path / "logs",
        multivenue_dir=tmp_path / "mv",
        news_dir=tmp_path / "news",
        news_policy_path=tmp_path / "news-policy.toml",
        news_llm_path=tmp_path / "llm.toml",
        engine_url="http://127.0.0.1:1",
    )
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=0)
    assert doc["db"]["present"] is False
    assert doc["rulesets"] == [] and doc["library"] == [] and doc["events"] == []
    assert doc["pnl"]["latest"] is None and doc["pnl"]["series"] == []
    assert doc["positions"]["run_dir"] is None
    assert doc["config"]["strategy_conf"] is None


def _get(port: int, path: str) -> tuple[int, str, bytes]:
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    conn.request("GET", path)
    resp = conn.getresponse()
    body = resp.read()
    ctype = resp.getheader("Content-Type", "")
    conn.close()
    return resp.status, ctype, body


def test_server_routes_over_loopback(tmp_path: pathlib.Path) -> None:
    inputs = _worker_dir(tmp_path)
    html = claude_worker.dashboard.HTML_PATH.read_bytes()
    assert b"SPDX-License-Identifier: Apache-2.0" in html[:512]
    # The offline law: nothing loads from anywhere (no script/link src, no URL).
    for needle in (b"<script src", b"<link", b"http://", b"https://", b"@import"):
        assert needle not in html, needle
    handler = claude_worker.dashboard.make_handler(claude_worker.dashboard._Cache(inputs), html)
    srv = http.server.HTTPServer(("127.0.0.1", 0), handler)
    port = srv.server_address[1]
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    try:
        status, ctype, body = _get(port, "/")
        assert status == 200 and ctype.startswith("text/html") and body == html
        status, ctype, body = _get(port, "/api/worker")
        assert status == 200 and ctype == "application/json"
        doc = json.loads(body)
        assert doc["v"] == 1 and doc["library"][0]["member_id"] == "m1"
        # Cached: a second call within CACHE_S returns the same bytes.
        assert _get(port, "/api/worker")[2] == body
        status, _ctype, body = _get(port, "/api/engine/state")
        assert status == 502 and b"unreachable" in body
        status, _ctype, _body = _get(port, "/api/engine/metrics")
        assert status == 502
        status, _ctype, _body = _get(port, "/nope")
        assert status == 404
        status, _ctype, _body = _get(port, "/api/engine/other")
        assert status == 404
    finally:
        srv.shutdown()
        srv.server_close()


def test_main_once_prints_the_document(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    inputs = _worker_dir(tmp_path)
    # Every operator default is redirected — a test never reads ~/multivenue.
    monkeypatch.setenv(claude_worker.pnl_report.REPORTS_DIR_ENV, str(inputs.reports_dir))
    monkeypatch.setenv(claude_worker.pnl_report.REPLAY_DIR_ENV, str(inputs.replay_dir))
    monkeypatch.setenv(claude_worker.dashboard.MULTIVENUE_DIR_ENV, str(inputs.multivenue_dir))
    code = claude_worker.dashboard.main(["--once", "--db", str(inputs.db_path)])
    assert code == 0
    doc = json.loads(capsys.readouterr().out)
    assert doc["v"] == 1 and doc["db"]["path"] == str(inputs.db_path)
    assert doc["config"]["strategy_conf"] == "STRATEGY=ai+xmm\n"
    assert doc["pnl"]["latest"]["day"] == "2026-09-04"


# ---- the NEWS panel (NEWS spec §15) ----

_NEWS_NOW_MS: int = 1_758_290_000_000


def _seed_news(inputs: claude_worker.dashboard.Inputs) -> None:
    """A news.db with one healthy source, one failing one, a full tier-0
    funnel and an event — everything the panel reads."""
    now = _NEWS_NOW_MS // 1000
    db = inputs.news_dir / claude_worker.news.DB_FILENAME
    with claude_worker.news.store.Store(db) as store:
        store.upsert_source("okx-ann", "json-okx-ann", "B", "www.okx.com", 1)
        store.upsert_source("mill", "rss", "C", "mill.example", 1)
        store.record_poll("okx-ann", now, claude_worker.news.store.PollOutcome(ok=True))
        for i in range(12):
            store.record_poll(
                "mill", now, claude_worker.news.store.PollOutcome(error="http 503")
            )
        store.record_poll(
            "mill", now, claude_worker.news.store.PollOutcome(refused_origin=True)
        )
        verdicts = (
            claude_worker.news.store.TIER0_PASS,
            claude_worker.news.store.TIER0_DROP_DUP,
            claude_worker.news.store.TIER0_DROP_VOCAB,
            claude_worker.news.store.TIER0_DROP_VOCAB,
        )
        for i in range(len(verdicts)):
            store.upsert_item(
                source="okx-ann",
                guid=f"g{i}",
                ts=now - i,
                fetched_ts=now,
                title=f"headline {i}",
                link="https://www.okx.com/a",
                text="body",
                origin="www.okx.com",
                class_="B",
                weight=1.0,
                venue="okx",
                tier0=verdicts[i],
            )
        store.add_items_total("okx-ann", len(verdicts))
        store.insert_event(
            kind="delisting",
            venue="okx",
            at_ts=now - 10,
            source="okx-inst",
            detail="FOO-USDT-SWAP left the set",
            created_ts=now,
            instrument="FOO-USDT-SWAP",
        )
        store.counter_inc(claude_worker.news.store.COUNTER_PARSE_EMPTY, 3)


def test_the_news_panel_renders_without_a_store(tmp_path: pathlib.Path) -> None:
    """The lane is optional: a worker that never installed news.toml must
    still serve a page, not a 500."""
    inputs = _worker_dir(tmp_path)
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=_NEWS_NOW_MS)
    news = doc["news"]
    assert isinstance(news, dict)
    assert news["db"]["present"] is False
    assert news["sources"] == [] and news["events"] == []
    assert news["funnel_24h"] == {} and news["items_24h"] == 0
    assert news["alert"] is None and news["calendar"] is None and news["scorecard"] is None
    # §15's later additions are empty, not absent: the page reads them
    # unconditionally, and a missing key is a JS exception, not a blank row.
    assert news["stories_open"] == [] and news["actions_24h"] == []
    assert news["budget_today"] == {} and news["timeline_24h"] == []
    assert news["red_rules"] == []
    # An absent llm.toml means no sidecar and, crucially, NOTHING DIALLED: a
    # page that probed 127.0.0.1:9393 every 10 s on a box with no sidecar
    # would be a connection error in the operator's log forever.
    assert news["llm"]["present"] is False
    assert news["llm"]["health"] is False and news["llm"]["url"] == ""
    # An absent policy is the SHIPPED state: every mode off, and VALID —
    # "the operator has not configured this" is a different thing from
    # "the operator's file has a mistake in it", and only the second is a
    # red rule. Both refuse every action.
    assert news["policy"]["present"] is False
    assert news["policy"]["valid"] is True
    assert set(news["policy"]["modes"].values()) == {
        claude_worker.news.actions.MODE_OFF
    }
    assert news["red_rules"] == []


def test_the_news_panel_reports_the_funnel_health_and_events(tmp_path: pathlib.Path) -> None:
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=_NEWS_NOW_MS)
    news = doc["news"]
    assert isinstance(news, dict)
    assert news["db"]["present"] is True
    assert news["items_24h"] == 4
    assert news["funnel_24h"] == {"pass": 1, "drop_dup": 1, "drop_vocab": 2}
    by_name = {str(r["name"]): r for r in news["sources"]}
    assert by_name["okx-ann"]["polls_ok"] == 1
    assert by_name["okx-ann"]["items_total"] == 4
    # The failing source is visibly failing — this is what `health` alerts on.
    assert by_name["mill"]["err_streak"] == 13
    assert by_name["mill"]["refused_origin_total"] == 1
    assert len(news["events"]) == 1
    assert news["events"][0]["instrument"] == "FOO-USDT-SWAP"
    assert news["counters"][claude_worker.news.store.COUNTER_PARSE_EMPTY] == 3


def test_the_news_panel_tail_survives_an_expiry_swarm(tmp_path: pathlib.Path) -> None:
    """MEASURED 2026-09-19: one poll of the live Deribit BTC option chain
    records 190 expiry events dated 18-72 h out. Ordered by `at_ts`, those
    hold every row of a 20-row tail for days and the one event an operator
    needs to see — a delisting — is invisible behind them. The tail is
    therefore ordered by what was RECORDED last, and events sharing a kind,
    a venue and an instant collapse to one row naming the count."""
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    now = _NEWS_NOW_MS // 1000
    expiry_at = now + 18 * 3_600
    with claude_worker.news.store.Store(
        inputs.news_dir / claude_worker.news.DB_FILENAME
    ) as store:
        for i in range(60):
            store.insert_event(
                kind="expiry",
                venue="deribit",
                at_ts=expiry_at,
                source="deribit-instruments-btc-option",
                detail="active option",
                created_ts=now,
                instrument=f"BTC-20SEP26-{100000 + i}-C",
            )
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=_NEWS_NOW_MS)
    news = doc["news"]
    assert isinstance(news, dict)
    events = news["events"]
    assert isinstance(events, list)
    # 60 expiries + the seeded delisting = 2 rows, not 61.
    assert len(events) == 2
    by_kind = {str(row["kind"]): row for row in events}
    assert by_kind["delisting"]["instrument"] == "FOO-USDT-SWAP"
    assert str(by_kind["expiry"]["detail"]).startswith("60 instruments (")
    assert "+57 more" in str(by_kind["expiry"]["detail"])
    assert by_kind["expiry"]["instrument"] == ""
    # The panel reverses the list, so the newest RECORDED row is last here.
    assert events[len(events) - 1]["kind"] == "expiry"


def test_the_news_panel_surfaces_a_red_alert_and_the_calendar(tmp_path: pathlib.Path) -> None:
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    inputs.news_dir.mkdir(parents=True, exist_ok=True)
    (inputs.news_dir / claude_worker.news.ALERT_FILE).write_text(
        "2026-09-19T14:00:00Z venue_risk OKX withdrawals halted\n", encoding="utf-8"
    )
    (inputs.news_dir / claude_worker.news.CALENDAR_FILE).write_text(
        '{"v": 1, "generated_ts": 1758290000, "events": []}', encoding="utf-8"
    )
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=_NEWS_NOW_MS)
    news = doc["news"]
    assert isinstance(news, dict)
    assert "withdrawals halted" in str(news["alert"])
    assert news["calendar"]["v"] == 1


def test_an_unreadable_news_store_is_still_a_page(tmp_path: pathlib.Path) -> None:
    """A truncated or foreign file where news.db belongs renders empty."""
    inputs = _worker_dir(tmp_path)
    inputs.news_dir.mkdir(parents=True, exist_ok=True)
    (inputs.news_dir / claude_worker.news.DB_FILENAME).write_bytes(b"not a database")
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=_NEWS_NOW_MS)
    news = doc["news"]
    assert isinstance(news, dict)
    assert news["db"]["present"] is True
    assert news["sources"] == []


def test_the_news_panel_is_wired_into_the_page() -> None:
    html = claude_worker.dashboard.HTML_PATH.read_text(encoding="utf-8")
    assert 'id="s-news"' in html and 'id="news"' in html
    assert "function renderNews()" in html
    assert "renderNews();" in html


# ---- NEWS §15: the scorecard, the stories board and the timeline ----------


def _seed_news_claims(inputs: claude_worker.dashboard.Inputs) -> str:
    """A story with a label, a RESOLVED resolution, an emitted action and a
    pending claim — the four things the board and the markers read."""
    now = _NEWS_NOW_MS // 1000
    db = inputs.news_dir / claude_worker.news.DB_FILENAME
    story_id = "st1"
    with claude_worker.news.store.Store(db) as store:
        store.upsert_story(
            {
                "story_id": story_id,
                "family": "crypto",
                "event_type": "delisting",
                "venues": '["okx"]',
                "assets": '["BTC"]',
                "first_ts": now - 600,
                "last_ts": now - 60,
                "item_count": 2,
                "origins": 2,
                "venue_origin": 1,
                "max_impact": "high",
                "state": claude_worker.news.store.STORY_LABELED,
                "assessments": 1,
            }
        )
        store.put_label(
            {
                "story_id": story_id,
                "model": claude_worker.news.cascade.MODEL_SESSION,
                "prompt_version": "label-v2",
                "cache_hit": 0,
                "market": "BTC-UP",
                "sym": 7,
                "descriptor": "BTC-UP",
                "venue": 0,
                "direction": "down",
                "confidence": 0.8,
                "half_life_s": 900.0,
                "vol": "up",
                "liquidity": "none",
                "ts": now - 600,
            }
        )
        store.put_resolution(
            {
                "subject_kind": claude_worker.news.resolve.SUBJECT_LABEL,
                "subject_id": story_id,
                "t0": now - 600,
                "t1": now - 60,
                "horizon_s": 540,
                "state": claude_worker.news.resolve.STATE_RESOLVED,
                "descriptor": "BTC-UP",
                "direction": "down",
                "confidence": 0.8,
                "hit": 1,
                "signed_bps": 12.5,
                "fwd_bps": -12.5,
                "vol_claimed": 1,
                "vol_reached_high": 1,
                "rv_ratio": 1.4,
                "resolved_ts": now - 30,
            }
        )
        store.put_resolution(
            {
                "subject_kind": claude_worker.news.resolve.SUBJECT_DECLARE,
                "subject_id": "d1",
                "t0": now - 300,
                "t1": now + 3600,
                "horizon_s": 3900,
                "state": claude_worker.news.resolve.STATE_PENDING,
                "descriptor": "BTC-UP",
                "vol_claimed": 1,
            }
        )
        store.record_action(
            {
                "ts": now - 590,
                "story_id": story_id,
                "kind": claude_worker.news.actions.KIND_SET_BIAS,
                "mode": claude_worker.news.actions.MODE_SHADOW,
                "detail": "",
            }
        )
        store.record_action(
            {
                "ts": now - 580,
                "story_id": story_id,
                "kind": claude_worker.news.actions.KIND_DECLARE_VOL_HIGH,
                "mode": claude_worker.news.actions.RECORD_REFUSED,
                "refused_reason": claude_worker.news.actions.REFUSED_MODE_OFF,
            }
        )
        store.budget_add(
            claude_worker.news.store.day_of(now),
            claude_worker.news.cascade.TIER1,
            calls=11,
            skipped=2,
        )
    return story_id


def test_news_section_carries_the_stories_board_and_the_verdicts(
    tmp_path: pathlib.Path,
) -> None:
    """§15 item 3: one row per open story with its label, the resolution
    that will judge it, and the policy's verdict — the three things the
    operator needs to decide whether the lane is earning anything."""
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    story_id = _seed_news_claims(inputs)
    news = claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)
    board = news["stories_open"]
    assert isinstance(board, list) and len(board) == 1
    row = board[0]
    assert row["story_id"] == story_id
    assert row["event_type"] == "delisting"
    assert row["label"]["direction"] == "down"
    assert row["label"]["model"] == claude_worker.news.cascade.MODEL_SESSION
    assert row["resolution"]["state"] == claude_worker.news.resolve.STATE_RESOLVED
    assert row["resolution"]["hit"] == 1
    # The NEWEST bias verdict, so a refusal followed by an emission reads
    # as emitted rather than as both.
    assert row["verdict"]["mode"] == claude_worker.news.actions.MODE_SHADOW


def test_news_section_counts_actions_and_the_budget(tmp_path: pathlib.Path) -> None:
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    _seed_news_claims(inputs)
    news = claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)
    counted = {(row["kind"], row["mode"]): row["count"] for row in news["actions_24h"]}
    assert counted[
        (claude_worker.news.actions.KIND_SET_BIAS, claude_worker.news.actions.MODE_SHADOW)
    ] == 1
    assert counted[
        (
            claude_worker.news.actions.KIND_DECLARE_VOL_HIGH,
            claude_worker.news.actions.RECORD_REFUSED,
        )
    ] == 1
    budget = news["budget_today"]
    assert budget[claude_worker.news.cascade.TIER1]["calls"] == 11
    assert budget[claude_worker.news.cascade.TIER1]["skipped"] == 2
    # The ceiling comes from the POLICY, and the safe policy an absent file
    # produces carries the DEFAULT ceilings — so the page shows the limit
    # that is actually in force rather than an unbounded 0.
    assert budget[claude_worker.news.cascade.TIER1]["ceiling"] == (
        claude_worker.news.cascade.DEFAULT_CEILINGS[claude_worker.news.cascade.TIER1]
    )


def test_news_section_timeline_carries_pending_and_judged_claims(
    tmp_path: pathlib.Path,
) -> None:
    """§15 item 5. A PENDING claim is a marker too: leaving it out until it
    resolves would make the lane look quieter than it is, and would hide a
    claim whose horizon is still running."""
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    story_id = _seed_news_claims(inputs)
    news = claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)
    markers = news["timeline_24h"]
    assert isinstance(markers, list) and len(markers) == 2
    by_kind = {str(m["kind"]): m for m in markers}
    label = by_kind[claude_worker.news.resolve.SUBJECT_LABEL]
    assert label["story_id"] == story_id
    assert label["state"] == claude_worker.news.resolve.STATE_RESOLVED
    assert label["hit"] == 1 and label["signed_bps"] == 12.5
    assert label["event_type"] == "delisting"
    assert label["ts"] == (_NEWS_NOW_MS // 1000 - 600) * 1000, "ms, like every other ts"
    declare = by_kind[claude_worker.news.resolve.SUBJECT_DECLARE]
    assert declare["state"] == claude_worker.news.resolve.STATE_PENDING
    assert declare["hit"] == 0


def test_news_section_reads_the_scorecard_file_and_never_recomputes_it(
    tmp_path: pathlib.Path,
) -> None:
    """The dashboard READS `scorecard.json`, so the number the operator sees
    and the number the N4 gate reads are the same number. A file that is
    not there is `None`, not a recomputation."""
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    assert claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)["scorecard"] is None
    now = _NEWS_NOW_MS // 1000
    _seed_news_claims(inputs)
    db = inputs.news_dir / claude_worker.news.DB_FILENAME
    with claude_worker.news.store.Store(db) as store:
        doc = claude_worker.news.resolve.build_scorecard(store, now)
        assert claude_worker.news.resolve.write_scorecard(
            inputs.news_dir / claude_worker.news.SCORECARD_FILE, doc
        )
    news = claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)
    card = news["scorecard"]
    assert isinstance(card, dict)
    assert card["generated_ts"] == now
    assert card["windows"]["all"]["direction"]["n"] == 1
    assert card["windows"]["all"]["direction"]["hits"] == 1
    # The legend lives IN the file so the page's text and the gate's rule
    # cannot drift apart — the page must not carry its own copy.
    assert "wilson_lo > base_rate" in str(card["legend"])
    html = (
        pathlib.Path(claude_worker.dashboard.__file__).parent
        / "dashboard"
        / "dashboard.html"
    ).read_text(encoding="utf-8")
    assert "renderNewsScorecard" in html and "SC.legend" in html
    assert "renderNewsMarkers" in html and "timeline_24h" in html


def test_news_red_rules_name_a_dead_source_and_an_invalid_policy(
    tmp_path: pathlib.Path,
) -> None:
    """§15 item 1. The maintenance rule is deliberately NOT here: it lives
    in `detect.maintenance_alerts`, which writes the ALERT file the panel
    already shows, and two implementations would eventually disagree."""
    inputs = _worker_dir(tmp_path)
    _seed_news(inputs)
    inputs.news_policy_path.write_text("[mode]\nnot_a_kind = 'live'\n", encoding="utf-8")
    news = claude_worker.dashboard.news_section(inputs, _NEWS_NOW_MS)
    rules = news["red_rules"]
    assert isinstance(rules, list)
    joined = " · ".join(str(r) for r in rules)
    assert "policy invalid" in joined
    # `mill` failed 12 polls in `_seed_news` and then had one refused by the
    # origin check — a refusal is not an OK poll, so the streak is 13.
    assert "source mill dead 13 polls" in joined
    # An unknown key means EVERY mode off — a typo silences the lane rather
    # than half-configuring it.
    assert news["policy"]["valid"] is False
    modes = news["policy"]["modes"]
    assert set(modes.values()) == {claude_worker.news.actions.MODE_OFF}


def test_the_xmm_summary_reads_the_quoted_perps_and_survives_absence(tmp_path):
    # XMM XH3: the config panel's slot-6 block (it replaced icdp.toml).
    assert claude_worker.dashboard._xmm_summary(tmp_path / "xmm.toml") == {"hash": None, "quoted": []}
    bad = tmp_path / "bad.toml"
    bad.write_text("[xmm\nquote_btc = 1\n", encoding="utf-8")
    assert claude_worker.dashboard._xmm_summary(bad)["quoted"] == []
    ok = tmp_path / "ok.toml"
    ok.write_text("[xmm]\nquote_eth = 1\nquote_btc = 1\nquote_ada = 0\n", encoding="utf-8")
    summary = claude_worker.dashboard._xmm_summary(ok)
    assert summary["quoted"] == ["BTC", "ETH"]
    assert len(summary["hash"]) == 64


# ---- HAR H3.6: the "Volatility (HAR, long)" panel's worker half ---------------


def _har_inputs(tmp_path: pathlib.Path) -> claude_worker.dashboard.Inputs:
    mv = tmp_path / "multivenue"
    (mv / "har").mkdir(parents=True)
    return claude_worker.dashboard.Inputs(
        db_path=tmp_path / "missing.db",
        reports_dir=tmp_path / "reports",
        regime_dir=tmp_path / "regime",
        candidates_dir=tmp_path / "candidates",
        replay_dir=tmp_path / "logs",
        multivenue_dir=mv,
        news_dir=tmp_path / "news",
        news_policy_path=tmp_path / "news-policy.toml",
        news_llm_path=tmp_path / "llm.toml",
        engine_url="http://127.0.0.1:1",
    )


def test_the_har_section_is_off_without_har_toml_and_names_a_refusal(
    tmp_path: pathlib.Path,
) -> None:
    inputs = _har_inputs(tmp_path)
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=0)
    har = doc["har"]
    assert har["configured"] is False and har["error"] is None and har["series"] == []
    assert (har["amber_drift"], har["red_day_age_s"]) == (0.10, 93_600)
    (inputs.multivenue_dir / "har.toml").write_text('[[series]]\nname = "btc"\n', encoding="utf-8")
    har = claude_worker.dashboard.har_section(inputs, 0)
    assert har["configured"] is False
    assert "`name` must be 1..=12 of [A-Z0-9]" in str(har["error"])


def test_the_har_section_carries_the_sources_the_ages_and_the_drift(
    tmp_path: pathlib.Path,
) -> None:
    """The page joins this with ``/state.har`` by name: each series' feed and
    fallbacks, the sources its seed actually spliced (the header's ``span``
    lines), how old the seed and the engine's state are, and ``drift.json``
    as the hourly ``compare`` wrote it -- a torn or foreign file is absent,
    never guessed."""
    inputs = _har_inputs(tmp_path)
    mv = inputs.multivenue_dir
    (mv / "har.toml").write_text(
        '[[series]]\nname = "BTC"\nfeed = "binance-usdm:btcusdt"\nfallback = ["binance:btcusdt"]\n'
        '[[series]]\nname = "BOT"\nfeed = "binance-usdm:botusdt"\n',
        encoding="utf-8",
    )
    har = mv / "har"
    (har / "seed-BTC.tsv").write_text(
        "# har-seed.tsv v1 (HAR H3) -- BTC: feed binance-usdm:btcusdt, now_ms=1.\n"
        "# span binance:btcusdt 1000 2000 30\n"
        "# span binance-usdm:btcusdt 2060 9000 40\n"
        "# span not-a-number x y z\n"
        "V\t1\n"
        "# span after-the-rows 1 2 3\n",
        encoding="utf-8",
    )
    (har / "state-BTC.tsv").write_text("V\t1\n", encoding="utf-8")
    drift = {
        "v": claude_worker.har_seed.DRIFT_VERSION,
        "now_ms": 5,
        "since_ms": 1,
        "until_ms": 5,
        "pairs": [
            {
                "series": "BTC",
                "a": "binance-usdm:btcusdt",
                "b": "binance:btcusdt",
                "days": 14,
                "median_abs": 0.012,
                "p90_abs": 0.03,
                "median_signed": 0.011,
            }
        ],
    }
    (har / "drift.json").write_text(json.dumps(drift), encoding="utf-8")
    now_ms = int((har / "drift.json").stat().st_mtime * 1000) + 90_000
    doc = claude_worker.dashboard.worker_payload(inputs, now_ms=now_ms)["har"]
    assert doc["configured"] is True and doc["error"] is None
    btc, bot = doc["series"]
    assert (btc["name"], btc["feed"], btc["fallback"]) == (
        "BTC",
        "binance-usdm:btcusdt",
        ["binance:btcusdt"],
    )
    assert btc["spans"] == [
        {"descriptor": "binance:btcusdt", "first_ms": 1000, "last_ms": 2000, "minutes": 30},
        {"descriptor": "binance-usdm:btcusdt", "first_ms": 2060, "last_ms": 9000, "minutes": 40},
    ], "only the header's well-formed span lines"
    assert 89 <= btc["seed_age_s"] <= 91 and 89 <= btc["state_age_s"] <= 91
    assert (bot["spans"], bot["seed_age_s"], bot["state_age_s"]) == (None, None, None)
    assert doc["drift"] == drift and 89 <= doc["drift_age_s"] <= 91
    (har / "drift.json").write_text('{"v": 2, "pairs": []}', encoding="utf-8")
    assert claude_worker.dashboard.har_section(inputs, now_ms)["drift"] is None
    (har / "drift.json").write_text('{"v": 1, "pai', encoding="utf-8")
    assert claude_worker.dashboard.har_section(inputs, now_ms)["drift"] is None


def test_the_har_panel_is_wired_into_the_page() -> None:
    html = claude_worker.dashboard.HTML_PATH.read_text(encoding="utf-8")
    assert 'id="s-har"' in html and 'id="har"' in html and 'id="har-sub"' in html
    assert "function renderHar()" in html and "renderHar();" in html
    # It joins the engine's /state.har with the worker's har by name, and the
    # thresholds are the worker's (one place).
    assert "E && E.har" in html and "W && W.har" in html
    assert "WH.red_day_age_s" in html and "WH.amber_drift" in html
