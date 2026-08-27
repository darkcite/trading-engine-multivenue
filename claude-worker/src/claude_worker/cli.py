# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Operator verb CLI (design §6) — thin Typer frontends over the library.

The §6 surface, verbatim: ``serve``, ``fetch``, ``backtest``, ``push``,
``positions``, ``stage-ruleset``, ``commit-ruleset`` — plus ``pnl``
(M4.3: the ONE additive verb the operator D1 ruling un-froze the
surface for; a THIN reader of the ``pnl_report`` module's files, no
socket, no engine spawn). Global exit codes::

    0 OK        2 usage/validation (bad args, bad file, schema)   3 GATE REFUSED
    4 transport (socket absent/busy/HMAC/protocol)                5 state (SQLite/seq)
    1 unexpected exception (fail-fast; traceback to stderr)

Dual-mode invariant (§5.2): verbs and ``serve`` call the *same* library
functions; verbs read ``BaseConfig`` only — ``ANTHROPIC_API_KEY`` is never
read outside ``serve`` (asserted by tests that run every verb with the key
unset). No override flag exists anywhere on this surface (asserted by the
``--help``-parse test).

Frame-sending verbs (``push``, ``stage-ruleset``, ``commit-ruleset``) open
the UDS connection and send the implicit Heartbeat first (``uds.py``
enforces heartbeat-precedes-payload in code). Read-only verbs (``fetch``,
``backtest``, ``positions``) never touch the socket — a data pull must not
signal AI liveness to the engine (§5.4; the ``positions`` row in §6 makes
this explicit).

Market map (S6 operator decision, S5 open question 2): the labeling
universe ``{market name -> SymbolId}`` and the HIP-4 ``(yes, no)`` pairs
live in one JSON file at ``CLAUDE_WORKER_MARKET_MAP``::

    {"markets": {"<name>": <sym>, ...}, "hip4_pairs": [[<yes>, <no>], ...]}

A missing file is a valid empty map (triage-only serve, no netting view);
a malformed file is a usage error (exit 2). As of 8h §6.2 the ``fetch``
verb OWNS the file's lifecycle (bootstrap + additive refresh via
``fetchers.refresh_market_map``); operator entries always win — the
reader here stays the shape contract.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import json
import pathlib
import sqlite3
import time
import typing

import httpx
import typer

import claude_worker.backtest
import claude_worker.config
import claude_worker.daemon
import claude_worker.features
import claude_worker.feeds
import claude_worker.fetchers
import claude_worker.frames
import claude_worker.pmlr
import claude_worker.pnl_report
import claude_worker.state
import claude_worker.uds

EXIT_OK: int = 0
EXIT_USAGE: int = 2
EXIT_GATE: int = 3
EXIT_TRANSPORT: int = 4
EXIT_STATE: int = 5

# Mirror of `core_types::MAX_STRATEGY_SLOTS` (u8 enable-mask ceiling).
_MAX_STRATEGY_SLOTS: int = 8
_MAX_PARAM_ID: int = 0xFFFF
_NS_PER_S: float = 1e9
_PX_QTY_SCALE: float = 1e6
_PAIR_LEN: int = 2  # a hip4_pairs entry is exactly [yes, no]

app: typer.Typer = typer.Typer(
    add_completion=False,
    pretty_exceptions_enable=False,
    help="AI-ingress worker: serve daemon + operator verbs (design §6).",
)


# ---- exit-code guard -----------------------------------------------------


def _guarded(fn: collections.abc.Callable[[], int]) -> typing.NoReturn:
    """Run one verb body and translate failures to the §6 exit codes.

    Anything not mapped here propagates — fail-fast, traceback to stderr,
    process exit 1 (``pretty_exceptions_enable=False`` keeps the raw
    traceback).
    """
    try:
        code = fn()
    except claude_worker.uds.UdsError as exc:
        typer.echo(f"transport: {exc}", err=True)
        raise typer.Exit(EXIT_TRANSPORT) from exc
    except claude_worker.backtest.GateRefused as exc:
        typer.echo(f"gate refused (final — no override exists): {exc}", err=True)
        raise typer.Exit(EXIT_GATE) from exc
    except (claude_worker.state.StateError, sqlite3.Error) as exc:
        typer.echo(f"state: {exc}", err=True)
        raise typer.Exit(EXIT_STATE) from exc
    except claude_worker.backtest.BacktestError as exc:
        typer.echo(f"backtest: {exc}", err=True)
        raise typer.Exit(EXIT_USAGE) from exc
    except (ValueError, OSError) as exc:
        typer.echo(f"error: {exc}", err=True)
        raise typer.Exit(EXIT_USAGE) from exc
    raise typer.Exit(code)


def _require(cond: bool, msg: str) -> None:
    """Validation helper: usage errors are ValueError -> exit 2."""
    if not cond:
        raise ValueError(msg)


