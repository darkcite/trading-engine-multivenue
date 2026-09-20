# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``python -m claude_worker.news <lane>`` — the package's CLI (spec §12).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Module surface only: the worker's 8-verb console surface is FROZEN, so every
NEWS lane is reached this way (the ``regime``/``candles``/``kronos``
precedent).

Lanes: ``cycle`` (the launchd target), ``health``, ``probe``,
``migrate-feeds``, ``report``, ``proposals``, and the six of §14/§12 —
``prompts``, ``ingest``, ``actions``, ``resolve``, ``scorecard``, ``replay``.

**No lane here reaches a model** (a test asserts it). ``prompts`` writes
questions to a file and ``ingest`` reads answers back from one; the tagger
in between is a session, and the answers are held to the same strict
parsers, counters, ceilings and resolutions a model's would be
(`claude_worker.news.session`). The three tiers land under
``model = "session"``, which is the only thing that separates them from an
automated answer in the scorecard.

Degraded mode (the ``regime-cycle`` law): an absent ``news.toml`` is one
printed line and exit 0, never a traceback.

Exit codes: 0 ok · 1 best-effort failure (counted, logged) · 2 usage/config
error, which for ``probe --record`` also means the reduced fixture would
exceed ``sources.FIXTURE_MAX_BYTES``.
"""

import argparse
import hashlib
import json
import os
import pathlib
import sys
import time
import typing
import urllib.parse

import httpx

import claude_worker.cli
import claude_worker.news
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.cycle
import claude_worker.news.detect
import claude_worker.news.filter
import claude_worker.news.local_llm
import claude_worker.news.resolve
import claude_worker.news.session
import claude_worker.news.sources
import claude_worker.news.store
import claude_worker.regime
import claude_worker.state
import claude_worker.strategist

EXIT_OK: int = 0
EXIT_REFUSED: int = 1
EXIT_TOO_BIG: int = 2

REPORT_HOURS_DEFAULT: int = 24
SECONDS_PER_HOUR: int = 3_600

#: Calendar entries and recent class-A events rendered into the analyst's
#: context block. Both are already capped by the file and the window; these
#: bound what one prompt spends on them.
ANALYST_CALENDAR: int = 12
ANALYST_EVENTS: int = 20

#: ``scorecard --window``; the names `resolve.WINDOWS` uses.
SCORECARD_WINDOWS: tuple[str, ...] = ("7d", "30d", "all")
#: ``replay`` columns, in order. The N4 vault tool reads this header.
REPLAY_COLUMNS: tuple[str, ...] = (
    "story_id",
    "market",
    "descriptor",
    "direction",
    "confidence",
    "half_life_s",
    "ts",
    "fwd_bps",
    "hit",
    "vol_reached_high",
    "rv_ratio",
    "model",
)

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

    cycle = lanes.add_parser(
        "cycle", parents=[common], help="fetch every due source, tier-0 it, store it (no model)"
    )
    cycle.add_argument(
        "--max-resolve",
        type=int,
        default=claude_worker.news.resolve.RESOLVE_BATCH,
        help="resolutions scored in this cycle; the wall budget is 30 s and each one is two "
        "indexed reads of candles.db, so the default is the batch, not a fraction of it",
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

    lanes.add_parser(
        "proposals", parents=[common], help="the universe/xsd proposals not yet applied"
    )

    llm = lanes.add_parser(
        "llm-args", parents=[common], help="verify llm.toml and print the llama-server argv"
    )
    llm.add_argument(
        "--fallback",
        action="store_true",
        help="render the fallback model instead of the primary (doc 03 §5's downsize)",
    )
    llm.add_argument(
        "--skip-sha",
        action="store_true",
        help="print the argv without hashing the weights (a 5 GB shasum is ~10 s)",
    )

    lanes.add_parser(
        "llm-health", parents=[common], help="what the local sidecar reports about itself"
    )

    prompts = lanes.add_parser(
        "prompts", parents=[common], help="write one tier's pending prompts as NDJSON (§14)"
    )
    prompts.add_argument("--tier", type=int, choices=(1, 2, 3), required=True)
    prompts.add_argument(
        "--limit", type=int, default=claude_worker.news.session.PROMPTS_LIMIT_DEFAULT
    )
    prompts.add_argument("--out", default="", help="NDJSON path; default is under news_dir")

    ingest = lanes.add_parser(
        "ingest", parents=[common], help="read one tier's answers NDJSON back (§14)"
    )
    ingest.add_argument("--tier", type=int, choices=(1, 2, 3), required=True)
    ingest.add_argument("--answers", required=True, help="NDJSON of {id, response} lines")

    actions = lanes.add_parser(
        "actions", parents=[common], help="policy -> recorded actions (shadow unless live)"
    )
    actions.add_argument(
        "--dry-run",
        action="store_true",
        help="downgrade every LIVE mode to shadow for this run; an off mode stays off",
    )

    resolve = lanes.add_parser(
        "resolve", parents=[common], help="score every claim whose horizon elapsed (§9.6)"
    )
    resolve.add_argument("--max", type=int, default=claude_worker.news.resolve.RESOLVE_BATCH)

    scorecard = lanes.add_parser("scorecard", parents=[common], help="print the scorecard")
    scorecard.add_argument(
        "--window", choices=SCORECARD_WINDOWS, default=claude_worker.news.resolve.WINDOW_ALL
    )

    replay = lanes.add_parser(
        "replay", parents=[common], help="export labels + resolutions as TSV for the N4 tool"
    )
    replay.add_argument("--since", required=True, help="ISO stamp, e.g. 2026-09-01T00:00:00Z")
    replay.add_argument("--until", default="", help="ISO stamp; default now")
    replay.add_argument("--out", required=True)
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


def _policy(paths: claude_worker.news.NewsPaths) -> claude_worker.news.actions.NewsPolicy:
    """The policy, or the SAFE one. An absent `news-policy.toml` is the
    shipped state: every mode `off`, nothing reaches a venue."""
    return claude_worker.news.actions.load_policy(paths.policy_path)


def _markets(paths: claude_worker.news.NewsPaths) -> dict[str, int]:
    """The market MENU — the map filtered to what can actually be traded and
    named (`filter.tradeable_markets`). A missing file narrows what a tier
    may name; it never stops a lane (`filter.vocabulary_from`'s contract).

    Every consumer of a market NAME goes through here — the tier-2 prompt,
    the strict parse that validates its answer, the analyst context and the
    emitter's name→sym resolution — so a market that has settled cannot be
    offered, named, or acted on. Tier 0's vocabulary deliberately does NOT
    come through here: a wider net costs nothing there, and its job is to
    keep items, not to price them.
    """
    return claude_worker.news.filter.tradeable_markets_from(
        paths.market_map_path, paths.replay_dir
    )


def _vocab(
    paths: claude_worker.news.NewsPaths, registry: claude_worker.news.sources.Registry
) -> claude_worker.news.filter.Vocabulary:
    return claude_worker.news.filter.vocabulary_from(
        paths.market_map_path, paths.replay_dir, registry.keywords
    )


def _out_path(paths: claude_worker.news.NewsPaths, override: str, name: str) -> pathlib.Path:
    return pathlib.Path(override).expanduser() if override else paths.file(name)


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
    policy = _policy(paths)
    state = claude_worker.state.State(paths.state_db_path)
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
        # doc 03 §4: the local tiers, after the aggregator. Bounded to
        # `tier1_batch_max` + `tier2_batch_max` per invocation and skipped
        # entirely unless `llm.toml` exists AND the policy routes a tier at
        # `local:…` AND the sidecar is up and serving the pinned model. Every
        # one of those is a counted refusal to ask, never a failure.
        llm = claude_worker.news.local_llm.run_local_tiers(
            state=state,
            store=store,
            registry=registry,
            policy=policy,
            cfg=claude_worker.news.local_llm.load_config(paths.llm_path),
            markets=_markets(paths),
            vocab=vocab,
            now_ts=now_ts,
        )
        # §12: the cycle also scores what has come due and rewrites the
        # scorecard. It costs no model call and no network — the prices come
        # from the 1 m candles lane — and it is what keeps `scorecard.json`
        # as fresh as the panel that reads it.
        resolved = claude_worker.news.resolve.resolve_due(store, paths, now_ts, args.max_resolve)
        claude_worker.news.resolve.write_scorecard(
            paths.file(claude_worker.news.SCORECARD_FILE),
            claude_worker.news.resolve.build_scorecard(store, now_ts, policy.ceilings),
        )
        pruned = store.prune(
            now_ts,
            registry.settings.items_retention_days,
            registry.settings.snapshots_retention_days,
        )
    state.close()
    stats.resolved = resolved.resolved
    sys.stdout.write(stats.line() + "\n")
    sys.stdout.write(stats.detail() + f" pruned_items={pruned['items']}\n")
    if llm.healthy or llm.calls or llm.tier1 or llm.tier2:
        sys.stdout.write("  " + llm.line() + "\n")
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


# ------------------------------------------------------------- proposals


def _proposals(args: argparse.Namespace) -> int:
    """The two proposal files, verbatim. They are the operator's to apply
    (ruling Q10), so this lane prints and changes nothing."""
    del args
    paths = claude_worker.news.paths_from_env()
    printed = 0
    for name in (
        claude_worker.news.UNIVERSE_PROPOSALS_FILE,
        claude_worker.news.XSD_PROPOSALS_FILE,
    ):
        path = paths.file(name)
        if not path.is_file():
            continue
        sys.stdout.write(f"--- {path}\n")
        try:
            sys.stdout.write(path.read_text(encoding="utf-8"))
        except OSError as exc:
            sys.stdout.write(f"(unreadable: {exc})\n")
            continue
        printed += 1
    if not printed:
        sys.stdout.write(f"news proposals: none under {paths.news_dir}\n")
    return EXIT_OK


# ----------------------------------------------------- §14 prompts / ingest


def _session_inputs(
    args: argparse.Namespace,
) -> tuple[
    claude_worker.news.NewsPaths,
    claude_worker.news.sources.Registry,
    claude_worker.news.filter.Vocabulary,
    dict[str, int],
] | None:
    """The four things both §14 lanes need, or ``None`` when there is no
    registry (the caller then exits 0 having said so)."""
    paths = claude_worker.news.paths_from_env()
    registry = _load_registry(paths, args.toml)
    if registry is None:
        return None
    return paths, registry, _vocab(paths, registry), _markets(paths)


def _prompts(args: argparse.Namespace) -> int:
    inputs = _session_inputs(args)
    if inputs is None:
        return EXIT_OK
    paths, registry, vocab, markets = inputs
    tier = claude_worker.news.session.TIER_OF_NUMBER[args.tier]
    now_ts = int(time.time())
    out = _out_path(paths, args.out, f"prompts-{args.tier}-{now_ts}.ndjson")
    policy = _policy(paths)
    state = claude_worker.state.State(paths.state_db_path)
    try:
        with claude_worker.news.store.Store(paths.db_path) as store:
            watcher = claude_worker.news.session.build_watcher(
                state=state,
                store=store,
                registry=registry,
                markets=markets,
                vocab=vocab,
                now_ts=now_ts,
                ceilings=policy.ceilings,
                context_fn=lambda: _analyst_context(paths, store, markets),
            )
            if tier == claude_worker.news.cascade.TIER1:
                lines = claude_worker.news.session.tier1_prompts(
                    watcher, store, registry, vocab, args.limit
                )
            elif tier == claude_worker.news.cascade.TIER2:
                lines = claude_worker.news.session.tier2_prompts(
                    watcher, store, registry, now_ts, args.limit
                )
            else:
                lines = claude_worker.news.session.tier3_prompts(watcher, args.limit)
            written = claude_worker.news.session.write_prompts(out, lines)
    finally:
        state.close()
    sys.stdout.write(f"news prompts: tier={args.tier} prompts={written} out={out}\n")
    return EXIT_OK


def _ingest(args: argparse.Namespace) -> int:
    inputs = _session_inputs(args)
    if inputs is None:
        return EXIT_OK
    paths, registry, vocab, markets = inputs
    tier = claude_worker.news.session.TIER_OF_NUMBER[args.tier]
    answers_path = pathlib.Path(args.answers).expanduser()
    if not answers_path.is_file():
        sys.stderr.write(f"news ingest: no answers file at {answers_path}\n")
        return EXIT_TOO_BIG
    answers, bad = claude_worker.news.session.read_answers(answers_path)
    now_ts = int(time.time())
    policy = _policy(paths)
    state = claude_worker.state.State(paths.state_db_path)
    try:
        with claude_worker.news.store.Store(paths.db_path) as store:
            common: dict[str, object] = {
                "state": state,
                "store": store,
                "registry": registry,
                "markets": markets,
                "vocab": vocab,
                "answers": answers,
                "now_ts": now_ts,
                "ceilings": policy.ceilings,
            }
            if tier == claude_worker.news.cascade.TIER3:
                stats = claude_worker.news.session.ingest_tier3(
                    context_fn=lambda: _analyst_context(paths, store, markets),
                    **typing.cast(typing.Any, common),
                )
            else:
                stats = claude_worker.news.session.ingest_tier12(
                    tier=tier, **typing.cast(typing.Any, common)
                )
    finally:
        state.close()
    stats.bad_lines = bad
    sys.stdout.write(stats.line(tier) + "\n")
    return EXIT_REFUSED if stats.rejected or stats.bad_lines else EXIT_OK


def _analyst_context(
    paths: claude_worker.news.NewsPaths,
    store: claude_worker.news.store.Store,
    markets: typing.Mapping[str, int],
) -> claude_worker.news.cascade.AnalystContext:
    """The engine's state, rendered for the analyst block (§9.4).

    Every read is best-effort and every failure narrows the context rather
    than failing the lane: an analyst told "regime unavailable" is being
    told the truth, and one told nothing at all would invent something.
    """
    words = claude_worker.regime.engine_words(paths.metrics_url)
    regime = "unavailable"
    if words:
        parts: list[str] = []
        for key in sorted(words):
            if key.endswith("_effective"):
                parts.append(f"{key}={claude_worker.regime.word_hex(words[key])}")
        regime = " ".join(parts) or "unavailable"
    calendar = _render_json_tail(paths.file(claude_worker.news.CALENDAR_FILE), "events")
    # `store.recent_events`, not `events_since`: that query orders by the
    # event's own instant, so 190 Deribit option expiries dated 18-72 h out
    # would BE the analyst's whole EVENTS block and it would never see a
    # listing or an outage. Measured 2026-09-20 on the live store.
    events = claude_worker.news.store.event_lines(
        claude_worker.news.store.recent_events(
            store,
            int(time.time()) - SECONDS_PER_HOUR * REPORT_HOURS_DEFAULT,
            ANALYST_EVENTS,
        )
    )
    return claude_worker.news.cascade.AnalystContext(
        markets=tuple(sorted(markets)),
        descriptors=tuple(sorted(claude_worker.news.detect.manifest_descriptors(paths.replay_dir))),
        regime=regime,
        calendar=calendar or "(none)",
        positions=claude_worker.strategist.positions_digest_text(
            claude_worker.strategist.gather_positions_payload(paths.replay_dir)
        )
        or "(none)",
        events="\n".join(events) or "(none)",
    )


def _render_json_tail(path: pathlib.Path, key: str) -> str:
    """The first `ANALYST_CALENDAR` entries of a §4.4 file's list, one per
    line. Absent or unreadable renders empty, never raises."""
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return ""
    rows = doc.get(key) if isinstance(doc, dict) else None
    if not isinstance(rows, list):
        return ""
    out: list[str] = []
    for i in range(min(ANALYST_CALENDAR, len(rows))):
        row = rows[i]
        if not isinstance(row, dict):
            continue
        out.append(
            f"{claude_worker.news.detect.iso(int(row.get('at_ts', 0)))} "
            f"{row.get('kind', '')} {row.get('detail', '')}"
        )
    return "\n".join(out)


# ---------------------------------------------------------------- actions


def _actions(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    registry = _load_registry(paths, args.toml)
    if registry is None:
        return EXIT_OK
    policy = _policy(paths)
    if args.dry_run:
        policy = claude_worker.news.actions.shadow_only(policy)
    now_ts = int(time.time())
    ctx = claude_worker.news.detect.context_from(paths)
    state = claude_worker.state.State(paths.state_db_path)
    try:
        with claude_worker.news.store.Store(paths.db_path) as store:
            if not policy.valid:
                store.counter_inc(claude_worker.news.store.COUNTER_POLICY_INVALID)
            # No client: the lane path never connects, so nothing it records
            # can reach a venue whatever the policy says (spec §10.1). Live
            # sends belong to `serve`, which owns the connection.
            emitter = claude_worker.news.actions.Emitter(
                state=state,
                store=store,
                policy=policy,
                market_map=_markets(paths),
                paths=paths,
                ctx=ctx,
            )
            stats = emitter.run(now_ts, now_ns=time.monotonic_ns())
    finally:
        state.close()
    sys.stdout.write(
        f"news actions: shadow={stats.shadow} live={stats.live} refused={stats.refused} "
        f"read={stats.read} recorded={stats.recorded} alerts={stats.alerts} "
        f"proposals={stats.proposals}"
        + (" (dry-run)" if args.dry_run else "")
        + "\n"
    )
    return EXIT_OK


# ------------------------------------------------------ resolve / scorecard


def _resolve(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    if not paths.db_path.is_file():
        sys.stdout.write(f"news resolve: no store at {paths.db_path}\n")
        return EXIT_OK
    now_ts = int(time.time())
    ceilings = _policy(paths).ceilings
    with claude_worker.news.store.Store(paths.db_path) as store:
        stats = claude_worker.news.resolve.resolve_due(store, paths, now_ts, args.max)
        doc = claude_worker.news.resolve.build_scorecard(store, now_ts, ceilings)
    written = claude_worker.news.resolve.write_scorecard(
        paths.file(claude_worker.news.SCORECARD_FILE), doc
    )
    sys.stdout.write(
        f"news resolve: due={stats.due} resolved={stats.resolved} "
        f"pending={stats.still_pending} unresolvable={stats.unresolvable}\n"
    )
    return EXIT_OK if written else EXIT_REFUSED


def _scorecard(args: argparse.Namespace) -> int:
    paths = claude_worker.news.paths_from_env()
    doc = None
    path = paths.file(claude_worker.news.SCORECARD_FILE)
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        doc = None
    if not isinstance(doc, dict):
        sys.stdout.write(f"news scorecard: nothing at {path} — run `resolve` first\n")
        return EXIT_OK
    windows = doc.get("windows")
    window = windows.get(args.window) if isinstance(windows, dict) else None
    if not isinstance(window, dict):
        sys.stdout.write(f"news scorecard: no window {args.window!r} in {path}\n")
        return EXIT_REFUSED
    sys.stdout.write(
        f"news scorecard: window={args.window} generated="
        f"{claude_worker.news.detect.iso(int(doc.get('generated_ts', 0)))}\n"
    )
    for name in ("direction", "vol", "intents"):
        section = window.get(name)
        if isinstance(section, dict):
            sys.stdout.write(f"  {name}: {json.dumps(section, sort_keys=True)}\n")
    for name in ("reliability", "lead_time", "cost_24h", "unresolvable", "pending"):
        if name in doc:
            sys.stdout.write(f"  {name}: {json.dumps(doc[name], sort_keys=True)}\n")
    sys.stdout.write(f"  legend: {doc.get('legend', '')}\n")
    return EXIT_OK


# ----------------------------------------------------------------- replay


def _replay(args: argparse.Namespace) -> int:
    """Labels joined to their resolutions, as TSV, for the N4 vault tool.

    Only RESOLVED rows: an unresolved claim has no forward return, and
    exporting it with a zero is how a null becomes a result.
    """
    paths = claude_worker.news.paths_from_env()
    if not paths.db_path.is_file():
        sys.stdout.write(f"news replay: no store at {paths.db_path}\n")
        return EXIT_OK
    try:
        since = claude_worker.news.detect.parse_iso(args.since)
        until = (
            claude_worker.news.detect.parse_iso(args.until) if args.until else int(time.time())
        )
    except ValueError as exc:
        sys.stderr.write(f"news replay: {exc}\n")
        return EXIT_TOO_BIG
    rows: list[str] = ["\t".join(REPLAY_COLUMNS)]
    exported = 0
    with claude_worker.news.store.Store(paths.db_path) as store:
        resolved = store.resolutions_resolved(since)
        for i in range(len(resolved)):
            row = resolved[i]
            if str(row["subject_kind"]) != claude_worker.news.resolve.SUBJECT_LABEL:
                continue
            if int(typing.cast(int, row["resolved_ts"])) > until:
                continue
            story_id = str(row["subject_id"])
            label = store.label(story_id)
            if label is None:
                continue
            rows.append(
                "\t".join(
                    (
                        story_id,
                        str(label["market"]),
                        str(row["descriptor"]),
                        str(row["direction"]),
                        f"{float(typing.cast(float, row['confidence'])):.4f}",
                        f"{float(typing.cast(float, label['half_life_s'])):.1f}",
                        str(row["t0"]),
                        f"{float(typing.cast(float, row['fwd_bps'])):.4f}",
                        str(int(typing.cast(int, row["hit"]))),
                        str(int(typing.cast(int, row["vol_reached_high"]))),
                        f"{float(typing.cast(float, row['rv_ratio'])):.4f}",
                        str(row["model"]),
                    )
                )
            )
            exported += 1
    out = pathlib.Path(args.out).expanduser()
    claude_worker.news.write_atomic(out, "\n".join(rows) + "\n")
    sys.stdout.write(f"news replay: rows={exported} out={out}\n")
    return EXIT_OK


# ------------------------------------------------------- the local sidecar


def _llm_args(args: argparse.Namespace) -> int:
    """The `llama-server` argv, after verifying the weights.

    `scripts/llm-serve.sh` execs what this prints, so the shell script carries
    no model path and no tuning number: the operator's `llm.toml` is the only
    source, and a weight file that does not match its pinned sha256 is a
    REFUSAL here rather than a model silently swapped under a tag the
    scorecard splits on.
    """
    paths = claude_worker.news.paths_from_env()
    cfg = claude_worker.news.local_llm.load_config(paths.llm_path)
    if not cfg.present:
        sys.stderr.write(f"llm-args: no config at {paths.llm_path}\n")
        return EXIT_REFUSED
    if not cfg.valid:
        sys.stderr.write(f"llm-args: {paths.llm_path} is not a valid llm.toml\n")
        return EXIT_TOO_BIG
    model = cfg.fallback_model_path if args.fallback else cfg.model_path
    want = cfg.fallback_sha256 if args.fallback else cfg.sha256
    if not model.is_file():
        sys.stderr.write(f"llm-args: no weights at {model}\n")
        return EXIT_TOO_BIG
    if want and not args.skip_sha:
        got = _sha256_of(model)
        if got != want:
            sys.stderr.write(
                f"llm-args: {model.name} is sha256 {got[:16]}…, "
                f"llm.toml pins {want[:16]}… — REFUSING\n"
            )
            return EXIT_TOO_BIG
    argv = claude_worker.news.local_llm.server_argv(cfg)
    if args.fallback:
        for i in range(len(argv)):
            if argv[i] == "-m":
                argv[i + 1] = str(model)
    sys.stdout.write(" ".join(argv) + "\n")
    return EXIT_OK


def _sha256_of(path: pathlib.Path, chunk: int = 1 << 20) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            block = handle.read(chunk)
            if not block:
                break
            digest.update(block)
    return digest.hexdigest()


def _llm_health(args: argparse.Namespace) -> int:
    """What the sidecar says about itself, and whether it is the pinned model.

    Exit 1 when it is down or serving something else — so a wrapper or a
    scheduled check can act on it — but never a traceback: a sidecar that is
    not there is a lane that does less, not a lane that breaks.
    """
    del args
    paths = claude_worker.news.paths_from_env()
    cfg = claude_worker.news.local_llm.load_config(paths.llm_path)
    if not cfg.present:
        sys.stdout.write(f"llm health: no config at {paths.llm_path} — no local tiers\n")
        return EXIT_OK
    with claude_worker.news.local_llm.LocalClient(cfg.base_url) as client:
        up = client.health()
        props = client.props() if up else {}
        metrics = client.metrics() if up else {}
    stem = claude_worker.news.local_llm.props_model_stem(props)
    pinned = cfg.model_path.name.removesuffix(".gguf").lower()
    sys.stdout.write(
        f"llm health: {'up' if up else 'DOWN'} at {cfg.base_url} tag={cfg.model_tag}\n"
        f"  serving={stem or '(unreported)'} pinned={pinned} "
        f"match={'yes' if (not stem or stem == pinned) else 'NO'}\n"
    )
    if metrics:
        keys = sorted(metrics)
        shown: list[str] = []
        for i in range(len(keys)):
            if "llamacpp" in keys[i] or "prompt" in keys[i] or "tokens" in keys[i]:
                shown.append(f"{keys[i]}={metrics[keys[i]]:.0f}")
        sys.stdout.write("  metrics: " + (" ".join(shown[:10]) or "(none)") + "\n")
    if not up or (stem and stem != pinned):
        return EXIT_REFUSED
    return EXIT_OK


_LANES: dict[str, typing.Callable[[argparse.Namespace], int]] = {
    "cycle": _cycle,
    "health": _health,
    "probe": _probe,
    "migrate-feeds": _migrate_feeds,
    "report": _report,
    "proposals": _proposals,
    "llm-args": _llm_args,
    "llm-health": _llm_health,
    "prompts": _prompts,
    "ingest": _ingest,
    "actions": _actions,
    "resolve": _resolve,
    "scorecard": _scorecard,
    "replay": _replay,
}


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    lane = _LANES.get(args.lane)
    if lane is None:
        return EXIT_TOO_BIG
    return lane(args)


if __name__ == "__main__":
    sys.exit(main())
