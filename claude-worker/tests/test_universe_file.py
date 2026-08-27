# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""M1 universe-file seeding tests (docs/mvp-progress.md M1d).

Covers ``fetchers.universe_file_proposals`` (the Python mirror of the
``core-config::universe`` allocation law), the additive pair merge in
``refresh_market_map``, and the ``CLAUDE_WORKER_UNIVERSE_FILE`` fetch
seam in ``run_secondary``. Additive file — the frozen suites are
untouched.

Convention: full ``import x`` only.
"""

import json
import pathlib

import claude_worker.fetchers
import claude_worker.frames

PM = claude_worker.frames.VENUE_POLYMARKET

T0 = "1" * 20
TY = "2" * 20
TN = "3" * 20
T3 = "4" * 20


def _write(tmp_path: pathlib.Path, text: str) -> pathlib.Path:
    p = tmp_path / "universe.toml"
    p.write_text(text)
    return p


def _toml(entries: list[str]) -> str:
    inner = ", ".join(f'"{e}"' for e in entries)
    return f"[polymarket]\nmarkets = [{inner}]\n"


def test_law_replication_and_pairs(tmp_path: pathlib.Path) -> None:
    """Flat token order: token[0] -> 42, then namespaced ordinals; a
    YES/NO entry contributes two consecutive syms and one pair."""
    p = _write(tmp_path, _toml([T0, f"{TY}:{TN}", T3]))
    universe = {42: PM, 2: PM, 3: PM, 4: PM}
    props, pairs, lines = claude_worker.fetchers.universe_file_proposals(p, universe)
    assert props == {T0: 42, TY: 2, TN: 3, T3: 4}
    assert pairs == ((2, 3),)
    assert any("entries=3" in ln for ln in lines)


def test_unobserved_syms_are_skipped(tmp_path: pathlib.Path) -> None:
    """Only observed PM syms are proposed; a pair with one unobserved
    leg names the observed leg but proposes no pair."""
    p = _write(tmp_path, _toml([T0, f"{TY}:{TN}"]))
    universe = {42: PM, 2: PM}  # sym 3 (the NO leg) not observed
    props, pairs, _ = claude_worker.fetchers.universe_file_proposals(p, universe)
    assert props == {T0: 42, TY: 2}
    assert pairs == ()


def test_non_pm_sym_in_universe_is_not_proposed(tmp_path: pathlib.Path) -> None:
    """A sym observed under a DIFFERENT venue byte never gets a PM
    token name (venue-byte guard)."""
    p = _write(tmp_path, _toml([T0]))
    universe = {42: claude_worker.frames.VENUE_BINANCE}
    props, pairs, _ = claude_worker.fetchers.universe_file_proposals(p, universe)
    assert props == {}
    assert pairs == ()


def test_missing_file_is_reported_not_fatal(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "absent.toml"
    props, pairs, lines = claude_worker.fetchers.universe_file_proposals(p, {42: PM})
    assert props == {}
    assert pairs == ()
    assert any("skipped" in ln for ln in lines)


def test_malformed_toml_is_reported_not_fatal(tmp_path: pathlib.Path) -> None:
    p = _write(tmp_path, "[polymarket\nmarkets = oops")
    props, pairs, lines = claude_worker.fetchers.universe_file_proposals(p, {42: PM})
    assert props == {}
    assert pairs == ()
    assert any("TOML parse error" in ln for ln in lines)


def test_bad_entries_are_skipped_without_consuming_ordinals(
    tmp_path: pathlib.Path,
) -> None:
    """Best-effort skip: a malformed entry consumes no flat ordinals.
    (The ENGINE refuses such a config outright, so a running engine
    can never disagree with these proposals — recorded behavior.)"""
    p = _write(tmp_path, _toml(["notatoken", T0, f"{TY}:{TY}"]))
    universe = {42: PM, 2: PM}
    props, pairs, lines = claude_worker.fetchers.universe_file_proposals(p, universe)
    assert props == {T0: 42}
    assert pairs == ()
    assert any("skipped=2" in ln for ln in lines)


def test_cex_sections_seed_descriptor_names(tmp_path: pathlib.Path) -> None:
    """CEX lists mirror the law: bn spot[0] -> 7 anchor, spot[1] ->
    venue<<24|2; usdm[0] -> venue<<24|513; okx/deribit/hl ->
    venue<<24|i+1 — proposed as §9.4 descriptor names, observed-only."""
    text = _toml([T0]) + (
        "[binance]\nspot = [\"btcusdt\", \"ethusdt\"]\nusdm = [\"btcusdt\"]\n"
        "[okx]\ninstruments = [\"BTC-USDT\", \"ETH-USDT-SWAP\"]\n"
        "[deribit]\ninstruments = [\"BTC-PERPETUAL\"]\n"
        "[hyperliquid]\ncoins = [\"BTC\"]\n"
    )
    p = _write(tmp_path, text)
    bn = claude_worker.frames.VENUE_BINANCE
    okx = claude_worker.frames.VENUE_OKX
    dbt = claude_worker.frames.VENUE_DERIBIT
    hl = claude_worker.frames.VENUE_HYPERLIQUID
    universe = {
        42: PM,
        7: bn,
        (bn << 24) | 2: bn,
        (bn << 24) | 513: bn,
        (okx << 24) | 1: okx,
        (okx << 24) | 2: okx,
        (dbt << 24) | 1: dbt,
        # hl coin NOT observed -> not proposed
    }
    props, _pairs, _lines = claude_worker.fetchers.universe_file_proposals(p, universe)
    assert props[T0] == 42
    assert props["binance:btcusdt"] == 7
    assert props["binance:ethusdt"] == (bn << 24) | 2
    assert props["binance-usdm:btcusdt"] == (bn << 24) | 513
    assert props["okx:BTC-USDT"] == (okx << 24) | 1
    assert props["okx:ETH-USDT-SWAP"] == (okx << 24) | 2
    assert props["deribit:BTC-PERPETUAL"] == (dbt << 24) | 1
    assert "hyperliquid:BTC" not in props


def test_refresh_appends_and_dedupes_pairs(tmp_path: pathlib.Path) -> None:
    """Operator pairs stay first and verbatim; proposals append,
    duplicates collapse; re-runs are idempotent."""
    mp = tmp_path / "map.json"
    claude_worker.fetchers.refresh_market_map(
        mp, {}, ((10, 11),), {}, {}, pair_proposals=((2, 3), (10, 11))
    )
    data = json.loads(mp.read_text())
    assert data["hip4_pairs"] == [[10, 11], [2, 3]]
    loaded = dict(data["markets"])
    claude_worker.fetchers.refresh_market_map(
        mp, loaded, ((10, 11), (2, 3)), {}, {}, pair_proposals=((2, 3),)
    )
    data2 = json.loads(mp.read_text())
    assert data2["hip4_pairs"] == [[10, 11], [2, 3]]


def test_run_secondary_env_gate_and_seeding(tmp_path: pathlib.Path) -> None:
    """Env unset ⇒ byte-identical pre-M1 behavior (no universe lines);
    env set ⇒ proposals land in the map and pairs join the machinery
    (no-rest mode: map ownership still runs, §6.2)."""
    universe = {42: PM, 2: PM, 3: PM}
    mp = tmp_path / "map.json"
    report = claude_worker.fetchers.run_secondary(
        universe,
        {},
        (),
        mp,
        tmp_path / "features",
        "run-x",
        True,
        None,
        None,
        env={},
    )
    assert not any("universe file" in ln for ln in report.lines)

    p = _write(tmp_path, _toml([T0, f"{TY}:{TN}"]))
    report2 = claude_worker.fetchers.run_secondary(
        universe,
        {},
        (),
        mp,
        tmp_path / "features",
        "run-x",
        True,
        None,
        None,
        env={"CLAUDE_WORKER_UNIVERSE_FILE": str(p)},
    )
    assert any("universe file" in ln for ln in report2.lines)
    data = json.loads(mp.read_text())
    assert data["markets"][T0] == 42
    assert data["markets"][TY] == 2
    assert data["markets"][TN] == 3
    assert [2, 3] in data["hip4_pairs"]
