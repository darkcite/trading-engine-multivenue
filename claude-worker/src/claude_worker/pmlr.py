# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Read-only PMLR replay-log reader (design §5.1 — the data_fetcher substrate).

Container (docs/wire-format.md §"Replay log"): one 64-byte header — magic
``b"PMLR"``, version u16 LE (current 3; this reader accepts <= 3), slot_kind
u8 at offset 6, epoch_ns u64 at offset 8 — then back-to-back 64-byte slots
with no framing bytes.

Version 3 (VT1, 2026-09-03, docs/venue-time-capture-plan.md): ``Tick``
carries ``flags`` at offset 49 (bit0 stale, bit1 venue-time-from-sentinel)
and ``venue_time_ms`` at offset 56. Both decode as 0 from v2 files (zeroed
pad = "venue time unknown, never stale") and as garbage from v1 — gate on
[`Reader.has_venue_time`] before trusting either. Venue time is data, not
a clock: ``ts_ns`` stays the ordering key.

The file is mapped read-only (``mmap.ACCESS_READ``) and decoded in place
with ``struct.unpack_from`` straight off the map — no per-record byte-slice
copies; the only objects built are the returned record tuples.

Deliberate divergence from the Rust reader (``core-io::PmlrReader``), per
design §11: the Rust offline auditor REJECTS a payload that is not a
multiple of 64 (corruption check), but the worker tails files the engine is
still flushing (the positions view reads mid-flush states), so a trailing
partial slot is tolerated here — ignored and surfaced as [`Reader.torn`].

v1 files (Phase-1 layouts) are readable, but the venue byte and the tail
padding of each slot are undefined garbage — consumers must gate on
[`Reader.version`] before trusting v2-only bytes.