# ---- market map (S6 operator decision) -----------------------------------


class MarketMap(typing.NamedTuple):
    """Operator-maintained mapping file contents (see module docstring)."""

    markets: dict[str, int]
    hip4_pairs: tuple[tuple[int, int], ...]


def _map_sym(value: object, where: str) -> int:
    _require(
        not isinstance(value, bool) and isinstance(value, int),
        f"market map: {where} must be an integer SymbolId, got {value!r}",
    )
    sym = typing.cast(int, value)
    _require(
        0 <= sym < claude_worker.frames.SYMBOL_ID_NONE,
        f"market map: {where} out of SymbolId range: {sym}",
    )
    return sym


def load_market_map(path: pathlib.Path) -> MarketMap:
    """Load and strictly validate the market-map file.

    Missing file -> empty map (documented degraded mode). Malformed file
    -> ValueError (exit 2): a half-readable operator mapping must never be
    silently partially applied.
    """
    if not path.exists():
        return MarketMap(markets={}, hip4_pairs=())
    try:
        raw = json.loads(path.read_text())
    except ValueError as exc:
        raise ValueError(f"market map {path}: invalid JSON: {exc}") from exc
    _require(isinstance(raw, dict), f"market map {path}: top level must be an object")
    obj = typing.cast(dict[str, object], raw)
    unknown = sorted(set(obj) - {"markets", "hip4_pairs"})
    _require(not unknown, f"market map {path}: unknown keys {unknown}")

    markets: dict[str, int] = {}
    markets_raw = obj.get("markets", {})
    _require(isinstance(markets_raw, dict), f"market map {path}: 'markets' must be an object")
    for name, value in typing.cast(dict[str, object], markets_raw).items():
        _require(bool(name), f"market map {path}: empty market name")
        markets[name] = _map_sym(value, f"markets[{name!r}]")

    pairs: list[tuple[int, int]] = []
    pairs_raw = obj.get("hip4_pairs", [])
    _require(isinstance(pairs_raw, list), f"market map {path}: 'hip4_pairs' must be an array")
    for i, entry in enumerate(typing.cast(list[object], pairs_raw)):
        _require(
            isinstance(entry, list) and len(typing.cast(list[object], entry)) == _PAIR_LEN,
            f"market map {path}: hip4_pairs[{i}] must be a [yes, no] pair",
        )
        yes_raw, no_raw = typing.cast(list[object], entry)
        yes = _map_sym(yes_raw, f"hip4_pairs[{i}][0]")
        no = _map_sym(no_raw, f"hip4_pairs[{i}][1]")
        _require(yes != no, f"market map {path}: hip4_pairs[{i}] legs must differ")
        pairs.append((yes, no))
    return MarketMap(markets=markets, hip4_pairs=tuple(pairs))


# ---- serve ---------------------------------------------------------------


@app.command()
def serve() -> None:
    """Run the full-auto daemon (the only mode that reads ANTHROPIC_API_KEY)."""

    def run() -> int:
        cfg = claude_worker.config.load_serve_from_env()
        market_map = load_market_map(cfg.market_map_path)
        return claude_worker.daemon.serve(cfg, symbol_map=market_map.markets)

    _guarded(run)


# ---- fetch ---------------------------------------------------------------


def _parse_symbols_csv(raw: str | None) -> set[int] | None:
    if raw is None:
        return None
    out: set[int] = set()
    for part in raw.split(","):
        item = part.strip()
        if not item:
            continue
        try:
            out.add(int(item))
        except ValueError as exc:
            raise ValueError(f"--symbols: {item!r} is not an integer SymbolId") from exc
    _require(bool(out), "--symbols: no SymbolIds given")
    return out


def _make_http_client() -> httpx.Client:
    """Seam for tests (monkeypatched to a MockTransport-backed client)."""
    return httpx.Client()


def _http_get(client: httpx.Client, url: str) -> str | None:
    """One best-effort GET for the §6.1 consumers (the ``feeds.fetch_feed``
    pattern): ``None`` = transport failure or non-200; callers count it."""
    try:
        response = client.get(url, timeout=claude_worker.fetchers.REST_TIMEOUT_S)
    except httpx.HTTPError:
        return None
    if response.status_code != httpx.codes.OK:
        return None
    return response.text


def _http_post(client: httpx.Client, url: str, body: str) -> str | None:
    """One best-effort JSON POST (HL ``/info`` candleSnapshot)."""
    try:
        response = client.post(
            url,
            content=body,
            headers={"Content-Type": "application/json"},
            timeout=claude_worker.fetchers.REST_TIMEOUT_S,
        )
    except httpx.HTTPError:
        return None
    if response.status_code != httpx.codes.OK:
        return None
    return response.text


