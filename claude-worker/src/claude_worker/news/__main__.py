# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``python -m claude_worker.news <lane>`` — the package's CLI (spec §12).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Module surface only: the worker's 8-verb console surface is FROZEN, so every
NEWS lane is reached this way (the ``regime``/``candles``/``kronos``
precedent).

Lanes here: ``cycle`` (the launchd target), ``health``, ``probe``,
``migrate-feeds``, ``report``. The model-facing lanes (``prompts``,
``ingest``, ``actions``, ``resolve``, ``scorecard``, ``replay``) arrive with
their modules in N2/N3. **No lane here reaches a model.**

Degraded mode (the ``regime-cycle`` law): an absent ``news.toml`` is one
printed line and exit 0, never a traceback.

Exit codes: 0 ok · 1 best-effort failure (counted, logged) · 2 usage/config
error, which for ``probe --record`` also means the reduced fixture would
exceed ``sources.FIXTURE_MAX_BYTES``.
"""

import argparse
import json
import os
import pathlib
import sys
import time
import typing
import urllib.parse

import httpx

import claude_worker.news
import claude_worker.news.cycle
import claude_worker.news.detect
import claude_worker.news.filter
import claude_worker.news.sources
import claude_worker.news.store

EXIT_OK: int = 0
EXIT_REFUSED: int = 1
EXIT_TOO_BIG: int = 2

REPORT_HOURS_DEFAULT: int = 24
SECONDS_PER_HOUR: int = 3_600

#: Where ``--record`` writes when ``--out`` is absent: the tracked fixture
#: dir of this checkout (``tests/fixtures/news``), reached from this file so
#: the lane needs no env key. Fixtures are TESTS, not research.
_FIXTURE_DIR_PARTS: tuple[str, ...] = ("tests", "fixtures", "news")

#: The six SEO mills (Q6): seeded at weight 0.3 by ``migrate-feeds`` so a
#: pasted stanza is never an independent origin by accident.
MILL_HOSTS: tuple[str, ...] = (
    "cryptopotato.com",
    "cryptonews.com",
    "u.today",
    "ambcrypto.com",
    "newsbtc.com",
    "bitcoinist.com",
)
MILL_WEIGHT: float = 0.3


def default_fixture_dir() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parents[3].joinpath(*_FIXTURE_DIR_PARTS)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python -m claude_worker.news")
    # `--toml` belongs to every lane, not to the program, so the operator can
    # write `news cycle --toml <path>` the way every other worker lane reads.
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--toml", default="", help="news.toml override")
    lanes = parser.add_subparsers(dest="lane", required=True)

    lanes.add_parser(
        "cycle", parents=[common], help="fetch every due source, tier-0 it, store it (no model)"
    )
    lanes.add_parser(
        "health", parents=[common], help="per-source poll health; exit 1 on a deep error streak"
    )

    probe = lanes.add_parser(
        "probe", parents=[common], help="fetch ONE source once and report what parsed"
    )
    probe.add_argument("--source", required=True, help="source name from news.toml")
    probe.add_argument(
        "--record",
        action="store_true",
        help="write the reduced payload to the fixture dir (keyed fields only, <= 32 KB)",
    )
    probe.add_argument("--out", default="", help="fixture dir override")

    lanes.add_parser(
        "migrate-feeds", parents=[common], help="print source stanzas for $RSS_FEEDS"
    )

    report = lanes.add_parser("report", parents=[common], help="what the lane saw in a window")
    report.add_argument("--hours", type=int, default=REPORT_HOURS_DEFAULT)
    return parser


def _registry_path(paths: claude_worker.news.NewsPaths, override: str) -> pathlib.Path:
    return pathlib.Path(override).expanduser() if override else paths.toml_path


def _load_registry(
    paths: claude_worker.news.NewsPaths, override: str
) -> claude_worker.news.sources.Registry | None:
    """The registry, or ``None`` when there is none — the caller then exits 0
    having said so (the ``regime-cycle`` law)."""
    toml_path = _registry_path(paths, override)
    if not toml_path.is_file():
        sys.stdout.write(f"news: no registry at {toml_path} — nothing to do\n")
        return None
    return claude_worker.news.sources.load_registry(toml_path)


# ---------------------------------------------------------------- cycle


def _cycle(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    registry = _load_registry(paths, args.toml)
    if registry is None:
        return EXIT_OK
    now_ts = int(time.time())
    now_ns = time.monotonic_ns()
    take_until = now_ns + int(claude_worker.news.cycle.CYCLE_TAKE_UNTIL_S * 1_000_000_000)
    vocab = claude_worker.news.filter.vocabulary_from(
        paths.market_map_path, paths.replay_dir, registry.keywords
    )
    ctx = claude_worker.news.detect.context_from(paths)
    with claude_worker.news.store.Store(paths.db_path) as store:
        claude_worker.news.cycle.register_sources(store, registry)
        recent, used = claude_worker.news.cycle.load_recent(store, registry, now_ts)
        spent = store.budget_today("tier1", now_ts)["calls"]
        caps = claude_worker.news.cycle.build_caps(
            registry, used, claude_worker.news.filter.DEFAULT_TIER1_CALLS_PER_DAY - spent
        )
        with httpx.Client() as http:
            fetcher = claude_worker.news.sources.Fetcher(registry, http)
            stats = claude_worker.news.cycle.aggregate_once(
                fetcher,
                store,
                registry=registry,
                vocab=vocab,
                recent=recent,
                caps=caps,
                now_ns=now_ns,
                now_ts=now_ts,
                take_until_ns=take_until,
                ctx=ctx,
            )
        pruned = store.prune(
            now_ts,
            registry.settings.items_retention_days,
            registry.settings.snapshots_retention_days,
        )
    sys.stdout.write(stats.line() + "\n")
    sys.stdout.write(stats.detail() + f" pruned_items={pruned['items']}\n")
    return EXIT_OK


# ---------------------------------------------------------------- health


def _health(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    registry = _load_registry(paths, args.toml)
    if registry is None:
        return EXIT_OK
    if not paths.db_path.is_file():
        sys.stdout.write(f"news health: no store at {paths.db_path} — run `cycle` first\n")
        return EXIT_OK
    worst = 0
    with claude_worker.news.store.Store(paths.db_path) as store:
        rows = store.source_rows()
        counters = store.counters()
    header = f"{'source':<32} {'kind':<22} {'en':>2} {'ok/total':>12} {'streak':>6}  last_error"
    sys.stdout.write(header + "\n" + "-" * len(header) + "\n")
    for i in range(len(rows)):
        row = rows[i]
        total = int(typing.cast(int, row["polls_total"]))
        ok = int(typing.cast(int, row["polls_ok"]))
        streak = int(typing.cast(int, row["err_streak"]))
        enabled = int(typing.cast(int, row["enabled"]))
        if enabled and streak > worst:
            worst = streak
        ratio = f"{ok}/{total}"
        sys.stdout.write(
            f"{row['name']!s:<32} {row['kind']!s:<22} {enabled:>2} {ratio:>12} "
            f"{streak:>6}  {str(row['last_error'])[:40]}\n"
        )
    if counters:
        sys.stdout.write(f"counters: {json.dumps(counters, sort_keys=True)}\n")
    if worst >= claude_worker.news.cycle.ERR_STREAK_ALERT:
        sys.stderr.write(
            f"news health: an enabled source is {worst} consecutive failures deep\n"
        )
        return EXIT_REFUSED
    return EXIT_OK


# ---------------------------------------------------------------- probe


def _record(
    source: claude_worker.news.sources.Source, payload: str, out_dir: pathlib.Path, cap: int
) -> int:
    reduced = claude_worker.news.sources.reduce_payload(source.kind, payload, cap)
    size = len(reduced.encode("utf-8"))
    if size > claude_worker.news.sources.FIXTURE_MAX_BYTES:
        sys.stderr.write(
            f"probe: reduced fixture for {source.kind} is {size} B, over the "
            f"{claude_worker.news.sources.FIXTURE_MAX_BYTES} B cap\n"
        )
        return EXIT_TOO_BIG
    out_dir.mkdir(parents=True, exist_ok=True)
    suffix = claude_worker.news.sources.fixture_suffix(source.kind)
    target = out_dir / f"{source.kind}{suffix}"
    claude_worker.news.write_atomic(target, reduced)
    sys.stdout.write(f"probe: recorded {target} ({size} B)\n")
    return EXIT_OK


def _report_parsed(
    source: claude_worker.news.sources.Source, parsed: claude_worker.news.sources.Parsed
) -> None:
    summary: dict[str, object] = {
        "source": source.name,
        "kind": source.kind,
        "items": len(parsed.items),
        "snapshot": None if parsed.snapshot is None else parsed.snapshot.count,
        "series": len(parsed.series),
    }
    if parsed.items:
        summary["first_title"] = parsed.items[0].title[:120]
    sys.stdout.write(json.dumps(summary, sort_keys=True) + "\n")


def _probe(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    registry = _load_registry(paths, args.toml)
    if registry is None:
        return EXIT_OK
    source = registry.by_name(args.source)
    if source is None:
        sys.stderr.write(f"probe: no source named {args.source!r} in the registry\n")
        return EXIT_REFUSED
    now_ns = time.monotonic_ns()
    fetched_ts = int(time.time())
    with httpx.Client() as http:
        fetcher = claude_worker.news.sources.Fetcher(registry, http)
        result = fetcher.fetch(source, now_ns)
    if result.status != claude_worker.news.sources.FETCH_STATUS_OK or result.payload is None:
        sys.stderr.write(
            f"probe: {source.name} -> {result.status} (http {result.http_status}, "
            f"{result.elapsed_ms} ms)\n"
        )
        return EXIT_REFUSED
    parsed = claude_worker.news.sources.parse(
        source, result.payload, fetched_ts, text_cap=registry.settings.text_cap
    )
    _report_parsed(source, parsed)
    if parsed.is_empty():
        sys.stderr.write(f"probe: {source.name} parsed EMPTY — the wire shape moved\n")
        return EXIT_REFUSED
    if not args.record:
        return EXIT_OK
    out_dir = pathlib.Path(args.out).expanduser() if args.out else default_fixture_dir()
    return _record(source, result.payload, out_dir, registry.settings.text_cap)


# ---------------------------------------------------------- migrate-feeds


def rss_feeds_from_env(env: typing.Mapping[str, str] | None = None) -> tuple[str, ...]:
    """``RSS_FEEDS`` as a tuple (``config._parse_rss_feeds``'s contract,
    mirrored rather than imported so the lane needs no ``BaseConfig`` — which
    would demand the HMAC key and the replay dir just to print stanzas)."""
    source = os.environ if env is None else env
    out: list[str] = []
    parts = source.get("RSS_FEEDS", "").split(",")
    for i in range(len(parts)):
        item = parts[i].strip()
        if item:
            out.append(item)
    return tuple(out)


def stanza_for(url: str, taken: typing.AbstractSet[str]) -> tuple[str, str] | None:
    """One ``sources`` entry for a feed URL, as ``(name, stanza)``; ``None``
    when the URL has no host or its name is already taken."""
    host = (urllib.parse.urlsplit(url).hostname or "").lower()
    if not host:
        return None
    slug: list[str] = []
    for ch in host:
        slug.append(ch if ch.isalnum() else "-")
    name = "".join(slug).strip("-")[: claude_worker.news.sources.NAME_MAX_LEN]
    if not name or name in taken:
        return None
    weight = 1.0
    for i in range(len(MILL_HOSTS)):
        if host == MILL_HOSTS[i] or host.endswith("." + MILL_HOSTS[i]):
            weight = MILL_WEIGHT
    return name, (
        f'  {{ name = "{name}", kind = "rss", url = "{url}", origin = "{host}", '
        f'class = "C", poll_s = 300, weight = {weight}, enabled = 1 }},'
    )


def _migrate_feeds(args: argparse.Namespace) -> int:
    del args
    feeds = rss_feeds_from_env()
    if not feeds:
        sys.stdout.write("news migrate-feeds: RSS_FEEDS is empty — nothing to migrate\n")
        return EXIT_OK
    sys.stdout.write("# paste into [registry].sources of ~/multivenue/news.toml,\n")
    sys.stdout.write("# then `probe --source <name>` each one before trusting it.\n")
    taken: set[str] = set()
    for i in range(len(feeds)):
        made = stanza_for(feeds[i], taken)
        if made is None:
            sys.stdout.write(f"  # skipped (no host or duplicate name): {feeds[i]}\n")
            continue
        taken.add(made[0])
        sys.stdout.write(made[1] + "\n")
    return EXIT_OK


# ---------------------------------------------------------------- report


def _report(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    if not paths.db_path.is_file():
        sys.stdout.write(f"news report: no store at {paths.db_path}\n")
        return EXIT_OK
    since = int(time.time()) - max(1, args.hours) * SECONDS_PER_HOUR
    with claude_worker.news.store.Store(paths.db_path) as store:
        items = store.items_since(since)
        events = store.events_since(since)
        actions = store.actions_since(since)
        counters = store.counters()
    by_verdict: dict[str, int] = {}
    by_source: dict[str, int] = {}
    for i in range(len(items)):
        verdict = str(items[i]["tier0"])
        by_verdict[verdict] = by_verdict.get(verdict, 0) + 1
        if verdict == claude_worker.news.filter.TIER0_PASS:
            name = str(items[i]["source"])
            by_source[name] = by_source.get(name, 0) + 1
    sys.stdout.write(f"news report: last {args.hours} h\n")
    sys.stdout.write(f"  items={len(items)} by_verdict={json.dumps(by_verdict, sort_keys=True)}\n")
    sys.stdout.write(f"  passes by source: {json.dumps(by_source, sort_keys=True)}\n")
    sys.stdout.write(f"  events={len(events)} actions={len(actions)}\n")
    for i in range(len(events)):
        row = events[i]
        sys.stdout.write(
            f"    event {row['kind']} {row['venue']} {row['instrument']} @{row['at_ts']}\n"
        )
    if counters:
        sys.stdout.write(f"  counters: {json.dumps(counters, sort_keys=True)}\n")
    return EXIT_OK


_LANES: dict[str, typing.Callable[[argparse.Namespace], int]] = {
    "cycle": _cycle,
    "health": _health,
    "probe": _probe,
    "migrate-feeds": _migrate_feeds,
    "report": _report,
}


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    lane = _LANES.get(args.lane)
    if lane is None:
        return EXIT_TOO_BIG
    return lane(args)


if __name__ == "__main__":
    sys.exit(main())
