# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
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
        '"thesis"',
        "crypto",
        "86400000",
    ):
        assert needle in text, f"static block is missing {needle!r}"


def test_static_block_teaches_the_live_caps_and_gates() -> None:
    """Drift guard (2026-08-30): the static block taught the superseded
    $100/$250/$1000 demo tier and the pre-amendment gates long after
    both moved. A keyed serve reads this text as law, so pin it to the
    numbers `backtest.GateThresholds` actually enforces."""
    text = typing.cast(str, claude_worker.strategist.system_blocks()[0]["text"])
    for needle in ("$10,000", "$20,000", "$100,000", "$7,500", ">= 50 OOS legs",
                   ">= 10 round trips", ">= 1 OOS trading day"):
        assert needle in text, f"static block is missing {needle!r}"
    for stale in ("$250", "$1000", ">= 2 OOS trading days", "<= $200"):
        assert stale not in text, f"static block still teaches the superseded {stale!r}"
    # The published mirrors and the gate module must agree.
    thresholds = claude_worker.backtest.GateThresholds()
    assert claude_worker.strategist.ROW_MAX_RISK_USD == thresholds.max_order_notional_usd
    assert claude_worker.strategist.SYM_MAX_RISK_USD == thresholds.max_symbol_notional_usd
    assert claude_worker.strategist.TABLE_MAX_RISK_USD == thresholds.max_total_notional_usd


def test_static_block_teaches_the_v2_grammar() -> None:
    """The v2 vocabulary must be in the prompt: without it a keyed serve
    can only author the legacy v1 sugar — no funding, options, depth,
    positions, groups, holds or confirms."""
    text = typing.cast(str, claude_worker.strategist.system_blocks()[0]["text"])
    for needle in ('"instrument"', '"feature"', '"combine"', '"enter"', '"exit"',
                   '"group"', '"min_hold_s"', '"confirm_pair"', "diff_bps",
                   "apr24", "mark_iv", "depth_imb", "clock_utc_sod", "4320"):
        assert needle in text, f"static block is missing {needle!r}"
    # Every FeatId token and every combine token is offered.
    for feature in claude_worker.strategist.FEATURES:
        assert feature in text, f"static block never names the feature {feature!r}"
    for combine in claude_worker.strategist.COMBINES:
        assert f'"{combine}"' in text, f"static block never names the combine {combine!r}"


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
        _row(max_risk_usd=10_000.01),  # above the $10k per-leg cap
        _row(max_risk_usd=True),
        _row(instrument="okx:BTC-USDT"),  # v1 + v2 keys in one row
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


# ---- VM2 v2 grammar arm --------------------------------------------------


def _v2(**overrides: object) -> dict[str, object]:
    """The live xv-v2 shape (artifact bfbc5349…, committed 2026-08-30)."""
    base: dict[str, object] = {
        "name": "xv-okx-bnspot",
        "family": "crypto",
        "instrument": "okx:BTC-USDT",
        "ref": "binance:btcusdt",
        "feature": "mid",
        "combine": "diff_bps",
        "abs": True,
        "enter": 3.0,
        "exit": 1.0,
        "horizon_ms": 60_000,
        "max_risk_usd": 3_000.0,
    }
    base.update(overrides)
    return {key: value for key, value in base.items() if value is not None}


def test_parse_proposal_v2_row_round_trips() -> None:
    proposal = claude_worker.strategist.parse_proposal(_proposal_json([_v2()]))
    assert proposal is not None
    row = proposal.rows[0]
    assert row["instrument"] == "okx:BTC-USDT"
    assert row["combine"] == "diff_bps"
    # Emission order is deterministic — artifact_bytes hashes these bytes.
    assert list(row) == [
        "name", "family", "instrument", "ref", "feature", "combine",
        "abs", "enter", "exit", "horizon_ms", "max_risk_usd",
    ]


def test_parse_proposal_v2_full_position_row() -> None:
    """The s1-v2 shape: a confirm_pair funding spread with holds."""
    row = _v2(
        name="s1-coti",
        instrument="binance-usdm:cotiusdt",
        ref="bybit-linear:COTIUSDT",
        feature="apr24",
        combine="diff",
        enter=0.50,
        exit=0.10,
        confirm_feature="apr72",
        confirm=0.30,
        confirm_abs=True,
        confirm_pair=True,
        group=3,
        min_hold_s=28_800,
        max_hold_s=864_000,
        horizon_ms=3_600_000,
    )
    assert claude_worker.strategist.parse_proposal(_proposal_json([row])) is not None


