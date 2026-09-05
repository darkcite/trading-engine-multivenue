# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG4 strategy library (docs/regime-and-dashboard-plan.md §5.2): the
additive state tables, member identity + labels, add / import-catalog
(every hash + thesis preserved, ONLY the active table validated),
the regime query, the evidence row from an additive-path harness run,
and the module lanes end to end through ``main``.

No live engine, no live harness: the harness is the ``run_fn`` seam.
Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib

import pytest

import claude_worker.backtest
import claude_worker.frames
import claude_worker.library
import claude_worker.regime
import claude_worker.state
import claude_worker.strategist

XV_ROW: dict[str, object] = {
    "horizon_ms": 60000,
    "max_risk_usd": 3000.0,
    "name": "xv-okx-bnspot",
    "family": "crypto",
    "instrument": "okx:BTC-USDT",
    "ref": "binance:btcusdt",
    "feature": "mid",
    "combine": "diff_bps",
    "enter": 3.0,
    "abs": True,
    "exit": 1.0,
}


def _row(name: str, **extra: object) -> dict[str, object]:
    row = dict(XV_ROW)
    row["name"] = name
    row.update(extra)
    return row


def _artifact(path: pathlib.Path, rows: list[dict[str, object]]) -> str:
    data = json.dumps({"rows": rows}).encode()
    path.write_bytes(data)
    return hashlib.sha256(data).hexdigest()


# ---- state tables ----


def test_state_library_tables_roundtrip(tmp_path: pathlib.Path) -> None:
    state = claude_worker.state.State(tmp_path / "state.db")
    try:
        assert state.library_insert("m1", "one", "vm-rows", "/p/m1.json", "candidate", [["vol:low"]], {"source": "t"}, ts=10)
        assert not state.library_insert("m1", "renamed", "vm-rows", "/p/x.json", "validated", [], {}, ts=11), "insert-or-ignore"
        row = state.library_member("m1")
        assert row is not None and row.name == "one" and row.status == "candidate" and row.labels == [["vol:low"]]
        state.library_set_status("m1", "validated", ts=12)
        state.library_set_labels("m1", [["trend:bull"], ["trend:bear"]], "hard", ts=13)
        state.library_set_thesis("m1", "why", ts=14)
        row = state.library_member("m1")
        assert row is not None
        assert (row.status, row.labels, row.regime_off, row.thesis, row.updated_ts) == (
            "validated", [["trend:bull"], ["trend:bear"]], "hard", "why", 14
        )
        with pytest.raises(claude_worker.state.StateError):
            state.library_set_status("m1", "bogus")
        with pytest.raises(claude_worker.state.StateError):
            state.library_set_status("nope", "retired")
        assert state.library_insert("m2", "two", "coded", "", "validated", [], {})
        assert [m.member_id for m in state.library_members()] == ["m1", "m2"]
        assert [m.member_id for m in state.library_members("candidate")] == []
        assert state.library_find("two") is not None and state.library_find("two").member_id == "m2"  # type: ignore[union-attr]
        assert state.library_find("zz") is None
        # evidence: upsert replaces
        state.evidence_upsert("m1", "run-1", "/w/run-1", n_ticks=10, n_fills=2, net_usd_0=1.0, net_usd_tier=-0.5,
                              max_dd_usd=0.2, regime_word_mode="0001", judged=True, detail_version=4, ts=1)
        state.evidence_upsert("m1", "run-1", "/w/run-1", n_ticks=11, n_fills=3, net_usd_0=2.0, net_usd_tier=0.5,
                              max_dd_usd=0.3, regime_word_mode="0002", judged=False, detail_version=4, ts=2)
        rows = state.evidence_for("m1")
        assert len(rows) == 1 and rows[0].n_fills == 3 and rows[0].judged is False and rows[0].net_usd_tier == 0.5
        # compositions
        state.composition_insert("h" * 64, "h" * 32, ["m1", "m2"], {"fast": "00", "slow": "01"}, "/c.json", None, ts=5)
        state.composition_mark("h" * 64, "staged_ts", ts=6)
        state.composition_mark("h" * 64, "committed_ts", ts=7)
        state.composition_insert("h" * 64, "h" * 32, ["m1"], {"fast": "02", "slow": "03"}, "/c.json", {"passed": True}, ts=8)
        comp = state.composition_row("h" * 64)
        assert comp is not None
        assert (comp.member_ids, comp.words, comp.gate, comp.composed_ts, comp.staged_ts, comp.committed_ts) == (
            ["m1"], {"fast": "02", "slow": "03"}, {"passed": True}, 8, 6, 7
        )
        with pytest.raises(claude_worker.state.StateError):
            state.composition_mark("h" * 64, "path")
        assert [c.table_hash for c in state.compositions()] == ["h" * 64]
        # the registry reader
        state.stage_ruleset("a" * 64, "/a.json", "/a.report.json", "session", ts=1, thesis="A")
        state.mark_ruleset_committed("a" * 64, ts=2)
        reg = state.rulesets_all()
        assert len(reg) == 1 and reg[0].hash == "a" * 64 and reg[0].thesis == "A" and reg[0].committed_ts == 2
    finally:
        state.close()


