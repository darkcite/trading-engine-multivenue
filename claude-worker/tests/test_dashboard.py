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
                "audit_pnl_version": 1,
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
    (mv / "strategy.conf").write_text("STRATEGY=ai+icdp\n", encoding="utf-8")
    (mv / "fees.toml").write_text('[fees]\npm = "0:350"\n', encoding="utf-8")
    (mv / "universe.toml").write_text(
        '[binance]\nspot = ["btcusdt"]\nusdm = ["btcusdt", "ethusdt"]\n', encoding="utf-8"
    )
    (mv / "icdp.toml").write_text(
        "[[instrument]]\ndescriptor = 'a'\n[[instrument]]\ndescriptor = 'b'\n", encoding="utf-8"
    )
    (mv / ".env").write_text("ANTHROPIC_API_KEY=sk-ant-secret\n", encoding="utf-8")
    return claude_worker.dashboard.Inputs(
        db_path=db,
        reports_dir=reports,
        regime_dir=regime_dir,
        candidates_dir=candidates,
        replay_dir=replay,
        multivenue_dir=mv,
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
    assert doc["config"]["strategy_conf"] == "STRATEGY=ai+icdp\n"
    assert doc["config"]["icdp"]["instruments"] == 2
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
    assert doc["config"]["strategy_conf"] == "STRATEGY=ai+icdp\n"
    assert doc["pnl"]["latest"]["day"] == "2026-09-04"
