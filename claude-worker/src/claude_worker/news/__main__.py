# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``python -m claude_worker.news <lane>`` — the package's CLI (spec §12).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Module surface only: the worker's 8-verb console surface is FROZEN, so every
NEWS lane is reached this way (the ``regime``/``candles``/``kronos``
precedent). N1.1 ships ONE lane — ``probe`` — because Appendix B's fixture
law makes it the tool that produces N1.1's own gate: every parser kind needs
a recorded fixture, and a fixture is recorded by ``probe --record`` on the
Mac (the sandbox has no network). ``cycle``, ``health``, ``migrate-feeds``
and ``report`` land in N1.2.

Degraded mode (the ``regime-cycle`` law): an absent ``news.toml`` is an
honest no-op with exit 0, never a traceback.

Exit codes: 0 ok · 1 refused (unknown source, fetch/parse failure) · 2 the
recorded fixture would exceed ``sources.FIXTURE_MAX_BYTES``.
"""

import argparse
import json
import pathlib
import sys
import time
import typing

import httpx

import claude_worker.news
import claude_worker.news.sources

EXIT_OK: int = 0
EXIT_REFUSED: int = 1
EXIT_TOO_BIG: int = 2

#: Where ``--record`` writes when ``--out`` is absent: the tracked fixture
#: dir of this checkout (``tests/fixtures/news``), reached from this file so
#: the lane needs no env key. Fixtures are TESTS, not research.
_FIXTURE_DIR_PARTS: tuple[str, ...] = ("tests", "fixtures", "news")


def default_fixture_dir() -> pathlib.Path:
    return pathlib.Path(__file__).resolve().parents[3].joinpath(*_FIXTURE_DIR_PARTS)


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="python -m claude_worker.news")
    lanes = parser.add_subparsers(dest="lane", required=True)
    probe = lanes.add_parser("probe", help="fetch ONE source once and report what parsed")
    probe.add_argument("--source", required=True, help="source name from news.toml")
    probe.add_argument(
        "--record",
        action="store_true",
        help="write the reduced payload to the fixture dir (keyed fields only, <= 32 KB)",
    )
    probe.add_argument("--out", default="", help="fixture dir override")
    probe.add_argument("--toml", default="", help="news.toml override")
    return parser


def _load_registry(
    paths: claude_worker.news.NewsPaths, override: str
) -> claude_worker.news.sources.Registry | None:
    toml_path = pathlib.Path(override).expanduser() if override else paths.toml_path
    if not toml_path.is_file():
        sys.stdout.write(f"news: no registry at {toml_path} — nothing to do\n")
        return None
    return claude_worker.news.sources.load_registry(toml_path)


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


def _report(source: claude_worker.news.sources.Source, parsed: object) -> None:
    result = typing.cast(claude_worker.news.sources.Parsed, parsed)
    summary: dict[str, object] = {
        "source": source.name,
        "kind": source.kind,
        "items": len(result.items),
        "snapshot": None if result.snapshot is None else result.snapshot.count,
        "series": len(result.series),
    }
    if result.items:
        summary["first_title"] = result.items[0].title[:120]
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
    _report(source, parsed)
    if parsed.is_empty():
        sys.stderr.write(f"probe: {source.name} parsed EMPTY — the wire shape moved\n")
        return EXIT_REFUSED
    if not args.record:
        return EXIT_OK
    out_dir = pathlib.Path(args.out).expanduser() if args.out else default_fixture_dir()
    return _record(source, result.payload, out_dir, registry.settings.text_cap)


def main(argv: list[str] | None = None) -> int:
    args = _build_parser().parse_args(argv)
    if args.lane == "probe":
        return _probe(args)
    return EXIT_REFUSED


if __name__ == "__main__":
    sys.exit(main())