# ---- identity + labels ----


def test_member_id_is_canonical_and_labels_are_derived() -> None:
    a = claude_worker.library.canonical_rows([_row("r", regimes=["vol:low"])])
    shuffled = dict(reversed(list(_row("r", regimes=["vol:low"]).items())))
    b = claude_worker.library.canonical_rows([shuffled])
    assert claude_worker.library.member_id_of(a) == claude_worker.library.member_id_of(b), "key order is not identity"
    assert claude_worker.library.labels_from_rows(a) == [["vol:low"]]
    two = claude_worker.library.canonical_rows([_row("a", regimes=["vol:low"]), _row("b", regimes=["vol:!low"], rel="leading")])
    assert claude_worker.library.labels_from_rows(two) == [["vol:low"], ["vol:!low", "rel:leading"]]
    mixed = claude_worker.library.canonical_rows([_row("a", regimes=["vol:low"]), _row("b")])
    assert claude_worker.library.labels_from_rows(mixed) == [], "an unlabelled row makes the member ANY"
    assert claude_worker.library.word_terms(["vol:low", "rel:leading", "slow:rel:*", "slow:trend:bull"]) == ["vol:low", "slow:trend:bull"]
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.canonical_rows([{"name": "x"}])
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.canonical_rows([])
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.validate_labels([["vol:purple"]])
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.validate_labels([[]])


def test_label_fits_is_exists_over_labels_and_fails_closed_on_unknown() -> None:
    low = {"fast": claude_worker.frames.regime_word(vol="low", source="measured"), "slow": claude_worker.regime.UNKNOWN_WORD}
    assert claude_worker.library.label_fits([], low)
    assert claude_worker.library.label_fits([["vol:low"]], low)
    assert not claude_worker.library.label_fits([["vol:high"]], low)
    assert claude_worker.library.label_fits([["vol:high"], ["vol:low"]], low), "∃ over labels"
    assert not claude_worker.library.label_fits([["slow:trend:bull"]], low), "a constrained UNKNOWN profile fails closed"
    assert claude_worker.library.label_fits([["rel:leading"]], low), "rel-only labels constrain nothing at word level"


# ---- add / files ----