def test_parse_proposal_v2_single_leg_and_rolling_window() -> None:
    # depth-imb-proof shape: no ref ⇒ no combine (lhs_only is inferred).
    single = _v2(name="imb", ref=None, combine=None, feature="depth_imb", enter=0.6)
    assert claude_worker.strategist.parse_proposal(_proposal_json([single])) is not None
    rolling = _v2(name="roll", ref=None, combine=None, feature="roll_mean",
                  window_min=60, enter=100.0)
    assert claude_worker.strategist.parse_proposal(_proposal_json([rolling])) is not None


@pytest.mark.parametrize(
    "row",
    [
        _v2(bogus=1),  # unknown key
        _v2(name=None),  # missing required
        _v2(enter=None),
        _v2(instrument=None),
        _v2(feature=None),
        _v2(horizon_ms=None),
        _v2(max_risk_usd=None),
        _v2(sym=42),  # v1 key mixed into a v2 row
        _v2(combine=None),  # ref without combine
        _v2(ref=None),  # combine without ref
        _v2(ref="okx:BTC-USDT"),  # ref == instrument
        _v2(feature="last"),  # not a FeatId token
        _v2(combine="lhs_only"),  # inferred, never a token
        _v2(combine="ratio_1e9"),  # enum name, not the JSON token
        _v2(window_min=60),  # window on a non-rolling feature
        _v2(feature="roll_mean"),  # rolling feature without a window
        _v2(feature="roll_mean", window_min=4_321),
        _v2(feature="roll_mean", window_min=0),
        _v2(ref_window_min=60),  # ref leg is `mid` — no window
        _v2(cmp="gt"),
        _v2(**{"abs": 1}),  # int is not a bool
        _v2(confirm=1.0),  # confirm without confirm_feature
        _v2(confirm_feature="apr72"),  # confirm_feature without confirm
        _v2(ref=None, combine=None, confirm_feature="mid", confirm=1.0,
            confirm_pair=True),  # paired confirm needs a second leg
        _v2(exit=None, group=3),  # group without exit
        _v2(exit=None, min_hold_s=60),
        _v2(exit=None, max_hold_s=60),
        _v2(min_hold_s=600, max_hold_s=600),  # age-out must outlast the hold
        _v2(min_hold_s=600, max_hold_s=599),
        _v2(group=255),  # 0xFF is GROUP_NONE
        _v2(group=-1),
        _v2(max_risk_usd=10_000.01),
        _v2(max_risk_usd=0),
        _v2(horizon_ms=9),
        _v2(instrument="okx:BTC–USDT"),  # noqa: RUF001 — the EN DASH is the point
        _v2(instrument=""),
        _v2(instrument="x" * 129),
        _v2(enter=float("inf")),
    ],
)
def test_parse_proposal_v2_row_malformed(row: dict[str, object]) -> None:
    assert claude_worker.strategist.parse_proposal(_proposal_json([row])) is None


# ---- RG3: grammar v2.1 regime keys (strategist-v3) -----------------------


def test_static_block_v3_teaches_the_regime_keys_and_variants() -> None:
    assert claude_worker.strategist.STRATEGIST_PROMPT_VERSION == "strategist-v4"
    text = typing.cast(str, claude_worker.strategist.system_blocks()[0]["text"])
    for needle in ('"regimes"', '"regime_off"', '"rel"', "REGIME LAW", "GATE, never a signal",
                   "exits", "NEVER gated", "VARIANTS", "DISJOINT", "--regime off",
                   "trend:bull", '"soft"', '"hard"', "lagging|inline|leading",
                   "WORKED EXAMPLE C"):
        assert needle in text, f"static block is missing {needle!r}"
    for dim, values in claude_worker.strategist._REGIME_DIM_VALUES.items():  # noqa: SLF001
        assert dim in text
        for v in values:
            assert v in text, f"static block never names {dim}:{v}"
    # The worked examples parse under the mirror (a prompt that teaches an
    # unparseable row would be a lie).
    for example in (
        {"name": "xv-btc-okx-bn", "family": "crypto", "instrument": "okx:BTC-USDT",
         "ref": "binance:btcusdt", "feature": "mid", "combine": "diff_bps", "abs": True,
         "enter": 3.0, "regimes": ["vol:!high", "slow:shape:chop|mixed"],
         "horizon_ms": 60000, "max_risk_usd": 3000.0},
        {"name": "mom-eth-bull", "instrument": "okx:ETH-USDT-SWAP", "feature": "mid",
         "ref": "binance-usdm:ethusdt", "ref_feature": "roll_mean", "ref_window_min": 30,
         "combine": "diff_bps", "enter": 25.0, "exit": 5.0, "side": "bid",
         "regimes": ["trend:bull", "shape:trend"], "rel": "leading|inline",
         "horizon_ms": 300000, "max_risk_usd": 2000.0},
    ):
        assert claude_worker.strategist.parse_proposal(_proposal_json([example])) is not None, example


