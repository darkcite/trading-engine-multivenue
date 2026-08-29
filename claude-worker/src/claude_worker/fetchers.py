# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Venue REST consumers (design §6.1) + market-map ownership (§6.2).

The four public, keyless consumers deferred from 8g §15 (H-D5 LOCKED):

- **PM Gamma markets** — ``GET /markets?clob_token_ids=<id>`` /
  ``?slug=<slug>`` on ``POLYMARKET_GAMMA_HOST``: names + metadata for the
  market map + per-sym metadata feature headers.
- **OKX candles** — ``GET /api/v5/market/candles`` on ``OKX_REST_HOST``.
- **Deribit chart** — ``GET /api/v2/public/get_tradingview_chart_data``
  on ``DERIBIT_REST_HOST``.
- **HL candleSnapshot** — ``POST /info {"type": "candleSnapshot"}`` on
  ``HYPERLIQUID_API_HOST``.

Module doctrine (features.py precedent, preserved): this module **never
imports an HTTP client** — callers inject ``get_fn(url)`` /
``post_fn(url, body)`` (httpx lives with the consumer, ``cli.py``). Every
call is gated by the pinned :class:`claude_worker.features.RestBudget`
(default 60 req/venue/h, env ``CLAUDE_WORKER_REST_BUDGET_PER_H``);
exhaustion is a counted skip, never a wait. Parsers follow the
``labeling.py`` strictness precedent: a malformed response (or row) is a
logged skip, never a crash — REST is best-effort enrichment; the replay
log stays the primary data source.

Market-map ownership (H-D7 LOCKED, §6.2): the observed universe comes
from the LATEST run's tick capture; names resolve via Gamma (PM) and the
``<venue>:<instrument>`` convention (CEX). Bootstrap writes the complete
``{"markets": {...}, "hip4_pairs": [...]}`` shape; refresh is ADDITIVE
(operator entries are never deleted or overwritten; conflicts are
reported and left alone); every write is atomic (same-dir temp file +
``os.replace``). The read side is ``cli.load_market_map`` — its shape is
the write contract and it stays untouched.

Recorded H3 interpretations (progress log carries the full list):

- Instrument DESCRIPTORS come from the market-map names themselves: a
  PM entry whose name is an all-digit token id (≥ ``PM_TOKEN_RUN_MIN``
  digits, the Rust discovery mirror) or a slug seeds the Gamma consumer;
  ``okx:<instId>`` / ``deribit:<instrument>`` / ``hyperliquid:<coin>``
  names drive the candle consumers. The engine's boot flags (the true
  ordinal authority — ``paper.rs``: "ordinals follow flag order") are
  not visible to the worker, so unseeded syms are REPORTED unresolved
  rather than guessed — except the one clap-default mirror below.
- ``_ENGINE_DEFAULT_NAMES`` mirrors the bin's clap defaults
  (``--binance-symbol btcusdt`` / ``--binance-sym-id 7``): an observed
  Binance-venue sym 7 resolves to ``binance:btcusdt`` with zero REST. A
  non-default boot surfaces as a §6.2 conflict — exactly the §14
  SymbolId-stability caveat's visibility (recorded design, not fixed).
- ``CLAUDE_WORKER_REST_BUDGET_PER_H`` is read here, not in
  ``BaseConfig``: the frozen 202 construct ``ServeConfig`` directly
  (``test_llm``/``test_daemon``), so the dataclass field tuple is a
  frozen surface; an env read at the fetch seam is additive.
- Venue REST hosts reuse the ENGINE'S existing ``.env`` keys
  (``POLYMARKET_GAMMA_HOST`` etc.) with the same defaults — no new keys
  beyond the three §7.5 ones.
- HIP-4 ``(yes, no)`` pairs: none live today (design §6.2); operator
  pairs are preserved verbatim and nothing is fabricated — the HL
  outcome-metadata derivation lands when outcome coins exist.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import json
import os
import pathlib
import re
import time
import tomllib
import typing

import claude_worker.features
import claude_worker.frames
import claude_worker.pmlr

# ---- budget / env -------------------------------------------------------

REST_BUDGET_ENV: str = "CLAUDE_WORKER_REST_BUDGET_PER_H"
REST_BUDGET_PER_H_DEFAULT: int = 60
BUDGET_WINDOW_NS: int = 3_600_000_000_000  # one hour (fixed window, §6.1)
REST_TIMEOUT_S: float = 10.0  # consumed by the cli-side get/post wrappers

# Venue REST hosts: the engine's existing .env keys, same defaults
# (crates/core-config) — deliberately no new host keys.
PM_GAMMA_HOST_ENV: str = "POLYMARKET_GAMMA_HOST"
PM_GAMMA_HOST_DEFAULT: str = "gamma-api.polymarket.com"
OKX_REST_HOST_ENV: str = "OKX_REST_HOST"
OKX_REST_HOST_DEFAULT: str = "www.okx.com"
DERIBIT_REST_HOST_ENV: str = "DERIBIT_REST_HOST"
DERIBIT_REST_HOST_DEFAULT: str = "www.deribit.com"
HL_API_HOST_ENV: str = "HYPERLIQUID_API_HOST"
HL_API_HOST_DEFAULT: str = "api.hyperliquid.xyz"