def test_add_member_writes_file_and_index_and_detects_drift(tmp_path: pathlib.Path) -> None:
    state = claude_worker.state.State(tmp_path / "state.db")
    lib = tmp_path / "library"
    try:
        member, new = claude_worker.library.add_member(
            state, lib, [_row("a", regimes=["vol:low"]), _row("b", regimes=["vol:!low"])], "xv-vol",
            thesis="two variants", origin={"source": "test"}, ts=1,
        )
        assert new and member.labels == [["vol:low"], ["vol:!low"]] and member.status == "candidate"
        path = claude_worker.library.member_file(lib, member.member_id)
        assert path.is_file()
        again, new2 = claude_worker.library.add_member(state, lib, [_row("b", regimes=["vol:!low"]), _row("a", regimes=["vol:low"])], "other-name", ts=2)
        assert new2 and again.member_id != member.member_id, "row ORDER is part of the artifact bytes"
        same, new3 = claude_worker.library.add_member(state, lib, [_row("a", regimes=["vol:low"]), _row("b", regimes=["vol:!low"])], "renamed", ts=3)
        assert not new3 and same.member_id == member.member_id
        assert state.library_member(member.member_id).name == "xv-vol"  # type: ignore[union-attr]
        loaded = claude_worker.library.load_member(lib, state.library_member(member.member_id))  # type: ignore[arg-type]
        assert loaded.rows == member.rows and loaded.thesis == "two variants"
        # status/labels flow to the file
        claude_worker.library.set_status(state, lib, state.library_member(member.member_id), "validated")  # type: ignore[arg-type]
        claude_worker.library.set_labels(state, lib, state.library_member(member.member_id), [["trend:bull"]], "hard")  # type: ignore[arg-type]
        on_disk = json.loads(path.read_text())
        assert on_disk["status"] == "validated" and on_disk["labels"] == [["trend:bull"]] and on_disk["regime_off"] == "hard"
        # drift: rows edited on disk no longer hash to the id
        on_disk["rows"][0]["enter"] = 4.0
        path.write_text(json.dumps(on_disk))
        with pytest.raises(claude_worker.library.LibraryError):
            claude_worker.library.read_member(path)
        with pytest.raises(claude_worker.library.LibraryError):
            claude_worker.library.add_member(state, lib, [_row("z")], "bad", regime_off="sometimes")
    finally:
        state.close()


# ---- import-catalog ----