def _fetch_news(cfg: claude_worker.config.BaseConfig) -> None:
    """One mechanical news poll (§6 ``fetch --news``): fetch the allowlist,
    dedupe via the shared SQLite, write items NDJSON — NO LLM steps; in
    semi-manual the session is the triage/labeling brain."""
    state = claude_worker.state.State(cfg.db_path)
    try:
        items: list[claude_worker.feeds.FeedItem] = []
        fetch_errors = 0
        http = _make_http_client()
        try:
            for url in cfg.rss_feeds:
                fetched = claude_worker.feeds.fetch_feed(http, url)
                if fetched is None:
                    fetch_errors += 1
                else:
                    items.extend(fetched)
        finally:
            http.close()
        fresh, dups = claude_worker.feeds.dedupe_items(state, items)
        out_dir = cfg.features_dir / "news"
        out_dir.mkdir(parents=True, exist_ok=True)
        path = out_dir / f"items-{time.time_ns()}.ndjson"
        lines: list[str] = []
        for item in fresh:
            lines.append(
                json.dumps(
                    {
                        "id": item.guid,
                        "feed": item.feed,
                        "ts": item.ts,
                        "title": item.title,
                        "link": item.link,
                        "text": item.text,
                    },
                    separators=(",", ":"),
                )
            )
        path.write_text(("\n".join(lines) + "\n") if lines else "")
        typer.echo(str(path))
        typer.echo(
            f"news: feeds={len(cfg.rss_feeds)} new={len(fresh)} dup={dups}"
            f" fetch_errors={fetch_errors}",
            err=True,
        )
    finally:
        state.close()


@app.command()
def fetch(
    replay_dir: typing.Annotated[
        pathlib.Path | None,
        typer.Option("--replay-dir", help="Override CLAUDE_WORKER_REPLAY_DIR."),
    ] = None,
    symbols: typing.Annotated[
        str | None,
        typer.Option("--symbols", help="CSV of SymbolIds: restrict written feature files."),
    ] = None,
    no_rest: typing.Annotated[
        bool,
        typer.Option("--no-rest", help="Skip the rate-budgeted venue REST secondary."),
    ] = False,
    news: typing.Annotated[
        bool,
        typer.Option("--news", help="Also run one mechanical news poll (no LLM steps)."),
    ] = False,
) -> None:
    """data_fetcher one-shot: replay logs -> feature files; prints paths."""

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        base_dir = cfg.replay_dir if replay_dir is None else replay_dir
        run_dir = claude_worker.features.latest_run_dir(base_dir)
        _require(run_dir is not None, f"no run-* dirs under {base_dir}")
        assert run_dir is not None  # narrowed by _require
        syms = _parse_symbols_csv(symbols)
        result = claude_worker.features.collect_run(run_dir, cfg.features_dir, syms=syms)
        for feature_path in result.feature_paths:
            typer.echo(str(feature_path))
        for torn_name in result.torn_files:
            typer.echo(f"torn tail (engine mid-flush): {torn_name}", err=True)
        # 8h §6: venue REST secondary + market-map ownership. (The 8g
        # "no venue URL consumers exist until 8h" deviation note retired
        # here.) The httpx client is constructed LAZILY on the first
        # actual request, so a fetch with no consumer targets — or with
        # --no-rest — never touches the seam at all.
        universe = claude_worker.fetchers.observed_universe(run_dir)
        market_map = load_market_map(cfg.market_map_path)
        holder: list[httpx.Client] = []

        def _client() -> httpx.Client:
            if not holder:
                holder.append(_make_http_client())
            return holder[0]

        def get_fn(url: str) -> str | None:
            return _http_get(_client(), url)

        def post_fn(url: str, body: str) -> str | None:
            return _http_post(_client(), url, body)

        try:
            report = claude_worker.fetchers.run_secondary(
                universe=universe,
                markets=market_map.markets,
                hip4_pairs=market_map.hip4_pairs,
                map_path=cfg.market_map_path,
                features_dir=cfg.features_dir,
                run_name=run_dir.name,
                no_rest=no_rest,
                get_fn=None if no_rest else get_fn,
                post_fn=None if no_rest else post_fn,
            )
        finally:
            if holder:
                holder[0].close()
        for rest_path in report.files:
            typer.echo(str(rest_path))
        for line in report.lines:
            typer.echo(line, err=True)
        if news:
            _fetch_news(cfg)
        return EXIT_OK

    _guarded(run)


# ---- backtest ------------------------------------------------------------


