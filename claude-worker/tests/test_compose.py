# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG4 composer (docs/regime-and-dashboard-plan.md §5.3): the
neighbourhood, the rule-8 identity/region mirror, the cap mirror,
selection order + admission, idempotent emission, the pooled gate
(evidence rows, the frozen pooled report, the on/off delta, LOWO, the
2 h wall budget — FAIL, never wait), promotion (freeze pin, hash-change
only, the frozen stage/commit pair on the fake UDS server), the window
pool's count-only pruning, and the lane through ``main``.

No live engine, no live harness: the harness is the ``run_fn`` seam.
Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib
import time

import pytest

import claude_worker.backtest
import claude_worker.compose
import claude_worker.config
import claude_worker.frames
import claude_worker.library
import claude_worker.regime
import claude_worker.state
import claude_worker.window_root
import tests.conftest
import tests.craft
import tests.test_library

S = 10**9
EPOCH = 1_788_000_000_000_000_000


def _row(name: str, **extra: object) -> dict[str, object]:
    return tests.test_library._row(name, **extra)  # the shared row builder


def _words(**dims: str) -> dict[str, int]:
    fast = claude_worker.frames.regime_word(source="measured", **dims)
    return {"fast": fast, "slow": fast}


# ---- neighbourhood ----


def test_neighbourhood_is_hamming_one_in_dimension_space() -> None:
    words = _words(trend="neutral", shape="mixed", vol="low", fund="pos", level="normal", stretch="neutral")
    nb = claude_worker.compose.neighbourhood(words)
    # the word + per profile the sum of (values - 1) over the six market dims = 2 x (2+2+2+1+2+2)
    assert len(nb) == 1 + 2 * 11
    assert nb[0] == words
    fast_dims = [claude_worker.frames.regime_word_dims(w["fast"]) for w in nb[1:12]]
    assert all(d["source"] == "measured" for d in fast_dims), "the SOURCE byte is kept"
    assert {d["trend"] for d in fast_dims} == {"bear", "bull", "neutral"}
    # an unknown dimension neighbours every known value of it
    unk = {"fast": claude_worker.regime.UNKNOWN_WORD, "slow": claude_worker.regime.UNKNOWN_WORD}
    assert len(claude_worker.compose.neighbourhood(unk)) == 1 + 2 * (3 + 3 + 3 + 2 + 3 + 3)


# ---- rule 8 / caps mirrors ----


def test_rule8_conflicts_only_on_one_identity_with_intersecting_regions() -> None:
    any_row = _row("a")
    low = _row("b", regimes=["vol:low"])
    not_low = _row("c", regimes=["vol:!low"])
    other = _row("d", enter=4.0, regimes=["vol:low"])
    assert claude_worker.compose.rows_conflict(any_row, low), "ANY intersects everything"
    assert not claude_worker.compose.rows_conflict(low, not_low), "disjoint variants of one signal are legal"
    assert not claude_worker.compose.rows_conflict(low, other), "a different enter is a different rule"
    assert claude_worker.compose.rows_conflict(low, _row("e", horizon_ms=1000, max_risk_usd=10.0, regimes=["vol:low|high"]))
    rel_a = _row("f", regimes=["vol:low"], rel="leading")
    rel_b = _row("g", regimes=["vol:low"], rel="lagging|inline")
    assert not claude_worker.compose.rows_conflict(rel_a, rel_b), "disjoint REL nibbles"
    assert claude_worker.compose.rows_conflict(rel_a, _row("h", regimes=["vol:low"], rel="slow:lagging"))
    assert claude_worker.compose.rows_conflict(low, _row("i", regimes=["slow:trend:bull"])), "fast vol:low vs slow-only: both profiles intersect"


def test_caps_mirror_counts_both_legs_of_position_rows() -> None:
    rows = [_row("a", max_risk_usd=9000.0), _row("b", max_risk_usd=9000.0, enter=5.0)]
    u = claude_worker.compose.cap_usage(rows)
    assert u.table_usd == 36000.0 and u.max_symbol_usd == 18000.0 and u.max_leg_usd == 9000.0 and u.rows == 2
    assert claude_worker.compose.caps_ok(rows) is None
    assert "symbol" in (claude_worker.compose.caps_ok([*rows, _row("c", max_risk_usd=3000.0, enter=6.0)]) or "")
    assert "leg" in (claude_worker.compose.caps_ok([_row("z", max_risk_usd=10001.0)]) or "")
    refire = [_row("r", max_risk_usd=10000.0)]
    refire[0].pop("exit")
    assert claude_worker.compose.cap_usage(refire).table_usd == 10000.0, "a stateless row charges one leg"
    many = [_row(f"n{i}", max_risk_usd=1.0, enter=float(i + 1)) for i in range(257)]
    assert "rows" in (claude_worker.compose.caps_ok(many) or "")