def test_import_catalog_preserves_every_hash_and_validates_only_the_active_table(tmp_path: pathlib.Path) -> None:
    state = claude_worker.state.State(tmp_path / "state.db")
    lib = tmp_path / "library"
    artifacts = tmp_path / "artifacts"
    artifacts.mkdir()
    candidates = tmp_path / "candidates"
    candidates.mkdir()
    try:
        # registry: an OLD committed table (file gone from its path, installed under artifacts),
        # the ACTIVE table (latest committed), a staged-only table, a row with no file anywhere.
        old_rows = [_row("old")]
        old_hash = _artifact(tmp_path / "old-src.json", old_rows)
        (artifacts / f"{old_hash[:32]}.json").write_bytes((tmp_path / "old-src.json").read_bytes())
        (tmp_path / "old-src.json").unlink()
        state.stage_ruleset(old_hash, str(tmp_path / "old-src.json"), "/r", "session", ts=1, thesis="old thesis")
        state.mark_ruleset_committed(old_hash, ts=2)
        active_rows = [_row("v-low", regimes=["vol:low"]), _row("v-not", regimes=["vol:!low"])]
        active_hash = _artifact(tmp_path / "active.json", active_rows)
        state.stage_ruleset(active_hash, str(tmp_path / "active.json"), "/r", "session", ts=3, thesis="active thesis")
        state.mark_ruleset_committed(active_hash, ts=4)
        staged_hash = _artifact(tmp_path / "staged.json", [_row("staged-only")])
        state.stage_ruleset(staged_hash, str(tmp_path / "staged.json"), "/r", "auto", ts=5, model="m", thesis="staged thesis")
        state.stage_ruleset("f" * 64, str(tmp_path / "gone.json"), "/r", "session", ts=6)
        # candidates dir: a fresh proposal, a report sidecar (ignored), a copy of the active table (dedup by hash)
        _artifact(candidates / "20260905-cand.json", [_row("cand")])
        (candidates / "20260905-cand.report.json").write_text("{}")
        (candidates / "dup.json").write_bytes((tmp_path / "active.json").read_bytes())
        (candidates / "junk.json").write_text("[1,2]")
        # coded member: icdp.toml + regime.toml labels
        icdp = tmp_path / "icdp.toml"
        icdp.write_text("[icdp]\nx = 1\n")
        regime_toml = tmp_path / "regime.toml"
        regime_toml.write_text('[labels.icdp]\noff = "soft"\nterm1 = ["fast:shape:trend"]\n')

        lines: list[str] = []
        stats = claude_worker.library.import_catalog(
            state, lib, artifacts, candidates, icdp_toml=icdp, regime_toml=regime_toml, report=lines.append, ts=10
        )
        assert stats.registry_rows == 4 and stats.candidates == 3 and stats.coded == 1
        assert stats.skipped_missing == 1 and stats.skipped_malformed == 1
        assert stats.inserted == 5, "old + active + staged + cand + icdp"
        members = {m.name: m for m in state.library_members()}
        assert members["old"].status == "candidate" and members["old"].thesis == "old thesis"
        assert members["old"].origin["source_hash"] == old_hash and members["old"].origin["author_mode"] == "session"
        assert members["v+1"].status == "validated" and members["v+1"].thesis == "active thesis"
        assert members["v+1"].labels == [["vol:low"], ["vol:!low"]]
        assert members["staged-only"].status == "candidate" and members["staged-only"].origin["model"] == "m"
        assert members["cand"].status == "candidate" and members["cand"].origin["source"] == "candidates"
        assert members["icdp"].kind == "coded" and members["icdp"].labels == [["fast:shape:trend"]] and members["icdp"].status == "validated"
        assert stats.validated == [members["v+1"].member_id]
        assert claude_worker.library.active_hash(state) == active_hash
        # RG8: an unlabelled member never becomes validated — label it first.
        with pytest.raises(claude_worker.library.LibraryError, match="RG8"):
            claude_worker.library.set_status(state, lib, members["old"], "validated")
        claude_worker.library.set_labels(state, lib, members["old"], [["vol:low"]], None)
        old_row = state.library_member(members["old"].member_id)
        assert old_row is not None
        # idempotent: a re-run inserts nothing and never downgrades
        claude_worker.library.set_status(state, lib, old_row, "validated")
        stats2 = claude_worker.library.import_catalog(
            state, lib, artifacts, candidates, icdp_toml=icdp, regime_toml=regime_toml, report=lines.append, ts=11
        )
        assert stats2.inserted == 0 and state.library_member(members["old"].member_id).status == "validated"  # type: ignore[union-attr]
        # RG8: an UNLABELLED active table imports as a candidate, with a tell.
        state3 = claude_worker.state.State(tmp_path / "state3.db")
        try:
            bare_rows = [_row("bare-a"), _row("bare-b")]
            bare_hash = _artifact(artifacts / "bare.json", bare_rows)
            state3.stage_ruleset(bare_hash, str(artifacts / "bare.json"), "/tmp/r", "session", ts=5)
            state3.mark_ruleset_committed(bare_hash, ts=6)
            tells: list[str] = []
            st3 = claude_worker.library.import_catalog(
                state3, tmp_path / "lib3", artifacts, tmp_path / "no-candidates", report=tells.append, ts=12
            )
            assert st3.validated == [] and any("unlabelled — candidate (RG8)" in t for t in tells)
            assert all(m.status == "candidate" for m in state3.library_members())
        finally:
            state3.close()
        # dry-run touches nothing
        state2 = claude_worker.state.State(tmp_path / "state2.db")
        try:
            dry = claude_worker.library.import_catalog(
                state2, tmp_path / "lib2", artifacts, candidates, icdp_toml=icdp, regime_toml=regime_toml, dry_run=True, report=lines.append
            )
            assert dry.inserted == 0 and state2.library_members() == [] and not (tmp_path / "lib2").exists()
        finally:
            state2.close()
    finally:
        state.close()


# ---- regime query ----


def test_query_words_parses_declarations_with_unknown_fill() -> None:
    words, source = claude_worker.library.query_words("trend:bull,vol:low", 0)
    assert source == "query"
    fast = claude_worker.frames.regime_word_dims(words["fast"])
    assert fast["trend"] == "bull" and fast["vol"] == "low" and fast["shape"] == "unknown" and fast["source"] == "measured"
    assert words["slow"] == words["fast"], "one declaration applies to both profiles"
    words2, _ = claude_worker.library.query_words("trend:bull;trend:bear", 0)
    assert claude_worker.frames.regime_word_dims(words2["slow"])["trend"] == "bear"
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.query_words("trend:sideways", 0)
    with pytest.raises(claude_worker.library.LibraryError):
        claude_worker.library.query_words("a;b;c", 0)


