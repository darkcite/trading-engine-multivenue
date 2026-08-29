# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""universe_refresh — M3 daily Polymarket up/down universe refresh.

A standalone MODULE (``python -m claude_worker.universe_refresh``) —
NOT a verb: the 7-verb CLI surface is FROZEN (``cli.py`` untouched).
The M3 launchd engine wrapper runs it best-effort before every boot,
automating the M1-R1 recipe (docs/mvp-progress.md): resolve the
current ``<underlying>-up-or-down-on-<date>-2026`` markets via the
Gamma lane and rewrite ONLY the ``[polymarket]`` ``markets`` array of
``universe.toml``. Wholesale PM replacement is the CLEAN path per the
CLAUDE.md universe runbook (expired dailies drop out of the observed
universe; stale map names are harmless).

Laws:

- **Date law**: dailies resolve 16:00Z (live-verified 2026-08-22:
  ``endDate 2026-08-22T16:00:00Z``; the NEXT day's market lists ~2
  days early, so several coexist). The refresh targets the nearest
  UNRESOLVED daily: today (UTC) before 16:00Z, else tomorrow. The
  00:00Z daily-restart therefore always picks "the day's" markets.
- **Slug law** (live-verified format
  ``bitcoin-up-or-down-on-august-22-2026``): lowercase month name,
  unpadded day. Single-digit-day padding is UNVERIFIED until a
  1st–9th passes — so both unpadded and zero-padded candidates are
  tried, unpadded first.
- **Order laws**: config ``underlyings`` order IS the markets-array
  order IS the ``[pairs]`` index space — the pairs section is never
  touched, so keep ``underlyings`` aligned with ``binance.spot``.
  Token order is ``Up:Down`` (= yes:no, M1 file law); when Gamma
  lists outcomes ``Down/Up`` the pair is swapped to keep Up first.
- **Best-effort law** (fetchers discipline): ANY failure — missing
  config, network, unusable body, unresolved slug — leaves
  ``universe.toml`` byte-untouched and exits 1; the wrapper boots on
  the existing file. Success rewrites atomically (tmp + rename) and
  exits 0. Idempotent: resolving to the identical array still
  rewrites to the same bytes.
- No live API calls in tests — HTTP rides an injectable ``get_fn``
  (``fetchers.fetch_pm_gamma`` pattern); the ``main`` default is the
  worker's standard httpx GET (the ``cli._http_get`` pattern — NOT
  urllib: uv-managed CPython's stdlib SSL lacks the CA wiring httpx
  carries, observed live at the first launchd boot 2026-08-22).

Config (``~/multivenue/pm-dailies.toml``, TOML): ``[dailies]
underlyings = ["bitcoin", "ethereum"]``. See
``pm-dailies.toml.example``.

**BST3 (equity dailies, binance-stocks-plan §3, 2026-08-29):** a
second, OPTIONAL family ``[equity_dailies] underlyings = ["nvda",
…]`` — Polymarket equity up/downs (M1-R1 amended per plan §0.4).
Equity laws, all live-verified 2026-08-29:

- **Slug law**: the SAME builder (``nvda-up-or-down-on-august-31-2026``).
- **Equity date law**: equities resolve at the US close — ``endDate
  20:00Z`` verified in EDT — so the target date is computed in
  America/New_York (zoneinfo, DST-correct): today if before 16:00 ET
  on a trading day, else the NEXT trading day (weekends + the static
  NYSE holiday table below, operator-maintained per year). Monday's
  market is listed by Saturday (verified).
- **Ordering law**: the rewritten array = the crypto block (existing
  law) followed by the equity block, so the ``[pairs]`` index space
  never moves and ``[pairs]`` is never rewritten.
- **Family-independence law** (amends all-or-nothing): a crypto
  failure still aborts the whole refresh, file untouched. An equity
  failure (holiday gap, unlisted market, Gamma miss) REUSES the
  previous file's equity tail verbatim and refreshes crypto —
  exit 0 with an ``equity=stale`` note. A stale resolved equity
  market is a quiet subscription (M1 law), never a boot risk.