# Candle shape (warm-up windows for the strategist, §6.1).
CANDLE_INTERVAL: str = "1m"
DERIBIT_RESOLUTION: str = "1"  # minutes, Deribit's TradingView grammar
CANDLE_WINDOW_MS: int = 3_600_000  # one hour of 1m candles
OKX_CANDLE_LIMIT: int = 60

# Rust `ingress-polymarket` discovery mirrors (discovery.rs).
PM_TOKEN_RUN_MIN: int = 10
PM_TOKEN_MAX: int = 80

# M1 universe-file seeding (docs/mvp-progress.md M1d). Read at the
# fetch seam — the BaseConfig field tuple is frozen (H3 precedent).
UNIVERSE_FILE_ENV: str = "CLAUDE_WORKER_UNIVERSE_FILE"
# Rust `core-config::universe` allocation-law mirrors: PM token[0]
# takes the legacy anchor; later tokens take flat namespaced ordinals
# (VenueId::Polymarket is 0, so `make_symbol_id(PM, i+1)` == i+1).
# Binance spot[0] takes anchor 7; every other instrument takes
# `venue_byte << 24 | ordinal`, usdm ordinals from base 512.
PM_LEGACY_ANCHOR_SYM: int = 42
BN_LEGACY_ANCHOR_SYM: int = 7
SYMBOL_VENUE_SHIFT: int = 24
BN_USDM_ORDINAL_BASE: int = 512
# WS5: `[binance] usdm_dated` delivery futures — own ordinal block
# (mirrors core-config BN_DATED_ORDINAL_BASE), shared `binance-usdm:`
# descriptor namespace.
BN_DATED_ORDINAL_BASE: int = 2048
# WS9: `[bybit] linear` block (mirrors core-config
# BYBIT_LINEAR_ORDINAL_BASE; spot ordinals are file-order from 1).
BYBIT_LINEAR_ORDINAL_BASE: int = 512

_SLUG_RE: typing.Pattern[str] = re.compile(r"^[a-z0-9]+(?:-[a-z0-9]+)*$")

VENUE_NAMES: dict[int, str] = {
    claude_worker.frames.VENUE_POLYMARKET: "polymarket",
    claude_worker.frames.VENUE_BINANCE: "binance",
    claude_worker.frames.VENUE_OKX: "okx",
    claude_worker.frames.VENUE_DERIBIT: "deribit",
    claude_worker.frames.VENUE_HYPERLIQUID: "hyperliquid",
}

# CEX venues whose map names carry the instrument (``<venue>:<inst>``).
_CANDLE_VENUES: tuple[int, ...] = (
    claude_worker.frames.VENUE_OKX,
    claude_worker.frames.VENUE_DERIBIT,
    claude_worker.frames.VENUE_HYPERLIQUID,
)

# Clap-default mirror (crates/cli bin: --binance-symbol btcusdt,
# --binance-sym-id 7): the ONLY zero-REST name resolution. Keyed
# (venue byte from the tick record, raw SymbolId).
_ENGINE_DEFAULT_NAMES: dict[tuple[int, int], str] = {
    (claude_worker.frames.VENUE_BINANCE, 7): "binance:btcusdt",
}


def rest_budget_per_h(env: collections.abc.Mapping[str, str] | None = None) -> int:
    """Per-venue hourly REST budget from ``CLAUDE_WORKER_REST_BUDGET_PER_H``
    (default 60). A malformed or negative value is a usage error — a
    silently-wrong budget must not burn a venue's goodwill."""
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    raw = source.get(REST_BUDGET_ENV, "")
    if not raw:
        return REST_BUDGET_PER_H_DEFAULT
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError(f"{REST_BUDGET_ENV} must be an integer: {raw!r}") from exc
    if value < 0:
        raise ValueError(f"{REST_BUDGET_ENV} must be >= 0: {value}")
    return value


def venue_budgets(per_h: int) -> dict[int, claude_worker.features.RestBudget]:
    """One fixed-window :class:`RestBudget` per REST-consuming venue."""
    out: dict[int, claude_worker.features.RestBudget] = {}
    for venue in (claude_worker.frames.VENUE_POLYMARKET, *_CANDLE_VENUES):
        out[venue] = claude_worker.features.RestBudget(per_h, BUDGET_WINDOW_NS)
    return out


# ---- observed universe (§6.2) -------------------------------------------


def observed_universe(run_dir: pathlib.Path) -> dict[int, int]:
    """Distinct ``sym -> venue`` over one run dir's ``*-ticks.pmlr``.

    The venue byte comes from the v2 tick record; v1 files pin venue 0
    (the ``features.tick_features`` precedent, docs/migration.md).
    """
    universe: dict[int, int] = {}
    for path in sorted(run_dir.glob("*-ticks.pmlr")):
        with claude_worker.pmlr.Reader(path) as reader:
            v2 = reader.version >= claude_worker.pmlr.VENUE_BYTE_MIN_VERSION
            for tick in reader.ticks():
                universe[tick.sym] = tick.venue if v2 else 0
    return universe