@app.command()
def backtest(
    ruleset: typing.Annotated[
        pathlib.Path,
        typer.Option("--ruleset", help="Ruleset JSON artifact to evaluate."),
    ],
    replay_dir: typing.Annotated[
        pathlib.Path | None,
        typer.Option("--replay-dir", help="Override CLAUDE_WORKER_REPLAY_DIR."),
    ] = None,
    split: typing.Annotated[
        str,
        typer.Option("--split", help="IS/OOS split passed to the harness."),
    ] = "70/30",
) -> None:
    """Run the backtest harness; write the report next to the ruleset.

    Exit 0 = gates passed; exit 3 = gate refused (report still written);
    exit 2 = the harness output could not be trusted at all.
    """

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        _require(ruleset.is_file(), f"--ruleset: no such file: {ruleset}")
        base_dir = cfg.replay_dir if replay_dir is None else replay_dir
        outcome = claude_worker.backtest.run_backtest(ruleset, base_dir, split=split)
        gates = outcome.gates
        typer.echo(str(outcome.report_path))
        typer.echo(
            f"gates: pnl_positive={gates.pnl_positive} min_trades={gates.min_trades}"
            f" min_days={gates.min_days} max_drawdown={gates.max_drawdown}"
            f" bounds={gates.bounds} -> {'PASS' if outcome.all_passed else 'FAIL'}"
        )
        return EXIT_OK if outcome.all_passed else EXIT_GATE

    _guarded(run)


# ---- push ----------------------------------------------------------------

_VENUES: dict[str, int] = {
    "polymarket": claude_worker.frames.VENUE_POLYMARKET,
    "binance": claude_worker.frames.VENUE_BINANCE,
    "okx": claude_worker.frames.VENUE_OKX,
    "deribit": claude_worker.frames.VENUE_DERIBIT,
    "hyperliquid": claude_worker.frames.VENUE_HYPERLIQUID,
    "ai": claude_worker.frames.VENUE_AI,
}
_SIDES: dict[str, int] = {
    "bid": claude_worker.frames.SIDE_BID,
    "ask": claude_worker.frames.SIDE_ASK,
}


class _KindSpec(typing.NamedTuple):
    kind: int
    required: frozenset[str]
    allowed: frozenset[str]


def _spec(kind: int, required: set[str], optional: set[str] | None = None) -> _KindSpec:
    extra: set[str] = set() if optional is None else optional
    return _KindSpec(kind=kind, required=frozenset(required), allowed=frozenset(required | extra))


# The §3 per-kind table as data: which CLI options each kind requires and
# which it may carry at all (anything else is refused before a seq is
# allocated — the engine's shape validator is authoritative, this is the
# operator-typo firewall). "sym" covers the --sym|--symbol pair.
_PUSH_KINDS: dict[str, _KindSpec] = {
    "heartbeat": _spec(claude_worker.frames.KIND_HEARTBEAT, set()),
    "enable": _spec(claude_worker.frames.KIND_ENABLE_STRATEGY, {"strategy"}),
    "disable": _spec(claude_worker.frames.KIND_DISABLE_STRATEGY, {"strategy"}),
    "set-fair-value": _spec(
        claude_worker.frames.KIND_SET_FAIR_VALUE,
        {"sym", "px", "ttl-s"},
        {"expire-on-silence"},
    ),
    "set-bias": _spec(
        claude_worker.frames.KIND_SET_BIAS,
        {"sym", "px", "ttl-s"},
        {"expire-on-silence"},
    ),
    "set-param": _spec(
        claude_worker.frames.KIND_SET_PARAM,
        {"strategy", "param-id", "px"},
        {"sym"},
    ),
    "order-intent": _spec(
        claude_worker.frames.KIND_ORDER_INTENT,
        {"sym", "venue", "side", "px", "qty", "ttl-s"},
    ),
    "halt": _spec(claude_worker.frames.KIND_HALT_REQUEST, set()),
}


class _Wire(typing.NamedTuple):
    """Validated, scaled §3 wire fields for one payload frame."""

    sym: int
    px: int
    qty: int
    ttl_ns: int
    kind: int
    venue: int
    strategy_id: int
    side: int
    param_id: int
    flags: int


def _scale_px_qty(value: float) -> int:
    return round(value * _PX_QTY_SCALE)


def _scale_ttl(ttl_s: float) -> int:
    ttl_ns = round(ttl_s * _NS_PER_S)
    # A TTL that rounds to 0 would mean "no expiry" on the wire — the
    # opposite of what a tiny TTL asks for. Refuse.
    _require(ttl_ns > 0, f"--ttl-s: must be > 0 (and >= 1 ns): {ttl_s}")
    return ttl_ns


def _resolve_sym(
    sym: int | None,
    symbol: str | None,
    markets: dict[str, int],
    map_path: pathlib.Path,
) -> int:
    _require(sym is None or symbol is None, "give exactly one of --sym / --symbol")
    if symbol is not None:
        resolved = markets.get(symbol)
        _require(
            resolved is not None,
            f"--symbol {symbol!r}: not in market map ({map_path})",
        )
        return typing.cast(int, resolved)
    assert sym is not None  # caller checked the group is provided
    _require(
        0 <= sym < claude_worker.frames.SYMBOL_ID_NONE,
        f"--sym out of SymbolId range: {sym}",
    )
    return sym