# ---- selection / admission / emission ----


def _library(tmp_path: pathlib.Path) -> tuple[claude_worker.state.State, pathlib.Path]:
    state = claude_worker.state.State(tmp_path / "state.db")
    lib = tmp_path / "library"
    # A: ANY, validated (a pre-RG8 row — the state-level setter bypasses the
    # library's RG8 refusal, exactly like a legacy state.db), evidence +5
    a, _ = claude_worker.library.add_member(state, lib, [_row("xv-any")], "xv-any")
    state.library_set_status(a.member_id, "validated")
    for k in range(4):
        state.evidence_upsert(a.member_id, f"run-{k}", "/w", n_ticks=1, n_fills=1, net_usd_0=2.0, net_usd_tier=1.25,
                              max_dd_usd=0.1, regime_word_mode="", judged=True, detail_version=4)
    # B: vol variants, validated, evidence +10
    b, _ = claude_worker.library.add_member(
        state, lib, [_row("xv-vlow", regimes=["vol:low"]), _row("xv-vnot", regimes=["vol:!low"])], "xv-vol", status="validated"
    )
    for k in range(2):  # window ids = the first two pool windows of `_pool` (evidence is cached by id)
        state.evidence_upsert(b.member_id, f"run-{EPOCH + k * 7200 * S}", "/w", n_ticks=1, n_fills=1, net_usd_0=6.0,
                              net_usd_tier=5.0, max_dd_usd=0.1, regime_word_mode="", judged=True, detail_version=4)
    # C: candidate on a different signal (enter 4), labelled for high vol only
    claude_worker.library.add_member(state, lib, [_row("xv-hi", enter=4.0, regimes=["vol:high"])], "xv-hi")
    # D: validated, trend-labelled on another signal (enter 5) — fits bull/bear only
    claude_worker.library.add_member(
        state, lib, [_row("mom-bull", enter=5.0, regimes=["trend:bull"]), _row("mom-bear", enter=5.0, regimes=["trend:bear"])],
        "mom", status="validated",
    )
    # E: retired ANY on yet another signal
    e, _ = claude_worker.library.add_member(state, lib, [_row("old", enter=6.0)], "old")
    state.library_set_status(e.member_id, "retired")
    return state, lib


def test_compose_selects_by_fit_orders_by_evidence_and_dedups_rule8(tmp_path: pathlib.Path) -> None:
    state, lib = _library(tmp_path)
    try:
        words = _words(trend="neutral", vol="low")
        comp = claude_worker.compose.compose(state, lib, words, "test")
        names = [s.member.name for s in comp.members]
        # B (evidence +10) first; D fits the neighbourhood (trend one step away) but conflicts with nothing;
        # A is ANY ⇒ RG8 excludes it outright (its rows would also lose to B on rule 8).
        assert names == ["xv-vol", "mom"]
        fits = {s.member.name: s.fit for s in comp.members}
        assert fits == {"xv-vol": "word", "mom": "neighbour"}
        skipped = dict(comp.skipped)
        assert "RG8" in skipped["xv-any"] and "candidate" in skipped["xv-hi"] and "old" not in skipped
        # --include-any admits it into the selection, where rule 8 stops it.
        with_any = claude_worker.compose.compose(state, lib, words, "test", include_any=True)
        assert "rule 8" in dict(with_any.skipped)["xv-any"]
        assert [s.member.name for s in with_any.members] == ["xv-vol", "mom"]
        assert [r["name"] for r in comp.rows] == ["xv-vlow", "xv-vnot", "mom-bull", "mom-bear"]
        # idempotent: same inputs ⇒ same bytes ⇒ same hash; the artifact hashes to it
        again = claude_worker.compose.compose(state, lib, words, "test")
        assert again.full_hash == comp.full_hash and again.data == comp.data
        assert hashlib.sha256(comp.data).hexdigest() == comp.full_hash and comp.hash128 == comp.full_hash[:32]
        path = claude_worker.compose.write_composition(tmp_path / "comps", comp)
        assert path.name == f"{comp.hash128}.json" and path.read_bytes() == comp.data
        # candidates opt in; a candidate whose label fits the neighbourhood (vol high is one step from low) comes in
        with_c = claude_worker.compose.compose(state, lib, words, "test", include_candidates=True)
        assert "xv-hi" in [s.member.name for s in with_c.members]
        # a single fitting member: the table hash IS the member id
        only_words = _words(trend="bull", vol="high")
        single = claude_worker.compose.compose(state, lib, only_words, "test")
        assert [s.member.name for s in single.members] == ["xv-vol", "mom"]
        # evidence fit: C has no evidence ⇒ not selected by --fit-from-evidence under a far word
        far = _words(trend="neutral", vol="normal", shape="chop", fund="neg", level="low", stretch="ext_up")
        comp_far = claude_worker.compose.compose(state, lib, far, "test", fit_from_evidence=True, include_candidates=True)
        assert "xv-hi" in [s.member.name for s in comp_far.members], "vol:high is one step from normal"
        # nothing fits ⇒ ComposeError
        empty_state = claude_worker.state.State(tmp_path / "empty.db")
        try:
            with pytest.raises(claude_worker.compose.ComposeError):
                claude_worker.compose.compose(empty_state, tmp_path / "nolib", words, "test")
        finally:
            empty_state.close()
        thesis = claude_worker.compose.thesis_of(comp)
        assert thesis.startswith("composed (test) fast=[") and "xv-vol(" in thesis and ",word)" in thesis
    finally:
        state.close()


