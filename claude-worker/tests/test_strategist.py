"""strategist.py units (design §12 strategist rows) + the 8h llm.py
surface growth. Pure/file-level tests — the cycle composition lives in
``test_research_cycle.py``. NO live SDK anywhere (house rule).

Convention: full ``import x`` only. No ``from x import y``.
"""

import inspect
import json
import pathlib
import types
import typing

import anthropic
import pytest

import claude_worker.backtest
import claude_worker.config
import claude_worker.llm
import claude_worker.state
import claude_worker.strategist

# ---- canned §7.3 shapes --------------------------------------------------


def _row(**overrides: object) -> dict[str, object]:
    base: dict[str, object] = {
        "name": "t-row",
        "family": "crypto",
        "trigger": {"type": "level_breach", "level": 0.42},
        "sym": 42,
        "side": "bid",
        "edge_bps": 80,
        "horizon_ms": 1500,
        "max_risk_usd": 50.0,
    }
    base.update(overrides)
    return base


def _proposal_json(rows: list[dict[str, object]] | None = None, thesis: str = "lag fade") -> str:
    return json.dumps({"thesis": thesis, "rows": [_row()] if rows is None else rows})


# ---- env keys (§7.5, read at the seam) -----------------------------------


def test_interval_default_and_override() -> None:
    assert claude_worker.strategist.interval_s(env={}) == 21_600
    assert claude_worker.strategist.interval_s(env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": "60"}) == 60


@pytest.mark.parametrize("raw", ["abc", "1.5", "0", "-1"])
def test_interval_strict_parse(raw: str) -> None:
    with pytest.raises(ValueError, match="CLAUDE_WORKER_STRATEGIST_INTERVAL_S"):
        claude_worker.strategist.interval_s(env={"CLAUDE_WORKER_STRATEGIST_INTERVAL_S": raw})


def test_daily_cap_default_override_and_zero_kill_switch() -> None:
    assert claude_worker.strategist.daily_cap(env={}) == 12
    assert claude_worker.strategist.daily_cap(env={"CLAUDE_WORKER_STRATEGIST_DAILY_CAP": "3"}) == 3
    assert claude_worker.strategist.daily_cap(env={"CLAUDE_WORKER_STRATEGIST_DAILY_CAP": "0"}) == 0


@pytest.mark.parametrize("raw", ["abc", "-1", "2.5"])
def test_daily_cap_strict_parse(raw: str) -> None:
    with pytest.raises(ValueError, match="CLAUDE_WORKER_STRATEGIST_DAILY_CAP"):
        claude_worker.strategist.daily_cap(env={"CLAUDE_WORKER_STRATEGIST_DAILY_CAP": raw})


# ---- §7.2 prompt architecture --------------------------------------------


def test_system_blocks_static_and_cache_marked() -> None:
    blocks = claude_worker.strategist.system_blocks()
    assert len(blocks) == 1
    block = blocks[0]
    assert block["type"] == "text"
    assert block["cache_control"] == {"type": "ephemeral"}
    text = typing.cast(str, block["text"])
    # The grammar/caps contract lives in the STATIC block.
    for needle in (
        '"rows"',
        "cross_deviation",
        "level_breach",
        "$100",
        "$250",
        "$1000",
        '"thesis"',
        "crypto",
        "86400000",
    ):
        assert needle in text, f"static block is missing {needle!r}"


def _digest_fixture(tmp_path: pathlib.Path) -> pathlib.Path:
    features = tmp_path / "features"
    run_dir = features / "run-1"
    run_dir.mkdir(parents=True)
    (run_dir / "42.json").write_text('{"sym":42,"ticks":9}')
    (run_dir / "42-ohlcv.json").write_text('{"sym":42,"candles":[[1,2,3,4,5,6]]}')
    news = features / "news"
    news.mkdir()
    (news / "items-100.ndjson").write_text('{"id":"g1","title":"older"}\n')
    (news / "items-200.ndjson").write_text('{"id":"g2","title":"newer"}\n')
    return features


def test_build_digest_sections_and_determinism(tmp_path: pathlib.Path) -> None:
    features = _digest_fixture(tmp_path)
    markets = {"btc-daily": 42, "binance:btcusdt": 7}
    digest = claude_worker.strategist.build_digest(features, "run-1", markets, universe=[42, 7])
    assert "btc-daily -> sym 42" in digest
    assert "OBSERVED CAPTURE UNIVERSE" in digest and "7, 42" in digest
    assert '42.json: {"sym":42,"ticks":9}' in digest
    assert "42-ohlcv.json" in digest
    assert '"g1"' in digest and '"g2"' in digest
    assert digest.index('"g1"') < digest.index('"g2"'), "news oldest->newest"
    # Deterministic for identical inputs — the SQLite dedupe key.
    assert digest == claude_worker.strategist.build_digest(
        features, "run-1", markets, universe=[42, 7]
    )


def test_build_digest_cap_enforced(tmp_path: pathlib.Path) -> None:
    features = _digest_fixture(tmp_path)
    (features / "run-1" / "42.json").write_text("x" * 100_000)
    digest = claude_worker.strategist.build_digest(features, "run-1", {"m": 42}, cap=500)
    marker = claude_worker.strategist._TRUNCATION_MARKER
    assert len(digest) <= 500 + len(marker)
    assert marker in digest


def test_user_prompt_carries_digest_not_static_block(tmp_path: pathlib.Path) -> None:
    features = _digest_fixture(tmp_path)
    digest = claude_worker.strategist.build_digest(features, "run-1", {"m": 42})
    prompt = claude_worker.strategist.build_user_prompt(digest)
    assert digest in prompt
    static_text = typing.cast(str, claude_worker.strategist.system_blocks()[0]["text"])
    assert static_text not in prompt, "static/dynamic split violated"


def test_revision_prompt_carries_gates_report_and_prior_rows() -> None:
    prompt = claude_worker.strategist.build_revision_prompt(
        "DIGEST-BODY", '{"rows":[]}', "pnl_positive=False -> FAIL", '{"gates":{}}'
    )
    for needle in ("DIGEST-BODY", '{"rows":[]}', "pnl_positive=False -> FAIL", '{"gates":{}}', "FAILED"):
        assert needle in prompt


# ---- §7.3 strict output parse --------------------------------------------


def test_parse_proposal_good_two_rows() -> None:
    rows = [
        _row(),
        _row(name="x-dev", trigger={"type": "cross_deviation", "ref": 7}, side="ask"),
    ]
    proposal = claude_worker.strategist.parse_proposal(_proposal_json(rows))
    assert proposal is not None
    assert proposal.thesis == "lag fade"
    assert len(proposal.rows) == 2
    assert list(proposal.rows[0]) == [
        "name", "family", "trigger", "sym", "side", "edge_bps", "horizon_ms", "max_risk_usd",
    ], "canonical key order"
    assert proposal.rows[1]["trigger"] == {"type": "cross_deviation", "ref": 7}


@pytest.mark.parametrize(
    "raw",
    [
        "not json",
        "[1]",
        '"str"',
        json.dumps({"thesis": "t"}),  # missing rows
        json.dumps({"rows": [_row()]}),  # missing thesis
        json.dumps({"thesis": "t", "rows": [_row()], "extra": 1}),  # extra top key
        json.dumps({"thesis": "", "rows": [_row()]}),  # empty thesis
        json.dumps({"thesis": 7, "rows": [_row()]}),  # non-str thesis
        json.dumps({"thesis": "x" * 4001, "rows": [_row()]}),  # thesis over cap
        json.dumps({"thesis": "t", "rows": "nope"}),  # rows not a list
        json.dumps({"thesis": "t", "rows": []}),  # zero rows
        json.dumps({"thesis": "t", "rows": [1]}),  # row not an object
    ],
)
def test_parse_proposal_top_level_malformed(raw: str) -> None:
    assert claude_worker.strategist.parse_proposal(raw) is None


@pytest.mark.parametrize(
    "row",
    [
        {k: v for k, v in _row().items() if k != "side"},  # missing key
        _row(bogus=1),  # unknown key
        _row(name=""),
        _row(name="x" * 65),
        _row(name="naïve"),  # non-ascii
        _row(family="memes"),
        _row(side="buy"),
        _row(sym=True),  # bool sneaks int
        _row(sym=-1),
        _row(edge_bps=10_001),
        _row(edge_bps=80.5),  # fractional in integer field
        _row(edge_bps=True),
        _row(horizon_ms=9),
        _row(horizon_ms=86_400_001),
        _row(max_risk_usd=0),
        _row(max_risk_usd=100.5),  # above the $100 row cap
        _row(max_risk_usd=True),
        _row(trigger={"type": "level_breach", "level": 1.5}),
        _row(trigger={"type": "level_breach", "level": -0.1}),
        _row(trigger={"type": "level_breach", "level": 0.4, "ref": 7}),  # rule-6 mirror
        _row(trigger={"type": "cross_deviation", "ref": 42}),  # ref == sym
        _row(trigger={"type": "cross_deviation", "ref": 7, "level": 0.5}),
        _row(trigger={"type": "cross_deviation"}),
        _row(trigger={"type": "warp"}),
        _row(trigger="level_breach"),
    ],
)
def test_parse_proposal_row_malformed(row: dict[str, object]) -> None:
    assert claude_worker.strategist.parse_proposal(_proposal_json([row])) is None


def test_parse_proposal_oversized_row_count() -> None:
    ok = [_row(name=f"r{i}") for i in range(256)]
    assert claude_worker.strategist.parse_proposal(_proposal_json(ok)) is not None
    over = [_row(name=f"r{i}") for i in range(257)]
    assert claude_worker.strategist.parse_proposal(_proposal_json(over)) is None


# ---- candidate files + §8.1 install --------------------------------------


def test_write_candidate_canonical_hash_and_atomic(tmp_path: pathlib.Path) -> None:
    proposal = claude_worker.strategist.parse_proposal(_proposal_json())
    assert proposal is not None
    candidate = claude_worker.strategist.write_candidate(tmp_path / "cand", proposal, now_s=0.0)
    assert candidate.path.name == f"19700101T000000Z-{candidate.hash128_hex}.json"
    assert candidate.thesis == "lag fade"
    # The file hash is exactly what the frozen worker path recomputes.
    full_hash, hash128 = claude_worker.backtest.ruleset_hashes(candidate.path)
    assert full_hash == candidate.full_hash
    assert hash128.hex() == candidate.hash128_hex
    # Canonical artifact: rows only (validator is unknown-key-strict).
    body = json.loads(candidate.path.read_text())
    assert set(body) == {"rows"}
    assert body["rows"][0]["name"] == "t-row"
    assert not list(candidate.path.parent.glob("*.tmp")), "atomic write leaves no temp"


def test_archive_rejected_marker(tmp_path: pathlib.Path) -> None:
    path = claude_worker.strategist.archive_rejected(tmp_path / "cand", "garbage-output", now_s=0.0)
    assert path.name.endswith(".rejected.json")
    assert path.read_text() == "garbage-output"
    assert not list(path.parent.glob("*.tmp"))


def test_install_candidate_atomic_and_overwrites(tmp_path: pathlib.Path) -> None:
    proposal = claude_worker.strategist.parse_proposal(_proposal_json())
    assert proposal is not None
    candidate = claude_worker.strategist.write_candidate(tmp_path / "cand", proposal)
    ruleset_dir = tmp_path / "rulesets"
    target = claude_worker.strategist.install_candidate(
        ruleset_dir, candidate.path, candidate.hash128_hex
    )
    assert target == ruleset_dir / f"{candidate.hash128_hex}.json"
    assert target.read_bytes() == candidate.path.read_bytes()
    assert not list(ruleset_dir.glob("*.tmp"))
    # Idempotent re-install (promote retry path).
    again = claude_worker.strategist.install_candidate(
        ruleset_dir, candidate.path, candidate.hash128_hex
    )
    assert again == target and target.read_bytes() == candidate.path.read_bytes()


def test_candidates_dir_is_worker_dir(tmp_path: pathlib.Path) -> None:
    assert (
        claude_worker.strategist.candidates_dir(tmp_path / "worker" / "state.db")
        == tmp_path / "worker" / "candidates"
    )


# ---- §7.5 budget ledger arithmetic ---------------------------------------

_DAY_NS = 86_400_000_000_000


def test_calls_today_counts_utc_day_only(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    now_ns = 3 * _DAY_NS + 12 * 3_600_000_000_000  # day 3, 12:00 UTC
    kind = claude_worker.strategist.EVENT_STRATEGIST_CALL
    st.record_event(kind, "{}", ts_ns=now_ns - 3_600_000_000_000)  # today
    st.record_event(kind, "{}", ts_ns=claude_worker.strategist.utc_day_start_ns(now_ns))  # midnight
    st.record_event(kind, "{}", ts_ns=now_ns - _DAY_NS)  # yesterday
    st.record_event("frame_sent", "{}", ts_ns=now_ns)  # other kind
    assert claude_worker.strategist.calls_today(st, now_ns) == 2
    st.close()


def test_call_detail_fields() -> None:
    completion = claude_worker.llm.Completion(
        text="x", input_tokens=1000, output_tokens=200, cache_read_input_tokens=900,
        cache_creation_input_tokens=0,
    )
    detail = json.loads(claude_worker.strategist.call_detail(completion, "proposal"))
    assert detail == {
        "model": "claude-fable-5",
        "purpose": "proposal",
        "input_tokens": 1000,
        "output_tokens": 200,
        "cache_read_input_tokens": 900,
        "cache_creation_input_tokens": 0,
        "cache_read": True,
    }
    cold = claude_worker.llm.Completion("x", 1000, 200, 0, 500)
    assert json.loads(claude_worker.strategist.call_detail(cold, "revision"))["cache_read"] is False


# ---- background call + SQLite dedupe (§7.4/§7.6) -------------------------


def test_call_with_cache_miss_then_dedupe_hit(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "state.db"
    # A second OPEN handle mimics serve's own connection (WAL, §5.3).
    serve_handle = claude_worker.state.State(db)
    calls: list[tuple[list[dict[str, object]], str]] = []

    def complete_fn(
        system: list[dict[str, object]], prompt: str
    ) -> claude_worker.llm.Completion:
        calls.append((system, prompt))
        return claude_worker.llm.Completion("resp-1", 10, 5, 0, 0)

    first = claude_worker.strategist.call_with_cache(db, "same prompt", complete_fn)
    assert first.text == "resp-1"
    assert first.sqlite_cache_hit is False
    assert first.completion is not None and first.completion.input_tokens == 10
    assert len(calls) == 1
    # The §7.2 static block rode the call, cache_control-marked.
    assert calls[0][0] == claude_worker.strategist.system_blocks()
    assert calls[0][1] == "same prompt"

    second = claude_worker.strategist.call_with_cache(db, "same prompt", complete_fn)
    assert second == claude_worker.strategist.CallResult("resp-1", True, None)
    assert len(calls) == 1, "dedupe hit: zero API cost"
    serve_handle.close()


def test_call_with_cache_version_scoping(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "state.db"
    st = claude_worker.state.State(db)
    # Pre-seed a stale row under a DIFFERENT template version: must miss.
    st.cached_complete(
        claude_worker.config.MODEL_STRATEGIST, "strategist-v0", "p", lambda _m, _p: "old"
    )
    st.close()
    result = claude_worker.strategist.call_with_cache(
        db, "p", lambda _s, _p: claude_worker.llm.Completion("new", 1, 1, 0, 0)
    )
    assert (result.text, result.sqlite_cache_hit) == ("new", False)


# ---- llm.py surface growth (§7.2; frozen callers untouched) --------------


class _SystemFakeMessages:
    """SDK-shaped double that ACCEPTS the grown surface (system + usage)."""

    def __init__(self, text: str, usage: types.SimpleNamespace | None) -> None:
        self._text = text
        self._usage = usage
        self.kwargs: list[dict[str, object]] = []

    def create(self, **kwargs: object) -> types.SimpleNamespace:
        self.kwargs.append(kwargs)
        block = anthropic.types.TextBlock(type="text", text=self._text, citations=None)
        message = types.SimpleNamespace(content=[block])
        if self._usage is not None:
            message.usage = self._usage
        return message


class _SystemFakeClient:
    def __init__(self, text: str = "ok", usage: types.SimpleNamespace | None = None) -> None:
        self.messages = _SystemFakeMessages(text, usage)


def _client(fake: _SystemFakeClient) -> anthropic.Anthropic:
    return typing.cast(anthropic.Anthropic, fake)


def test_complete_message_returns_usage_and_passes_system() -> None:
    usage = types.SimpleNamespace(
        input_tokens=1200, output_tokens=340, cache_read_input_tokens=1100,
        cache_creation_input_tokens=0,
    )
    fake = _SystemFakeClient("out", usage)
    blocks = claude_worker.strategist.system_blocks()
    completion = claude_worker.llm.complete_message(
        _client(fake),
        claude_worker.config.MODEL_STRATEGIST,
        "prompt",
        max_tokens=claude_worker.llm.STRATEGIST_MAX_TOKENS,
        system=blocks,
    )
    assert completion == claude_worker.llm.Completion("out", 1200, 340, 1100, 0)
    sent = fake.messages.kwargs[0]
    assert sent["model"] == "claude-fable-5"
    assert sent["max_tokens"] == 4096
    assert sent["system"] == blocks


def test_complete_message_omits_system_when_none_and_zeroes_absent_usage() -> None:
    fake = _SystemFakeClient("out", usage=None)
    completion = claude_worker.llm.complete_message(_client(fake), "m", "p")
    assert completion == claude_worker.llm.Completion("out", 0, 0, 0, 0)
    assert "system" not in fake.messages.kwargs[0], "pre-8h call shape preserved"
    assert fake.messages.kwargs[0]["max_tokens"] == claude_worker.llm.LLM_MAX_TOKENS


def test_complete_message_rejects_bool_usage_fields() -> None:
    usage = types.SimpleNamespace(
        input_tokens=True, output_tokens=-5, cache_read_input_tokens=7,
        cache_creation_input_tokens="x",
    )
    completion = claude_worker.llm.complete_message(_client(_SystemFakeClient("t", usage)), "m", "p")
    assert completion == claude_worker.llm.Completion("t", 0, 0, 7, 0)


def test_strategist_token_budget_constant() -> None:
    assert claude_worker.llm.STRATEGIST_MAX_TOKENS == 4096


# ---- §8.2 additive state surface (existing call sites byte-unchanged) ----


def test_stage_ruleset_signature_is_additive() -> None:
    sig = inspect.signature(claude_worker.state.State.stage_ruleset)
    model = sig.parameters["model"]
    thesis = sig.parameters["thesis"]
    assert model.kind is inspect.Parameter.KEYWORD_ONLY and model.default is None
    assert thesis.kind is inspect.Parameter.KEYWORD_ONLY and thesis.default is None


def test_stage_ruleset_attribution_written_and_preserved(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    full_hash = "ab" * 32
    # Pre-8h call shape: attribution stays NULL, row shape unchanged.
    st.stage_ruleset(full_hash, "rs.json", "rs.report.json", "session", 100)
    assert st.ruleset_row(full_hash) == (
        full_hash, "rs.json", "rs.report.json", True, "session", 100, None,
    )
    assert st.ruleset_attribution(full_hash) == (None, None)
    # §8.2: the auto path writes the pre-provisioned columns.
    st.stage_ruleset(
        full_hash, "rs.json", "rs.report.json", "auto", 200,
        model="claude-fable-5", thesis="fade the lag",
    )
    assert st.ruleset_attribution(full_hash) == ("claude-fable-5", "fade the lag")
    # A later restage WITHOUT attribution (e.g. §8.3 restage-prior via the
    # frozen pair) PRESERVES it (COALESCE).
    st.stage_ruleset(full_hash, "rs.json", "rs.report.json", "session", 300)
    assert st.ruleset_attribution(full_hash) == ("claude-fable-5", "fade the lag")
    assert st.ruleset_row(full_hash) == (
        full_hash, "rs.json", "rs.report.json", True, "session", 300, None,
    ), "ruleset_row stays the pinned 7-tuple"
    # Explicit new values overwrite.
    st.stage_ruleset(
        full_hash, "rs.json", "rs.report.json", "auto", 400, model="m2", thesis="t2"
    )
    assert st.ruleset_attribution(full_hash) == ("m2", "t2")
    assert st.ruleset_attribution("cd" * 32) is None
    st.close()