def _provided_options(  # noqa: PLR0913, PLR0917 — one parameter per §6 push option, deliberately
    sym: int | None,
    symbol: str | None,
    venue: str | None,
    strategy: int | None,
    side: str | None,
    px: float | None,
    qty: float | None,
    ttl_s: float | None,
    param_id: int | None,
    expire_on_silence: bool,
) -> set[str]:
    provided: set[str] = set()
    if sym is not None or symbol is not None:
        provided.add("sym")
    if venue is not None:
        provided.add("venue")
    if strategy is not None:
        provided.add("strategy")
    if side is not None:
        provided.add("side")
    if px is not None:
        provided.add("px")
    if qty is not None:
        provided.add("qty")
    if ttl_s is not None:
        provided.add("ttl-s")
    if param_id is not None:
        provided.add("param-id")
    if expire_on_silence:
        provided.add("expire-on-silence")
    return provided


def _push_wire(  # noqa: PLR0913 — one parameter per §6 push option, deliberately
    kind_name: str,
    *,
    sym: int | None,
    symbol: str | None,
    venue: str | None,
    strategy: int | None,
    side: str | None,
    px: float | None,
    qty: float | None,
    ttl_s: float | None,
    param_id: int | None,
    expire_on_silence: bool,
    market_map: MarketMap,
    map_path: pathlib.Path,
) -> _Wire:
    """The §3 per-kind required-argument mirror (§6): validate + scale
    BEFORE any state/socket work, so a bad invocation never burns a seq."""
    spec = _PUSH_KINDS.get(kind_name)
    _require(spec is not None, f"--kind must be one of {'|'.join(_PUSH_KINDS)}: got {kind_name!r}")
    assert spec is not None  # narrowed by _require
    provided = _provided_options(
        sym, symbol, venue, strategy, side, px, qty, ttl_s, param_id, expire_on_silence
    )
    missing = sorted(spec.required - provided)
    _require(not missing, f"--kind {kind_name}: missing required options: {missing}")
    forbidden = sorted(provided - spec.allowed)
    _require(not forbidden, f"--kind {kind_name}: options not accepted for this kind: {forbidden}")

    wire_sym = claude_worker.frames.SYMBOL_ID_NONE
    if "sym" in provided:
        wire_sym = _resolve_sym(sym, symbol, market_map.markets, map_path)

    wire_strategy = claude_worker.frames.STRATEGY_SLOT_NONE
    if strategy is not None:
        _require(
            0 <= strategy < _MAX_STRATEGY_SLOTS,
            f"--strategy must be 0..{_MAX_STRATEGY_SLOTS - 1}: {strategy}",
        )
        wire_strategy = strategy

    wire_param = 0
    if param_id is not None:
        _require(0 <= param_id <= _MAX_PARAM_ID, f"--param-id must be 0..{_MAX_PARAM_ID}")
        wire_param = param_id

    wire_px = 0
    if px is not None:
        if spec.kind == claude_worker.frames.KIND_SET_FAIR_VALUE:
            _require(px >= 0, f"--px: fair value must be >= 0: {px}")
        if spec.kind == claude_worker.frames.KIND_ORDER_INTENT:
            _require(px > 0, f"--px: intent price must be > 0: {px}")
        wire_px = _scale_px_qty(px)

    wire_qty = 0
    if qty is not None:
        _require(qty > 0, f"--qty must be > 0: {qty}")
        wire_qty = _scale_px_qty(qty)

    wire_ttl = 0 if ttl_s is None else _scale_ttl(ttl_s)

    wire_venue = claude_worker.frames.VENUE_AI
    if venue is not None:
        parsed = _VENUES.get(venue.lower())
        _require(parsed is not None, f"--venue must be one of {'|'.join(_VENUES)}: got {venue!r}")
        wire_venue = typing.cast(int, parsed)
        _require(
            wire_venue != claude_worker.frames.VENUE_AI,
            "--venue: order-intent targets a real market venue, never 'ai' (§3)",
        )

    wire_side = claude_worker.frames.SIDE_NONE
    if side is not None:
        parsed_side = _SIDES.get(side.lower())
        _require(parsed_side is not None, f"--side must be bid|ask: got {side!r}")
        wire_side = typing.cast(int, parsed_side)

    if spec.kind == claude_worker.frames.KIND_ORDER_INTENT:
        # §3: OrderIntent strategy_id is pinned to the ai-exec slot.
        wire_strategy = claude_worker.frames.STRATEGY_SLOT_AI_EXEC

    flags = claude_worker.frames.FLAG_EXPIRE_ON_SILENCE if expire_on_silence else 0

    return _Wire(
        sym=wire_sym,
        px=wire_px,
        qty=wire_qty,
        ttl_ns=wire_ttl,
        kind=spec.kind,
        venue=wire_venue,
        strategy_id=wire_strategy,
        side=wire_side,
        param_id=wire_param,
        flags=flags,
    )


