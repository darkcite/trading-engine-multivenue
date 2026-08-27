# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Read-only PMLR replay-log reader (design §5.1 — the data_fetcher substrate).

Container (docs/wire-format.md §"Replay log"): one 64-byte header — magic
``b"PMLR"``, version u16 LE (current 2; this reader accepts <= 2), slot_kind
u8 at offset 6, epoch_ns u64 at offset 8 — then back-to-back 64-byte slots
with no framing bytes.

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
VERSION_MAX: int = 2
# First version whose slots carry a valid venue byte / zeroed tail padding
# (v1 predates both — those bytes are undefined garbage there).
VENUE_BYTE_MIN_VERSION: int = 2

# Slot kinds (docs/wire-format.md header table; wire-stable, never renumber).
SLOT_KIND_TICK: int = 0
SLOT_KIND_SIGNAL: int = 1
SLOT_KIND_FILL: int = 2
SLOT_KIND_ORDER: int = 3
SLOT_KIND_AI_CMD: int = 4
SLOT_KIND_CHANNEL_EVENT: int = 5

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
    ``venue`` is garbage when the file is v1."""

    ts_ns: int
    sym: int
    venue_seq: int
    bid_px: int
    bid_qty: int
    ask_px: int
    ask_qty: int
    venue: int

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


# magic 4s · version u16 · slot_kind u8 · pad x · epoch_ns u64  (16 of 64 B)
_HEADER: struct.Struct = struct.Struct("<4sHBxQ")
_TICK: struct.Struct = struct.Struct("<QIIqqqqB")
_FILL: struct.Struct = struct.Struct("<QIB3xqqQ")
_AI_CMD: struct.Struct = struct.Struct("<QIIqqQBBBBHH")

# Decoded-prefix lengths; the remainder of each 64-B slot is tail padding.
_TICK_PREFIX_LEN: int = 49
_FILL_PREFIX_LEN: int = 40
_AI_CMD_PREFIX_LEN: int = 48

assert _TICK.size == _TICK_PREFIX_LEN
assert _FILL.size == _FILL_PREFIX_LEN
assert _AI_CMD.size == _AI_CMD_PREFIX_LEN


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
