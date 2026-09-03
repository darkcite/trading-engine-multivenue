# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""AiCmd wire-frame packing (design §4.1; Rust counterpart: ``ingress-ai/src/frame.rs``).

Frame layout, byte-identical to the Rust packer (pinned by the shared golden
vectors in ``tests/fixtures/ai_frame_golden.txt``)::

    [len: u16 LE = 80] [AiCmd: 64 B LE] [tag: 16 B]
    total = 82 B; tag = HMAC-SHA256(AI_INGRESS_HMAC_KEY, cmd_bytes[0..64])[0..16]

AiCmd field layout (design §3; offsets relative to the command, i.e. frame
offset minus 2): ts_ns u64 · seq u32 · sym u32 · px i64 · qty i64 · ttl_ns
u64 · kind u8 · venue u8 · strategy_id u8 · side u8 · param_id u16 · flags
u16 · 16 explicit zero pad bytes.

Allocation discipline (§4.3): one preallocated 82-byte ``bytearray`` per
connection — [`new_frame_buffer`] once, then ``struct.pack_into`` rewrites
it in place for every frame. ``pack_frame`` re-zeroes the 16 pad bytes on
each call so a reused buffer always yields canonical command bytes (the
engine's shape validator rejects non-zero padding).

This module packs verbatim and does **no** per-kind shape validation — the
§3 required-argument table is enforced by the push-verb layer (item 12) and,
authoritatively, by the engine's accept path. Sequence numbers come from the
durable SQLite allocator (``state.py``), never from this module.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import hmac
import struct

# ---- frame geometry (Rust: ingress-ai frame.rs consts) ----

FRAME_LEN: int = 82
LEN_FIELD_VALUE: int = 80
CMD_OFFSET: int = 2
CMD_LEN: int = 64
TAG_OFFSET: int = CMD_OFFSET + CMD_LEN
TAG_LEN: int = 16

# ---- AiCmd kinds (core-types AiCmdKind; wire-stable, never renumber) ----

KIND_HEARTBEAT: int = 0
KIND_ENABLE_STRATEGY: int = 1
KIND_DISABLE_STRATEGY: int = 2
KIND_SET_FAIR_VALUE: int = 3
KIND_SET_BIAS: int = 4
KIND_SET_PARAM: int = 5
KIND_ORDER_INTENT: int = 6
KIND_RULESET_STAGE: int = 7
KIND_RULESET_COMMIT: int = 8
KIND_HALT_REQUEST: int = 9
# VM2 V1 (D-1): one settled funding print as the venue published it —
# px = rate ×1e9 RAW (the engine's funding_print_divisor applies any
# ÷8 law), qty = the venue print timestamp in ms (> 0).
KIND_FUNDING_SEED: int = 10
# VM2 V1 (D-2): restore one position row post-restart — param_id =
# row index, px = entry px ×1e6, qty = position AGE in SECONDS (≥ 0),
# ttl_ns MUST be 0 (the engine drain expires any nonzero ttl).
KIND_POSITION_SEED: int = 11
# RG0 (docs/regime-and-dashboard-plan.md §4.4): DECLARE one horizon
# profile's regime — param_id = profile (0 fast / 1 slow), px = the
# declared RegimeWord (one-hot-or-empty per dimension byte, SOURCE byte
# EMPTY — the engine stamps DECLARED), qty = the sender's own MEASURED
# word (audit only; 0 or a state word), ttl_ns > 0 ENFORCED,
# strategy_id = STRATEGY_SLOT_NONE, flags bit 0 (expire-on-silence) legal.
KIND_SET_REGIME: int = 12

# ---- regime word bit map (core-types regime.rs; wire-stable) ----
# One byte per dimension, one-hot inside the byte. Names index the bit.

REGIME_PROFILE_FAST: int = 0
REGIME_PROFILE_SLOW: int = 1
REGIME_PROFILES: int = 2
REGIME_DIMS: dict[str, int] = {
    "trend": 0,
    "shape": 1,
    "vol": 2,
    "fund": 3,
    "level": 4,
    "stretch": 5,
    "source": 6,
}
REGIME_VALUES: dict[str, tuple[str, ...]] = {
    "trend": ("bear", "neutral", "bull"),
    "shape": ("chop", "mixed", "trend"),
    "vol": ("low", "normal", "high"),
    "fund": ("neg", "pos"),
    "level": ("low", "normal", "high"),
    "stretch": ("ext_down", "neutral", "ext_up"),
    "source": ("measured", "declared", "unknown"),
}
# Bit 7 of every MARKET dimension byte (0..=5): "could not be judged".
# Fail-closed per dimension: a label that constrains the dimension
# refuses it, a label that omits the dimension allows it.
REGIME_DIM_UNKNOWN_BIT: int = 0x80
REGIME_UNKNOWN_WORD: int = 0x0004_8080_8080_8080


def regime_word(**dims: str) -> int:
    """Build a RegimeWord from dimension=value keywords (e.g.
    ``regime_word(trend="bull", vol="high")``). Omitted dimensions stay
    EMPTY ("any" for gating; legal on the wire). ``"unknown"`` on a market
    dimension sets its unknown mark (bit 7). Unknown names raise."""
    word = 0
    for name, value in dims.items():
        if name not in REGIME_DIMS:
            raise ValueError(f"unknown regime dimension {name!r}")
        if name != "source" and value == "unknown":
            word |= REGIME_DIM_UNKNOWN_BIT << (8 * REGIME_DIMS[name])
            continue
        values = REGIME_VALUES[name]
        if value not in values:
            raise ValueError(f"unknown value {value!r} for regime dimension {name!r}")
        word |= 1 << (8 * REGIME_DIMS[name] + values.index(value))
    return word


def regime_word_dims(word: int) -> dict[str, str]:
    """Decode a RegimeWord into ``{dimension: value}`` for every populated
    byte (the unknown mark decodes to ``"unknown"``, malformed bytes to
    ``"?"``)."""
    out: dict[str, str] = {}
    for name, byte_index in REGIME_DIMS.items():
        b = (word >> (8 * byte_index)) & 0xFF
        if b == 0:
            continue
        if name != "source" and b == REGIME_DIM_UNKNOWN_BIT:
            out[name] = "unknown"
            continue
        values = REGIME_VALUES[name]
        if b & (b - 1) == 0 and b.bit_length() - 1 < len(values):
            out[name] = values[b.bit_length() - 1]
        else:
            out[name] = "?"
    return out


def regime_word_is_wire_declared(word: int) -> bool:
    """The engine's ``RegimeWord::is_wire_declared`` law: every populated
    market byte one-hot over its legal bits (a known value or the unknown
    mark), SOURCE byte empty, reserved byte 7 empty."""
    if word < 0 or word >> 64:
        return False
    if (word >> 48) & 0xFF or (word >> 56) & 0xFF:
        return False
    for name, byte_index in REGIME_DIMS.items():
        if name == "source":
            continue
        b = (word >> (8 * byte_index)) & 0xFF
        if b == 0 or b == REGIME_DIM_UNKNOWN_BIT:
            continue
        if b & (b - 1) or b.bit_length() > len(REGIME_VALUES[name]):
            return False
    return True


# ---- sentinels and enum bytes (core-types) ----

SYMBOL_ID_NONE: int = 0xFFFF_FFFF
STRATEGY_SLOT_NONE: int = 0xFF
STRATEGY_SLOT_AI_EXEC: int = 4
STRATEGY_SLOT_VM: int = 5
SIDE_BID: int = 0
SIDE_ASK: int = 1
SIDE_NONE: int = 0xFF
VENUE_POLYMARKET: int = 0
VENUE_BINANCE: int = 1
VENUE_OKX: int = 2
VENUE_DERIBIT: int = 3
VENUE_HYPERLIQUID: int = 4
VENUE_AI: int = 5
# WS9: the sixth market-data venue.
VENUE_BYBIT: int = 6
FLAG_EXPIRE_ON_SILENCE: int = 1

# len u16 + AiCmd head (50 B); the 16 pad bytes are zeroed separately.
_HEAD: struct.Struct = struct.Struct("<HQIIqqQBBBBHH")
_CMD_HEAD_LEN: int = 48  # command bytes before the explicit tail padding
_PAD_LEN: int = 16
_PAD_START: int = CMD_OFFSET + _CMD_HEAD_LEN  # 50 (frame-relative)
_PAD_END: int = TAG_OFFSET  # 66
_ZERO_PAD: bytes = bytes(_PAD_LEN)

assert _HEAD.size == CMD_OFFSET + _CMD_HEAD_LEN, "len field + command head"
assert _PAD_END - _PAD_START == _PAD_LEN
assert _CMD_HEAD_LEN + _PAD_LEN == CMD_LEN


def new_frame_buffer() -> bytearray:
    """The ONE per-connection frame buffer (§4.3). Preallocate once; every
    ``pack_frame`` call rewrites it in place."""
    return bytearray(FRAME_LEN)


def tag16(key: bytes, cmd_bytes: bytes) -> bytes:
    """Truncated-16 HMAC-SHA256 over the 64 command bytes (design §4.1;
    Rust counterpart: ``core_crypto::hmac_sha256_tag16``)."""
    return hmac.new(key, cmd_bytes, hashlib.sha256).digest()[:TAG_LEN]


def pack_frame(  # noqa: PLR0913 — one keyword per §3 wire field, deliberately
    buf: bytearray,
    key: bytes,
    *,
    ts_ns: int,
    seq: int,
    sym: int,
    px: int,
    qty: int,
    ttl_ns: int,
    kind: int,
    venue: int,
    strategy_id: int,
    side: int,
    param_id: int,
    flags: int,
) -> bytearray:
    """Pack one 82-B frame into ``buf`` in place and return ``buf``.

    ``buf`` must be a ``FRAME_LEN`` bytearray from [`new_frame_buffer`];
    the same object is reused for every frame on a connection — no
    allocation per frame beyond the HMAC internals. The returned value is
    the same object, for ``sock.sendall(pack_frame(...))`` call shapes.
    """
    if len(buf) != FRAME_LEN:
        raise ValueError(f"frame buffer must be {FRAME_LEN} B, got {len(buf)}")
    _HEAD.pack_into(
        buf,
        0,
        LEN_FIELD_VALUE,
        ts_ns,
        seq,
        sym,
        px,
        qty,
        ttl_ns,
        kind,
        venue,
        strategy_id,
        side,
        param_id,
        flags,
    )
    # Canonical zero padding on every pack — the buffer is reused.
    buf[_PAD_START:_PAD_END] = _ZERO_PAD
    buf[TAG_OFFSET:FRAME_LEN] = tag16(key, bytes(buf[CMD_OFFSET:TAG_OFFSET]))
    return buf


def cmd_bytes(buf: bytearray) -> bytes:
    """The 64 command bytes of a packed frame (test/audit helper)."""
    return bytes(buf[CMD_OFFSET:TAG_OFFSET])