Convention: full ``import x`` only. No ``from x import y``.
"""

import mmap
import pathlib
import struct
import typing

MAGIC: bytes = b"PMLR"
HEADER_SIZE: int = 64
SLOT_SIZE: int = 64
VERSION_MAX: int = 3
# First version whose slots carry a valid venue byte / zeroed tail padding
# (v1 predates both — those bytes are undefined garbage there).
VENUE_BYTE_MIN_VERSION: int = 2
# First version whose Tick slots carry `flags` + `venue_time_ms` (VT1).
VENUE_TIME_MIN_VERSION: int = 3

# `TickRec.flags` bits (core-types `TICK_FLAG_*`, wire-stable).
TICK_FLAG_STALE: int = 1
TICK_FLAG_VENUE_TIME_SENTINEL: int = 2

# Slot kinds (docs/wire-format.md header table; wire-stable, never renumber).
SLOT_KIND_TICK: int = 0
SLOT_KIND_SIGNAL: int = 1
SLOT_KIND_FILL: int = 2
SLOT_KIND_ORDER: int = 3
SLOT_KIND_AI_CMD: int = 4
SLOT_KIND_CHANNEL_EVENT: int = 5
# WS11 (D5 fold-in): kind 6 = OptSummary (M2.3). Decoded by the
# dedicated [`OptSummaryReader`] below — the generic [`Reader`] keeps
# refusing it (its typed accessors cover kinds 0/2/4 only).
SLOT_KIND_OPT_SUMMARY: int = 6

_KNOWN_SLOT_KINDS: frozenset[int] = frozenset(
    (
        SLOT_KIND_TICK,
        SLOT_KIND_SIGNAL,
        SLOT_KIND_FILL,
        SLOT_KIND_ORDER,
        SLOT_KIND_AI_CMD,
        SLOT_KIND_CHANNEL_EVENT,
    )
)


class PmlrError(Exception):
    """Malformed PMLR container: bad magic, unsupported version, unknown
    slot kind, or a file shorter than its own header."""


class TickRec(typing.NamedTuple):
    """One `Tick` slot (wire-format.md). Prices/qtys fixed-point 1e6.
    ``venue`` is garbage when the file is v1; ``flags`` and
    ``venue_time_ms`` (v3) are 0 in v2 files and garbage in v1."""

    ts_ns: int
    sym: int
    venue_seq: int
    bid_px: int
    bid_qty: int
    ask_px: int
    ask_qty: int
    venue: int
    flags: int
    venue_time_ms: int

    def is_stale(self) -> bool:
        """``flags & TICK_FLAG_STALE`` — meaningful only when the file
        ``has_venue_time`` (v3+)."""
        return self.flags & TICK_FLAG_STALE != 0

    def mid(self) -> int:
        """Mid price, integer arithmetic, rounds toward zero (Rust twin:
        ``Tick::mid`` — Rust ``/`` truncates; Python ``//`` floors, so
        negative sums need the explicit truncation branch)."""
        total = self.bid_px + self.ask_px
        return total // 2 if total >= 0 else -((-total) // 2)


class FillRec(typing.NamedTuple):
    """One `Fill` slot. ``side``: 0 = Bid (buy), 1 = Ask (sell)."""

    ts_ns: int
    sym: int
    side: int
    px: int
    qty: int
    order_id: int


class AiCmdRec(typing.NamedTuple):
    """One `AiCmd` slot (kind 4; ts_ns is engine-monotonic per the §3
    capture amendment)."""

    ts_ns: int
    seq: int
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


class OrderRec(typing.NamedTuple):
    """One kind-3 `Order` slot (engine-orders.pmlr, M4.1 — every order
    ACCEPTED by the engine; VM2 V6 adds the decode for PositionSeed
    reconstruction). ``px``/``qty`` are ×1e6; ``strategy_id`` is the
    strategy-set slot (0xFF = unattributed)."""

    ts_ns: int
    sym: int
    side: int
    kind: int
    px: int
    qty: int
    client_oid: int
    venue: int
    strategy_id: int


# magic 4s · version u16 · slot_kind u8 · pad x · epoch_ns u64  (16 of 64 B)
_HEADER: struct.Struct = struct.Struct("<4sHBxQ")
# ts u64 · sym u32 · venue_seq u32 · 4×i64 · venue u8 · flags u8 (VT1) ·
# pad 6 · venue_time_ms u64 (VT1) — the whole 64-B slot is field-covered.
_TICK: struct.Struct = struct.Struct("<QIIqqqqBB6xQ")
_FILL: struct.Struct = struct.Struct("<QIB3xqqQ")
_AI_CMD: struct.Struct = struct.Struct("<QIIqqQBBBBHH")
# VM2 V6: kind-3 Order (engine-orders.pmlr, M4.1) — ts u64 · sym u32 ·
# side u8 · kind u8 · pad2 · px i64 · qty i64 · client_oid u64 ·
# venue u8 · strategy_id u8 (M4.1 M-b).
_ORDER: struct.Struct = struct.Struct("<QIBB2xqqQBB")

# Decoded-prefix lengths; the remainder of each 64-B slot is tail padding.
_TICK_PREFIX_LEN: int = 64
_FILL_PREFIX_LEN: int = 40
_AI_CMD_PREFIX_LEN: int = 48
_ORDER_PREFIX_LEN: int = 42

assert _TICK.size == _TICK_PREFIX_LEN
assert _FILL.size == _FILL_PREFIX_LEN
assert _AI_CMD.size == _AI_CMD_PREFIX_LEN
assert _ORDER.size == _ORDER_PREFIX_LEN


class Reader:
    """Open handle on one PMLR file: mmap'd, read-only, index-by-slot.

    Usage::

        with claude_worker.pmlr.Reader(path) as r:
            for fill in r.fills():
                ...
    """

    def __init__(self, path: pathlib.Path) -> None:
        self._file = path.open("rb")
        try:
            size = path.stat().st_size
            if size < HEADER_SIZE:
                raise PmlrError(f"{path}: truncated before header ({size} B)")
            self._map: mmap.mmap = mmap.mmap(self._file.fileno(), 0, access=mmap.ACCESS_READ)
        except BaseException:
            self._file.close()
            raise
        try:
            magic, version, slot_kind, epoch_ns = _HEADER.unpack_from(self._map, 0)
            if magic != MAGIC:
                raise PmlrError(f"{path}: bad magic {magic!r}")
            if version > VERSION_MAX:
                raise PmlrError(f"{path}: version {version} unsupported (max {VERSION_MAX})")
            if slot_kind not in _KNOWN_SLOT_KINDS:
                raise PmlrError(f"{path}: unknown slot kind {slot_kind}")
        except BaseException:
            self.close()
            raise
        self.version: int = int(version)
        self.slot_kind: int = int(slot_kind)
        self.epoch_ns: int = int(epoch_ns)
        payload = size - HEADER_SIZE
        self._count: int = payload // SLOT_SIZE
        #: True when a trailing partial slot was ignored (engine mid-flush).
        self.torn: bool = payload % SLOT_SIZE != 0

    def close(self) -> None:
        """Unmap and close. Idempotent."""
        if hasattr(self, "_map"):
            self._map.close()
        self._file.close()

    def __enter__(self) -> "Reader":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __len__(self) -> int:
        return self._count

    @property
    def has_venue_time(self) -> bool:
        """True when this file's Tick slots carry ``flags`` /
        ``venue_time_ms`` (header version >= 3). A False file replays
        under the v2 law: venue time unknown, never stale."""
        return self.version >= VENUE_TIME_MIN_VERSION

    def _offset(self, index: int, want_kind: int, label: str) -> int:
        if self.slot_kind != want_kind:
            raise PmlrError(f"slot kind {self.slot_kind} file; {label} decode refused")
        if index < 0 or index >= self._count:
            raise IndexError(f"slot {index} out of range ({self._count} records)")
        return HEADER_SIZE + index * SLOT_SIZE

    # ---- typed decoders (unpack_from directly off the map — no copies) ----

    def tick(self, index: int) -> TickRec:
        """Decode slot ``index`` as a Tick."""
        return TickRec._make(
            _TICK.unpack_from(self._map, self._offset(index, SLOT_KIND_TICK, "tick"))
        )

    def fill(self, index: int) -> FillRec:
        """Decode slot ``index`` as a Fill."""
        return FillRec._make(
            _FILL.unpack_from(self._map, self._offset(index, SLOT_KIND_FILL, "fill"))
        )

    def ai_cmd(self, index: int) -> AiCmdRec:
        """Decode slot ``index`` as an AiCmd."""
        return AiCmdRec._make(
            _AI_CMD.unpack_from(self._map, self._offset(index, SLOT_KIND_AI_CMD, "ai_cmd"))
        )

    def order(self, index: int) -> OrderRec:
        """Decode slot ``index`` as an Order (VM2 V6)."""
        return OrderRec._make(
            _ORDER.unpack_from(self._map, self._offset(index, SLOT_KIND_ORDER, "order"))
        )

    def ticks(self) -> typing.Iterator[TickRec]:
        """All Tick records, in file order."""
        for i in range(self._count):
            yield self.tick(i)

    def fills(self) -> typing.Iterator[FillRec]:
        """All Fill records, in file order."""
        for i in range(self._count):
            yield self.fill(i)

    def ai_cmds(self) -> typing.Iterator[AiCmdRec]:
        """All AiCmd records, in file order."""
        for i in range(self._count):
            yield self.ai_cmd(i)

    def orders(self) -> typing.Iterator[OrderRec]:
        """All Order records, in file order (VM2 V6)."""
        for i in range(self._count):
            yield self.order(i)


# ---------------------------------------------------------------------------
# WS11 (D5 fold-in): the kind-6 OptSummary decode + the run anchor —
# both previously private to iv_digest.py (and the anchor duplicated
# in candles.py); ONE home now, importers alias.
# ---------------------------------------------------------------------------

# The whole 64-byte slot is field-covered (no tail pad): ts u64 · sym
# u32 · venue u8 · flags u8 · pad2 · 4×i64 (mark_px_1e9, mark_iv_1e9,
# underlying_px_1e9, open_interest_1e6) · 4×i32 (delta_1e9, gamma_1e9,
# vega_1e6, theta_1e6). docs/wire-format.md `OptSummary`.
_OPT_SUMMARY: struct.Struct = struct.Struct("<QIBB2xqqqqiiii")
assert _OPT_SUMMARY.size == SLOT_SIZE

# OptSummary venue-optional-field flags (docs/wire-format.md).
OPT_SUMMARY_FLAG_MARK_PX: int = 1
OPT_SUMMARY_FLAG_OPEN_INTEREST: int = 2


class OptSummaryRec(typing.NamedTuple):
    """One decoded kind-6 `OptSummary` slot."""

    ts_ns: int
    sym: int
    venue: int
    flags: int
    mark_px_1e9: int
    mark_iv_1e9: int
    underlying_px_1e9: int
    open_interest_1e6: int
    delta_1e9: int
    gamma_1e9: int
    vega_1e6: int
    theta_1e6: int


class OptSummaryReader:
    """Kind-6 sibling of [`Reader`] (which refuses unknown slot kinds
    by design). Same container rules: mmap'd read-only, one header
    layout, torn trailing slot tolerated (the engine may be
    mid-flush). Raises [`PmlrError`] on a malformed container or a
    non-kind-6 file."""

    def __init__(self, path: pathlib.Path) -> None:
        self._file = path.open("rb")
        try:
            size = path.stat().st_size
            if size < HEADER_SIZE:
                raise PmlrError(f"{path}: truncated before header ({size} B)")
            self._map: mmap.mmap = mmap.mmap(self._file.fileno(), 0, access=mmap.ACCESS_READ)
        except BaseException:
            self._file.close()
            raise
        try:
            magic, version, slot_kind, epoch_ns = _HEADER.unpack_from(self._map, 0)
            if magic != MAGIC:
                raise PmlrError(f"{path}: bad magic {magic!r}")
            if version > VERSION_MAX:
                raise PmlrError(
                    f"{path}: version {version} unsupported (max {VERSION_MAX})"
                )
            if slot_kind != SLOT_KIND_OPT_SUMMARY:
                raise PmlrError(
                    f"{path}: slot kind {slot_kind} is not OptSummary"
                    f" ({SLOT_KIND_OPT_SUMMARY})"
                )
        except BaseException:
            self.close()
            raise
        self.version: int = int(version)
        self.epoch_ns: int = int(epoch_ns)
        payload = size - HEADER_SIZE
        self._count: int = payload // SLOT_SIZE
        self.torn: bool = payload % SLOT_SIZE != 0

    def close(self) -> None:
        """Unmap and close. Idempotent."""
        if hasattr(self, "_map"):
            self._map.close()
        self._file.close()

    def __enter__(self) -> "OptSummaryReader":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __len__(self) -> int:
        return self._count

    def record(self, index: int) -> OptSummaryRec:
        """Decode slot ``index`` (unpack_from straight off the map)."""
        if index < 0 or index >= self._count:
            raise IndexError(f"slot {index} out of range ({self._count} records)")
        offset = HEADER_SIZE + index * SLOT_SIZE
        return OptSummaryRec._make(_OPT_SUMMARY.unpack_from(self._map, offset))

    def records(self) -> typing.Iterator[OptSummaryRec]:
        """All records, in file (= venue-thread time) order."""
        for i in range(self._count):
            yield self.record(i)


# ---------------------------------------------------------------------------
# VM2 V6: the kind-7 DepthTopK decode. WS10-B made Depth the FIRST
# kind-DETERMINED slot size — 192-byte slots (kinds 0–6 stay 64 B), so
# neither [`Reader`] nor the 64-B stride applies; this sibling carries
# its own stride. docs/wire-format.md `DepthTopK`.
# ---------------------------------------------------------------------------

SLOT_KIND_DEPTH: int = 7
DEPTH_SLOT_SIZE: int = 192
#: Levels per side (core-types `DEPTH_K`).
DEPTH_K: int = 5
#: `DepthTopK.flags` bit 0: book resyncing after a venue seq break —
#: never trade (or digest) a known-broken snapshot.
DEPTH_FLAG_STALE: int = 1

# ts u64 · sym u32 · venue u8 · k u8 · flags u8 · pad x ·
# 5×(px_1e6 i64, qty_1e6 i64) bids · 5×(…) asks; 16 B tail pad.
_DEPTH: struct.Struct = struct.Struct("<QIBBBx20q")
assert _DEPTH.size == DEPTH_SLOT_SIZE - 16


class DepthRec(typing.NamedTuple):
    """One decoded kind-7 `DepthTopK` slot. ``bids``/``asks`` are
    best-first ``(px_1e6, qty_1e6)`` pairs; unpopulated levels are
    ``(0, 0)`` (the `DepthLevel::EMPTY` sentinel)."""

    ts_ns: int
    sym: int
    venue: int
    k: int
    flags: int
    bids: tuple[tuple[int, int], ...]
    asks: tuple[tuple[int, int], ...]


class DepthReader:
    """Kind-7 sibling of [`Reader`]/[`OptSummaryReader`] with the
    192-byte stride. Same container rules otherwise: mmap'd read-only,
    one header layout, torn trailing slot tolerated."""

    def __init__(self, path: pathlib.Path) -> None:
        self._file = path.open("rb")
        try:
            size = path.stat().st_size
            if size < HEADER_SIZE:
                raise PmlrError(f"{path}: truncated before header ({size} B)")
            self._map: mmap.mmap = mmap.mmap(self._file.fileno(), 0, access=mmap.ACCESS_READ)
        except BaseException:
            self._file.close()
            raise
        try:
            magic, version, slot_kind, epoch_ns = _HEADER.unpack_from(self._map, 0)
            if magic != MAGIC:
                raise PmlrError(f"{path}: bad magic {magic!r}")
            if version > VERSION_MAX:
                raise PmlrError(
                    f"{path}: version {version} unsupported (max {VERSION_MAX})"
                )
            if slot_kind != SLOT_KIND_DEPTH:
                raise PmlrError(
                    f"{path}: slot kind {slot_kind} is not Depth ({SLOT_KIND_DEPTH})"
                )
        except BaseException:
            self.close()
            raise
        self.version: int = int(version)
        self.epoch_ns: int = int(epoch_ns)
        payload = size - HEADER_SIZE
        self._count: int = payload // DEPTH_SLOT_SIZE
        self.torn: bool = payload % DEPTH_SLOT_SIZE != 0

    def close(self) -> None:
        """Unmap and close. Idempotent."""
        if hasattr(self, "_map"):
            self._map.close()
        self._file.close()

    def __enter__(self) -> "DepthReader":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    def __len__(self) -> int:
        return self._count

    def record(self, index: int) -> DepthRec:
        """Decode slot ``index`` (unpack_from straight off the map)."""
        if index < 0 or index >= self._count:
            raise IndexError(f"slot {index} out of range ({self._count} records)")
        offset = HEADER_SIZE + index * DEPTH_SLOT_SIZE
        f = _DEPTH.unpack_from(self._map, offset)
        return DepthRec(
            f[0],
            f[1],
            f[2],
            f[3],
            f[4],
            tuple((f[5 + 2 * i], f[6 + 2 * i]) for i in range(DEPTH_K)),
            tuple((f[15 + 2 * i], f[16 + 2 * i]) for i in range(DEPTH_K)),
        )

    def records(self) -> typing.Iterator[DepthRec]:
        """All records, in file (= venue-thread time) order."""
        for i in range(self._count):
            yield self.record(i)


def run_anchor_ns(run_dir: pathlib.Path) -> int | None:
    """WS11 (D5): the run's monotonic anchor — min first-ts across its
    readable tick files (the harness §3.3 / monitor RunSpan law).
    Previously duplicated in candles.py and iv_digest.py; both alias
    this now."""
    anchor: int | None = None
    for path in sorted(run_dir.glob("*-ticks.pmlr")):
        try:
            with Reader(path) as reader:
                if reader.slot_kind != SLOT_KIND_TICK or len(reader) == 0:
                    continue
                first = reader.tick(0).ts_ns
        except (PmlrError, OSError, ValueError):
            continue
        anchor = first if anchor is None else min(anchor, first)
    return anchor