@app.command()
def push(  # noqa: PLR0913, PLR0917 — one parameter per §6 push option, deliberately
    kind: typing.Annotated[
        str,
        typer.Option(
            "--kind",
            help="heartbeat|enable|disable|set-fair-value|set-bias|set-param|order-intent|halt",
        ),
    ],
    sym: typing.Annotated[int | None, typer.Option("--sym", help="Numeric SymbolId.")] = None,
    symbol: typing.Annotated[
        str | None, typer.Option("--symbol", help="Market name (resolved via the market map).")
    ] = None,
    venue: typing.Annotated[
        str | None, typer.Option("--venue", help="Target venue (order-intent only).")
    ] = None,
    strategy: typing.Annotated[
        int | None, typer.Option("--strategy", help="StrategySet slot index.")
    ] = None,
    side: typing.Annotated[str | None, typer.Option("--side", help="bid|ask.")] = None,
    px: typing.Annotated[
        float | None, typer.Option("--px", help="Price / value (scaled 1e6 at pack).")
    ] = None,
    qty: typing.Annotated[
        float | None, typer.Option("--qty", help="Quantity (scaled 1e6 at pack).")
    ] = None,
    ttl_s: typing.Annotated[
        float | None, typer.Option("--ttl-s", help="TTL in seconds (> 0).")
    ] = None,
    param_id: typing.Annotated[
        int | None, typer.Option("--param-id", help="SetParam selector.")
    ] = None,
    expire_on_silence: typing.Annotated[
        bool,
        typer.Option(
            "--expire-on-silence",
            help="Tie the entry to heartbeat liveness (set-fair-value / set-bias).",
        ),
    ] = False,
) -> None:
    """Send one frame (after the implicit Heartbeat; heartbeat kind sends
    exactly one frame — it is its own heartbeat)."""

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        market_map = load_market_map(cfg.market_map_path)
        wire = _push_wire(
            kind,
            sym=sym,
            symbol=symbol,
            venue=venue,
            strategy=strategy,
            side=side,
            px=px,
            qty=qty,
            ttl_s=ttl_s,
            param_id=param_id,
            expire_on_silence=expire_on_silence,
            market_map=market_map,
            map_path=cfg.market_map_path,
        )
        state = claude_worker.state.State(cfg.db_path)
        try:
            client = claude_worker.uds.UdsClient(
                cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state
            )
            client.connect()
            try:
                if wire.kind == claude_worker.frames.KIND_HEARTBEAT:
                    seq = client.send_heartbeat()
                else:
                    client.send_heartbeat()
                    seq = client.send_cmd(
                        sym=wire.sym,
                        px=wire.px,
                        qty=wire.qty,
                        ttl_ns=wire.ttl_ns,
                        kind=wire.kind,
                        venue=wire.venue,
                        strategy_id=wire.strategy_id,
                        side=wire.side,
                        param_id=wire.param_id,
                        flags=wire.flags,
                    )
            finally:
                client.close()
        finally:
            state.close()
        typer.echo(f"sent kind={kind} seq={seq}")
        return EXIT_OK

    _guarded(run)


# ---- positions -----------------------------------------------------------


def _resolve_run_dir(cfg: claude_worker.config.BaseConfig, run_dir: str | None) -> pathlib.Path:
    if run_dir is None or run_dir == "latest":
        resolved = claude_worker.features.latest_run_dir(cfg.replay_dir)
        _require(resolved is not None, f"no run-* dirs under {cfg.replay_dir}")
        return typing.cast(pathlib.Path, resolved)
    path = pathlib.Path(run_dir).expanduser()
    _require(path.is_dir(), f"--run-dir: no such directory: {path}")
    return path


def _collect_run_marks(run_dir: pathlib.Path) -> tuple[dict[int, int], list[str]]:
    """Latest tick marks from the CURRENT run dir (read-only; nothing is
    written — this is the ``positions`` view, not ``fetch``)."""
    marks: dict[int, int] = {}
    torn: list[str] = []
    for path in sorted(run_dir.glob("*-ticks.pmlr")):
        with claude_worker.pmlr.Reader(path) as reader:
            claude_worker.features.collect_marks(reader, into=marks)
            if reader.torn:
                torn.append(path.name)
    return marks, torn