def test_parse_proposal_v3_regime_keys_round_trip_in_canonical_order() -> None:
    row = _v2(regimes=["trend:bull|neutral", "slow:vol:!high"], regime_off="hard", rel="lagging|inline")
    proposal = claude_worker.strategist.parse_proposal(_proposal_json([row]))
    assert proposal is not None
    out = proposal.rows[0]
    assert out["regimes"] == ["trend:bull|neutral", "slow:vol:!high"]
    assert out["regime_off"] == "hard"
    assert out["rel"] == "lagging|inline"
    assert list(out) == [
        "name", "family", "instrument", "ref", "feature", "combine", "abs", "enter", "exit",
        "regimes", "regime_off", "rel", "horizon_ms", "max_risk_usd",
    ], "the regime keys sit before horizon_ms in the canonical emission"
    # Unlabelled rows still parse (legacy artifacts) — the prompt asks,
    # the validator does not require.
    assert claude_worker.strategist.parse_proposal(_proposal_json([_v2()])) is not None


@pytest.mark.parametrize(
    "term,ok",
    [
        ("trend:bull", True),
        ("fast:trend:bull|neutral", True),
        ("slow:vol:!high", True),
        ("stretch:*", True),
        ("source:measured|declared", True),
        ("vol:low|unknown", True),
        ("rel:lagging|inline", True),
        ("slow:rel:leading", True),
        ("", False),
        ("trend", False),
        ("trend:", False),
        ("trend:sideways", False),
        ("mood:happy", False),
        ("medium:trend:bull", False),
        ("trend:bull|", False),
        ("rel:unknown", False),
        ("source:unknown|nope", False),
        ("fast:", False),
        ("a:b:c:d", False),
        ("x" * 65, False),
    ],
)
def test_regime_term_structural_mirror(term: str, ok: bool) -> None:
    assert claude_worker.strategist.regime_term_ok(term) is ok


@pytest.mark.parametrize(
    "row",
    [
        _v2(regimes=[]),  # empty list
        _v2(regimes="trend:bull"),  # not a list
        _v2(regimes=[1]),  # not a string
        _v2(regimes=["trend:sideways"]),  # unknown value
        _v2(regimes=["mood:happy"]),  # unknown dimension
        _v2(regimes=["trend:bull"] * 17),  # more terms than (profile, dim) pairs
        _v2(regime_off="soft"),  # off without regimes
        _v2(rel="lagging"),  # rel without regimes
        _v2(regimes=["trend:bull"], regime_off="maybe"),
        _v2(regimes=["trend:bull"], rel="rel:lagging"),  # the array form, not the sugar
        _v2(regimes=["trend:bull"], rel="sideways"),
        _v2(regimes=["trend:bull"], rel="fast:"),
        _v2(regimes=["trend:bull"], rel=7),
    ],
)
def test_parse_proposal_v3_regime_keys_malformed(row: dict[str, object]) -> None:
    assert claude_worker.strategist.parse_proposal(_proposal_json([row])) is None


