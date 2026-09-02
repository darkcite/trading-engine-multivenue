# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""latency_probe — venue feed-delay + order-path RTT calibration.

A standalone MODULE (``python -m claude_worker.latency_probe``), never a verb
and never in any engine path. It measures, from the host it runs on:

* **feed delay** per venue stream: ``t_recv_local - venue_timestamp -
  clock_offset`` for every market-data message the venue stamps (Binance
  USDM bookTicker ``E``/``T``, Binance aggTrade ``E``/``T``, OKX ``ts``,
  Bybit ``ts``/``cts``, Deribit ``timestamp``, Hyperliquid ``time``;
  Binance SPOT bookTicker carries no timestamp and is recorded for
  lead-lag only);
* **order-path RTT** proxy per venue: TCP connect, TLS handshake and the
  steady-state request round trip on a kept-alive HTTPS connection to the
  venue's REST edge (the same edge an order would take);
* **clock offset** venue - host from the venue's time endpoint with the
  RTT/2 correction, so feed delays are venue-clock-relative and the host's
  NTP error cancels.

Every message is written to ``<out>/<venue>.ndjson`` (venue, stream, local
wall + monotonic receive ns, venue timestamps, top of book / trade price)
so lead-lag can be recomputed offline in venue time; ``<out>/summary.json``
carries the per-venue statistics that feed the backtest harness's
activation-Δ table (``crates/cli/src/backtest.rs`` ``ModelParams``).

Why this exists (docs/venue-latency.md): the harness models an order as
active ``Δ_venue`` after emit and evaluates it against ticks in LOCAL
receive time, so Δ must be feed one-way + order one-way FOR THE HOST AND
NETWORK THE ENGINE RUNS ON. Those numbers are location facts, not
constants — rerun this for every deployment and every location.

Stdlib only (a minimal RFC 6455 client; no new dependency, no licence
surface change). Offline research/ops path — allocation is fine.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import base64
import json
import os
import pathlib
import socket
import ssl
import statistics
import struct
import sys
import threading
import time
import typing
import urllib.parse

MS_PER_S = 1000.0
NS_PER_MS = 1_000_000
_WS_GUID_KEY_BYTES = 16
_OP_CONT = 0x0
_OP_TEXT = 0x1
_OP_BIN = 0x2
_OP_CLOSE = 0x8
_OP_PING = 0x9
_OP_PONG = 0xA
_LEN_16 = 126
_LEN_64 = 127
_MAX_7BIT = 125
_MAX_16BIT = 0xFFFF
_HTTP_SWITCHING = 101
_STATUS_OK = 200
_RECV_CHUNK = 1 << 16
_DEFAULT_REST_SAMPLES = 20
_REST_SPACING_S = 0.25
_DEFAULT_REST_EVERY_S = 180.0
_DEFAULT_MINUTES = 20.0
_SOCKET_TIMEOUT_S = 30.0
_PERCENTILES = (0.5, 0.9, 0.99)
_FRAME_HEAD_BYTES = 2
_STATUS_LINE_MIN_PARTS = 2



def _mono_raw_ns() -> int:
    """The engine's capture clock (core-time: CLOCK_MONOTONIC_RAW), so probe
    receive times join engine tick timestamps by venue sequence number."""
    clk = getattr(time, "CLOCK_MONOTONIC_RAW", None)
    return time.clock_gettime_ns(clk) if clk is not None else time.monotonic_ns()


# ---------------------------------------------------------------------------
# Minimal RFC 6455 client (text frames; ping/pong; continuation)
# ---------------------------------------------------------------------------


def encode_frame(opcode: int, payload: bytes, mask_key: bytes) -> bytes:
    """Client→server frame: FIN set, masked (RFC 6455 §5.1 requires it)."""
    head = bytes([0x80 | opcode])
    n = len(payload)
    if n <= _MAX_7BIT:
        head += bytes([0x80 | n])
    elif n <= _MAX_16BIT:
        head += bytes([0x80 | _LEN_16]) + struct.pack("!H", n)
    else:
        head += bytes([0x80 | _LEN_64]) + struct.pack("!Q", n)
    masked = bytes(b ^ mask_key[i & 3] for i, b in enumerate(payload))
    return head + mask_key + masked