# ---- gate ----


class _Harness:
    """A steerable fake harness: nets by replay-root kind, detail sidecars
    for evidence runs, every argv recorded."""

    def __init__(self) -> None:
        self.seen: list[list[str]] = []
        self.pooled_net = 12.0
        self.off_net = 4.0
        self.lowo_net: dict[str, float] = {}
        self.evidence_net = 1.5

    def __call__(self, argv: list[str]) -> str:
        self.seen.append(list(argv))
        ruleset = pathlib.Path(argv[argv.index("--ruleset") + 1])
        digest = hashlib.sha256(ruleset.read_bytes()).hexdigest()
        replay = pathlib.Path(argv[argv.index("--replay-dir") + 1])
        if "--emit-detail" in argv:
            pathlib.Path(argv[argv.index("--emit-detail") + 1]).write_text(json.dumps(tests.test_library._fake_detail()))
            net = self.evidence_net
        elif "--regime" in argv and argv[argv.index("--regime") + 1] == "off":
            net = self.off_net
        elif replay.name.startswith("lowo-"):
            net = self.lowo_net.get(replay.name, 3.0)
        else:
            net = self.pooled_net
        return json.dumps({
            "schema_version": 1, "ruleset_hash": digest, "split": argv[argv.index("--split") + 1],
            "oos": {"net_pnl_usd": net, "trades": 80, "trading_days": 2, "max_drawdown_usd": 9.0, "legs": 80, "round_trips": 14},
            "bounds": {"max_order_notional_usd": 3000.0, "max_symbol_notional_usd": 15000.0, "max_total_notional_usd": 21000.0},
            "position_rows": 2,
        })


def _pool(tmp_path: pathlib.Path, n: int) -> tuple[pathlib.Path, list[pathlib.Path]]:
    pool = tmp_path / "windows"
    windows = []
    for k in range(n):
        w = pool / f"run-{EPOCH + k * 7200 * S}"
        w.mkdir(parents=True)
        windows.append(w)
    return pool, windows


