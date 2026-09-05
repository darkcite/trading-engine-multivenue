# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG5 — the worker regime lane (docs/regime-and-dashboard-plan.md §5.1).

Measurement over candles.db (the engine's seed law, judged at the last
minute the store holds), the report, the 24 h history, the percentile
refresh that rewrites ONLY the six lines, declarations (declared.json +
SetRegime frames through a recording client), the post-boot re-push
with the remaining TTL, the label grammar mirror + ``regime_allows``,
and the module lanes' exit codes. No socket, no network: the engine's
``/metrics`` is an unreachable URL here (the fallback chain is the point).

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib

import pytest

import claude_worker.frames
import claude_worker.regime
import claude_worker.uds

import tests.regime_fixture

EXAMPLE = tests.regime_fixture.EXAMPLE
MEMBERS = tests.regime_fixture.MEMBERS
BTC = tests.regime_fixture.BTC
NOW_MS = tests.regime_fixture.NOW_MS
S = 1_000_000_000
DEAD_URL = "http://127.0.0.1:9/metrics"  # nothing listens on the discard port
_artifact = tests.regime_fixture.artifact
_db = tests.regime_fixture.candles_db


def test_read_regime_params_maps_descriptors_to_dense_ids(tmp_path):
    art = claude_worker.regime.read_regime_params(_artifact(tmp_path))
    assert art.btc == BTC and art.fund == BTC
    assert art.descriptors == [BTC, *MEMBERS]
    assert art.ids[BTC] == 0 and art.params.btc_ref == 0 and art.params.fund_ref == 0
    assert art.params.members == (1, 2, 3, 4)
    assert art.params.confirm_min == 3
    assert art.params.profiles[0].trend_w_min == 60 and art.params.profiles[1].trend_w_min == 240
    assert art.params.profiles[0] == claude_worker.regime.FAST_DEFAULT
    assert art.params.profiles[1] == claude_worker.regime.SLOW_DEFAULT
    # A missing profile key is an error, like the engine's parser.
    bad = tmp_path / "bad.toml"
    bad.write_text(
        "".join(
            line
            for line in EXAMPLE.read_text(encoding="utf-8").splitlines(keepends=True)
            if not line.startswith("fund_prints = 9")
        ),
        encoding="utf-8",
    )
    with pytest.raises(ValueError, match="missing"):
        claude_worker.regime.read_regime_params(bad)


def test_measure_judges_the_last_stored_minute_and_reports_the_lag(tmp_path):
    art = _artifact(tmp_path)
    db = _db(tmp_path, minutes=400, lag_min=0)
    m = claude_worker.regime.measure(art, db, NOW_MS)
    assert m.age_min == 0 and m.minute == NOW_MS // 60_000 - 1
    assert m.rows == 400 * 5
    fast = m.evaluator.measured[0]
    dims = claude_worker.frames.regime_word_dims(fast)
    # A steady +20 bps/min uptrend on BTC and every member: BULL, TREND,
    # ext_up; VOL is unknown (percentiles 0 in the example); FUND from
    # the latest print (+0.0007 ⇒ pos); LEVEL unknown (percentiles 0).
    assert dims["trend"] == "bull" and dims["shape"] == "trend" and dims["stretch"] == "ext_up"
    assert dims["vol"] == "unknown" and dims["level"] == "unknown" and dims["fund"] == "pos"
    assert dims["source"] == "measured"
    assert m.funding is not None and m.funding[0] == 700_000
    assert m.evaluator.rel_of(0, 1) == claude_worker.regime.REL_INLINE
    # A store lagging 45 minutes behind the wall clock judges at ITS last
    # minute and says so (the candles lane is hourly).
    db2 = _db(tmp_path / "lag", minutes=400, lag_min=45)
    m2 = claude_worker.regime.measure(art, db2, NOW_MS)
    assert m2.age_min == 45 and m2.minute == NOW_MS // 60_000 - 46
    assert claude_worker.frames.regime_word_dims(m2.evaluator.measured[0])["trend"] == "bull"