def decode_frame(buf: bytes) -> tuple[int, bool, bytes, int] | None:
    """Parse one server frame from ``buf``. Returns (opcode, fin, payload,
    consumed) or None when the buffer holds an incomplete frame."""
    if len(buf) < _FRAME_HEAD_BYTES:
        return None
    fin = bool(buf[0] & 0x80)
    opcode = buf[0] & 0x0F
    masked = bool(buf[1] & 0x80)
    n = buf[1] & 0x7F
    pos = 2
    if n == _LEN_16:
        if len(buf) < pos + 2:
            return None
        n = struct.unpack("!H", buf[pos : pos + 2])[0]
        pos += 2
    elif n == _LEN_64:
        if len(buf) < pos + 8:
            return None
        n = struct.unpack("!Q", buf[pos : pos + 8])[0]
        pos += 8
    key = b""
    if masked:
        if len(buf) < pos + 4:
            return None
        key = buf[pos : pos + 4]
        pos += 4
    if len(buf) < pos + n:
        return None
    payload = buf[pos : pos + n]
    if masked:
        payload = bytes(b ^ key[i & 3] for i, b in enumerate(payload))
    return opcode, fin, payload, pos + n


class WsClient:
    """One TLS WebSocket connection. ``recv_text`` answers pings itself."""

    def __init__(self, url: str, timeout_s: float = _SOCKET_TIMEOUT_S) -> None:
        u = urllib.parse.urlsplit(url)
        self.host = u.hostname or ""
        self.port = u.port or 443
        self.path = (u.path or "/") + (("?" + u.query) if u.query else "")
        self.timeout_s = timeout_s
        self.sock: ssl.SSLSocket | None = None
        self.buf = b""
        self.fragments: list[bytes] = []
        self.connect_ms = 0.0
        self.tls_ms = 0.0

    def connect(self) -> None:
        t0 = time.perf_counter()
        raw = socket.create_connection((self.host, self.port), timeout=self.timeout_s)
        t1 = time.perf_counter()
        ctx = ssl.create_default_context()
        self.sock = ctx.wrap_socket(raw, server_hostname=self.host)
        t2 = time.perf_counter()
        self.connect_ms = (t1 - t0) * MS_PER_S
        self.tls_ms = (t2 - t1) * MS_PER_S
        key = base64.b64encode(os.urandom(_WS_GUID_KEY_BYTES)).decode("ascii")
        req = (
            f"GET {self.path} HTTP/1.1\r\nHost: {self.host}\r\nUpgrade: websocket\r\n"
            f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
            "User-Agent: multivenue-latency-probe/1\r\n\r\n"
        )
        self.sock.sendall(req.encode("ascii"))
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(_RECV_CHUNK)
            if not chunk:
                raise ConnectionError(f"{self.host}: closed during handshake")
            head += chunk
        status_line, _, rest = head.partition(b"\r\n")
        parts = status_line.split(b" ")
        if len(parts) < _STATUS_LINE_MIN_PARTS or int(parts[1]) != _HTTP_SWITCHING:
            raise ConnectionError(f"{self.host}: handshake refused: {status_line!r}")
        _, _, self.buf = rest.partition(b"\r\n\r\n")

    def send_text(self, text: str) -> None:
        assert self.sock is not None
        self.sock.sendall(encode_frame(_OP_TEXT, text.encode("utf-8"), os.urandom(4)))

    def _send_control(self, opcode: int, payload: bytes) -> None:
        assert self.sock is not None
        self.sock.sendall(encode_frame(opcode, payload, os.urandom(4)))

    def recv_text(self) -> str | None:
        """Next complete text message, or None on close."""
        assert self.sock is not None
        while True:
            frame = decode_frame(self.buf)
            if frame is None:
                chunk = self.sock.recv(_RECV_CHUNK)
                if not chunk:
                    return None
                self.buf += chunk
                continue
            opcode, fin, payload, used = frame
            self.buf = self.buf[used:]
            if opcode == _OP_PING:
                self._send_control(_OP_PONG, payload)
            elif opcode == _OP_CLOSE:
                return None
            elif opcode in (_OP_TEXT, _OP_BIN, _OP_CONT):
                self.fragments.append(payload)
                if fin:
                    msg = b"".join(self.fragments)
                    self.fragments = []
                    return msg.decode("utf-8", errors="replace")

    def close(self) -> None:
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None