def test_gate_runs_evidence_pooled_off_and_lowo_and_binds_the_report(tmp_path: pathlib.Path) -> None:
    state, lib = _library(tmp_path)
    try:
        comp = claude_worker.compose.compose(state, lib, _words(trend="neutral", vol="low"), "test")
        path = claude_worker.compose.write_composition(tmp_path / "comps", comp)
        pool, windows = _pool(tmp_path, 4)
        harness = _Harness()
        lines: list[str] = []
        verdict = claude_worker.compose.gate(
            state, comp, path, pool, windows, tmp_path / "work", fees=["--fee-bps", "okx:8:10"],
            run_fn=harness, report=lines.append, ts=7,
        )
        assert verdict.passed, verdict.reasons
        # evidence: B had run-0/run-1 already (2 of 4 windows), D none ⇒ 2 + 4 runs
        assert verdict.evidence_runs == 6
        assert len(state.evidence_for(comp.members[1].member.member_id)) == 4
        # the pooled run is the FROZEN argv (no extras) and wrote the binding report beside the artifact
        pooled = [a for a in harness.seen if a[-2:] == ["--split", "70/30"] and "--regime" not in a and "--emit-detail" not in a and not pathlib.Path(a[a.index("--replay-dir") + 1]).name.startswith("lowo-")]
        assert len(pooled) == 1 and pooled[0][3] == str(path) and pooled[0][5] == str(pool)
        assert verdict.report_path == claude_worker.backtest.report_path_for(path)
        claude_worker.backtest.check_stage_binding(path, verdict.report_path)  # type: ignore[arg-type]
        assert verdict.pooled_net == 12.0 and verdict.off_net == 4.0
        assert [w for w, _n in verdict.lowo] == [w.name for w in windows] and all(n == 3.0 for _w, n in verdict.lowo)
        # LOWO roots are symlink roots of the other windows
        lowo_roots = sorted((tmp_path / "work").glob("lowo-*"))
        assert len(lowo_roots) == 4 and all(len(list(r.iterdir())) == 3 for r in lowo_roots)
        assert all(p.is_symlink() for r in lowo_roots for p in r.iterdir())
        assert any("pooled:" in ln and "PASS" in ln for ln in lines) and any("regime off:" in ln for ln in lines)
        assert verdict.as_dict()["passed"] is True and len(verdict.as_dict()["lowo"]) == 4
    finally:
        state.close()


def test_gate_fails_on_off_delta_lowo_floor_and_budget(tmp_path: pathlib.Path) -> None:
    state, lib = _library(tmp_path)
    try:
        comp = claude_worker.compose.compose(state, lib, _words(trend="neutral", vol="low"), "test")
        path = claude_worker.compose.write_composition(tmp_path / "comps", comp)
        pool, windows = _pool(tmp_path, 4)
        # on/off: the label must not be worse than its absence
        harness = _Harness()
        harness.off_net = 13.0
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None)
        assert not v.passed and any("on/off delta negative" in r for r in v.reasons)
        # lowo: one held-out root flips the sign
        harness = _Harness()
        harness.lowo_net["lowo-2"] = -0.5
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None)
        assert not v.passed and any("lowo" in r and windows[2].name in r for r in v.reasons)
        # --no-lowo skips those runs
        harness = _Harness()
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None, lowo=False)
        assert v.passed and v.lowo == [] and not any(pathlib.Path(a[a.index("--replay-dir") + 1]).name.startswith("lowo-") for a in harness.seen)
        # pooled gate fail
        harness = _Harness()
        harness.pooled_net = -1.0
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None)
        assert not v.passed and any("pooled gates: pnl_positive" in r for r in v.reasons)
        # below the window floor: no harness run at all
        harness = _Harness()
        v = claude_worker.compose.gate(state, comp, path, pool, windows[:3], tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None)
        assert not v.passed and "< 4" in v.reasons[0] and harness.seen == []
        # the 2 h wall budget: a clock that jumps past it after the first run ⇒ FAIL, no further runs
        ticks = iter([0.0, 0.0, 10.0, 8000.0, 8000.0, 8000.0, 8000.0])
        budget = claude_worker.compose.WallBudget(7200.0, clock=lambda: next(ticks))
        harness = _Harness()
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=harness, report=lambda _s: None, budget=budget)
        assert not v.passed and any("wall budget" in r for r in v.reasons) and len(harness.seen) <= 2
        later = iter([0.0, 5.0])
        with pytest.raises(claude_worker.compose.BudgetExceeded):
            claude_worker.compose.WallBudget(1.0, clock=lambda: next(later)).check("x")
        # a harness error is a verdict, not a crash
        v = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=lambda _a: "garbage", report=lambda _s: None)
        assert not v.passed and any("harness:" in r for r in v.reasons)
    finally:
        state.close()


# ---- promote ----


