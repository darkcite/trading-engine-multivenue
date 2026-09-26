# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Offline tests for claude_worker.latency_probe — frame codec, venue
parsers, delay statistics. No network anywhere (the probe's network
paths are exercised by running the module on the deployment host, which
is the documented per-location calibration step)."""

import json
import os
import pathlib
import threading
import typing

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


#: MX7, captured LIVE 2026-09-23 from wss://wbs-api.mexc.com/ws: one BINARY
#: `PushDataV3ApiWrapper` for `spot@public.aggre.bookTicker.v3.api.pb@10ms@
#: BTCUSDT` — f1 channel, f3 symbol, f6 sendTime, f315 the bookTicker body
#: (f1 bid, f2 bidQty, f3 ask, f4 askQty, f5 version, f6 varint).
MEXC_SPOT_FRAME = bytes.fromhex(
    "0a3373706f74407075626c69632e61676772652e626f6f6b5469636b65722e76332e6170"
    "692e70624031306d7340425443555344541a074254435553445430c78aefe78c34da133f"
    "0a0838363530332e3932120b31342e39313333353330381a0838363530332e3933220830"
    "2e3030343936352a0b383139393932373534343530c18aefe78c34"
)
#: ...and one JSON `push.depth.full` from wss://contract.mexc.com/edge (the
#: same minute; levels trimmed to two).
MEXC_PERP_FRAME = (
    '{"symbol":"BTC_USDT","data":{"cts":1790145447834,"asks":[[86469.5,398,1],'
    '[86469.6,143995,1]],"bids":[[86469.4,48926,6],[86468.2,14455,4]],'
    '"version":42029847745},"channel":"push.depth.full","ts":1790145447838}'
)


def test_parse_mexc_spot_protobuf_bookticker() -> None:
    r = lp.parse_mexc(MEXC_SPOT_FRAME)[0]
    assert (r["stream"], r["venue_ts_ms"], r["venue_ts2_ms"], r["bid"], r["ask"], r["seq"]) == (
        "aggre.bookTicker", 1790145447239.0, None, 86503.92, 86503.93, 81999275445)


def test_parse_mexc_futures_depth_full_json() -> None:
    r = lp.parse_mexc(MEXC_PERP_FRAME)[0]
    assert (r["stream"], r["venue_ts_ms"], r["venue_ts2_ms"], r["bid"], r["ask"], r["seq"]) == (
        "depth.full", 1790145447838.0, 1790145447834.0, 86469.4, 86469.5, 42029847745)


def test_parse_mexc_yields_nothing_for_acks_pongs_and_other_bodies() -> None:
    for text in ('{"id":0,"code":0,"msg":"spot@public.aggre.bookTicker.v3.api.pb@10ms@BTCUSDT"}',
                 '{"id":0,"code":0,"msg":"PONG"}',
                 '{"channel":"rs.sub.depth.full","data":"success","ts":1790145447754}',
                 '{"channel":"pong","data":1790145447754,"ts":1790145447754}',
                 "[]"):
        assert lp.parse_mexc(text) == [], text
    # A binary wrapper with no f315 body (another channel) is not a quote.
    assert lp.parse_mexc(MEXC_SPOT_FRAME[: MEXC_SPOT_FRAME.index(b"\xda\x13")]) == []


def test_pb_fields_is_an_order_agnostic_walk_with_hard_failures() -> None:
    # f6 varint 300, f3 "ab", a fixed64 and a fixed32 to skip — in that order.
    msg = b"\x30\xac\x02" + b"\x1a\x02ab" + b"\x09" + bytes(8) + b"\x15" + bytes(4)
    assert lp.pb_fields(msg) == {6: 300, 3: b"ab"}
    assert lp.pb_fields(b"\x1a\x02ab\x30\xac\x02") == {3: b"ab", 6: 300}
    for bad in (
        b"\x1a\x05ab",  # length runs past the buffer
        b"\x30\xac",  # truncated varint
        b"\x30" + b"\xff" * 10 + b"\x01",  # an 11-byte varint
        b"\x0b",  # wire type 3: a group, refused
        b"\x02\x00",  # field number 0
        b"\x09" + bytes(7),  # truncated fixed64
    ):
        with pytest.raises(ValueError):
            lp.pb_fields(bad)
    with pytest.raises(ValueError):
        lp.parse_mexc(MEXC_SPOT_FRAME[:-3])  # a torn frame never parses


def test_recv_message_keeps_binary_messages_raw() -> None:
    ws = lp.WsClient("wss://probe.example/ws")
    ws.sock = typing.cast(typing.Any, object())  # buffered frames only: never read
    body = b"\x0a\x01\xff\xfe"  # not UTF-8: a decode would destroy it
    ws.buf = (b"\x82\x04" + body  # FIN | binary
              + b"\x02\x02" + body[:2] + b"\x80\x02" + body[2:]  # binary, then continuation
              + b"\x81\x04pong")  # FIN | text
    assert ws.recv_message() == body
    assert ws.recv_message() == body
    assert ws.recv_message() == "pong"


def test_time_extractors() -> None:
    assert lp._t_binance(b'{"serverTime": 1700000000000}') == 1700000000000.0
    assert lp._t_okx(b'{"code":"0","data":[{"ts":"1700000000001"}]}') == 1700000000001.0
    bybit_body = b'{"result":{"timeSecond":"1700000000","timeNano":"1700000000002000000"}}'
    assert lp._t_bybit(bybit_body) == 1700000000002.0
    assert lp._t_deribit(b'{"jsonrpc":"2.0","result":1700000000003}') == 1700000000003.0
    # MX7, live bodies 2026-09-23: futures /api/v1/contract/ping; the spot
    # host's /api/v3/time is the Binance body.
    assert lp._t_mexc(b'{"success":true,"code":0,"data":1790145267966}') == 1790145267966.0
    assert lp._t_binance(b'{"serverTime":1790145267773}') == 1790145267773.0
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
                     "mexc", "mexc-perp", "polymarket", "hypercall"]
    hosts = {v.name: v.ws_url for v in lp.VENUES}
    assert "stream.binance.com" in hosts["binance"]
    assert "ws.okx.com" in hosts["okx"]
    assert "stream.bybit.com" in hosts["bybit"]
    # MX7: two classes, two hosts on BOTH planes -> two rows.
    assert hosts["mexc"] == "wss://wbs-api.mexc.com/ws"
    assert hosts["mexc-perp"] == "wss://contract.mexc.com/edge"
    rest = {v.name: v.rest_host for v in lp.VENUES}
    assert (rest["mexc"], rest["mexc-perp"]) == ("api.mexc.com", "contract.mexc.com")
    assert hosts["polymarket"] == ""  # REST RTT only: the CLOB WS needs an asset id
    # HC0: one host on both planes; no REST time endpoint -> the in-band
    # ClockSync keepalive carries the offset, its nonce re-rendered per send.
    assert hosts["hypercall"] == "wss://api.hypercall.xyz/ws"
    assert rest["hypercall"] == "api.hypercall.xyz"
    hc = next(v for v in lp.VENUES if v.name == "hypercall")
    assert hc.time_ms_of is lp._t_none and hc.subscribe_fn is not None
    assert hc.keepalive is not None and lp._NONCE_TOKEN in hc.keepalive
    assert os.path.basename(lp.__file__) == "latency_probe.py"


# --- HC0: Hypercall -----------------------------------------------------------

# Captured 2026-09-25 19:04-20:10Z (plan §1.3); the Trade body is the docs
# example (no Trade arrived in any probe window).
_HC_QUOTE = (
    '{"type":"IndicativeMarketData","instrument":"SNDK-20260927-1948-P","best_bid":"165.1175",'
    '"best_ask":"182.6095","indicative_bid_size":"11.568993","indicative_ask_size":"11.568993",'
    '"num_providers":1,"rfq_provider_quotes":[{"wallet":"0xe55b0000000000000000000000000000e86e",'
    '"bid_price":"165.1175","ask_price":"182.6095","max_bid_size":"11.568993",'
    '"max_ask_size":"11.568993","updated_at":1790363969110}],"published_at":1790363969125,'
    '"timestamp":1790363969110}')
_HC_INDEX = (
    '{"type":"IndexPriceUpdate","prices":[{"underlying":"AAPL","price":"340.3",'
    '"timestamp":1790363640978},{"underlying":"BTC","price":"109330","timestamp":1790363641100}],'
    '"timestamp":1790363640978}')


def test_parse_hypercall_quote_index_trade_clock_and_listing() -> None:
    (q,) = lp.parse_hypercall(_HC_QUOTE)
    assert (q["stream"], q["venue_ts_ms"], q["venue_ts2_ms"]) == (
        "indicative", 1790363969125.0, 1790363969110.0)
    assert (q["bid"], q["ask"], q["sym"], q["providers"]) == (
        165.1175, 182.6095, "SNDK-20260927-1948-P", 1)
    (ix,) = lp.parse_hypercall(_HC_INDEX)
    # ts = the NEWEST entry, ts2 = the frame's stamp (the OLDEST source).
    assert (ix["stream"], ix["venue_ts_ms"], ix["venue_ts2_ms"], ix["n"]) == (
        "index", 1790363641100.0, 1790363640978.0, 2)
    (tr,) = lp.parse_hypercall('{"type":"Trade","symbol":"BTC-20261002-100000-C",'
                               '"price":"0.0523","size":"5.0","side":"buy",'
                               '"timestamp":1737331200000}')
    assert (tr["stream"], tr["venue_ts_ms"], tr["px"], tr["side"]) == (
        "trade", 1737331200000.0, 0.0523, "buy")
    (cs,) = lp.parse_hypercall('{"type":"ClockSynced","nonce":"1790363641000000000",'
                               '"server_at":1790363641341}')
    assert (cs["stream"], cs["venue_ts_ms"], cs["sent_wall_ns"]) == (
        "clocksync", 1790363641341.0, 1790363641000000000)
    (mu,) = lp.parse_hypercall('{"type":"MarketUpdate","action":"Created","symbol":'
                               '"MU-20261002-1080-P","strike":"1080","is_call":false,'
                               '"underlying":"MU","expiry":1790971200,"timestamp":1790363000000}')
    assert (mu["stream"], mu["action"], mu["sym"]) == (
        "market_update", "Created", "MU-20261002-1080-P")
    # A one-sided quote (best_bid null) is legal and keeps its stamps.
    (one,) = lp.parse_hypercall('{"type":"IndicativeMarketData","instrument":"X","best_bid":null,'
                                '"best_ask":"1.5","num_providers":1,"published_at":2,'
                                '"timestamp":1}')
    assert (one["bid"], one["ask"], one["venue_ts_ms"]) == (None, 1.5, 2.0)
    for other in ('{"type":"Subscribed","channel":"index_prices"}',
                  '{"type":"Error","message":"bad"}', '[1,2]', b"\x00\x01"):
        assert lp.parse_hypercall(other) == []
    # A foreign nonce (not our wall-clock ns) is kept as a stamp without a send time.
    (foreign,) = lp.parse_hypercall('{"type":"ClockSynced","nonce":"probe-1","server_at":5}')
    assert foreign["sent_wall_ns"] is None


def _hc_row(name: str, expiry_ms: int, und: float) -> dict:
    return {"instrument_name": name, "expiration_timestamp": expiry_ms, "underlying_price": und}


def test_hypercall_pick_symbols_skips_the_blackout_and_takes_the_nearest_strikes() -> None:
    now = 1_790_000_000_000
    soon = now + 60 * 60 * 1000          # inside the 2 h pre-expiry blackout
    near = now + 20 * 60 * 60 * 1000     # the nearest quotable expiry
    far = now + 7 * 24 * 60 * 60 * 1000
    rows = [
        _hc_row("BOT-20260925-2.5-C", soon, 2.6),
        _hc_row("BOT-20260926-2.5-C", near, 2.6),
        _hc_row("BOT-20260926-2.5-P", near, 2.6),
        _hc_row("BOT-20260926-3-C", near, 2.6),
        _hc_row("BOT-20260926-1-P", near, 2.6),
        _hc_row("BOT-20261003-2.6-C", far, 2.6),
        {"instrument_name": "garbage", "expiration_timestamp": near, "underlying_price": 2.6},
        {"instrument_name": "BOT-20260926-2-C", "expiration_timestamp": None,
         "underlying_price": 2.6},
    ]
    picked = lp.hypercall_pick_symbols(rows, now, per_underlying=3)
    assert picked == ["BOT-20260926-2.5-C", "BOT-20260926-2.5-P", "BOT-20260926-3-C"]
    assert lp.hypercall_pick_symbols(rows[:1], now) == []


def test_ws_clock_offsets_use_the_midpoint_rule(tmp_path: pathlib.Path) -> None:
    nd = tmp_path / "hypercall.ndjson"
    sent = 1_790_000_000_000 * lp.NS_PER_MS
    rows = [
        # sent at T, received at T+40 ms, venue stamped T+20+7 -> offset +7 ms
        {"stream": "clocksync", "venue_ts_ms": 1_790_000_000_027.0, "sent_wall_ns": sent,
         "t_recv_wall_ns": sent + 40 * lp.NS_PER_MS},
        {"stream": "clocksync", "venue_ts_ms": 5.0, "sent_wall_ns": None,
         "t_recv_wall_ns": sent},
        {"stream": "indicative", "venue_ts_ms": 1.0, "t_recv_wall_ns": sent},
    ]
    with open(nd, "w", encoding="ascii") as f:
        for r in rows:
            f.write(json.dumps(r) + "\n")
    assert lp.ws_clock_offsets(nd) == [pytest.approx(7.0)]
    assert lp.ws_clock_offsets(tmp_path / "missing.ndjson") == []


def test_raw_kind_buckets_by_the_first_type_value() -> None:
    assert lp.raw_kind(_HC_QUOTE) == "IndicativeMarketData"
    assert lp.raw_kind('{"type" : "Subscribed","channel":"trades"}') == "Subscribed"
    assert lp.raw_kind('{"stream":"btcusdt@aggTrade"}') == "other"
    assert lp.raw_kind('{"type":5}') == "other"
    assert lp.raw_kind(b"\x0a\x01") == "binary"


def test_select_venues_filters_in_table_order_and_refuses_unknown_names() -> None:
    assert lp.select_venues("") == lp.VENUES
    assert [v.name for v in lp.select_venues("hypercall, okx")] == ["okx", "hypercall"]
    with pytest.raises(ValueError, match="nope"):
        lp.select_venues("okx,nope")


def test_collector_renders_a_fresh_nonce_per_keepalive(tmp_path: pathlib.Path) -> None:
    hc = next(v for v in lp.VENUES if v.name == "hypercall")
    col = lp.Collector(hc, tmp_path, threading.Event(), raw_per_kind=1)
    first = json.loads(col._keepalive_text())
    assert first["type"] == "ClockSync" and first["nonce"].isdigit()
    assert lp._NONCE_TOKEN not in col._keepalive_text()
    # Golden-frame tap: the first N per kind, verbatim; binary never.
    with open(col.raw_path, "w", encoding="utf-8") as raw:
        col._tap(raw, _HC_QUOTE, 1)
        col._tap(raw, _HC_QUOTE, 2)
        col._tap(raw, _HC_INDEX, 3)
        col._tap(raw, b"\x00", 4)
    lines = [json.loads(x) for x in col.raw_path.read_text(encoding="utf-8").splitlines()]
    assert [(x["kind"], x["t_recv_wall_ns"]) for x in lines] == [
        ("IndicativeMarketData", 1), ("IndexPriceUpdate", 3)]
    assert lines[0]["msg"] == _HC_QUOTE
    assert col.raw_counts == {"IndicativeMarketData": 1, "IndexPriceUpdate": 1}
