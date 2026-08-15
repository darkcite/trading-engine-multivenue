"""Operator verb CLI (design §6) — thin Typer frontends over the library.

The §6 surface, verbatim: ``serve``, ``fetch``, ``backtest``, ``push``,
``positions``, ``stage-ruleset``, ``commit-ruleset``. Global exit codes::

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
a malformed file is a usage error (exit 2).

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
import claude_worker.frames
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
        # REST secondary: the RestBudget mechanics are pinned (features.py)
        # but no venue URL consumers exist until 8h — with or without
        # --no-rest there is nothing to fetch yet (deviation note in the
        # progress log). The flag is part of the frozen §6 surface.
        if not no_rest:
            pass
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


# ---- entry point ---------------------------------------------------------


def main() -> None:
    """[project.scripts] entry point."""
    app()