def test_report_history_and_cycle(tmp_path, monkeypatch):
    art = _artifact(tmp_path)
    db = _db(tmp_path)
    directory = tmp_path / "regime"
    monkeypatch.setenv(claude_worker.regime.METRICS_URL_ENV, DEAD_URL)
    m = claude_worker.regime.measure(art, db, NOW_MS)
    text = claude_worker.regime.render_report(m, {}, None, [], NOW_MS)
    assert "[fast] measured: trend=bull shape=trend" in text
    assert "declared: none" in text and "engine: unreachable" in text and "24h: (no history yet)" in text
    # The cycle appends one history line and refreshes the percentiles
    # once per UTC day (a stamp file), never declares.
    lines: list[str] = []
    m1 = claude_worker.regime.run_cycle(art, db, directory, NOW_MS, lines.append)
    assert m1 is not None
    assert (directory / claude_worker.regime.HISTORY_FILE).is_file()
    assert (directory / "params-refreshed-utc-day").is_file()
    assert any("percentiles refreshed" in l for l in lines)
    assert (tmp_path / "regime.toml.bak").is_file(), "the refresh keeps a backup"
    lines.clear()
    claude_worker.regime.run_cycle(art, db, directory, NOW_MS + 60_000, lines.append)
    assert not any("percentiles refreshed" in l for l in lines), "once per day"
    tail = claude_worker.regime.history_tail(directory, NOW_MS + 60_000)
    assert len(tail) == 2 and tail[0]["minute"] == m.minute
    timeline = claude_worker.regime.words_timeline(tail, "fast")
    # The first cycle's refresh set the funding percentiles (9 prints ≥
    # the minimum), so the second measurement judges LEVEL: two runs.
    assert [n for _, n in timeline] == [1, 1]
    assert timeline[0][0].startswith("trend=bull") and "level=unknown" in timeline[0][0]
    assert "level=high" in timeline[1][0]
    # Outside the 24 h window the tail is empty.
    assert claude_worker.regime.history_tail(directory, NOW_MS + 30 * 3_600_000) == []
    text = claude_worker.regime.render_report(m, {}, None, tail, NOW_MS + 60_000)
    assert "24h: trend=bull" in text and " -> " in text
    assert not (directory / claude_worker.regime.DECLARED_FILE).exists(), "the cycle never declares"
    # Absent inputs are honest no-ops.
    assert claude_worker.regime.run_cycle(tmp_path / "nope.toml", db, directory, NOW_MS, lines.append) is None


def test_refresh_params_rewrites_only_the_six_lines(tmp_path):
    art = _artifact(tmp_path)
    db = _db(tmp_path, minutes=8 * 1440)  # 8 days of closes
    before = art.read_text(encoding="utf-8")
    values = claude_worker.regime.refresh_params(art, db, NOW_MS)
    after = art.read_text(encoding="utf-8")
    assert values["fast"]["rv_samples"] >= 150 and values["slow"]["rv_samples"] >= 10
    assert values["fast"]["fund_samples"] == 9 and values["slow"]["fund_samples"] == 12
    # A constant-drift series: every hourly RV sample is the same ⇒ p30 == p70.
    assert values["fast"]["rv_p30_bps_1e9"] == values["fast"]["rv_p70_bps_1e9"] > 0
    # Funding percentiles over the signed rates (nearest rank).
    assert values["fast"]["fund_p30_1e9"] < values["fast"]["fund_p70_1e9"]
    # Only the six percentile lines changed; comments beside them survive.
    diff = [
        (a, b) for a, b in zip(before.splitlines(), after.splitlines(), strict=True) if a != b
    ]
    assert len(diff) == 8, diff  # 4 keys × 2 profiles (fast + slow)
    for a, b in diff:
        key = a.split("=")[0].strip()
        assert key in claude_worker.regime._PERCENTILE_KEYS  # noqa: SLF001
        assert b.startswith(f"{key} = ")
        if "#" in a:
            assert "#" in b, "the comment survived"
    reparsed = claude_worker.regime.read_regime_params(art)
    assert reparsed.params.profiles[0].rv_p30_bps_1e9 == values["fast"]["rv_p30_bps_1e9"]
    # With the percentiles set, VOL judges (constant RV == p30 ⇒ NORMAL).
    m = claude_worker.regime.measure(art, db, NOW_MS)
    assert claude_worker.frames.regime_word_dims(m.evaluator.measured[0])["vol"] == "normal"
    # Too few samples keep zeros (dimension stays ABSENT), honestly.
    tiny = claude_worker.regime.compute_percentiles(art, _db(tmp_path / "tiny", minutes=100), NOW_MS)
    assert tiny["fast"]["rv_p30_bps_1e9"] == 0 and tiny["fast"]["rv_samples"] == 1
    assert tiny["fast"]["rv_p70_bps_1e9"] == 0


