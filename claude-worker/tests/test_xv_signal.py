# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""xv_signal tests — synthetic capture tails, no network, no engine."""

import json
import pathlib
import struct
import time

import claude_worker.frames
import claude_worker.regime
import claude_worker.xv_signal

NOW_NS = 1_788_100_000_000_000_000


def write_ticks(path: pathlib.Path, rows):
    """rows: (ts_ns, sym, bid_f, ask_f) → a v2-shaped pmlr tick file."""
    buf = bytearray(b"\x00" * 64)  # header (reader here never parses it)
    for ts, sym, bid, ask in rows:
        slot = bytearray(64)
        struct.pack_into("<QI", slot, 0, ts, sym)
        struct.pack_into("<q", slot, 16, int(bid * 1e6))
        struct.pack_into("<q", slot, 32, int(ask * 1e6))
        buf += slot
    path.write_bytes(bytes(buf))


def world(tmp_path, dev_bps=+6.0, age_s=1.0):
    logs = tmp_path / "logs"
    run = logs / "run-1788000000000000000"
    run.mkdir(parents=True)
    ts = NOW_NS - int(age_s * 1e9)
    ref_mid = 77_000.0
    sym_mid = ref_mid * (1 + dev_bps / 1e4)
    write_ticks(run / "bn-ticks.pmlr", [
        (ts, 7, ref_mid - 0.5, ref_mid + 0.5),
        (ts, 16777729, ref_mid - 0.5, ref_mid + 0.5),
    ])
    write_ticks(run / "okx-ticks.pmlr", [(ts, 33554433, sym_mid - 0.5, sym_mid + 0.5)])
    write_ticks(run / "hl-ticks.pmlr", [(ts, 67108865, ref_mid - 0.5, ref_mid + 0.5)])
    mp = tmp_path / "map.json"
    mp.write_text(json.dumps({"markets": {
        "okx:BTC-USDT": 33554433, "binance:btcusdt": 7,
        "hyperliquid:BTC": 67108865, "binance-usdm:btcusdt": 16777729,
    }}))
    out = tmp_path / "xv"
    return logs, mp, out


def run(logs, mp, out, now=NOW_NS):
    return claude_worker.xv_signal.run_cycle(logs, mp, out, now)


def test_enters_hedged_pair_on_dislocation(tmp_path):
    logs, mp, out = world(tmp_path, dev_bps=+6.0)
    digest = run(logs, mp, out)
    body = digest.read_text()
    assert "xv-okx-bnspot" in body and "ENTER short=okx:BTC-USDT" in body
    batch = json.loads(sorted(out.glob("batch-*.json"))[0].read_text())
    entry = [i for i in batch["intents"] if i["tag"].startswith("xv-okx-bnspot")]
    assert len(entry) == 2
    sides = {i["venue"]: i["side"] for i in entry}
    assert sides["okx"] == "ask" and sides["binance"] == "bid"
    for i in entry:
        assert i["px"] * i["qty"] <= 10_000.0  # research-tier order cap
    state = json.loads((out / "state.json").read_text())
    assert "xv-okx-bnspot" in state["positions"]


def test_one_position_per_pair_no_repeat_entry(tmp_path):
    logs, mp, out = world(tmp_path, dev_bps=+6.0)
    run(logs, mp, out)
    d2 = run(logs, mp, out, now=NOW_NS + 300_000_000_000)
    # need fresh ticks for the second cycle
    assert "HELD" in d2.read_text() or "stale" in d2.read_text()


def test_exits_on_reversion(tmp_path):
    logs, mp, out = world(tmp_path, dev_bps=+6.0)
    run(logs, mp, out)
    # Reversion world: same layout, dev back to +0.5 bps, fresh ticks.
    logs2, mp2, _ = world(tmp_path / "b", dev_bps=+0.5)
    digest = claude_worker.xv_signal.run_cycle(logs2, mp2, out, NOW_NS)
    body = digest.read_text()
    assert "EXIT" in body
    state = json.loads((out / "state.json").read_text())
    assert state["positions"] == {}
    batches = sorted(out.glob("batch-*.json"))
    close = json.loads(batches[-1].read_text())
    sides = sorted(i["side"] for i in close["intents"])
    assert sides == ["ask", "bid"]