# ---- strict parsers (labeling.py discipline: skip, never crash) ---------


class Candle(typing.NamedTuple):
    """One OHLCV bar, venue-normalized. ``ts_ms`` is the bar OPEN time
    (wall, ms). Floats are fine here — candles are research features on
    the offline plane, never accounting inputs."""

    ts_ms: int
    open: float
    high: float
    low: float
    close: float
    volume: float


class GammaMarket(typing.NamedTuple):
    """One strict-parsed Gamma market row (the fields we consume; extra
    response keys are tolerated — this is an external API, not model
    output)."""

    question: str
    slug: str
    token_ids: tuple[str, ...]
    outcomes: tuple[str, ...]


def _number(value: object) -> float | None:
    """Bool-rejecting numeric coercion (labeling.py pattern)."""
    if isinstance(value, bool):
        return None
    if isinstance(value, (int, float)):
        return float(value)
    return None


def _numeric_str(value: object) -> float | None:
    """A venue's stringified decimal (``"0.5"``) or bare number -> float;
    None on anything else (bool included)."""
    if isinstance(value, str):
        try:
            return float(value)
        except ValueError:
            return None
    return _number(value)


def _epoch_ms(value: object) -> int | None:
    """Millisecond timestamp: int (bool-rejected) or digit string."""
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value if value >= 0 else None
    if isinstance(value, str) and value.isdigit():
        return int(value)
    return None


def _token_id_list(raw: object) -> tuple[str, ...] | None:
    """Gamma's double-encoded ``clobTokenIds``: a JSON string whose
    content is itself a JSON array of decimal-digit strings."""
    if not isinstance(raw, str):
        return None
    try:
        inner = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(inner, list):
        return None
    out: list[str] = []
    for item in typing.cast(list[object], inner):
        if not isinstance(item, str) or not item.isdigit():
            return None
        if not PM_TOKEN_RUN_MIN <= len(item) <= PM_TOKEN_MAX:
            return None
        out.append(item)
    return tuple(out)


def _str_list(raw: object) -> tuple[str, ...]:
    """Gamma's double-encoded ``outcomes``; malformed -> empty (the
    field is enrichment, not identity)."""
    if not isinstance(raw, str):
        return ()
    try:
        inner = json.loads(raw)
    except ValueError:
        return ()
    if not isinstance(inner, list):
        return ()
    out: list[str] = []
    for item in typing.cast(list[object], inner):
        if not isinstance(item, str):
            return ()
        out.append(item)
    return tuple(out)


def parse_gamma_markets(raw: str) -> tuple[list[GammaMarket], int] | None:
    """Strict parse of a Gamma ``/markets`` body. ``None`` = unusable
    body; otherwise ``(rows, malformed_rows_skipped)``."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, list):
        return None
    rows: list[GammaMarket] = []
    malformed = 0
    for entry in typing.cast(list[object], obj):
        if not isinstance(entry, dict):
            malformed += 1
            continue
        row = typing.cast(dict[str, object], entry)
        question = row.get("question")
        slug = row.get("slug")
        token_ids = _token_id_list(row.get("clobTokenIds"))
        if (
            not isinstance(question, str)
            or not question
            or not isinstance(slug, str)
            or not slug
            or token_ids is None
            or not token_ids
        ):
            malformed += 1
            continue
        rows.append(
            GammaMarket(
                question=question,
                slug=slug,
                token_ids=token_ids,
                outcomes=_str_list(row.get("outcomes")),
            )
        )
    return rows, malformed


def parse_okx_candles(raw: str) -> tuple[list[Candle], int] | None:
    """Strict parse of ``/api/v5/market/candles``. OKX returns rows
    NEWEST-first; the result is normalized OLDEST-first. ``None`` =
    unusable body; otherwise ``(candles, malformed_rows_skipped)``."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict):
        return None
    body = typing.cast(dict[str, object], obj)
    if body.get("code") != "0":
        return None
    data = body.get("data")
    if not isinstance(data, list):
        return None
    candles: list[Candle] = []
    malformed = 0
    for entry in typing.cast(list[object], data):
        if not isinstance(entry, list) or len(typing.cast(list[object], entry)) < 6:
            malformed += 1
            continue
        row = typing.cast(list[object], entry)
        ts_ms = _epoch_ms(row[0])
        values = [_numeric_str(row[i]) for i in range(1, 6)]
        if ts_ms is None or any(v is None for v in values):
            malformed += 1
            continue
        o, h, low, c, v = typing.cast(list[float], values)
        candles.append(Candle(ts_ms=ts_ms, open=o, high=h, low=low, close=c, volume=v))
    candles.reverse()
    return candles, malformed