def test_percentile_nearest_rank_law():
    p = claude_worker.regime.percentile_nearest_rank
    assert p([], 30) == 0
    assert p([5], 30) == 5 and p([5], 70) == 5
    assert p([10, 20, 30, 40, 50, 60, 70, 80, 90, 100], 30) == 30
    assert p([10, 20, 30, 40, 50, 60, 70, 80, 90, 100], 70) == 70
    assert p([100, 10], 30) == 10 and p([100, 10], 70) == 100


class _RecordingClient:
    """A ``UdsClient`` stand-in: records every ``send_cmd`` kwargs."""

    def __init__(self) -> None:
        self.sent: list[dict[str, object]] = []

    def send_cmd(self, **kwargs: object) -> int:
        self.sent.append(kwargs)
        return len(self.sent)


def test_declarations_persist_send_and_repush_with_remaining_ttl(tmp_path):
    directory = tmp_path / "regime"
    dims = claude_worker.regime.parse_declaration("trend:bull, shape:trend,vol:unknown")
    assert dims == {"trend": "bull", "shape": "trend", "vol": "unknown"}
    word = claude_worker.regime.declaration_word(dims)
    assert claude_worker.frames.regime_word_is_wire_declared(word)
    assert claude_worker.frames.regime_word_dims(word) == dims
    for bad in ("", "trend", "trend:sideways", "source:declared", "trend:bull,trend:bear", "rel:lagging", "trend:bull|neutral"):
        with pytest.raises(ValueError):
            claude_worker.regime.parse_declaration(bad)
    path = claude_worker.regime.persist_declared(directory, {"fast": word}, NOW_MS, 900, "operator")
    assert path == directory / claude_worker.regime.DECLARED_FILE
    entries = claude_worker.regime.load_declared(directory)
    assert set(entries) == {"fast"}
    assert entries["fast"]["dims"] == dims and entries["fast"]["source"] == "operator"
    assert claude_worker.regime.declared_is_fresh(entries["fast"], NOW_MS + 899_000)
    assert not claude_worker.regime.declared_is_fresh(entries["fast"], NOW_MS + 900_000)
    assert not claude_worker.regime.declared_is_fresh(entries["fast"], NOW_MS - 1)
    # A second profile merges; the first entry survives.
    slow = claude_worker.regime.declaration_word({"trend": "neutral"})
    claude_worker.regime.persist_declared(directory, {"slow": slow}, NOW_MS + 60_000, 600, "strategist")
    entries = claude_worker.regime.load_declared(directory)
    assert set(entries) == {"fast", "slow"} and entries["slow"]["source"] == "strategist"
    # Frames: one SetRegime per profile, px = the word, qty = the audit word.
    client = _RecordingClient()
    seqs = claude_worker.regime.send_declarations(
        client, {"fast": word, "slow": slow}, 900 * S, {"fast": 0x0102, "slow": 0}
    )
    assert seqs == [1, 2]
    f0 = client.sent[0]
    assert f0["kind"] == claude_worker.frames.KIND_SET_REGIME
    assert f0["px"] == word and f0["qty"] == 0x0102 and f0["ttl_ns"] == 900 * S
    assert f0["param_id"] == 0 and client.sent[1]["param_id"] == 1
    assert f0["sym"] == claude_worker.frames.SYMBOL_ID_NONE
    assert f0["strategy_id"] == claude_worker.frames.STRATEGY_SLOT_NONE
    # Re-push 5 minutes later: both still fresh, each with ITS remaining
    # TTL — fast has 600 s left, slow (declared 60 s later, ttl 600) 360 s.
    client = _RecordingClient()
    lines: list[str] = []
    n = claude_worker.regime.repush_declared(client, directory, NOW_MS + 300_000, lines.append)
    assert n == 2 and len(client.sent) == 2
    assert client.sent[0]["ttl_ns"] == 600 * S and client.sent[1]["ttl_ns"] == 360 * S
    assert client.sent[0]["px"] == word and client.sent[1]["px"] == slow
    assert any("re-sent 2" in l for l in lines)
    # slow expires first (NOW+660 s): after it only fast goes, with its
    # remaining 239 s; after both expired, nothing.
    client = _RecordingClient()
    assert claude_worker.regime.repush_declared(client, directory, NOW_MS + 661_000, lines.append) == 1
    assert client.sent[0]["param_id"] == 0 and client.sent[0]["ttl_ns"] == 239 * S
    client = _RecordingClient()
    assert claude_worker.regime.repush_declared(client, directory, NOW_MS + 2_000_000, lines.append) == 0
    assert client.sent == []
    # A torn file is a fresh start, never a crash.
    path.write_text("{not json", encoding="utf-8")
    assert claude_worker.regime.load_declared(directory) == {}


