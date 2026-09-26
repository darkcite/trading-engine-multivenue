# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har.toml -- the HAR H3 series list, the worker's reader.

``~/multivenue/har.toml`` names the long-tenor HAR series the engine runs
(``core_config::har`` parses the same file at boot). One ``[[series]]``
block per series, at most :data:`MAX_SERIES`::

    [[series]]
    name     = "BTC"                    # the Hypercall underlying: 1..=12 of [A-Z0-9]
    feed     = "binance-usdm:btcusdt"   # the live minute source (a boot-universe descriptor)
    fallback = ["binance:btcusdt"]      # the SEED's older sources, newest first

The engine reads ``name`` and ``feed`` and ignores ``fallback`` once it has
checked it; the worker reads all three -- :mod:`claude_worker.har_backfill`
fetches every source's 1 m history, and :mod:`claude_worker.har_seed`
splices them into one replay (each fallback fills only the minutes before
the previous source's first minute).

THE ENGINE'S GRAMMAR, LINE FOR LINE. The file is the TOML subset every
``core_config`` artifact reads (``icdp.rs``'s ``strip_comment`` /
``parse_value``): ``[[series]]`` headers, one ``key = value`` per line, a
value is a ``"string"`` without escapes, an integer, or a ONE-line array.
This reader is that parser, not ``tomllib`` -- standard TOML accepts files
the engine refuses (a multi-line array, a literal string, an inline table),
and the worker must never cut seeds from a file the engine would refuse.
Every refusal names the line, as the engine's does: an unknown section or
key, a key before any header, a duplicate key, a bad name (1..=12 of
``[A-Z0-9]``), a bad descriptor (:func:`valid_descriptor`), more than
:data:`MAX_FALLBACKS` fallbacks, a fallback that repeats the feed or
itself, a duplicate name or feed, none or more than :data:`MAX_SERIES`
series.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import os
import pathlib
import typing

#: Where the file lives unless ``--har-toml`` (or the env) says otherwise.
DEFAULT_PATH: str = "~/multivenue/har.toml"
PATH_ENV: str = "CLAUDE_WORKER_HAR_TOML"
#: The engine boxes at most this many ``LongVolEngine``s (the Hypercall
#: underlyings, O-HC5) -- ``core_config::har::HAR_MAX_SERIES``.
MAX_SERIES: int = 12
#: Older sources a series may splice in front of its feed.
MAX_FALLBACKS: int = 4
#: ``name`` is 1..=12 bytes of ``[A-Z0-9]``.
NAME_MAX: int = 12
#: A descriptor is at most this many bytes.
DESCRIPTOR_MAX: int = 64
#: The keys a ``[[series]]`` block may carry (``HAR_KEYS`` in the engine).
SERIES_KEYS: tuple[str, ...] = ("name", "feed", "fallback")
_HEADER: str = "[[series]]"
_INT_DIGITS_MAX: int = 19
_I64_MAX: int = (1 << 63) - 1
_INSTRUMENT_BYTES: frozenset[str] = frozenset(
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_.:/-"
)

#: A parsed value: a string, an integer, or a one-line array of either.
Value = str | int | list[str] | list[int]


@dataclasses.dataclass(frozen=True, slots=True)
class Series:
    """One ``[[series]]`` block."""

    name: str
    feed: str
    fallback: tuple[str, ...]

    def sources(self) -> tuple[str, ...]:
        """The feed, then its fallbacks: newest source first."""
        return (self.feed, *self.fallback)


class HarConfigError(ValueError):
    """A ``har.toml`` the engine would refuse; the message names why."""


def valid_name(s: str) -> bool:
    """1..=12 bytes of ``[A-Z0-9]`` -- a Hypercall underlying's own spelling."""
    return 0 < len(s) <= NAME_MAX and all(c.isascii() and (c.isupper() or c.isdigit()) for c in s)


def valid_descriptor(s: str) -> bool:
    """``<venue>:<instrument>`` in at most 64 bytes: the venue is
    ``[a-z][a-z0-9-]*``, the instrument one or more of ``[A-Za-z0-9_.:/-]``
    (``okx:MU-USDT-SWAP``, ``hyperliquid:xyz:SP500``, ``mexc-perp:SPY_USDT``)."""
    venue, sep, inst = s.partition(":")
    return (
        bool(sep)
        and len(s) <= DESCRIPTOR_MAX
        and bool(venue)
        and "a" <= venue[0] <= "z"
        and all(c.isascii() and (c.islower() or c.isdigit() or c == "-") for c in venue)
        and bool(inst)
        and all(c in _INSTRUMENT_BYTES for c in inst)
    )


def _strip_comment(line: str) -> str:
    """A trailing ``#`` comment (outside a string) and the surrounding
    whitespace removed."""
    in_str = False
    for i, ch in enumerate(line):
        if ch == '"':
            in_str = not in_str
        elif ch == "#" and not in_str:
            return line[:i].strip()
    return line.strip()


def _parse_int(t: str, ln: int) -> int:
    digits = t.removeprefix("-")
    if not digits or len(digits) > _INT_DIGITS_MAX or not (digits.isascii() and digits.isdigit()):
        raise HarConfigError(f"line {ln}: bad integer `{t}`")
    if len(digits) > 1 and digits.startswith("0"):
        raise HarConfigError(f"line {ln}: leading zero in `{t}`")
    v = int(digits)
    if v > _I64_MAX:
        raise HarConfigError(f"line {ln}: integer overflow `{t}`")
    return -v if t.startswith("-") else v


def _parse_str(t: str, ln: int) -> str:
    if not t.startswith('"'):
        raise HarConfigError(f"line {ln}: expected a quoted string")
    if len(t) < 2 or not t.endswith('"'):  # noqa: PLR2004 - the two quotes
        raise HarConfigError(f"line {ln}: unterminated string")
    body = t[1:-1]
    if '"' in body or "\\" in body:
        raise HarConfigError(f"line {ln}: strings carry no escapes")
    return body


def _parse_value(s: str, ln: int) -> Value:
    t = s.strip()
    if t.startswith('"'):
        return _parse_str(t, ln)
    if t.startswith("["):
        if not t.endswith("]"):
            raise HarConfigError(f"line {ln}: unterminated array")
        body = t[1:-1]
        parts = [p.strip() for p in body.split(",") if p.strip()]
        if body.lstrip().startswith('"'):
            return [_parse_str(p, ln) for p in parts]
        return [_parse_int(p, ln) for p in parts]
    return _parse_int(t, ln)


#: One block's ``key -> (value, line)``, and the block's header line.
_Block = tuple[dict[str, tuple[Value, int]], int]


def _blocks(text: str) -> list[_Block]:
    """The ``[[series]]`` blocks of ``text`` (the engine's line loop)."""
    out: list[_Block] = []
    cur: dict[str, tuple[Value, int]] | None = None
    for idx, raw in enumerate(text.split("\n")):
        ln = idx + 1
        line = _strip_comment(raw.removesuffix("\r"))
        if not line:
            continue
        if line.startswith("["):
            if line != _HEADER:
                raise HarConfigError(f"line {ln}: unknown section `{line}`")
            if len(out) >= MAX_SERIES:
                raise HarConfigError(f"line {ln}: more than {MAX_SERIES} series")
            cur = {}
            out.append((cur, ln))
            continue
        if cur is None:
            raise HarConfigError(f"line {ln}: key before any section header")
        key, sep, val = line.partition("=")
        key = key.strip()
        if not sep:
            raise HarConfigError(f"line {ln}: expected `key = value`")
        if not key or not all(c.isascii() and (c.isalnum() or c == "_") for c in key):
            raise HarConfigError(f"line {ln}: bad key `{key}`")
        if key in cur:
            raise HarConfigError(f"line {ln}: duplicate key `{key}`")
        cur[key] = (_parse_value(val, ln), ln)
    return out


def _show(v: Value) -> str:
    """A value as the file spells it, the way the engine's messages do."""
    if isinstance(v, str):
        return f'"{v}"'
    if isinstance(v, int):
        return str(v)
    return "an array"


def _finish(block: _Block) -> Series:
    kv, ln = block
    where = f"[[series]] at line {ln}"
    for key, (_, kl) in kv.items():
        if key not in SERIES_KEYS:
            raise HarConfigError(f"line {kl}: unknown [[series]] key `{key}`")
    if "name" not in kv:
        raise HarConfigError(f"{where}: `name` is required")
    name = kv["name"][0]
    if not isinstance(name, str) or not valid_name(name):
        raise HarConfigError(f"{where}: `name` must be 1..=12 of [A-Z0-9] (got {_show(name)})")
    if "feed" not in kv:
        raise HarConfigError(f"{where}: `feed` is required")
    feed = kv["feed"][0]
    if not isinstance(feed, str) or not valid_descriptor(feed):
        raise HarConfigError(
            f"{where}: `feed` {_show(feed)} is not a `<venue>:<instrument>` descriptor"
        )
    raw = kv["fallback"][0] if "fallback" in kv else []
    if not isinstance(raw, list) or any(not isinstance(v, str) for v in raw):
        raise HarConfigError(f"{where}: `fallback` must be a one-line array of descriptors")
    fallback = tuple(typing.cast(list[str], raw))
    if len(fallback) > MAX_FALLBACKS:
        raise HarConfigError(f"{where}: at most {MAX_FALLBACKS} fallbacks (got {len(fallback)})")
    seen = {feed}
    for fb in fallback:
        if not valid_descriptor(fb):
            raise HarConfigError(
                f'{where}: fallback "{fb}" is not a `<venue>:<instrument>` descriptor'
            )
        if fb in seen:
            raise HarConfigError(f'{where}: `fallback` repeats "{fb}"')
        seen.add(fb)
    return Series(name, feed, fallback)


def parse(text: str) -> list[Series]:
    """Every ``[[series]]`` of ``text``, in file order; :class:`HarConfigError`
    names the first refusal (the engine's, word for word)."""
    out = [_finish(b) for b in _blocks(text)]
    if not out:
        raise HarConfigError("at least one [[series]] is required")
    names: set[str] = set()
    feeds: set[str] = set()
    for s in out:
        if s.name in names:
            raise HarConfigError(f'series name "{s.name}" appears twice')
        if s.feed in feeds:
            raise HarConfigError(f'feed "{s.feed}" appears twice')
        names.add(s.name)
        feeds.add(s.feed)
    return out


def default_path() -> pathlib.Path:
    """``$CLAUDE_WORKER_HAR_TOML`` or :data:`DEFAULT_PATH`, expanded."""
    return pathlib.Path(os.environ.get(PATH_ENV, "") or DEFAULT_PATH).expanduser()


def read(path: pathlib.Path) -> list[Series]:
    """:func:`parse` of the file at ``path`` (a file that is not UTF-8 is
    refused like the engine refuses it; ``OSError`` propagates: a named
    path that cannot be read is the caller's error to report)."""
    data = path.read_bytes()
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError as e:
        raise HarConfigError(f"{path}: not UTF-8") from e
    return parse(text)