def _cfg(tmp_path: pathlib.Path, sock: pathlib.Path) -> claude_worker.config.BaseConfig:
    (tmp_path / "logs").mkdir(exist_ok=True)
    return claude_worker.config.load_base_from_env({
        "AI_INGRESS_SOCK": str(sock),
        "AI_INGRESS_HMAC_KEY": tests.conftest.TEST_KEY.hex(),
        "AI_RULESET_DIR": str(tmp_path / "rulesets"),
        "CLAUDE_WORKER_REPLAY_DIR": str(tmp_path / "logs"),
        "CLAUDE_WORKER_DB": str(tmp_path / "state.db"),
        "CLAUDE_WORKER_FEATURES_DIR": str(tmp_path / "features"),
        "CLAUDE_WORKER_MARKET_MAP": str(tmp_path / "market-map.json"),
        "RSS_FEEDS": "",
    })


def test_promote_refuses_freeze_and_same_hash_then_stages_and_commits(
    tmp_path: pathlib.Path, fake_uds: tests.conftest.FakeUdsServer
) -> None:
    state, lib = _library(tmp_path)
    try:
        comp = claude_worker.compose.compose(state, lib, _words(trend="neutral", vol="low"), "test")
        path = claude_worker.compose.write_composition(tmp_path / "comps", comp)
        pool, windows = _pool(tmp_path, 4)
        verdict = claude_worker.compose.gate(state, comp, path, pool, windows, tmp_path / "work", fees=[], run_fn=_Harness(), report=lambda _s: None)
        assert verdict.passed and verdict.report_path is not None
        state.composition_insert(comp.full_hash, comp.hash128, [], {}, str(path), None)
        cfg = _cfg(tmp_path, fake_uds.sock_path)
        freeze = tmp_path / "comps" / "FREEZE"
        freeze.write_text("frozen\n")
        r = claude_worker.compose.promote(state, cfg, comp, path, verdict.report_path, freeze_file=freeze, metrics_url="http://127.0.0.1:1/metrics")
        assert not r.done and "FREEZE" in r.tell and fake_uds.frames == []
        freeze.unlink()
        # the active table is this very hash ⇒ no-op
        state.stage_ruleset(comp.full_hash, str(path), str(verdict.report_path), "session", ts=100)
        state.mark_ruleset_committed(comp.full_hash, ts=101)
        r = claude_worker.compose.promote(state, cfg, comp, path, verdict.report_path, freeze_file=freeze, metrics_url="http://127.0.0.1:1/metrics")
        assert not r.done and "already active" in r.tell and fake_uds.frames == []
        # the SAME rows live under a hand-written (non-canonical) artifact ⇒ no flip either
        hand = tmp_path / "rulesets" / "hand.json"
        hand.parent.mkdir(exist_ok=True)
        hand.write_text(json.dumps({"rows": [dict(reversed(list(r.items()))) for r in comp.rows]}, indent=1))
        hand_hash = hashlib.sha256(hand.read_bytes()).hexdigest()
        assert hand_hash != comp.full_hash
        state.stage_ruleset(hand_hash, str(hand), "/h.report.json", "session", ts=150)
        state.mark_ruleset_committed(hand_hash, ts=151)
        r = claude_worker.compose.promote(state, cfg, comp, path, verdict.report_path, freeze_file=freeze, metrics_url="http://127.0.0.1:1/metrics")
        assert not r.done and "these very rows" in r.tell and fake_uds.frames == []
        # a different, later-committed hash is active ⇒ install + stage + commit through the frozen pair
        state.stage_ruleset("b" * 64, "/b.json", "/b.report.json", "session", ts=200)
        state.mark_ruleset_committed("b" * 64, ts=201)
        assert claude_worker.library.active_hash(state) == "b" * 64
        r = claude_worker.compose.promote(
            state, cfg, comp, path, verdict.report_path, freeze_file=freeze,
            metrics_url="http://127.0.0.1:1/metrics", report=lambda _s: None, wait_s=0.0,
        )
        assert r.done and "unreachable" in r.tell and r.staged_seq is not None and r.committed_seq is not None
        installed = tmp_path / "rulesets" / f"{comp.hash128}.json"
        assert installed.read_bytes() == comp.data and installed.with_suffix(".report.json").is_file()
        deadline = 50
        while len(fake_uds.frames) < 3 and deadline:  # heartbeat + stage + commit
            deadline -= 1
            time.sleep(0.02)
        kinds = [fake_uds.cmd_field(i, "kind") for i in range(len(fake_uds.frames))]
        assert kinds == [claude_worker.frames.KIND_HEARTBEAT, claude_worker.frames.KIND_RULESET_STAGE, claude_worker.frames.KIND_RULESET_COMMIT]
        assert fake_uds.errors == []
        row = state.ruleset_row(comp.full_hash)
        assert row is not None and row[3] is True and row[5] is not None and row[6] is not None
        assert state.ruleset_attribution(comp.full_hash)[1].startswith("composed (test)")  # type: ignore[index]
        c = state.composition_row(comp.full_hash)
        assert c is not None and c.staged_ts is not None and c.committed_ts is not None
        assert claude_worker.library.active_hash(state) == comp.full_hash
    finally:
        state.close()


