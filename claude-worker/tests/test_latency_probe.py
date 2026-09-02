# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Offline tests for claude_worker.latency_probe — frame codec, venue
parsers, delay statistics. No network anywhere (the probe's network
paths are exercised by running the module on the deployment host, which
is the documented per-location calibration step)."""

import json
import os
import pathlib

import pytest

import claude_worker.latency_probe as lp

# --- RFC 6455 frame codec ---------------------------------------------------


@pytest.mark.parametrize("n", [0, 5, 125, 126, 300, 65535, 65536, 70000])
def test_frame_roundtrip_all_length_classes(n: int) -> None:
    payload = bytes(i & 0xFF for i in range(n))
    frame = lp.encode_frame(1, payload, b"\x01\x02\x03\x04")
    decoded = lp.decode_frame(frame)
    assert decoded is not None
    opcode, fin, got, used = decoded
    assert (opcode, fin, used) == (1, True, len(frame))
    assert got == payload


def test_decode_incomplete_frame_returns_none() -> None:
    frame = lp.encode_frame(1, b"hello world", b"\x00\x00\x00\x00")
    for cut in (0, 1, 3, len(frame) - 1):
        assert lp.decode_frame(frame[:cut]) is None


def test_decode_unmasked_server_frame() -> None:
    # server frames are unmasked: 0x81 (FIN|text), len 4, "pong"
    assert lp.decode_frame(b"\x81\x04pong") == (1, True, b"pong", 6)


# --- venue parsers ---------------------------------------------------------


def test_parse_binance_bookticker_and_aggtrade() -> None:
    bt = json.dumps({"stream": "btcusdt@bookTicker",
                     "data": {"e": "bookTicker", "u": 1, "s": "BTCUSDT", "b": "100.5", "B": "1",
                              "a": "100.7", "A": "2", "T": 1700000000123, "E": 1700000000125}})
    at = json.dumps({"stream": "btcusdt@aggTrade",
                     "data": {"e": "aggTrade", "E": 1700000000130, "s": "BTCUSDT", "p": "100.6",
                              "q": "0.1", "T": 1700000000128}})
    spot_bt = json.dumps({"stream": "btcusdt@bookTicker",
                          "data": {"u": 1, "s": "BTCUSDT", "b": "1", "B": "1", "a": "2", "A": "2"}})
    r = lp.parse_binance(bt)[0]
    assert (r["stream"], r["venue_ts_ms"], r["venue_ts2_ms"], r["bid"], r["ask"]) == (
        "bookTicker", 1700000000125.0, 1700000000123.0, 100.5, 100.7)
    r = lp.parse_binance(at)[0]
    assert (r["stream"], r["venue_ts_ms"], r["venue_ts2_ms"], r["px"]) == (
        "aggTrade", 1700000000130.0, 1700000000128.0, 100.6)
    r = lp.parse_binance(spot_bt)[0]
    assert r["venue_ts_ms"] is None and r["bid"] == 1.0
    assert lp.parse_binance(json.dumps({"result": None, "id": 1})) == []


def test_parse_okx_bbo_trades_and_pong() -> None:
    bbo = json.dumps({"arg": {"channel": "bbo-tbt", "instId": "BTC-USDT"},
                      "data": [{"asks": [["100.7", "1", "0", "1"]],
                                "bids": [["100.5", "2", "0", "1"]],
                                "ts": "1700000000111", "seqId": 5}]})
    tr = json.dumps({"arg": {"channel": "trades", "instId": "BTC-USDT"},
                     "data": [{"px": "100.6", "sz": "1", "side": "buy", "ts": "1700000000112"}]})
    r = lp.parse_okx(bbo)[0]
    assert (r["stream"], r["venue_ts_ms"], r["bid"], r["ask"]) == (
        "bbo-tbt", 1700000000111.0, 100.5, 100.7)
    r = lp.parse_okx(tr)[0]
    assert (r["stream"], r["venue_ts_ms"], r["px"]) == ("trades", 1700000000112.0, 100.6)
    assert lp.parse_okx("pong") == []
    assert lp.parse_okx(json.dumps({"event": "subscribe", "arg": {"channel": "bbo-tbt"}})) == []


def test_parse_bybit_book_and_trade() -> None:
    ob = json.dumps({"topic": "orderbook.1.BTCUSDT", "type": "delta", "ts": 1700000000200,
                     "data": {"s": "BTCUSDT", "b": [["100.5", "1"]], "a": [["100.7", "1"]],
                              "u": 1, "seq": 2},
                     "cts": 1700000000198})
    tr = json.dumps({"topic": "publicTrade.BTCUSDT", "ts": 1700000000201,
                     "data": [{"T": 1700000000199, "p": "100.6", "v": "1", "S": "Buy"}]})
    r = lp.parse_bybit(ob)[0]
    assert (r["venue_ts_ms"], r["venue_ts2_ms"], r["bid"], r["ask"]) == (
        1700000000200.0, 1700000000198.0, 100.5, 100.7)
    r = lp.parse_bybit(tr)[0]
    assert (r["stream"], r["venue_ts_ms"], r["venue_ts2_ms"], r["px"]) == (
        "publicTrade", 1700000000201.0, 1700000000199.0, 100.6)
    assert lp.parse_bybit(json.dumps({"op": "pong", "success": True})) == []


def test_parse_deribit_and_hyperliquid() -> None:
    q = json.dumps({"jsonrpc": "2.0", "method": "subscription",
                    "params": {"channel": "quote.BTC-PERPETUAL",
                               "data": {"timestamp": 1700000000300, "best_bid_price": 100.5,
                                        "best_ask_price": 100.7, "best_bid_amount": 1,
                                        "best_ask_amount": 1}}})
    t = json.dumps({"jsonrpc": "2.0", "method": "subscription",
                    "params": {"channel": "trades.BTC-PERPETUAL.raw",
                               "data": [{"timestamp": 1700000000301, "price": 100.6}]}})
    r = lp.parse_deribit(q)[0]
    assert (r["stream"], r["venue_ts_ms"], r["bid"], r["ask"]) == (
        "quote", 1700000000300.0, 100.5, 100.7)
    assert lp.parse_deribit(t)[0]["px"] == 100.6
    assert lp.parse_deribit(json.dumps({"jsonrpc": "2.0", "id": 1, "result": ["x"]})) == []
    hl = json.dumps({"channel": "l2Book",
                     "data": {"coin": "BTC", "time": 1700000000400,
                              "levels": [[{"px": "100.5", "sz": "1", "n": 1}],
                                         [{"px": "100.7", "sz": "1", "n": 1}]]}})
    r = lp.parse_hyperliquid(hl)[0]
    assert (r["stream"], r["venue_ts_ms"], r["bid"], r["ask"]) == (
        "l2Book", 1700000000400.0, 100.5, 100.7)
    assert lp.parse_hyperliquid(json.dumps({"channel": "pong"})) == []


def test_time_extractors() -> None:
    assert lp._t_binance(b'{"serverTime": 1700000000000}') == 1700000000000.0
    assert lp._t_okx(b'{"code":"0","data":[{"ts":"1700000000001"}]}') == 1700000000001.0
    bybit_body = b'{"result":{"timeSecond":"1700000000","timeNano":"1700000000002000000"}}'
    assert lp._t_bybit(bybit_body) == 1700000000002.0
    assert lp._t_deribit(b'{"jsonrpc":"2.0","result":1700000000003}') == 1700000000003.0
    assert lp._t_none(b"1700000000") is None


# --- statistics ------------------------------------------------------------


def test_percentiles_and_feed_delay_stats(tmp_path: pathlib.Path) -> None:
    p = lp.percentiles([5.0, 1.0, 3.0, 2.0, 4.0])
    assert (p["n"], p["min"], p["p50"], p["p90"], p["p99"]) == (5.0, 1.0, 3.0, 5.0, 5.0)
    assert lp.percentiles([]) == {}
    nd = tmp_path / "okx.ndjson"
    rows = []
    for i in range(10):
        venue_ts = 1_700_000_000_000 + i * 100
        # host receives 40 ms after the venue stamped it; host clock runs +7 ms ahead of the venue
        recv_ms = venue_ts + 40 + 7
        rows.append({"venue": "okx", "stream": "bbo-tbt", "venue_ts_ms": venue_ts,
                     "venue_ts2_ms": venue_ts - 5, "t_recv_wall_ns": recv_ms * lp.NS_PER_MS,
                     "t_recv_mono_ns": 0})
    with open(nd, "w", encoding="ascii") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    st = lp.feed_delay_stats(nd, offset_ms=-7.0)  # venue - host = -7 ms
    assert st["bbo-tbt"]["delay_ms"]["p50"] == pytest.approx(40.0)
    assert st["bbo-tbt"]["delay2_ms"]["p50"] == pytest.approx(45.0)
    assert lp.feed_delay_stats(tmp_path / "missing.ndjson", 0.0) == {}


def test_venue_table_is_the_engines_edge() -> None:
    names = [v.name for v in lp.VENUES]
    assert names == ["binance", "binance-usdm", "okx", "bybit", "deribit", "hyperliquid",
                     "polymarket"]
    hosts = {v.name: v.ws_url for v in lp.VENUES}
    assert "stream.binance.com" in hosts["binance"]
    assert "ws.okx.com" in hosts["okx"]
    assert "stream.bybit.com" in hosts["bybit"]
    assert hosts["polymarket"] == ""  # REST RTT only: the CLOB WS needs an asset id
    assert os.path.basename(lp.__file__) == "latency_probe.py"