def _positions_payload(
    run_dir: pathlib.Path,
    torn: tuple[bool, list[str]],  # (fills file torn, torn tick-file names)
    views: dict[int, "claude_worker.features.PositionView"],
    pair_views: list["claude_worker.features.Hip4PairView"],
    total: int,
) -> dict[str, object]:
    fills_torn, ticks_torn = torn
    to_usd = claude_worker.features.to_usd
    positions_out: list[dict[str, object]] = []
    for sym in sorted(views):
        view = views[sym]
        positions_out.append(
            {
                "sym": view.sym,
                "net_qty": view.net_qty / _PX_QTY_SCALE,
                "avg_px": view.avg_px / _PX_QTY_SCALE,
                "mark_px": view.mark_px / _PX_QTY_SCALE,
                "realized_usd": to_usd(view.realized),
                "unrealized_usd": to_usd(view.unrealized),
                "exposure_usd": to_usd(view.exposure),
            }
        )
    pairs_out: list[dict[str, object]] = []
    for pv in pair_views:
        pairs_out.append(
            {
                "yes_sym": pv.yes_sym,
                "no_sym": pv.no_sym,
                "net_qty": pv.net_qty / _PX_QTY_SCALE,
                "flattened_qty": pv.flattened_qty / _PX_QTY_SCALE,
                "exposure_usd": to_usd(pv.exposure),
            }
        )
    return {
        "run_dir": str(run_dir),
        "fills_torn": fills_torn,
        "ticks_torn": ticks_torn,
        "positions": positions_out,
        "hip4_pairs": pairs_out,
        "total_exposure_usd": to_usd(total),
    }


def _echo_positions_human(payload: dict[str, object]) -> None:
    typer.echo(f"run {payload['run_dir']}")
    if payload["fills_torn"]:
        typer.echo("fills: torn tail (engine mid-flush) — last partial record ignored", err=True)
    for name in typing.cast(list[str], payload["ticks_torn"]):
        typer.echo(f"ticks: torn tail (engine mid-flush): {name}", err=True)
    for pos in typing.cast(list[dict[str, object]], payload["positions"]):
        typer.echo(
            f"sym {pos['sym']}  net {pos['net_qty']:.6f}  avg {pos['avg_px']:.6f}"
            f"  mark {pos['mark_px']:.6f}  realized {pos['realized_usd']:.2f}"
            f"  unrealized {pos['unrealized_usd']:.2f}  exposure {pos['exposure_usd']:.2f}"
        )
    for pair in typing.cast(list[dict[str, object]], payload["hip4_pairs"]):
        typer.echo(
            f"hip4 {pair['yes_sym']}/{pair['no_sym']}  net {pair['net_qty']:.6f}"
            f"  flattened {pair['flattened_qty']:.6f}  exposure {pair['exposure_usd']:.2f}"
        )
    typer.echo(f"total exposure: {typing.cast(float, payload['total_exposure_usd']):.2f} USD")


@app.command()
def positions(
    run_dir: typing.Annotated[
        str | None,
        typer.Option("--run-dir", help="Run dir path, or 'latest' (the default)."),
    ] = None,
    json_out: typing.Annotated[
        bool, typer.Option("--json", help="Machine-readable output.")
    ] = False,
) -> None:
    """Read-only live-state view (§6): tail engine-fills.pmlr + latest
    tick marks from the CURRENT run dir. No UDS, no heartbeat, no seq —
    the engine is a producer, never a server."""

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        market_map = load_market_map(cfg.market_map_path)
        resolved = _resolve_run_dir(cfg, run_dir)
        fills, fills_torn = claude_worker.features.read_fills(resolved)
        reconstructed = claude_worker.features.reconstruct_positions(fills)
        marks, ticks_torn = _collect_run_marks(resolved)
        views = claude_worker.features.position_views(reconstructed, marks)
        pair_views = claude_worker.features.hip4_pair_views(views, market_map.hip4_pairs)
        total = claude_worker.features.total_exposure(views, pair_views)
        payload = _positions_payload(resolved, (fills_torn, ticks_torn), views, pair_views, total)
        if json_out:
            typer.echo(json.dumps(payload, indent=2, sort_keys=True))
        else:
            _echo_positions_human(payload)
        return EXIT_OK

    _guarded(run)


# ---- stage-ruleset / commit-ruleset --------------------------------------

_FULL_HASH_HEX_LEN: int = 64  # sha256 hex


def _parse_full_hash(raw: str) -> str:
    value = raw.lower()
    _require(
        len(value) == _FULL_HASH_HEX_LEN,
        f"--hash must be {_FULL_HASH_HEX_LEN} hex chars (full sha256)",
    )
    try:
        bytes.fromhex(value)
    except ValueError as exc:
        raise ValueError("--hash is not valid hex") from exc
    return value