# ---- the window pool ----


def _run(logs: pathlib.Path, epoch: int, seconds: int, version: int = 3) -> pathlib.Path:
    ts = list(range(1_000, seconds * S, 50 * S))
    return tests.craft.write_run(logs, epoch, ts, version=version)


def test_pool_ensure_keeps_the_newest_k_by_count_only(tmp_path: pathlib.Path) -> None:
    logs = tmp_path / "logs"
    pool = tmp_path / "windows"
    window_s = 600.0
    _run(logs, EPOCH, 1500)                      # 2 complete 600 s windows + a partial tail
    _run(logs, EPOCH + 5000 * S, 1300)           # 2 complete windows
    _run(logs, EPOCH + 9000 * S, 1300, version=2)  # stale-blind: refused
    lines: list[str] = []
    got = claude_worker.window_root.pool_ensure(logs, pool, 3, None, report=lines.append, window_s=window_s)
    assert [p.name for p in got] == [f"run-{EPOCH + 600 * S}", f"run-{EPOCH + 5000 * S}", f"run-{EPOCH + 5600 * S}"]
    assert all((p / "pm-ticks.pmlr").is_file() for p in got)
    assert sum(1 for ln in lines if ln.startswith("window-pool: cut")) == 3
    # a newer run arrives: the oldest cut is pruned, the count stays 3, existing cuts are reused
    _run(logs, EPOCH + 20000 * S, 700)
    lines.clear()
    got2 = claude_worker.window_root.pool_ensure(logs, pool, 3, None, report=lines.append, window_s=window_s)
    assert [p.name for p in got2] == [f"run-{EPOCH + 5000 * S}", f"run-{EPOCH + 5600 * S}", f"run-{EPOCH + 20000 * S}"]
    assert sum(1 for ln in lines if ln.startswith("window-pool: cut")) == 1
    assert any("pruned" in ln for ln in lines) and not (pool / f"run-{EPOCH + 600 * S}").exists()
    assert claude_worker.window_root.pool_windows(pool) == got2
    with pytest.raises(claude_worker.window_root.WindowError):
        claude_worker.window_root.pool_ensure(logs, pool, 0, None)
    root = claude_worker.window_root.symlink_root(tmp_path / "root", got2[1:])
    assert sorted(p.name for p in root.iterdir()) == [got2[1].name, got2[2].name]
    assert claude_worker.window_root.run_pmlr_version(logs / f"run-{EPOCH + 9000 * S}") == 2
    assert claude_worker.window_root.complete_windows(logs / f"run-{EPOCH}", window_s) == [(0.0, 600.0), (600.0, 1200.0)]