# ---------------------------------------------------------------------------
# Venue message parsers → records {stream, venue_ts_ms, venue_ts2_ms, bid, ask, px}
# ---------------------------------------------------------------------------


def _rec(stream: str, ts: float | None, ts2: float | None = None, **kw) -> dict:
    r: dict = {"stream": stream, "venue_ts_ms": ts, "venue_ts2_ms": ts2}
    r.update(kw)
    return r


def _f(v) -> float | None:
    return None if v is None else float(v)


def parse_binance(text: str) -> list[dict]:
    m = json.loads(text)
    d = m.get("data")
    if not isinstance(d, dict):
        return []
    stream = str(m.get("stream", ""))
    if stream.endswith("@bookTicker"):
        return [_rec("bookTicker", _f(d.get("E")), _f(d.get("T")),
                     bid=_f(d.get("b")), ask=_f(d.get("a")), seq=d.get("u"))]
    if stream.endswith("@aggTrade"):
        return [_rec("aggTrade", _f(d.get("E")), _f(d.get("T")), px=_f(d.get("p")),
                     seq=d.get("a"))]
    return []


def parse_okx(text: str) -> list[dict]:
    if text == "pong":
        return []
    m = json.loads(text)
    arg = m.get("arg") or {}
    channel = str(arg.get("channel", ""))
    out = []
    for d in m.get("data") or []:
        if channel == "bbo-tbt":
            bids = d.get("bids") or [[None]]
            asks = d.get("asks") or [[None]]
            out.append(_rec("bbo-tbt", _f(d.get("ts")), bid=_f(bids[0][0]), ask=_f(asks[0][0]),
                            seq=d.get("seqId")))
        elif channel == "trades":
            out.append(_rec("trades", _f(d.get("ts")), px=_f(d.get("px")),
                            seq=d.get("tradeId")))
    return out


def parse_bybit(text: str) -> list[dict]:
    m = json.loads(text)
    topic = str(m.get("topic", ""))
    ts = _f(m.get("ts"))
    if topic.startswith("orderbook.1."):
        d = m.get("data") or {}
        b = d.get("b") or []
        a = d.get("a") or []
        return [_rec("orderbook.1", ts, _f(m.get("cts")),
                     bid=_f(b[0][0]) if b else None, ask=_f(a[0][0]) if a else None,
                     seq=d.get("u"))]
    if topic.startswith("publicTrade."):
        return [_rec("publicTrade", ts, _f(t.get("T")), px=_f(t.get("p")))
                for t in m.get("data") or []]
    return []


def parse_deribit(text: str) -> list[dict]:
    m = json.loads(text)
    params = m.get("params") or {}
    channel = str(params.get("channel", ""))
    d = params.get("data")
    if channel.startswith("quote.") and isinstance(d, dict):
        return [_rec("quote", _f(d.get("timestamp")),
                     bid=_f(d.get("best_bid_price")), ask=_f(d.get("best_ask_price")))]
    if channel.startswith("trades.") and isinstance(d, list):
        return [_rec("trades", _f(t.get("timestamp")), px=_f(t.get("price"))) for t in d]
    return []


def parse_hyperliquid(text: str) -> list[dict]:
    m = json.loads(text)
    if m.get("channel") != "l2Book":
        return []
    d = m.get("data") or {}
    levels = d.get("levels") or [[], []]
    bid = _f(levels[0][0]["px"]) if levels[0] else None
    ask = _f(levels[1][0]["px"]) if len(levels) > 1 and levels[1] else None
    return [_rec("l2Book", _f(d.get("time")), bid=bid, ask=ask)]


# ---------------------------------------------------------------------------
# Venue table (WS endpoints = the engine's; REST = the venue's public edge)
# ---------------------------------------------------------------------------


class VenueSpec(typing.NamedTuple):
    name: str
    ws_url: str
    subscribe: str | None
    keepalive: str | None
    keepalive_s: float
    parse: typing.Callable[[str], list[dict]]
    rest_host: str
    rest_path: str
    rest_method: str
    rest_body: str | None
    time_ms_of: typing.Callable[[bytes], float | None]