def test_label_grammar_mirror_and_regime_allows():
    r = claude_worker.regime
    bull = r.label_masks(["trend:bull"])
    assert bull["slow"] == 0, "an untouched profile stays ANY"
    fast = bull["fast"]
    assert fast & 0xFF == 0b100, "TREND byte = bull only"
    assert (fast >> 8) & 0xFF == 0b111 | r.DIM_UNKNOWN_BIT, "omitted SHAPE = any incl. the mark"
    assert (fast >> 24) & 0xFF == 0b11 | r.DIM_UNKNOWN_BIT, "FUND has two values"
    assert (fast >> 48) & 0xFF == 0b011, "SOURCE defaults to measured|declared"
    assert (fast >> 56) & 0xFF == 0
    assert r.label_masks(["slow:vol:!high"])["fast"] == 0
    assert (r.label_masks(["slow:vol:!high"])["slow"] >> 16) & 0xFF == 0b011
    assert (r.label_masks(["vol:*"])["fast"] >> 16) & 0xFF == 0b111 | r.DIM_UNKNOWN_BIT
    assert (r.label_masks(["vol:low|unknown"])["fast"] >> 16) & 0xFF == 0b001 | r.DIM_UNKNOWN_BIT
    assert (r.label_masks(["source:*"])["fast"] >> 48) & 0xFF == 0b111
    for bad in (
        ["trend:sideways"], ["mood:happy"], ["medium:trend:bull"], ["trend:bull", "fast:trend:bear"],
        ["rel:lagging"], ["trend"], ["trend:"], ["source:unknown|nope"],
    ):
        with pytest.raises(ValueError):
            r.label_masks(bad)
    # The gate: a measured bull word passes a bull label, a bear word does
    # not, UNKNOWN fails every constrained label and passes ANY.
    bull_word = claude_worker.frames.regime_word(
        trend="bull", shape="trend", vol="normal", fund="pos", level="normal", stretch="neutral", source="measured"
    )
    bear_word = claude_worker.frames.regime_word(
        trend="bear", shape="trend", vol="normal", fund="pos", level="normal", stretch="neutral", source="measured"
    )
    assert r.regime_allows(["trend:bull"], {"fast": bull_word, "slow": r.UNKNOWN_WORD})
    assert not r.regime_allows(["trend:bull"], {"fast": bear_word, "slow": r.UNKNOWN_WORD})
    assert not r.regime_allows(["trend:bull"], {"fast": r.UNKNOWN_WORD, "slow": r.UNKNOWN_WORD})
    assert not r.regime_allows(["trend:bull"], {})
    assert r.regime_allows([], {}), "an unlabelled lane is always open"
    assert not r.regime_allows(["trend:bull", "slow:trend:bull"], {"fast": bull_word, "slow": r.UNKNOWN_WORD})
    assert r.regime_allows(["trend:bull", "slow:trend:bull"], {"fast": bull_word, "slow": bull_word})
    # An unknown-marked VOL passes a label that omits VOL and fails one that names it.
    marked = claude_worker.frames.regime_word(trend="bull", vol="unknown", source="measured")
    assert r.regime_allows(["trend:bull"], {"fast": marked})
    assert not r.regime_allows(["trend:bull", "vol:normal"], {"fast": marked})