def test_parse_proposal_v2_mirrors_the_live_artifacts() -> None:
    """The strongest check available offline: every row shape the engine
    validator has actually ADMITTED must survive this mirror. These are
    the VM2 V7/V8 artifacts (authored by a git-excluded local one-shot;
    see docs/research-tools-exclusion-plan.md), xv-v2 among them."""
    live: list[dict[str, object]] = [
        # xv-v2 — committed live 2026-08-30
        _v2(),
        # cvfc-v2 — grouped per-coin funding carry with both holds
        _v2(name="cvfc-sol-0", instrument="binance-usdm:solusdt",
            ref="deribit:SOL_USDC-PERPETUAL", feature="apr24", combine="diff",
            enter=0.20, exit=0.0, group=1, min_hold_s=345_600,
            max_hold_s=1_728_000, max_risk_usd=4_950.0),
        # basis-proof — spot↔perp bps with a funding confirm, no pair
        _v2(name="basis-btc", instrument="binance-usdm:btcusdt",
            ref="binance:btcusdt", feature="mid", combine="diff_bps",
            enter=3.0, exit=0.5, confirm_feature="apr24", confirm=0.05,
            confirm_abs=True, max_hold_s=86_400, max_risk_usd=9_900.0),
        # iv-spread-proof — the OptSummary channel
        _v2(name="iv-btc-atm", instrument="deribit:BTC-30AUG26-77500-C",
            ref="okx:BTC-USD-260830-77500-C", feature="mark_iv",
            combine="diff", enter=0.03, exit=0.005, max_hold_s=21_600,
            max_risk_usd=2_000.0),
        # depth-imb-proof — single leg on the WS10-B depth channel
        _v2(name="imb-btc-okx", instrument="okx:BTC-USDT", ref=None,
            combine=None, feature="depth_imb", enter=0.6, exit=0.1,
            max_hold_s=3_600, max_risk_usd=5_000.0),
    ]
    proposal = claude_worker.strategist.parse_proposal(_proposal_json(live))
    assert proposal is not None, "a live-admitted artifact shape was refused"
    assert len(proposal.rows) == len(live)


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


# ---- RG5: the regime verdict + REGIME digest section (strategist-v4) -----


def test_static_block_v4_asks_for_the_regime_verdict() -> None:
    text = typing.cast(str, claude_worker.strategist.system_blocks()[0]["text"])
    for needle in ('"regime": VERDICT', "REGIME VERDICT", '"measured"', "trend:bull,shape:trend",
                   "OPTIONAL", "TTL", "the rows fit the regime"):
        assert needle in text, f"static block is missing {needle!r}"


def test_parse_proposal_accepts_the_optional_regime_verdict_and_refuses_bad_ones() -> None:
    base = {"thesis": "confirm", "rows": [_row()]}
    p = claude_worker.strategist.parse_proposal(json.dumps({**base, "regime": {"fast": "measured"}}))
    assert p is not None and p.regime == {"fast": "measured"}
    p = claude_worker.strategist.parse_proposal(
        json.dumps({**base, "regime": {"fast": "trend:bull, shape:trend", "slow": "measured"}})
    )
    assert p is not None and p.regime == {"fast": "trend:bull, shape:trend", "slow": "measured"}
    # Omitted = None (the pre-RG5 contract is unchanged).
    p = claude_worker.strategist.parse_proposal(json.dumps(base))
    assert p is not None and p.regime is None
    for bad in (
        {"fast": "sideways"},          # not a dimension:value
        {"fast": "trend:sideways"},    # unknown value
        {"fast": 7},                   # not a string
        {},                            # empty map
        {"medium": "measured"},        # unknown profile
        {"fast": "trend:bull|neutral"},  # a region, not a word
        {"fast": "rel:lagging"},       # rel is per-symbol, never declared
        "measured",                    # not a map
        None,
    ):
        assert claude_worker.strategist.parse_proposal(json.dumps({**base, "regime": bad})) is None, bad
    # Any other top-level key is still malformed.
    assert claude_worker.strategist.parse_proposal(json.dumps({**base, "verdict": {}})) is None


def test_build_digest_regime_section_is_optional(tmp_path: pathlib.Path) -> None:
    without = claude_worker.strategist.build_digest(tmp_path, None, {"m": 1})
    assert "REGIME" not in without
    with_section = claude_worker.strategist.build_digest(
        tmp_path, None, {"m": 1}, regime="  [fast] measured: trend=bull shape=trend"
    )
    assert "\nREGIME (worker-measured words" in with_section
    assert "[fast] measured: trend=bull" in with_section
    assert with_section.startswith(without.rstrip("\n"))