@app.command("stage-ruleset")
def stage_ruleset(
    ruleset: typing.Annotated[
        pathlib.Path, typer.Option("--ruleset", help="Ruleset JSON artifact.")
    ],
    report: typing.Annotated[
        pathlib.Path, typer.Option("--report", help="Worker backtest report (R.report.json).")
    ],
    by: typing.Annotated[
        str,
        typer.Option("--by", help="Attribution: session (default for verbs) or auto."),
    ] = "session",
) -> None:
    """GATE BINDING SITE (§6): recompute sha256(R), require the report to
    match hash + schema + gates.all_passed; record the registry row; send
    RulesetStage{hash128}. Any mismatch is exit 3. NO OVERRIDE EXISTS."""

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        _require(by in ("session", "auto"), f"--by must be session|auto: got {by!r}")
        _require(ruleset.is_file(), f"--ruleset: no such file: {ruleset}")
        state = claude_worker.state.State(cfg.db_path)
        try:
            client = claude_worker.uds.UdsClient(
                cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state
            )
            client.connect()
            try:
                client.send_heartbeat()
                seq, full_hash = claude_worker.backtest.stage_ruleset(
                    state, client, ruleset, report, by
                )
            finally:
                client.close()
        finally:
            state.close()
        typer.echo(f"staged {full_hash}")
        typer.echo(f"sent kind=ruleset-stage seq={seq}")
        return EXIT_OK

    _guarded(run)


@app.command("commit-ruleset")
def commit_ruleset(
    hash_hex: typing.Annotated[
        str | None, typer.Option("--hash", help="Full sha256 hex (64 chars).")
    ] = None,
    ruleset: typing.Annotated[
        pathlib.Path | None,
        typer.Option("--ruleset", help="Ruleset file (hash recomputed from bytes)."),
    ] = None,
) -> None:
    """Send RulesetCommit for a STAGED, gates-passed hash (§6). An
    unstaged or gate-failed hash is exit 3."""

    def run() -> int:
        cfg = claude_worker.config.load_base_from_env()
        _require(
            (hash_hex is None) != (ruleset is None),
            "give exactly one of --hash / --ruleset",
        )
        if hash_hex is not None:
            full_hash = _parse_full_hash(hash_hex)
        else:
            assert ruleset is not None  # narrowed by the exactly-one check
            _require(ruleset.is_file(), f"--ruleset: no such file: {ruleset}")
            full_hash, _hash128 = claude_worker.backtest.ruleset_hashes(ruleset)
        state = claude_worker.state.State(cfg.db_path)
        try:
            client = claude_worker.uds.UdsClient(
                cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state
            )
            client.connect()
            try:
                client.send_heartbeat()
                seq = claude_worker.backtest.commit_ruleset(state, client, full_hash)
            finally:
                client.close()
        finally:
            state.close()
        typer.echo(f"committed {full_hash}")
        typer.echo(f"sent kind=ruleset-commit seq={seq}")
        return EXIT_OK

    _guarded(run)


# ---- pnl (M4.3 — the D1-unfrozen additive verb) --------------------------


@app.command()
def pnl(
    date: str = typer.Option(
        "", help="UTC day YYYY-MM-DD; default = the newest report on disk."
    ),
    json_out: bool = typer.Option(
        False, "--json", help="Print the raw report JSON instead of the summary."
    ),
) -> None:
    """Read the shadow-P&L report (thin: the report pair is produced by
    ``python -m claude_worker.pnl_report``; this verb only finds and
    prints it — no socket, no engine spawn, BaseConfig-free)."""

    def run() -> int:
        reports_dir = claude_worker.pnl_report.resolve_reports_dir()
        if date:
            json_path, _ = claude_worker.pnl_report.report_paths(reports_dir, date)
            _require(
                json_path.is_file(),
                f"no shadow-P&L report for {date} under {reports_dir} — run"
                " `python -m claude_worker.pnl_report` first",
            )
        else:
            latest = claude_worker.pnl_report.latest_report(reports_dir)
            _require(
                latest is not None,
                f"no shadow-P&L reports under {reports_dir} — run"
                " `python -m claude_worker.pnl_report` first",
            )
            json_path = typing.cast(pathlib.Path, latest)
        if json_out:
            typer.echo(json_path.read_text(encoding="utf-8").rstrip("\n"))
        else:
            summary_path = json_path.parent / (
                json_path.name.removesuffix(".json") + ".summary.txt"
            )
            if summary_path.is_file():
                typer.echo(summary_path.read_text(encoding="utf-8").rstrip("\n"))
            else:
                typer.echo(json_path.read_text(encoding="utf-8").rstrip("\n"))
        typer.echo(f"report: {json_path}", err=True)
        return EXIT_OK

    _guarded(run)


# ---- entry point ---------------------------------------------------------


def main() -> None:
    """[project.scripts] entry point."""
    app()