def parse_deribit_chart(raw: str) -> list[Candle] | None:
    """Strict parse of ``get_tradingview_chart_data``. Columnar arrays —
    any malformed cell or length mismatch rejects the whole body (there
    is no per-row identity to skip on)."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, dict):
        return None
    result = typing.cast(dict[str, object], obj).get("result")
    if not isinstance(result, dict):
        return None
    body = typing.cast(dict[str, object], result)
    if body.get("status") != "ok":
        return None
    columns: list[list[float]] = []
    ticks_raw = body.get("ticks")
    if not isinstance(ticks_raw, list):
        return None
    ticks: list[int] = []
    for item in typing.cast(list[object], ticks_raw):
        ms = _epoch_ms(item)
        if ms is None:
            return None
        ticks.append(ms)
    for key in ("open", "high", "low", "close", "volume"):
        col_raw = body.get(key)
        if not isinstance(col_raw, list) or len(typing.cast(list[object], col_raw)) != len(ticks):
            return None
        col: list[float] = []
        for item in typing.cast(list[object], col_raw):
            value = _number(item)
            if value is None:
                return None
            col.append(value)
        columns.append(col)
    candles: list[Candle] = []
    for i in range(len(ticks)):
        candles.append(
            Candle(
                ts_ms=ticks[i],
                open=columns[0][i],
                high=columns[1][i],
                low=columns[2][i],
                close=columns[3][i],
                volume=columns[4][i],
            )
        )
    return candles


def parse_hl_candles(raw: str) -> tuple[list[Candle], int] | None:
    """Strict parse of a ``candleSnapshot`` response (array of ``{t, o,
    h, l, c, v}`` objects; numerics are strings). ``None`` = unusable
    body; otherwise ``(candles, malformed_rows_skipped)``."""
    try:
        obj = json.loads(raw)
    except ValueError:
        return None
    if not isinstance(obj, list):
        return None
    candles: list[Candle] = []
    malformed = 0
    for entry in typing.cast(list[object], obj):
        if not isinstance(entry, dict):
            malformed += 1
            continue
        row = typing.cast(dict[str, object], entry)
        ts_ms = _epoch_ms(row.get("t"))
        values = [_numeric_str(row.get(k)) for k in ("o", "h", "l", "c", "v")]
        if ts_ms is None or any(v is None for v in values):
            malformed += 1
            continue
        o, h, low, c, v = typing.cast(list[float], values)
        candles.append(Candle(ts_ms=ts_ms, open=o, high=h, low=low, close=c, volume=v))
    return candles, malformed


# ---- descriptor derivation (map names -> consumer targets) --------------


class Targets(typing.NamedTuple):
    """Consumer inputs derived from map names ∩ observed universe.

    ``pm_seeds``: ``(sym, seed)`` where seed is a token id or slug name.
    ``candles``: ``(venue, sym, instrument)`` for okx/deribit/hl names.
    """

    pm_seeds: list[tuple[int, str]]
    candles: list[tuple[int, int, str]]


def _pm_seed_kind(name: str) -> str | None:
    """Classify a PM map name: ``token`` / ``slug`` / None (a resolved
    question is not a seed — Gamma cannot be queried by question)."""
    if ":" in name:
        return None
    if name.isdigit():
        # All-digit names are token-or-nothing: a sub-threshold digit
        # run is never a real Gamma slug (Rust PM_TOKEN_RUN_MIN mirror).
        if PM_TOKEN_RUN_MIN <= len(name) <= PM_TOKEN_MAX:
            return "token"
        return None
    if _SLUG_RE.fullmatch(name) is not None:
        return "slug"
    return None


def derive_targets(markets: dict[str, int], universe: dict[int, int]) -> Targets:
    """Map names ∩ observed universe -> consumer targets, name-sorted
    (deterministic request order). One sym may carry several seed names;
    the first (sorted) seed wins — one Gamma call per sym."""
    pm_seeds: list[tuple[int, str]] = []
    pm_seeded: set[int] = set()
    candles: list[tuple[int, int, str]] = []
    for name in sorted(markets):
        sym = markets[name]
        venue = universe.get(sym)
        if venue is None:
            continue
        if venue == claude_worker.frames.VENUE_POLYMARKET:
            if sym not in pm_seeded and _pm_seed_kind(name) is not None:
                pm_seeds.append((sym, name))
                pm_seeded.add(sym)
        elif venue in _CANDLE_VENUES:
            prefix = VENUE_NAMES[venue] + ":"
            if name.startswith(prefix) and len(name) > len(prefix):
                candles.append((venue, sym, name[len(prefix) :]))
    return Targets(pm_seeds=pm_seeds, candles=candles)


# ---- the four consumers (§6.1) ------------------------------------------


class FetchStats(typing.NamedTuple):
    """Per-consumer accounting surfaced in fetch output."""

    requested: int
    fetched: int
    budget_skipped: int
    failed: int
    malformed: int


def _gamma_url(host: str, seed: str) -> str:
    if _pm_seed_kind(seed) == "token":
        return f"https://{host}/markets?clob_token_ids={seed}"
    return f"https://{host}/markets?slug={seed}"


def fetch_pm_gamma(
    budget: claude_worker.features.RestBudget,
    get_fn: typing.Callable[[str], str | None],
    host: str,
    seeds: list[tuple[int, str]],
) -> tuple[dict[int, GammaMarket], FetchStats]:
    """PM Gamma consumer: seed (token id or slug) -> the matching market
    row per sym. A response row matches a token seed when its
    ``clobTokenIds`` contains the seed (the Rust ``find_by_token``
    mirror) and a slug seed on exact slug equality."""
    out: dict[int, GammaMarket] = {}
    fetched = 0
    budget_skipped = 0
    failed = 0
    malformed = 0
    for sym, seed in seeds:
        if not budget.try_acquire():
            budget_skipped += 1
            continue
        payload = get_fn(_gamma_url(host, seed))
        if payload is None:
            failed += 1
            continue
        parsed = parse_gamma_markets(payload)
        if parsed is None:
            malformed += 1
            continue
        rows, bad_rows = parsed
        malformed += bad_rows
        kind = _pm_seed_kind(seed)
        match: GammaMarket | None = None
        for row in rows:
            if (kind == "token" and seed in row.token_ids) or (
                kind == "slug" and row.slug == seed
            ):
                match = row
                break
        if match is None:
            failed += 1
            continue
        fetched += 1
        out[sym] = match
    return out, FetchStats(
        requested=len(seeds),
        fetched=fetched,
        budget_skipped=budget_skipped,
        failed=failed,
        malformed=malformed,
    )


def _okx_candles_url(host: str, inst_id: str) -> str:
    return (
        f"https://{host}/api/v5/market/candles"
        f"?instId={inst_id}&bar={CANDLE_INTERVAL}&limit={OKX_CANDLE_LIMIT}"
    )


def _deribit_chart_url(host: str, instrument: str, start_ms: int, end_ms: int) -> str:
    return (
        f"https://{host}/api/v2/public/get_tradingview_chart_data"
        f"?instrument_name={instrument}&start_timestamp={start_ms}"
        f"&end_timestamp={end_ms}&resolution={DERIBIT_RESOLUTION}"
    )


def _hl_info_url(host: str) -> str:
    return f"https://{host}/info"


def _hl_candle_body(coin: str, start_ms: int, end_ms: int) -> str:
    return json.dumps(
        {
            "type": "candleSnapshot",
            "req": {
                "coin": coin,
                "interval": CANDLE_INTERVAL,
                "startTime": start_ms,
                "endTime": end_ms,
            },
        },
        separators=(",", ":"),
    )


def fetch_venue_candles(
    venue: int,
    budget: claude_worker.features.RestBudget,
    get_fn: typing.Callable[[str], str | None],
    post_fn: typing.Callable[[str, str], str | None],
    host: str,
    targets: list[tuple[int, str]],
    now_ms: int | None = None,
) -> tuple[dict[int, tuple[str, list[Candle]]], FetchStats]:
    """One venue's candle consumer over ``(sym, instrument)`` targets.

    Dispatches on the venue byte: OKX GET, Deribit GET (windowed),
    HL POST (windowed). Result maps ``sym -> (instrument, candles)``.
    """
    end_ms = int(time.time() * 1000) if now_ms is None else now_ms
    start_ms = end_ms - CANDLE_WINDOW_MS
    out: dict[int, tuple[str, list[Candle]]] = {}
    fetched = 0
    budget_skipped = 0
    failed = 0
    malformed = 0
    for sym, instrument in targets:
        if not budget.try_acquire():
            budget_skipped += 1
            continue
        if venue == claude_worker.frames.VENUE_OKX:
            payload = get_fn(_okx_candles_url(host, instrument))
        elif venue == claude_worker.frames.VENUE_DERIBIT:
            payload = get_fn(_deribit_chart_url(host, instrument, start_ms, end_ms))
        elif venue == claude_worker.frames.VENUE_HYPERLIQUID:
            payload = post_fn(_hl_info_url(host), _hl_candle_body(instrument, start_ms, end_ms))
        else:
            raise ValueError(f"fetch_venue_candles: not a candle venue: {venue}")
        if payload is None:
            failed += 1
            continue
        candles: list[Candle] | None
        if venue == claude_worker.frames.VENUE_OKX:
            parsed_okx = parse_okx_candles(payload)
            if parsed_okx is None:
                candles = None
            else:
                candles, bad = parsed_okx
                malformed += bad
        elif venue == claude_worker.frames.VENUE_DERIBIT:
            candles = parse_deribit_chart(payload)
        else:
            parsed_hl = parse_hl_candles(payload)
            if parsed_hl is None:
                candles = None
            else:
                candles, bad = parsed_hl
                malformed += bad
        if candles is None:
            malformed += 1
            continue
        fetched += 1
        out[sym] = (instrument, candles)
    return out, FetchStats(
        requested=len(targets),
        fetched=fetched,
        budget_skipped=budget_skipped,
        failed=failed,
        malformed=malformed,
    )


# ---- feature-file output (§6.1: beside the replay-derived ones) ---------


def write_candle_file(
    features_dir: pathlib.Path,
    run_name: str,
    sym: int,
    venue: int,
    instrument: str,
    candles: list[Candle],
) -> pathlib.Path:
    """``<features>/<run>/<sym>-ohlcv.json`` beside the replay-derived
    ``<sym>.json``. Candles oldest-first, compact arrays."""
    out_dir = features_dir / run_name
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"{sym}-ohlcv.json"
    payload = {
        "sym": sym,
        "venue": VENUE_NAMES.get(venue, str(venue)),
        "instrument": instrument,
        "interval": CANDLE_INTERVAL,
        "candles": [[c.ts_ms, c.open, c.high, c.low, c.close, c.volume] for c in candles],
    }
    path.write_text(json.dumps(payload, separators=(",", ":")))
    return path


def write_market_meta_file(
    features_dir: pathlib.Path,
    run_name: str,
    sym: int,
    market: GammaMarket,
) -> pathlib.Path:
    """``<features>/<run>/<sym>-meta.json``: the PM Gamma metadata
    feature header (§6.1)."""
    out_dir = features_dir / run_name
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"{sym}-meta.json"
    payload = {
        "sym": sym,
        "venue": "polymarket",
        "question": market.question,
        "slug": market.slug,
        "token_ids": list(market.token_ids),
        "outcomes": list(market.outcomes),
    }
    path.write_text(json.dumps(payload, separators=(",", ":")))
    return path


# ---- market-map bootstrap + additive refresh (§6.2) ---------------------


class MapRefresh(typing.NamedTuple):
    """Outcome of one bootstrap/refresh pass, surfaced in fetch output."""

    path: pathlib.Path
    created: bool
    added: dict[str, int]
    conflicts: list[str]
    unresolved: list[tuple[int, int]]  # (sym, venue) with no name after the pass


def default_names(universe: dict[int, int]) -> dict[str, int]:
    """Zero-REST resolutions: the engine clap-default mirror rows whose
    (venue, sym) is actually observed."""
    out: dict[str, int] = {}
    for (venue, sym), name in _ENGINE_DEFAULT_NAMES.items():
        if universe.get(sym) == venue:
            out[name] = sym
    return out


def gamma_names(resolved: dict[int, GammaMarket]) -> dict[str, int]:
    """Gamma resolutions -> map names: the question AND the slug both
    name the sym (the slug keeps future fetches seeded — Gamma cannot be
    queried by question)."""
    out: dict[str, int] = {}
    for sym, market in resolved.items():
        out[market.question] = sym
        out[market.slug] = sym
    return out


def _pm_token_entry(entry: str) -> tuple[str, ...] | None:
    """Parse one universe-file PM market entry into its token tuple:
    ``"<token>"`` -> 1-tuple, ``"<yes>:<no>"`` -> 2-tuple, anything
    else -> None (best-effort — the ENGINE is the config validator)."""

    def _is_token(s: str) -> bool:
        return s.isdigit() and PM_TOKEN_RUN_MIN <= len(s) <= PM_TOKEN_MAX

    parts = entry.split(":")
    if len(parts) == 1 and _is_token(parts[0]):
        return (parts[0],)
    if len(parts) == 2 and _is_token(parts[0]) and _is_token(parts[1]) and parts[0] != parts[1]:
        return (parts[0], parts[1])
    return None


def universe_file_proposals(
    path: pathlib.Path,
    universe: dict[int, int],
) -> tuple[dict[str, int], tuple[tuple[int, int], ...], list[str]]:
    """M1 universe-file seeding (docs/mvp-progress.md M1d).

    Replicates the DETERMINISTIC engine allocation law over the
    ``[polymarket] markets`` list (flat token order; token[0] ->
    ``PM_LEGACY_ANCHOR_SYM``, token[i>=1] -> ``i+1``) and proposes
    ``{token_id: sym}`` map names for every allocated sym the capture
    actually OBSERVED as Polymarket. YES/NO pair entries whose both
    syms are observed become additive pair proposals for the map's
    pair machinery (positions netting). Best-effort by design: a
    missing/malformed file yields no proposals plus one report line —
    never an error (the engine validates the config at boot; the
    worker only seeds).
    """
    lines: list[str] = []
    try:
        raw = path.read_text()
    except OSError as exc:
        lines.append(f"universe file skipped: {path}: {exc}")
        return {}, (), lines
    try:
        data = tomllib.loads(raw)
    except tomllib.TOMLDecodeError as exc:
        lines.append(f"universe file skipped: {path}: TOML parse error: {exc}")
        return {}, (), lines
    pm_section = data.get("polymarket")
    entries: list[str] = []
    if isinstance(pm_section, dict):
        raw_markets = pm_section.get("markets")
        if isinstance(raw_markets, list):
            for item in raw_markets:
                if isinstance(item, str):
                    entries.append(item)

    proposals: dict[str, int] = {}
    pairs: list[tuple[int, int]] = []
    flat = 0
    skipped = 0
    for entry in entries:
        tokens = _pm_token_entry(entry)
        if tokens is None:
            skipped += 1
            continue
        syms: list[int] = []
        for token in tokens:
            sym = PM_LEGACY_ANCHOR_SYM if flat == 0 else flat + 1
            flat += 1
            syms.append(sym)
            if universe.get(sym) == claude_worker.frames.VENUE_POLYMARKET:
                proposals[token] = sym
        if len(syms) == 2 and all(
            universe.get(s) == claude_worker.frames.VENUE_POLYMARKET for s in syms
        ):
            pairs.append((syms[0], syms[1]))

    # CEX sections: same law mirror, §9.4 descriptor names. Only
    # observed (venue-byte-matching) syms are proposed.
    def _propose_list(
        section: str,
        key: str,
        venue: int,
        name_prefix: str,
        ordinal_base: int,
        anchor: int | None,
    ) -> None:
        sec = data.get(section)
        if not isinstance(sec, dict):
            return
        raw = sec.get(key)
        if not isinstance(raw, list):
            return
        for i, item in enumerate(raw):
            if not isinstance(item, str) or not item:
                continue
            if anchor is not None and i == 0:
                sym = anchor
            else:
                sym = (venue << SYMBOL_VENUE_SHIFT) | (ordinal_base + i + 1)
            if universe.get(sym) == venue:
                proposals[f"{name_prefix}{item}"] = sym

    _propose_list(
        "binance", "spot", claude_worker.frames.VENUE_BINANCE, "binance:", 0, BN_LEGACY_ANCHOR_SYM
    )
    _propose_list(
        "binance",
        "usdm",
        claude_worker.frames.VENUE_BINANCE,
        "binance-usdm:",
        BN_USDM_ORDINAL_BASE,
        None,
    )
    _propose_list(
        "binance",
        "usdm_dated",
        claude_worker.frames.VENUE_BINANCE,
        "binance-usdm:",
        BN_DATED_ORDINAL_BASE,
        None,
    )
    _propose_list("okx", "instruments", claude_worker.frames.VENUE_OKX, "okx:", 0, None)
    _propose_list(
        "deribit", "instruments", claude_worker.frames.VENUE_DERIBIT, "deribit:", 0, None
    )
    _propose_list(
        "hyperliquid", "coins", claude_worker.frames.VENUE_HYPERLIQUID, "hyperliquid:", 0, None
    )
    # WS9: Bybit — spot from ordinal 1, linear from base 512 (mirrors
    # core-config BYBIT_LINEAR_ORDINAL_BASE).
    _propose_list("bybit", "spot", claude_worker.frames.VENUE_BYBIT, "bybit:", 0, None)
    _propose_list(
        "bybit",
        "linear",
        claude_worker.frames.VENUE_BYBIT,
        "bybit-linear:",
        BYBIT_LINEAR_ORDINAL_BASE,
        None,
    )

    lines.append(
        f"universe file {path.name}: entries={len(entries)}"
        f" proposals={len(proposals)} pairs={len(pairs)} skipped={skipped}"
    )
    return proposals, tuple(pairs), lines


def refresh_market_map(
    path: pathlib.Path,
    markets: dict[str, int],
    hip4_pairs: tuple[tuple[int, int], ...],
    proposals: dict[str, int],
    universe: dict[int, int],
    *,
    pair_proposals: tuple[tuple[int, int], ...] = (),
) -> MapRefresh:
    """Bootstrap (file absent) or additive refresh (file present).

    ``markets``/``hip4_pairs`` are the strict-loaded CURRENT contents
    (``cli.load_market_map`` — the caller loads; a malformed operator
    file already failed there, exit 2, and this function is never
    reached: a half-readable map is never "repaired" by overwrite).

    Additive law (H-D7): new names only; an existing name proposing a
    DIFFERENT sym is a reported conflict and left alone; operator
    entries and pair order survive byte-for-byte. HIP-4 pairs: none
    derivable live today — preserved verbatim, never fabricated.

    The write is atomic: same-dir temp file + ``os.replace`` — a
    half-written map can never exist.
    """
    created = not path.exists()
    added: dict[str, int] = {}
    conflicts: list[str] = []
    for name in sorted(proposals):
        sym = proposals[name]
        existing = markets.get(name)
        if existing is None:
            added[name] = sym
        elif existing != sym:
            conflicts.append(
                f"market map conflict: {name!r} is {existing} in the map,"
                f" resolves to {sym} — operator entry left alone"
            )
    merged = dict(markets)
    merged.update(added)
    named_syms = set(merged.values())
    unresolved = sorted(
        (sym, venue) for sym, venue in universe.items() if sym not in named_syms
    )
    # Pair merge (M1): operator pairs preserved verbatim and first;
    # universe-file proposals appended additively, deduped. Nothing is
    # ever removed or reordered.
    merged_pairs: list[tuple[int, int]] = [(int(a), int(b)) for a, b in hip4_pairs]
    for pair in pair_proposals:
        if pair not in merged_pairs:
            merged_pairs.append(pair)
    payload = {
        "markets": {name: merged[name] for name in sorted(merged)},
        "hip4_pairs": [list(pair) for pair in merged_pairs],
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")
    os.replace(tmp, path)
    return MapRefresh(
        path=path,
        created=created,
        added=added,
        conflicts=conflicts,
        unresolved=unresolved,
    )


# ---- fetch-verb orchestration (called by cli.fetch) ----------------------


class SecondaryReport(typing.NamedTuple):
    """Everything the fetch verb surfaces about §6: written files plus
    human summary lines (stderr; the machine surface is the files)."""

    files: list[pathlib.Path]
    lines: list[str]


def run_secondary(  # noqa: PLR0913 — one injected seam per §6 element, deliberately
    universe: dict[int, int],
    markets: dict[str, int],
    hip4_pairs: tuple[tuple[int, int], ...],
    map_path: pathlib.Path,
    features_dir: pathlib.Path,
    run_name: str,
    no_rest: bool,
    get_fn: typing.Callable[[str], str | None] | None,
    post_fn: typing.Callable[[str, str], str | None] | None,
    env: collections.abc.Mapping[str, str] | None = None,
    now_ms: int | None = None,
) -> SecondaryReport:
    """The §6 pass: consumers (unless ``--no-rest``) then map ownership.

    ``--no-rest`` is REAL: all four consumers are skipped and ``get_fn``
    / ``post_fn`` are never touched (callers pass None). Map bootstrap /
    refresh runs in BOTH modes — §6.2 says "on every fetch"; without
    REST only zero-REST resolutions (clap-default mirror) contribute.
    """
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    files: list[pathlib.Path] = []
    lines: list[str] = []
    proposals = default_names(universe)

    # M1 universe-file seeding (fetch-seam env; H3 precedent — no
    # BaseConfig field). Proposals join the map refresh AND the same
    # fetch's Gamma seed derivation below, so one fetch both names
    # the configured tokens and resolves their question/slug/meta.
    pair_proposals: tuple[tuple[int, int], ...] = ()
    universe_file_raw = source.get(UNIVERSE_FILE_ENV, "")
    if universe_file_raw:
        u_props, pair_proposals, u_lines = universe_file_proposals(
            pathlib.Path(universe_file_raw).expanduser(), universe
        )
        proposals.update(u_props)
        lines.extend(u_lines)

    if not no_rest:
        targets = derive_targets({**proposals, **markets}, universe)
        budgets = venue_budgets(rest_budget_per_h(source))
        if targets.pm_seeds:
            assert get_fn is not None  # caller wires the client when REST is on
            pm_budget = budgets[claude_worker.frames.VENUE_POLYMARKET]
            resolved, stats = fetch_pm_gamma(
                pm_budget,
                get_fn,
                source.get(PM_GAMMA_HOST_ENV, "") or PM_GAMMA_HOST_DEFAULT,
                targets.pm_seeds,
            )
            proposals.update(gamma_names(resolved))
            for sym in sorted(resolved):
                files.append(write_market_meta_file(features_dir, run_name, sym, resolved[sym]))
            lines.append(_stats_line("polymarket", stats, pm_budget))
        host_by_venue = {
            claude_worker.frames.VENUE_OKX: source.get(OKX_REST_HOST_ENV, "")
            or OKX_REST_HOST_DEFAULT,
            claude_worker.frames.VENUE_DERIBIT: source.get(DERIBIT_REST_HOST_ENV, "")
            or DERIBIT_REST_HOST_DEFAULT,
            claude_worker.frames.VENUE_HYPERLIQUID: source.get(HL_API_HOST_ENV, "")
            or HL_API_HOST_DEFAULT,
        }
        for venue in _CANDLE_VENUES:
            venue_targets = [(sym, inst) for v, sym, inst in targets.candles if v == venue]
            if not venue_targets:
                continue
            assert get_fn is not None and post_fn is not None
            series, stats = fetch_venue_candles(
                venue,
                budgets[venue],
                get_fn,
                post_fn,
                host_by_venue[venue],
                venue_targets,
                now_ms=now_ms,
            )
            for sym in sorted(series):
                instrument, candles = series[sym]
                files.append(
                    write_candle_file(features_dir, run_name, sym, venue, instrument, candles)
                )
            lines.append(_stats_line(VENUE_NAMES[venue], stats, budgets[venue]))
    else:
        lines.append("rest: skipped (--no-rest)")

    refresh = refresh_market_map(
        map_path, markets, hip4_pairs, proposals, universe, pair_proposals=pair_proposals
    )
    action = "bootstrapped" if refresh.created else "refreshed"
    lines.append(
        f"market map {action}: {refresh.path}"
        f" added={len(refresh.added)} conflicts={len(refresh.conflicts)}"
        f" unresolved={len(refresh.unresolved)}"
    )
    lines.extend(refresh.conflicts)
    for sym, venue in refresh.unresolved:
        venue_name = VENUE_NAMES.get(venue, str(venue))
        lines.append(
            f"unresolved sym {sym} ({venue_name}): add a market-map name for it"
            " (PM: a Gamma token id or slug; CEX: '<venue>:<instrument>')"
        )
    return SecondaryReport(files=files, lines=lines)


def _stats_line(
    venue_name: str,
    stats: FetchStats,
    budget: claude_worker.features.RestBudget,
) -> str:
    return (
        f"rest {venue_name}: requested={stats.requested} fetched={stats.fetched}"
        f" budget_skipped={stats.budget_skipped} failed={stats.failed}"
        f" malformed={stats.malformed} skipped_total={budget.skipped_total}"
    )