# ---- evidence ----


def _fake_detail(stale_blind: bool = False, word: str = "0001028080010102") -> dict[str, object]:
    return {
        "detail_version": 4,
        "window": {"merged_records": 5432},
        "fills": {"total": 19, "oos": 19},
        "oos": {"net_pnl_usd": -1.5, "fee_ladder_net_usd": [2.5, 1.0, -0.5]},
        "stale": {"runs": [{"lanes": {"bn": {"ticks": 100, "stale_blind": stale_blind}, "pm": {"ticks": 0, "stale_blind": True}}}]},
        "regime": {"profiles": [{"profile": "fast", "minutes": [{"word": "x", "bits": word, "minutes": 120}]}]},
    }


def _fake_run_fn(seen: list[list[str]], *, stale_blind: bool = False):  # type: ignore[no-untyped-def]
    def run_fn(argv: list[str]) -> str:
        seen.append(list(argv))
        ruleset = pathlib.Path(argv[argv.index("--ruleset") + 1])
        digest = hashlib.sha256(ruleset.read_bytes()).hexdigest()
        if "--emit-detail" in argv:
            pathlib.Path(argv[argv.index("--emit-detail") + 1]).write_text(json.dumps(_fake_detail(stale_blind)))
        return json.dumps({
            "schema_version": 1, "ruleset_hash": digest, "split": argv[argv.index("--split") + 1],
            "oos": {"net_pnl_usd": -1.5, "trades": 19, "trading_days": 1, "max_drawdown_usd": 3.25, "legs": 19, "round_trips": 4},
            "bounds": {"max_order_notional_usd": 3000.0, "max_symbol_notional_usd": 6000.0, "max_total_notional_usd": 6000.0},
            "position_rows": 1,
        })
    return run_fn


def test_run_evidence_records_the_row_from_the_additive_harness_path(tmp_path: pathlib.Path) -> None:
    state = claude_worker.state.State(tmp_path / "state.db")
    lib = tmp_path / "library"
    try:
        member, _ = claude_worker.library.add_member(state, lib, [_row("a", regimes=["vol:low"])], "a")
        window = tmp_path / "windows" / "run-1788000000000000000"
        window.mkdir(parents=True)
        seen: list[list[str]] = []
        ev = claude_worker.library.run_evidence(
            state, member, window, tmp_path / "work", fees=["--fee-bps", "okx:8:10"], run_fn=_fake_run_fn(seen), ts=99
        )
        argv = seen[0]
        assert argv[:2] == ["multivenue-engine", "backtest"] and argv[argv.index("--split") + 1] == "0/100"
        assert "--fee-bps" in argv and "--emit-detail" in argv and "--regime" not in argv
        assert hashlib.sha256(pathlib.Path(argv[argv.index("--ruleset") + 1]).read_bytes()).hexdigest() == member.member_id
        assert ev.window_id == window.name and ev.n_ticks == 5432 and ev.n_fills == 19
        assert ev.net_usd_0 == 2.5 and ev.net_usd_tier == -1.5 and ev.max_dd_usd == 3.25
        assert ev.judged is True and ev.regime_word_mode == "0001028080010102" and ev.detail_version == 4 and ev.ts == 99
        assert state.evidence_for(member.member_id)[0] == ev
        # a stale-blind lane with ticks flips `judged`
        claude_worker.library.run_evidence(state, member, window, tmp_path / "work", fees=[], run_fn=_fake_run_fn(seen, stale_blind=True))
        assert state.evidence_for(member.member_id)[0].judged is False
        s = claude_worker.library.evidence_summary(state.evidence_for(member.member_id))
        assert (s.windows, s.judged, s.positive_tier) == (1, 0, 0)
        coded = claude_worker.library.Member("icdp@x", "icdp", "coded", [], [], None, None, {}, "validated")
        with pytest.raises(claude_worker.library.LibraryError):
            claude_worker.library.run_evidence(state, coded, window, tmp_path / "work", fees=[])
    finally:
        state.close()