def test_current_words_fallback_chain(tmp_path):
    r = claude_worker.regime
    directory = tmp_path / "regime"
    # Engine unreachable + nothing declared ⇒ UNKNOWN on both.
    words, source = r.current_words(directory, NOW_MS, DEAD_URL)
    assert source == "unknown" and words == {"fast": r.UNKNOWN_WORD, "slow": r.UNKNOWN_WORD}
    # A fresh declaration stands in, stamped DECLARED over unknown dims.
    word = r.declaration_word({"trend": "bull"})
    r.persist_declared(directory, {"fast": word}, NOW_MS, 900, "operator")
    words, source = r.current_words(directory, NOW_MS + 1_000, DEAD_URL)
    assert source == "declared"
    dims = claude_worker.frames.regime_word_dims(words["fast"])
    assert dims["trend"] == "bull" and dims["source"] == "declared" and dims["shape"] == "unknown"
    assert words["slow"] == r.UNKNOWN_WORD
    assert r.regime_allows(["trend:bull"], words) and not r.regime_allows(["trend:bear"], words)
    # Expired ⇒ back to UNKNOWN.
    words, source = r.current_words(directory, NOW_MS + 901_000, DEAD_URL)
    assert source == "unknown"
    # The engine's page, when it answers, wins.
    text = (
        "engine_regime_fast_effective 1267189306916992\nengine_regime_slow_effective 4\n"
        "engine_regime_fast_measured 1\nengine_regime_bogus 7\n# TYPE x counter\n"
    )
    parsed = r.parse_metrics_words(text)
    assert parsed == {"fast_effective": 1267189306916992, "slow_effective": 4, "fast_measured": 1}
    assert r.engine_words(DEAD_URL) is None


def test_main_lanes_exit_codes(tmp_path, monkeypatch, capsys):
    art = _artifact(tmp_path)
    db = _db(tmp_path)
    directory = tmp_path / "regime"
    monkeypatch.setenv(claude_worker.regime.METRICS_URL_ENV, DEAD_URL)
    common = ["--regime", str(art), "--db", str(db), "--dir", str(directory), "--now-ms", str(NOW_MS)]
    assert claude_worker.regime.main(["report", *common]) == 0
    out = capsys.readouterr().out
    assert "[fast] measured: trend=bull" in out and "engine: unreachable" in out
    assert claude_worker.regime.main(["cycle", *common]) == 0
    assert claude_worker.regime.main(["history", *common]) == 0
    assert "fast=trend=bull" in capsys.readouterr().out
    assert claude_worker.regime.main(["refresh-params", "--dry-run", *common]) == 0
    assert "(dry-run)" in capsys.readouterr().out
    assert claude_worker.regime.main(["declare", "--fast", "trend:bull", "--slow", "measured", "--ttl", "60", "--no-send", *common]) == 0
    out = capsys.readouterr().out
    assert "fast=trend=bull" in out and "slow=trend=bull shape=trend" in out and "no frame sent" in out
    entries = claude_worker.regime.load_declared(directory)
    assert entries["fast"]["ttl_s"] == 60 and entries["slow"]["dims"]["trend"] == "bull"
    assert "source" not in entries["slow"]["dims"], "measured declares WITHOUT the SOURCE byte"
    assert claude_worker.regime.main(["declare", "--ttl", "60", "--no-send", *common]) == 2
    assert claude_worker.regime.main(["declare", "--fast", "trend:sideways", "--no-send", *common]) == 2
    assert claude_worker.regime.main(["report", "--regime", str(tmp_path / "nope.toml"), "--db", str(db), "--dir", str(directory)]) == 2
    assert claude_worker.regime.main(["cycle", "--regime", str(art), "--db", str(tmp_path / "nope.db"), "--dir", str(directory)]) == 2
    # The report shows the persisted declaration.
    assert claude_worker.regime.main(["report", *common]) == 0
    assert "declared: trend=bull" in capsys.readouterr().out


def test_lane_gate_any_label_is_open_without_any_source_and_labels_judge_words(tmp_path, monkeypatch):
    r = claude_worker.regime
    monkeypatch.setenv(r.METRICS_URL_ENV, DEAD_URL)
    assert r.lane_gate((), NOW_MS) == (True, "regime: any")
    bull = claude_worker.frames.regime_word(trend="bull", source="measured")
    ok, tell = r.lane_gate(("trend:bull",), NOW_MS, {"fast": bull, "slow": bull})
    assert ok and tell.startswith("regime: open label=['trend:bull'] (given)")
    # No words given: the current_words chain — unreachable engine, no
    # declaration ⇒ UNKNOWN fails a constrained profile closed.
    ok, tell = r.lane_gate(("trend:bull",), NOW_MS, None, tmp_path / "regime")
    assert not ok and "(unknown)" in tell and "ENTRIES BLOCKED" in tell
    # A fresh declaration in that directory opens it.
    r.persist_declared(tmp_path / "regime", {"fast": bull}, NOW_MS, 900, "operator")
    ok, tell = r.lane_gate(("trend:bull",), NOW_MS + 1000, None, tmp_path / "regime")
    assert ok and "(declared)" in tell