def test_pool_ensure_back_fills_the_funding_seed_on_reused_cuts(tmp_path: pathlib.Path) -> None:
    """A cut that predates the harness funding seed gains its
    ``funding-seed.tsv`` on the next ``pool_ensure`` with a ``candles.db``
    — no re-cut; a cut that has one is left alone."""
    import claude_worker.candles
    import claude_worker.seeds

    logs = tmp_path / "logs"
    pool = tmp_path / "windows"
    window_s = 600.0
    run = _run(logs, EPOCH, 1300)
    (run / "instrument-manifest.tsv").write_text("16777728\tbinance-usdm:btcusdt\n", encoding="utf-8")
    got = claude_worker.window_root.pool_ensure(logs, pool, 2, None, window_s=window_s)
    assert len(got) == 2 and not any((p / claude_worker.seeds.FUNDING_SEED_FILE).exists() for p in got)
    db = tmp_path / "candles.db"
    conn = claude_worker.candles.open_db(db)
    conn.execute(
        "CREATE TABLE funding (venue INTEGER NOT NULL, descriptor TEXT NOT NULL,"
        " ts_ms INTEGER NOT NULL, rate REAL NOT NULL, fetched_ts INTEGER NOT NULL,"
        " PRIMARY KEY (venue, descriptor, ts_ms)) WITHOUT ROWID"
    )
    epoch_ms = EPOCH // 1_000_000
    conn.execute("INSERT INTO funding VALUES (?,?,?,?,0)", (1, "binance-usdm:btcusdt", epoch_ms - 3_600_000, 0.0001))
    conn.commit()
    conn.close()
    (got[1] / claude_worker.seeds.FUNDING_SEED_FILE).write_text("# kept\n", encoding="utf-8")
    lines: list[str] = []
    got2 = claude_worker.window_root.pool_ensure(logs, pool, 2, (tmp_path / "nope.toml", db), report=lines.append, window_s=window_s)
    assert got2 == got
    body = [l for l in (got[0] / claude_worker.seeds.FUNDING_SEED_FILE).read_text("utf-8").splitlines() if not l.startswith("#")]
    assert body == [f"binance-usdm:btcusdt\t{epoch_ms - 3_600_000}\t100000"]
    assert (got[1] / claude_worker.seeds.FUNDING_SEED_FILE).read_text("utf-8") == "# kept\n"
    assert sum(1 for ln in lines if "back-filled 1 prints for 1 descriptors" in ln) == 1
    assert not any(ln.startswith("window-pool: cut") for ln in lines)


# ---- main ----


def test_compose_main_dry_run_freeze_and_gate_exit_codes(
    tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str], monkeypatch: pytest.MonkeyPatch
) -> None:
    state, lib = _library(tmp_path)
    state.close()
    common = ["--db", str(tmp_path / "state.db"), "--library-dir", str(lib), "--regime-dir", str(tmp_path / "regime"),
              "--regime-toml", str(tmp_path / "absent.toml"), "--candles-db", str(tmp_path / "absent.db")]
    assert claude_worker.compose.main([*common, "--regime", "trend:neutral,vol:low", "--dry-run"]) == 0
    out = capsys.readouterr().out
    assert "compose: words (query)" in out and "+ xv-vol" in out and "RG8" in out and "artifact" in out
    st = claude_worker.state.State(tmp_path / "state.db")
    try:
        comps = st.compositions()
        assert len(comps) == 1 and comps[0].gate is None and comps[0].words["fast"] == "0001808080018002"
    finally:
        st.close()
    assert claude_worker.compose.main([*common, "--regime", "trend:neutral,vol:low", "--dry-run", "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    assert payload["members"][0]["name"] == "xv-vol" and payload["words_source"] == "query"
    # freeze / unfreeze
    assert claude_worker.compose.main([*common, "--freeze"]) == 0
    assert (tmp_path / "compositions" / "FREEZE").is_file()
    assert claude_worker.compose.main([*common, "--unfreeze"]) == 0
    assert not (tmp_path / "compositions" / "FREEZE").exists()
    assert claude_worker.compose.main([*common, "--budget-s", "9000", "--dry-run"]) == 2
    # the gate through main with the fake harness on an existing pool (no refresh): PASS then FAIL exit 3
    pool, _windows = _pool(tmp_path, 4)
    harness = _Harness()
    monkeypatch.setattr(claude_worker.backtest, "default_run_fn", harness)
    args = [*common, "--regime", "trend:neutral,vol:low", "--pool", str(pool), "--no-refresh", "--fees", "none"]
    assert claude_worker.compose.main(args) == 0
    out = capsys.readouterr().out
    assert "gate PASS" in out
    harness.pooled_net = -2.0
    assert claude_worker.compose.main(args) == 3
    out = capsys.readouterr().out
    assert "gate FAIL" in out and "pooled gates" in out
    # words that fit nothing ⇒ exit 2 with the reasons
    assert claude_worker.compose.main([*common, "--regime", "trend:neutral,vol:low;trend:bull,shape:chop", "--dry-run"]) == 0
    capsys.readouterr()
    empty = ["--db", str(tmp_path / "e.db"), "--library-dir", str(tmp_path / "nolib"), "--regime-dir", str(tmp_path / "regime"),
             "--regime-toml", str(tmp_path / "absent.toml"), "--candles-db", str(tmp_path / "absent.db"), "--regime", "vol:low", "--dry-run"]
    assert claude_worker.compose.main(empty) == 2
    assert "no member fits" in capsys.readouterr().err