"""

import argparse
import collections.abc
import datetime
import os
import pathlib
import sys
import tomllib
import typing
import zoneinfo

import httpx

import claude_worker.fetchers

DEFAULT_UNIVERSE_PATH: str = "~/multivenue/universe.toml"
DEFAULT_DAILIES_PATH: str = "~/multivenue/pm-dailies.toml"

RESOLVE_HOUR_UTC: int = 16

EQUITY_RESOLVE_HOUR_ET: int = 16
EQUITY_TZ: str = "America/New_York"

# NYSE full-closure holidays, current year — STATIC, operator-maintained
# (plan BST3.2: no new dependency; extend when the calendar year turns).
NYSE_HOLIDAYS_2026: tuple[datetime.date, ...] = (
    datetime.date(2026, 1, 1),    # New Year's Day
    datetime.date(2026, 1, 19),   # MLK Day
    datetime.date(2026, 2, 16),   # Presidents' Day
    datetime.date(2026, 4, 3),    # Good Friday
    datetime.date(2026, 5, 25),   # Memorial Day
    datetime.date(2026, 6, 19),   # Juneteenth
    datetime.date(2026, 7, 3),    # Independence Day (observed)
    datetime.date(2026, 9, 7),    # Labor Day
    datetime.date(2026, 11, 26),  # Thanksgiving
    datetime.date(2026, 12, 25),  # Christmas
)

MONTH_NAMES: tuple[str, ...] = (
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
)


class ResolvedMarket(typing.NamedTuple):
    """One refreshed market row: the ``yes:no`` file entry + the
    human comment line."""

    entry: str
    question: str


def refresh_date(now_utc: datetime.datetime) -> datetime.date:
    """The nearest unresolved daily's date: today before 16:00Z,
    else tomorrow (date law)."""
    if now_utc.hour < RESOLVE_HOUR_UTC:
        return now_utc.date()
    return now_utc.date() + datetime.timedelta(days=1)


def is_us_trading_day(date: datetime.date) -> bool:
    """Weekday and not in the static holiday table (equity date law)."""
    return date.weekday() < 5 and date not in NYSE_HOLIDAYS_2026


def equity_refresh_date(now_utc: datetime.datetime) -> datetime.date:
    """The nearest unresolved EQUITY daily's date: today
    (America/New_York) if before 16:00 ET on a trading day, else the
    next trading day."""
    now_et = now_utc.astimezone(zoneinfo.ZoneInfo(EQUITY_TZ))
    date = now_et.date()
    if is_us_trading_day(date) and now_et.hour < EQUITY_RESOLVE_HOUR_ET:
        return date
    date += datetime.timedelta(days=1)
    while not is_us_trading_day(date):
        date += datetime.timedelta(days=1)
    return date


def slug_candidates(underlying: str, date: datetime.date) -> tuple[str, ...]:
    """Gamma slugs to try, most-likely first (slug law)."""
    month = MONTH_NAMES[date.month - 1]
    unpadded = f"{underlying}-up-or-down-on-{month}-{date.day}-{date.year}"
    if date.day >= 10:
        return (unpadded,)
    padded = f"{underlying}-up-or-down-on-{month}-{date.day:02d}-{date.year}"
    return (unpadded, padded)


def _up_down_entry(market: claude_worker.fetchers.GammaMarket) -> str | None:
    """``Up:Down`` token pair from a Gamma row (order law); ``None``
    when the row cannot honestly express one."""
    if len(market.token_ids) < 2:
        return None
    first, second = market.token_ids[0], market.token_ids[1]
    outcomes = tuple(o.strip().lower() for o in market.outcomes[:2])
    if outcomes == ("down", "up"):
        first, second = second, first
    return f"{first}:{second}"


def resolve_market(
    get_fn: collections.abc.Callable[[str], str | None],
    host: str,
    underlying: str,
    date: datetime.date,
) -> ResolvedMarket | None:
    """Resolve one underlying's daily via the Gamma lane; ``None`` on
    any failure (best-effort law)."""
    for slug in slug_candidates(underlying, date):
        payload = get_fn(claude_worker.fetchers._gamma_url(host, slug))
        if payload is None:
            continue
        parsed = claude_worker.fetchers.parse_gamma_markets(payload)
        if parsed is None:
            continue
        rows, _malformed = parsed
        for row in rows:
            if row.slug != slug:
                continue
            entry = _up_down_entry(row)
            if entry is not None:
                return ResolvedMarket(entry=entry, question=row.question)
    return None


def read_underlyings(dailies_path: pathlib.Path) -> tuple[str, ...] | None:
    """``[dailies] underlyings`` from the config; ``None`` on any
    problem (missing file, bad TOML, wrong shape, empty)."""
    try:
        raw = dailies_path.read_bytes()
    except OSError:
        return None
    try:
        obj = tomllib.loads(raw.decode("utf-8", errors="strict"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError):
        return None
    dailies = obj.get("dailies")
    if not isinstance(dailies, dict):
        return None
    underlyings = dailies.get("underlyings")
    if not isinstance(underlyings, list) or not underlyings:
        return None
    out: list[str] = []
    for u in typing.cast(list[object], underlyings):
        if not isinstance(u, str) or not u or not u.replace("-", "").isalnum():
            return None
        out.append(u.lower())
    return tuple(out)


def read_equity_underlyings(dailies_path: pathlib.Path) -> tuple[str, ...]:
    """``[equity_dailies] underlyings`` — OPTIONAL family (BST3): an
    absent/empty section is a valid pure-crypto config and returns
    ``()``; a PRESENT-but-malformed section also returns ``()`` with
    a stderr note (family-independence: equity config problems never
    block the crypto refresh)."""
    try:
        raw = dailies_path.read_bytes()
        obj = tomllib.loads(raw.decode("utf-8", errors="strict"))
    except (OSError, tomllib.TOMLDecodeError, UnicodeDecodeError):
        return ()
    section = obj.get("equity_dailies")
    if not isinstance(section, dict):
        return ()
    underlyings = section.get("underlyings")
    if not isinstance(underlyings, list):
        return ()
    out: list[str] = []
    for u in typing.cast(list[object], underlyings):
        if not isinstance(u, str) or not u or not u.replace("-", "").isalnum():
            print(
                "universe-refresh: malformed [equity_dailies] entry — equity family skipped",
                file=sys.stderr,
            )
            return ()
        out.append(u.lower())
    return tuple(out)


def parse_markets_entries(text: str) -> list[ResolvedMarket] | None:
    """Read the CURRENT ``[polymarket] markets`` array back as
    (comment, entry) rows — the equity-stale reuse path (BST3 family-
    independence law). ``None`` when the array cannot be located."""
    lines = text.split("\n")
    section = None
    for i, line in enumerate(lines):
        if line.strip() == "[polymarket]":
            section = i
            break
    if section is None:
        return None
    out: list[ResolvedMarket] = []
    in_array = False
    pending_comment = ""
    for line in lines[section + 1 :]:
        stripped = line.strip()
        if not in_array:
            if stripped.startswith("[") and stripped.endswith("]"):
                return None
            if stripped.startswith("markets") and "=" in stripped:
                in_array = True
                # single-line form: markets = ["a", "b"]
                if "]" in stripped:
                    inner = stripped.split("[", 1)[1].rsplit("]", 1)[0]
                    for tok in inner.split(","):
                        tok = tok.strip().strip('"')
                        if tok:
                            out.append(ResolvedMarket(entry=tok, question="(carried)"))
                    return out
            continue
        if stripped == "]":
            return out
        if stripped.startswith("#"):
            pending_comment = stripped.lstrip("# ").strip()
            continue
        if stripped.startswith('"'):
            entry = stripped.strip(",").strip('"')
            out.append(
                ResolvedMarket(entry=entry, question=pending_comment or "(carried)")
            )
            pending_comment = ""
    return None


def rewrite_polymarket_markets(text: str, markets: list[ResolvedMarket]) -> str | None:
    """Replace ONLY the ``markets = [ … ]`` array inside the
    ``[polymarket]`` section, byte-preserving everything else.
    ``None`` when the section/array cannot be located unambiguously
    (grammar is the strict core-config subset, so structure is
    predictable)."""
    lines = text.split("\n")
    section_start: int | None = None
    for i, line in enumerate(lines):
        if line.strip() == "[polymarket]":
            section_start = i
            break
    if section_start is None:
        return None
    open_idx: int | None = None
    for i in range(section_start + 1, len(lines)):
        stripped = lines[i].strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            return None  # next section before any markets array
        if stripped.startswith("markets") and "=" in stripped and stripped.endswith("["):
            open_idx = i
            break
        if stripped.startswith("markets") and "=" in stripped and "]" in stripped:
            # single-line form: markets = ["…"]
            open_idx = i
            close_idx = i
            return _splice(lines, open_idx, close_idx, markets)
    if open_idx is None:
        return None
    close_idx = None
    for i in range(open_idx + 1, len(lines)):
        if lines[i].strip() == "]":
            close_idx = i
            break
        if lines[i].strip().startswith("["):
            return None
    if close_idx is None:
        return None
    return _splice(lines, open_idx, close_idx, markets)


def _splice(
    lines: list[str],
    open_idx: int,
    close_idx: int,
    markets: list[ResolvedMarket],
) -> str:
    """Rebuild the file with the new multiline markets array."""
    body: list[str] = ["markets = ["]
    for m in markets:
        body.append(f"  # {m.question}")
        body.append(f'  "{m.entry}",')
    body.append("]")
    return "\n".join(lines[:open_idx] + body + lines[close_idx + 1 :])


def make_get(client: httpx.Client) -> collections.abc.Callable[[str], str | None]:
    """The worker-standard best-effort GET (``cli._http_get`` pattern):
    ``None`` = transport failure or non-200."""

    def get(url: str) -> str | None:
        try:
            response = client.get(url, timeout=claude_worker.fetchers.REST_TIMEOUT_S)
        except httpx.HTTPError:
            return None
        if response.status_code != httpx.codes.OK:
            return None
        return response.text

    return get


def run(
    universe_path: pathlib.Path,
    dailies_path: pathlib.Path,
    now_utc: datetime.datetime,
    get_fn: collections.abc.Callable[[str], str | None],
    env: collections.abc.Mapping[str, str],
) -> int:
    """The refresh: 0 = universe.toml rewritten; 1 = left untouched
    (reason on stderr)."""
    underlyings = read_underlyings(dailies_path)
    if underlyings is None:
        print(f"universe-refresh: unusable dailies config {dailies_path}", file=sys.stderr)
        return 1
    try:
        text = universe_path.read_text(encoding="utf-8")
    except OSError as e:
        print(f"universe-refresh: cannot read {universe_path}: {e}", file=sys.stderr)
        return 1
    host = (
        env.get(claude_worker.fetchers.PM_GAMMA_HOST_ENV, "")
        or claude_worker.fetchers.PM_GAMMA_HOST_DEFAULT
    )
    date = refresh_date(now_utc)
    resolved: list[ResolvedMarket] = []
    for u in underlyings:
        market = resolve_market(get_fn, host, u, date)
        if market is None:
            print(
                f"universe-refresh: {u} unresolved for {date.isoformat()} — keeping existing file",
                file=sys.stderr,
            )
            return 1
        resolved.append(market)

    # BST3: the OPTIONAL equity family — ordering law (equity block
    # AFTER crypto) + family-independence law (equity failure carries
    # the previous equity tail; crypto failure above still aborts).
    equity_note = ""
    equity_underlyings = read_equity_underlyings(dailies_path)
    if equity_underlyings:
        eq_date = equity_refresh_date(now_utc)
        eq_resolved: list[ResolvedMarket] = []
        eq_ok = True
        for u in equity_underlyings:
            market = resolve_market(get_fn, host, u, eq_date)
            if market is None:
                print(
                    f"universe-refresh: equity {u} unresolved for {eq_date.isoformat()}"
                    " — carrying the previous equity tail (equity=stale)",
                    file=sys.stderr,
                )
                eq_ok = False
                break
            eq_resolved.append(market)
        if eq_ok:
            resolved.extend(eq_resolved)
            equity_note = f" equity={len(eq_resolved)}@{eq_date.isoformat()}"
        else:
            old = parse_markets_entries(text)
            tail = old[len(underlyings) :] if old is not None else []
            resolved.extend(tail)
            equity_note = f" equity=stale(carried={len(tail)})"

    new_text = rewrite_polymarket_markets(text, resolved)
    if new_text is None:
        print(
            f"universe-refresh: cannot locate [polymarket] markets array in {universe_path}",
            file=sys.stderr,
        )
        return 1
    tmp = universe_path.with_name(universe_path.name + ".tmp")
    try:
        tmp.write_text(new_text, encoding="utf-8")
        os.replace(tmp, universe_path)
    except OSError as e:
        print(f"universe-refresh: write failed: {e}", file=sys.stderr)
        return 1
    print(
        f"universe-refresh: {len(resolved)} market(s) for {date.isoformat()}"
        f"{equity_note} -> {universe_path}",
        file=sys.stderr,
    )
    return 0


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.universe_refresh")
    parser.add_argument("--universe", default=None, help="universe.toml path")
    parser.add_argument("--dailies", default=DEFAULT_DAILIES_PATH, help="pm-dailies.toml path")
    parser.add_argument("--date", default=None, help="override UTC now, ISO date (tests)")
    args = parser.parse_args(argv)
    universe = args.universe or os.environ.get(
        claude_worker.fetchers.UNIVERSE_FILE_ENV, ""
    ) or DEFAULT_UNIVERSE_PATH
    if args.date is not None:
        now = datetime.datetime.combine(
            datetime.date.fromisoformat(args.date),
            datetime.time(0, 0),
            tzinfo=datetime.timezone.utc,
        )
    else:
        now = datetime.datetime.now(datetime.timezone.utc)
    with httpx.Client() as client:
        return run(
            pathlib.Path(universe).expanduser(),
            pathlib.Path(args.dailies).expanduser(),
            now,
            make_get(client),
            os.environ,
        )


if __name__ == "__main__":
    raise SystemExit(main())