def test_verdict_parse_words_and_the_serve_step(tmp_path, monkeypatch):
    r = claude_worker.regime
    monkeypatch.setenv(r.METRICS_URL_ENV, DEAD_URL)
    art = _artifact(tmp_path)
    db = _db(tmp_path)
    directory = tmp_path / "regime"
    m = r.measure(art, db, NOW_MS)
    measured = r.measured_declaration_words(m)
    assert set(measured) == {"fast", "slow"}
    assert all((w >> (8 * r.DIM_SOURCE)) & 0xFF == 0 for w in measured.values()), "SOURCE byte stripped"
    assert r.parse_verdict({"fast": "measured"}) == {"fast": "measured"}
    assert r.parse_verdict({"slow": "trend:neutral, vol:unknown"}) == {"slow": "trend:neutral, vol:unknown"}
    for bad in ({}, {"mid": "measured"}, {"fast": 1}, {"fast": "trend:up"}, {"fast": "trend:bull|bear"}):
        with pytest.raises(ValueError):
            r.parse_verdict(bad)
    words = r.verdict_words({"fast": "measured", "slow": "trend:neutral"}, measured)
    assert words["fast"] == measured["fast"]
    assert claude_worker.frames.regime_word_dims(words["slow"]) == {"trend": "neutral"}
    # The serve step: measure + history + auto-confirm both profiles
    # (persist only when the client is None), source serve-measured.
    out = r.serve_regime_step(art, db, directory, NOW_MS, None, 600)
    assert out.measurement is not None and out.skipped == "" and out.seqs == []
    assert set(out.declared) == {"fast", "slow"}
    assert "[fast] measured: trend=bull" in out.digest and "declared:" in out.digest
    assert len(r.history_tail(directory, NOW_MS)) == 1
    entries = r.load_declared(directory)
    assert {e["source"] for e in entries.values()} == {r.SOURCE_SERVE}
    # Serve's OWN earlier confirm is overwritten next time (frames via the
    # recording client); a fresher OPERATOR ruling on fast is respected.
    client = _RecordingClient()
    out = r.serve_regime_step(art, db, directory, NOW_MS + 60_000, client, 600)
    assert set(out.declared) == {"fast", "slow"} and out.seqs == [1, 2]
    assert client.sent[0]["ttl_ns"] == 600 * S and client.sent[0]["qty"] == m.evaluator.measured[0]
    r.persist_declared(directory, {"fast": r.declaration_word({"trend": "bear"})}, NOW_MS + 120_000, 900, r.SOURCE_OPERATOR)
    client = _RecordingClient()
    out = r.serve_regime_step(art, db, directory, NOW_MS + 180_000, client, 600)
    assert set(out.declared) == {"slow"} and [s["param_id"] for s in client.sent] == [1]
    assert r.load_declared(directory)["fast"]["source"] == r.SOURCE_OPERATOR
    # Once the operator's ruling expires, fast is auto-confirmed again.
    out = r.serve_regime_step(art, db, directory, NOW_MS + 1_100_000, _RecordingClient(), 600)
    assert set(out.declared) == {"fast", "slow"}
    # Absent inputs / a transport failure never raise.
    out = r.serve_regime_step(tmp_path / "nope.toml", db, directory, NOW_MS, None, 600)
    assert out.measurement is None and out.skipped.startswith("no regime artifact") and out.digest == r.REGIME_UNMEASURED_TEXT

    class _Refusing:
        def send_cmd(self, **kwargs):
            raise claude_worker.uds.UdsError("refused")

    out = r.serve_regime_step(art, db, directory, NOW_MS + 2_000_000, _Refusing(), 600)
    assert out.measurement is not None and out.skipped.startswith("transport: refused") and out.seqs == []
    assert r.load_declared(directory)["fast"]["source"] == r.SOURCE_SERVE, "persisted before the send"