# ---- lanes through main ----


def test_library_lanes_end_to_end(tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str]) -> None:
    db = tmp_path / "state.db"
    lib = tmp_path / "library"
    common = ["--db", str(db), "--dir", str(lib)]
    src = tmp_path / "R.json"
    _artifact(src, [_row("xv-vlow", regimes=["vol:low"]), _row("xv-vnot", regimes=["vol:!low"])])
    assert claude_worker.library.main(["add", *common, "--from", str(src), "--name", "xv-vol", "--thesis", "t"]) == 0
    out = capsys.readouterr().out
    assert "library add: added candidate" in out and "labels=[vol:low] | [vol:!low]" in out
    assert claude_worker.library.main(["add", *common, "--from", str(src), "--name", "again"]) == 0
    assert "exists" in capsys.readouterr().out
    # split by name prefix: two members from one file
    multi = tmp_path / "M.json"
    _artifact(multi, [_row("cvfc-sol-0"), _row("cvfc-sol-1"), _row("cvfc-xrp-0")])
    assert claude_worker.library.main(["add", *common, "--from", str(multi), "--name", "cvfc", "--split-by-name-prefix"]) == 0
    out = capsys.readouterr().out
    assert "cvfc-cvfc-sol" in out and "cvfc-cvfc-xrp" in out
    assert claude_worker.library.main(["list", *common]) == 0
    out = capsys.readouterr().out
    assert "xv-vol" in out and "candidate" in out and "cvfc-cvfc-sol" in out
    # fit filter: vol=high hides the vol member (its variants cover low + !low = everything known… so it fits)
    assert claude_worker.library.main(["list", *common, "--regime", "vol:high"]) == 0
    out = capsys.readouterr().out
    assert "fit " in out and "xv-vol" in out
    assert claude_worker.library.main(["list", *common, "--regime", "trend:bull", "--all"]) == 0
    out = capsys.readouterr().out
    assert "--- " in out, "a vol-labelled member does not fit a vol-unknown query (fail-closed)"
    # label / validate / retire by name, id prefix, id
    assert claude_worker.library.main(["label", *common, "xv-vol", "--regimes", "trend:bull,vol:low", "--regimes", "trend:bear"]) == 0
    assert "labels=[trend:bull,vol:low] | [trend:bear]" in capsys.readouterr().out
    assert claude_worker.library.main(["label", *common, "xv-vol", "--regimes", "vol:purple"]) == 2
    assert "does not parse" in capsys.readouterr().err
    assert claude_worker.library.main(["validate", *common, "xv-vol"]) == 0
    capsys.readouterr()
    assert claude_worker.library.main(["list", *common, "--json"]) == 0
    payload = json.loads(capsys.readouterr().out)
    by_name = {m["name"]: m for m in payload["members"]}
    assert by_name["xv-vol"]["status"] == "validated" and by_name["xv-vol"]["labels"] == [["trend:bull", "vol:low"], ["trend:bear"]]
    member_id = by_name["xv-vol"]["member_id"]
    assert claude_worker.library.main(["retire", *common, member_id[:10]]) == 0
    assert "-> retired" in capsys.readouterr().out
    assert claude_worker.library.main(["retire", *common, "nobody"]) == 2
    assert "no member" in capsys.readouterr().err
    assert claude_worker.library.main(["label", *common, member_id, "--any"]) == 0
    assert "labels=ANY" in capsys.readouterr().out
    assert claude_worker.library.main(["import-catalog", *common, "--artifacts", str(tmp_path / "none"), "--candidates", str(tmp_path / "none"), "--icdp", str(tmp_path / "none.toml")]) == 0
    assert "inserted=0" in capsys.readouterr().out
    assert claude_worker.library.main(["add", *common, "--from", str(tmp_path / "missing.json"), "--name", "x"]) == 2
    assert "not a rows artifact" in capsys.readouterr().err