def _t_binance(body: bytes) -> float | None:
    return _f(json.loads(body).get("serverTime"))


def _t_okx(body: bytes) -> float | None:
    data = json.loads(body).get("data") or []
    return _f(data[0].get("ts")) if data else None


def _t_bybit(body: bytes) -> float | None:
    r = json.loads(body).get("result") or {}
    nano = r.get("timeNano")
    if nano is None:
        return None
    n = int(nano)  # 1.7e18 exceeds float64's exact-integer range: split first
    return float(n // NS_PER_MS) + (n % NS_PER_MS) / NS_PER_MS


def _t_deribit(body: bytes) -> float | None:
    return _f(json.loads(body).get("result"))


def _t_none(_body: bytes) -> float | None:
    return None


VENUES: tuple[VenueSpec, ...] = (
    VenueSpec("binance",
              "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/btcusdt@aggTrade",
              None, None, 0.0, parse_binance,
              "api.binance.com", "/api/v3/time", "GET", None, _t_binance),
    VenueSpec("binance-usdm",
              "wss://fstream.binance.com/stream?streams=btcusdt@bookTicker/btcusdt@aggTrade",
              None, None, 0.0, parse_binance,
              "fapi.binance.com", "/fapi/v1/time", "GET", None, _t_binance),
    VenueSpec("okx", "wss://ws.okx.com:8443/ws/v5/public",
              json.dumps({"op": "subscribe", "args": [
                  {"channel": "bbo-tbt", "instId": "BTC-USDT"},
                  {"channel": "trades", "instId": "BTC-USDT"}]}),
              "ping", 20.0, parse_okx,
              "www.okx.com", "/api/v5/public/time", "GET", None, _t_okx),
    VenueSpec("bybit", "wss://stream.bybit.com/v5/public/spot",
              json.dumps({"op": "subscribe",
                          "args": ["orderbook.1.BTCUSDT", "publicTrade.BTCUSDT"]}),
              json.dumps({"op": "ping"}), 20.0, parse_bybit,
              "api.bybit.com", "/v5/market/time", "GET", None, _t_bybit),
    VenueSpec("deribit", "wss://www.deribit.com/ws/api/v2",
              json.dumps({"jsonrpc": "2.0", "id": 1, "method": "public/subscribe",
                          "params": {"channels": ["quote.BTC-PERPETUAL",
                                                  "trades.BTC-PERPETUAL.100ms"]}}),
              json.dumps({"jsonrpc": "2.0", "id": 2, "method": "public/test", "params": {}}), 25.0,
              parse_deribit,
              "www.deribit.com", "/api/v2/public/get_time", "GET", None, _t_deribit),
    VenueSpec("hyperliquid", "wss://api.hyperliquid.xyz/ws",
              json.dumps({"method": "subscribe",
                          "subscription": {"type": "l2Book", "coin": "BTC"}}),
              json.dumps({"method": "ping"}), 45.0, parse_hyperliquid,
              "api.hyperliquid.xyz", "/info", "POST", json.dumps({"type": "meta"}), _t_none),
    VenueSpec("polymarket", "", None, None, 0.0, parse_hyperliquid,
              "clob.polymarket.com", "/time", "GET", None, _t_none),
)


# ---------------------------------------------------------------------------
# REST probe: connect / TLS / kept-alive request RTT / clock offset
# ---------------------------------------------------------------------------


def _http_request(sock: ssl.SSLSocket, host: str, method: str, path: str,
                  body: str | None) -> tuple[float, float, int, bytes]:
    """One HTTP/1.1 request on a kept-alive TLS socket. Returns
    (first_byte_rtt_ms, full_ms, status, body)."""
    payload = body.encode("utf-8") if body else b""
    head = (f"{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: keep-alive\r\n"
            f"Accept: application/json\r\nUser-Agent: multivenue-latency-probe/1\r\n")
    if payload:
        head += f"Content-Type: application/json\r\nContent-Length: {len(payload)}\r\n"
    else:
        head += "Content-Length: 0\r\n"
    sock.sendall(head.encode("ascii") + b"\r\n" + payload)
    t0 = time.perf_counter()
    buf = b""
    t_first = 0.0
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(_RECV_CHUNK)
        if not chunk:
            raise ConnectionError(f"{host}: closed mid-response")
        if not t_first:
            t_first = time.perf_counter()
        buf += chunk
    head_bytes, _, rest = buf.partition(b"\r\n\r\n")
    lines = head_bytes.split(b"\r\n")
    status = int(lines[0].split(b" ")[1])
    length = 0
    chunked = False
    for ln in lines[1:]:
        k, _, v = ln.partition(b":")
        if k.strip().lower() == b"content-length":
            length = int(v.strip())
        if k.strip().lower() == b"transfer-encoding" and b"chunked" in v.lower():
            chunked = True
    if chunked:
        rest = _read_chunked(sock, rest)
    else:
        while len(rest) < length:
            chunk = sock.recv(_RECV_CHUNK)
            if not chunk:
                break
            rest += chunk
    t1 = time.perf_counter()
    return (t_first - t0) * MS_PER_S, (t1 - t0) * MS_PER_S, status, rest


def _read_chunked(sock: ssl.SSLSocket, buf: bytes) -> bytes:
    out = b""
    while True:
        while b"\r\n" not in buf:
            buf += sock.recv(_RECV_CHUNK)
        size_line, _, buf = buf.partition(b"\r\n")
        size = int(size_line.split(b";")[0], 16)
        if size == 0:
            return out
        while len(buf) < size + 2:
            buf += sock.recv(_RECV_CHUNK)
        out += buf[:size]
        buf = buf[size + 2 :]


def rest_probe(spec: VenueSpec, samples: int = _DEFAULT_REST_SAMPLES) -> dict:
    """DNS + TCP + TLS timings, then ``samples`` kept-alive requests; clock
    offset (venue - host, ms) from each timed response when the endpoint
    returns venue time."""
    t0 = time.perf_counter()
    addrs = socket.getaddrinfo(spec.rest_host, 443, type=socket.SOCK_STREAM)
    t1 = time.perf_counter()
    raw = socket.create_connection((spec.rest_host, 443), timeout=_SOCKET_TIMEOUT_S)
    t2 = time.perf_counter()
    ctx = ssl.create_default_context()
    sock = ctx.wrap_socket(raw, server_hostname=spec.rest_host)
    t3 = time.perf_counter()
    rtts: list[float] = []
    fulls: list[float] = []
    offsets: list[float] = []
    statuses: list[int] = []
    try:
        for _ in range(samples):
            t_send_wall_ms = time.time_ns() / NS_PER_MS
            rtt, full, status, body = _http_request(sock, spec.rest_host, spec.rest_method,
                                                    spec.rest_path, spec.rest_body)
            rtts.append(rtt)
            fulls.append(full)
            statuses.append(status)
            if status == _STATUS_OK:
                server_ms = spec.time_ms_of(body)
                if server_ms is not None:
                    offsets.append(server_ms - (t_send_wall_ms + rtt / 2.0))
            time.sleep(_REST_SPACING_S)
    finally:
        sock.close()
    return {
        "venue": spec.name,
        "host": spec.rest_host,
        "addr": addrs[0][4][0] if addrs else None,
        "dns_ms": (t1 - t0) * MS_PER_S,
        "tcp_connect_ms": (t2 - t1) * MS_PER_S,
        "tls_handshake_ms": (t3 - t2) * MS_PER_S,
        "rtt_ms": rtts,
        "full_ms": fulls,
        "status": statuses,
        "clock_offset_ms": offsets,
        "t_wall_ns": time.time_ns(),
    }


# ---------------------------------------------------------------------------
# WS collector thread
# ---------------------------------------------------------------------------


class Collector(threading.Thread):
    def __init__(self, spec: VenueSpec, out_dir: pathlib.Path, stop: threading.Event) -> None:
        super().__init__(name=f"probe-{spec.name}", daemon=True)
        self.spec = spec
        self.stop = stop
        self.path = out_dir / f"{spec.name}.ndjson"
        self.count = 0
        self.errors: list[str] = []
        self.connect_ms = 0.0
        self.tls_ms = 0.0

    def run(self) -> None:
        while not self.stop.is_set():
            try:
                self._session()
            except (OSError, ConnectionError, ValueError) as e:
                self.errors.append(f"{time.time():.0f} {e!r}")
                time.sleep(2.0)

    def _session(self) -> None:
        ws = WsClient(self.spec.ws_url)
        ws.connect()
        self.connect_ms = ws.connect_ms
        self.tls_ms = ws.tls_ms
        if self.spec.subscribe:
            ws.send_text(self.spec.subscribe)
        ws.sock.settimeout(5.0)  # type: ignore[union-attr]
        last_keepalive = time.monotonic()
        with open(self.path, "a", encoding="ascii") as f:
            while not self.stop.is_set():
                try:
                    text = ws.recv_text()
                except TimeoutError:
                    text = ""
                if text is None:
                    break
                t_wall = time.time_ns()
                t_mono = _mono_raw_ns()
                if text:
                    for r in self.spec.parse(text):
                        r["venue"] = self.spec.name
                        r["t_recv_wall_ns"] = t_wall
                        r["t_recv_mono_ns"] = t_mono
                        f.write(json.dumps(r, separators=(",", ":")) + "\n")
                        self.count += 1
                now = time.monotonic()
                if self.spec.keepalive and now - last_keepalive >= self.spec.keepalive_s:
                    ws.send_text(self.spec.keepalive)
                    last_keepalive = now
        ws.close()


# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------


def percentiles(values: list[float]) -> dict[str, float]:
    if not values:
        return {}
    s = sorted(values)
    n = len(s)
    out = {"n": float(n), "min": s[0], "mean": statistics.fmean(s)}
    for p in _PERCENTILES:
        out[f"p{int(p * 100)}"] = s[min(n - 1, int(p * n))]
    return out


def feed_delay_stats(ndjson_path: pathlib.Path, offset_ms: float) -> dict[str, dict]:
    """Per stream: (recv + offset) - venue_ts (ms), for ts and ts2, where
    offset = venue clock - host clock."""
    by_stream: dict[str, list[float]] = {}
    by_stream2: dict[str, list[float]] = {}
    if not ndjson_path.exists():
        return {}
    with open(ndjson_path, "r", encoding="ascii") as f:
        for line in f:
            r = json.loads(line)
            recv_ms = r["t_recv_wall_ns"] / NS_PER_MS
            ts = r.get("venue_ts_ms")
            ts2 = r.get("venue_ts2_ms")
            # venue clock = host clock + offset  =>  receive time on the venue's
            # clock is recv_ms + offset_ms; delay is that minus the venue stamp.
            if ts is not None:
                by_stream.setdefault(r["stream"], []).append(recv_ms + offset_ms - ts)
            if ts2 is not None:
                by_stream2.setdefault(r["stream"], []).append(recv_ms + offset_ms - ts2)
    out: dict[str, dict] = {}
    for k, v in by_stream.items():
        out[k] = {"delay_ms": percentiles(v)}
    for k, v in by_stream2.items():
        out.setdefault(k, {})["delay2_ms"] = percentiles(v)
    return out


def summarize(out_dir: pathlib.Path, rest_runs: list[dict], collectors: list[Collector]) -> dict:
    by_venue: dict[str, list[dict]] = {}
    for r in rest_runs:
        by_venue.setdefault(r["venue"], []).append(r)
    summary: dict = {"generated_wall_ns": time.time_ns(), "host": socket.gethostname(),
                     "venues": {}}
    for spec in VENUES:
        runs = by_venue.get(spec.name, [])
        rtts = [x for r in runs for x in r["rtt_ms"]]
        offs = [x for r in runs for x in r["clock_offset_ms"]]
        offset = statistics.median(offs) if offs else 0.0
        v: dict = {
            "rest_host": spec.rest_host,
            "rest_addr": runs[0]["addr"] if runs else None,
            "tcp_connect_ms": percentiles([r["tcp_connect_ms"] for r in runs]),
            "tls_handshake_ms": percentiles([r["tls_handshake_ms"] for r in runs]),
            "rest_rtt_ms": percentiles(rtts),
            "clock_offset_ms": percentiles(offs),
            "clock_offset_used_ms": offset,
            "offset_known": bool(offs),
        }
        col = next((c for c in collectors if c.spec.name == spec.name), None)
        if col is not None:
            v["ws_connect_ms"] = col.connect_ms
            v["ws_tls_ms"] = col.tls_ms
            v["ws_messages"] = col.count
            v["ws_errors"] = col.errors[-5:]
            v["streams"] = feed_delay_stats(col.path, offset)
        summary["venues"][spec.name] = v
    return summary


def render(summary: dict) -> str:
    lines = [f"{'venue':13} {'tcp':>6} {'tls':>6} {'rtt p50':>8} {'rtt p90':>8} {'offset':>8} "
             f"{'stream':14} {'feed p50':>9} {'feed p90':>9} {'feed p99':>9} {'n':>7}"]
    for name, v in summary["venues"].items():
        tcp = v["tcp_connect_ms"].get("p50", float("nan"))
        tls = v["tls_handshake_ms"].get("p50", float("nan"))
        r50 = v["rest_rtt_ms"].get("p50", float("nan"))
        r90 = v["rest_rtt_ms"].get("p90", float("nan"))
        off = v["clock_offset_used_ms"] if v["offset_known"] else float("nan")
        streams = v.get("streams") or {}
        if not streams:
            lines.append(f"{name:13} {tcp:6.1f} {tls:6.1f} {r50:8.1f} {r90:8.1f} {off:8.1f} "
                         f"{'-':14}")
            continue
        first = True
        for s, st in streams.items():
            for key in ("delay_ms", "delay2_ms"):
                d = st.get(key)
                if not d:
                    continue
                label = s + ("" if key == "delay_ms" else "/T")
                prefix = (f"{name:13} {tcp:6.1f} {tls:6.1f} {r50:8.1f} {r90:8.1f} {off:8.1f}"
                          if first else f"{'':13} {'':6} {'':6} {'':8} {'':8} {'':8}")
                first = False
                lines.append(f"{prefix} {label:14} {d['p50']:9.1f} {d['p90']:9.1f} "
                             f"{d['p99']:9.1f} {int(d['n']):7d}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Runner
# ---------------------------------------------------------------------------


def run(out_dir: pathlib.Path, minutes: float, rest_every_s: float, rest_samples: int,
        venues: tuple[VenueSpec, ...] = VENUES) -> dict:
    out_dir.mkdir(parents=True, exist_ok=True)
    stop = threading.Event()
    collectors = [Collector(s, out_dir, stop) for s in venues if s.ws_url]
    for c in collectors:
        c.start()
    rest_runs: list[dict] = []
    deadline = time.monotonic() + minutes * 60.0
    next_rest = time.monotonic()
    while time.monotonic() < deadline:
        if time.monotonic() >= next_rest:
            for s in venues:
                try:
                    rest_runs.append(rest_probe(s, rest_samples))
                except (OSError, ConnectionError, ValueError) as e:
                    print(f"rest {s.name}: {e!r}", file=sys.stderr)
            next_rest = time.monotonic() + rest_every_s
            with open(out_dir / "rest.json", "w", encoding="ascii") as f:
                json.dump(rest_runs, f)
        time.sleep(1.0)
    stop.set()
    for c in collectors:
        c.join(timeout=10.0)
    summary = summarize(out_dir, rest_runs, collectors)
    with open(out_dir / "summary.json", "w", encoding="ascii") as f:
        json.dump(summary, f, indent=1)
    return summary


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m claude_worker.latency_probe")
    p.add_argument("--out", required=True,
                   help="output directory (NDJSON per venue + summary.json)")
    p.add_argument("--minutes", type=float, default=_DEFAULT_MINUTES)
    p.add_argument("--rest-every-s", type=float, default=_DEFAULT_REST_EVERY_S)
    p.add_argument("--rest-samples", type=int, default=_DEFAULT_REST_SAMPLES)
    a = p.parse_args(argv)
    summary = run(pathlib.Path(os.path.expanduser(a.out)), a.minutes, a.rest_every_s,
                  a.rest_samples)
    print(render(summary))
    return 0


if __name__ == "__main__":
    sys.exit(main())