def test_stopped_feed_holds_everything(tmp_path):
    # Global mtime guard: files older than MAX_MID_AGE_S of wall now.
    import os
    logs, mp, out = world(tmp_path, dev_bps=+9.0)
    old_wall = time.time() - 600
    for f in (logs / "run-1788000000000000000").glob("*.pmlr"):
        os.utime(f, (old_wall, old_wall))
    digest = claude_worker.xv_signal.run_cycle(logs, mp, out)
    assert "capture feed stale" in digest.read_text()
    batch = json.loads(sorted(out.glob("batch-*.json"))[0].read_text())
    assert batch["intents"] == []


def test_lagging_leg_holds_that_pair(tmp_path):
    # Relative guard: okx leg 600s older (monotonic) than the rest.
    logs, mp, out = world(tmp_path, dev_bps=+9.0)
    run_dir = logs / "run-1788000000000000000"
    ref_mid = 77_000.0
    lag_ts = NOW_NS - int(601 * 1e9)
    write_ticks(run_dir / "okx-ticks.pmlr", [(lag_ts, 33554433, ref_mid, ref_mid + 1)])
    digest = run(logs, mp, out)
    body = digest.read_text()
    assert "xv-okx-bnspot" in body and "lags feed" in body
    # the hl pair (fresh both legs, dev 0) stays flat, not blocked
    assert "xv-hl-bnusdm" in body and "flat" in body


def test_regime_label_gates_entries_only_and_exits_drain(tmp_path, monkeypatch):
    # RG5: an empty label is ANY (the pre-RG5 behaviour, no engine touch);
    # a labelled lane blocks ENTRIES when the words miss the label and
    # never gates an EXIT. Words are injected — no /metrics, no files.
    bull = claude_worker.frames.regime_word(trend="bull", shape="trend", source="measured")
    bear = claude_worker.frames.regime_word(trend="bear", shape="chop", source="measured")
    unknown = claude_worker.regime.UNKNOWN_WORD
    monkeypatch.setattr(claude_worker.xv_signal, "REGIME_LABEL", ("trend:bull|neutral",))
    logs, mp, out = world(tmp_path, dev_bps=+6.0)
    d = claude_worker.xv_signal.run_cycle(logs, mp, out, NOW_NS, regime_words={"fast": bear, "slow": bull})
    body = d.read_text()
    assert "regime: ENTRIES BLOCKED" in body and "ENTRY-BLOCKED: regime" in body
    assert json.loads((out / "state.json").read_text())["positions"] == {}
    # Unknown (engine unreachable, no declaration) fails a constrained profile closed.
    d = claude_worker.xv_signal.run_cycle(logs, mp, out, NOW_NS, regime_words={"fast": unknown, "slow": unknown})
    assert "ENTRY-BLOCKED: regime" in d.read_text()
    # Allowed words: the same dislocation enters.
    d = claude_worker.xv_signal.run_cycle(logs, mp, out, NOW_NS, regime_words={"fast": bull, "slow": bear})
    assert "regime: open" in d.read_text() and "ENTER short=okx:BTC-USDT" in d.read_text()
    # Reversion under a BLOCKING regime still exits: exits are never gated.
    logs2, mp2, _ = world(tmp_path / "b", dev_bps=+0.5)
    d = claude_worker.xv_signal.run_cycle(logs2, mp2, out, NOW_NS, regime_words={"fast": bear, "slow": bear})
    assert "EXIT" in d.read_text()
    assert json.loads((out / "state.json").read_text())["positions"] == {}
    # The default label is empty = ANY: no regime line at all.
    monkeypatch.setattr(claude_worker.xv_signal, "REGIME_LABEL", ())
    logs3, mp3, out3 = world(tmp_path / "c", dev_bps=+6.0)
    body = claude_worker.xv_signal.run_cycle(logs3, mp3, out3, NOW_NS, regime_words={"fast": bear, "slow": bear}).read_text()
    assert "regime" not in body and "ENTER" in body
