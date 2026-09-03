# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""frames.py — golden vectors shared with Rust, tag16, buffer reuse.

The golden fixture (``tests/fixtures/ai_frame_golden.txt``) is consumed by
BOTH this suite and ``crates/ingress-ai/tests/golden_frames.rs`` — the two
packers cannot drift without one side going red.

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import hashlib
import hmac
import pathlib

import pytest

import claude_worker.frames

_FIXTURE: pathlib.Path = (
    pathlib.Path(__file__).resolve().parent / "fixtures" / "ai_frame_golden.txt"
)


@dataclasses.dataclass(frozen=True, slots=True)
class GoldenVector:
    name: str
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
    frame_hex: str


def load_golden() -> tuple[bytes, list[GoldenVector]]:
    key: bytes | None = None
    vectors: list[GoldenVector] = []
    for raw_line in _FIXTURE.read_text("utf-8").splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if parts[0] == "key":
            key = bytes.fromhex(parts[1])
            continue
        if len(parts) != 14:
            raise ValueError(f"bad golden line ({len(parts)} fields): {line[:60]}")
        vectors.append(
            GoldenVector(
                name=parts[0],
                ts_ns=int(parts[1]),
                seq=int(parts[2]),
                sym=int(parts[3]),
                px=int(parts[4]),
                qty=int(parts[5]),
                ttl_ns=int(parts[6]),
                kind=int(parts[7]),
                venue=int(parts[8]),
                strategy_id=int(parts[9]),
                side=int(parts[10]),
                param_id=int(parts[11]),
                flags=int(parts[12]),
                frame_hex=parts[13],
            )
        )
    if key is None:
        raise ValueError("golden fixture has no key line")
    if not vectors:
        raise ValueError("golden fixture has no vectors")
    return key, vectors


def _pack(buf: bytearray, key: bytes, v: GoldenVector) -> bytearray:
    return claude_worker.frames.pack_frame(
        buf,
        key,
        ts_ns=v.ts_ns,
        seq=v.seq,
        sym=v.sym,
        px=v.px,
        qty=v.qty,
        ttl_ns=v.ttl_ns,
        kind=v.kind,
        venue=v.venue,
        strategy_id=v.strategy_id,
        side=v.side,
        param_id=v.param_id,
        flags=v.flags,
    )


def test_golden_fixture_covers_every_kind() -> None:
    _key, vectors = load_golden()
    kinds = set()
    for i in range(len(vectors)):
        kinds.add(vectors[i].kind)
    # 0..=9 (8f) + 10/11 (VM2 seeds) + 12 (RG0 SetRegime).
    assert kinds == set(range(claude_worker.frames.KIND_SET_REGIME + 1)), (
        "one golden vector per AiCmdKind"
    )


def test_golden_vectors_pack_byte_identical() -> None:
    key, vectors = load_golden()
    buf = claude_worker.frames.new_frame_buffer()
    for i in range(len(vectors)):
        v = vectors[i]
        out = _pack(buf, key, v)
        assert out is buf, "pack_frame must reuse the one preallocated buffer"
        assert bytes(out).hex() == v.frame_hex, f"vector {v.name} drifted"


def test_tag16_matches_stdlib_hmac() -> None:
    key, vectors = load_golden()
    buf = claude_worker.frames.new_frame_buffer()
    _pack(buf, key, vectors[0])
    cmd = claude_worker.frames.cmd_bytes(buf)
    want = hmac.new(key, cmd, hashlib.sha256).digest()[: claude_worker.frames.TAG_LEN]
    assert bytes(buf[claude_worker.frames.TAG_OFFSET :]) == want


def test_buffer_reuse_rezeroes_padding() -> None:
    key, vectors = load_golden()
    buf = claude_worker.frames.new_frame_buffer()
    _pack(buf, key, vectors[0])
    # Dirty the pad region as a hostile reuse would.
    for i in range(50, 66):
        buf[i] = 0xAB
    v = vectors[0]
    _pack(buf, key, v)
    assert bytes(buf[50:66]) == bytes(16), "pad must be re-zeroed on every pack"
    assert bytes(buf).hex() == v.frame_hex


def test_pack_frame_rejects_wrong_buffer_size() -> None:
    key, vectors = load_golden()
    with pytest.raises(ValueError):
        _pack(bytearray(10), key, vectors[0])


def test_frame_geometry_constants() -> None:
    assert claude_worker.frames.FRAME_LEN == 82
    assert claude_worker.frames.LEN_FIELD_VALUE == 80
    assert claude_worker.frames.TAG_OFFSET == 66
    assert (
        claude_worker.frames.CMD_OFFSET + claude_worker.frames.CMD_LEN
        == claude_worker.frames.TAG_OFFSET
    )


# ---- RG0: regime word helpers (core-types regime.rs mirror) ----


def test_regime_word_matches_the_golden_set_regime_vector() -> None:
    # The set_regime_fast vector's px is the word bull/trend/normal/pos/
    # normal/neutral with an EMPTY source byte; its qty is the same word
    # stamped MEASURED. Both are pinned by the shared golden bytes.
    _key, vectors = load_golden()
    fast = [v for v in vectors if v.name == "set_regime_fast"][0]
    word = claude_worker.frames.regime_word(
        trend="bull",
        shape="trend",
        vol="normal",
        fund="pos",
        level="normal",
        stretch="neutral",
    )
    assert fast.px == word
    assert fast.qty == word | (1 << (8 * 6))
    assert fast.kind == claude_worker.frames.KIND_SET_REGIME
    assert fast.param_id == claude_worker.frames.REGIME_PROFILE_FAST
    assert claude_worker.frames.regime_word_is_wire_declared(fast.px)
    assert not claude_worker.frames.regime_word_is_wire_declared(fast.qty)
    slow = [v for v in vectors if v.name == "set_regime_slow_eos"][0]
    assert slow.px == claude_worker.frames.regime_word(trend="bull")
    assert slow.param_id == claude_worker.frames.REGIME_PROFILE_SLOW
    assert slow.flags == claude_worker.frames.FLAG_EXPIRE_ON_SILENCE


def test_regime_word_roundtrips_and_refuses_unknown_names() -> None:
    word = claude_worker.frames.regime_word(trend="bear", stretch="ext_up")
    assert claude_worker.frames.regime_word_dims(word) == {
        "trend": "bear",
        "stretch": "ext_up",
    }
    assert claude_worker.frames.regime_word_dims(0) == {}
    assert claude_worker.frames.regime_word_dims(0b011) == {"trend": "?"}
    # The all-unknown word: every market dimension marked, SOURCE unknown.
    assert claude_worker.frames.regime_word_dims(
        claude_worker.frames.REGIME_UNKNOWN_WORD
    ) == {
        "trend": "unknown",
        "shape": "unknown",
        "vol": "unknown",
        "fund": "unknown",
        "level": "unknown",
        "stretch": "unknown",
        "source": "unknown",
    }
    assert claude_worker.frames.regime_word(vol="unknown") == (
        claude_worker.frames.REGIME_DIM_UNKNOWN_BIT << 16
    )
    with pytest.raises(ValueError):
        claude_worker.frames.regime_word(mood="happy")
    with pytest.raises(ValueError):
        claude_worker.frames.regime_word(fund="high")


def test_regime_word_wire_law() -> None:
    f = claude_worker.frames
    assert f.regime_word_is_wire_declared(0)
    assert f.regime_word_is_wire_declared(f.regime_word(trend="bull"))
    # A declarer may mark a dimension unknown explicitly.
    assert f.regime_word_is_wire_declared(f.regime_word(trend="bull", vol="unknown"))
    # Two bits in one byte, a bit outside the valid mask, a SOURCE bit,
    # a reserved-byte bit, a negative or an over-wide value.
    assert not f.regime_word_is_wire_declared(0b011)
    assert not f.regime_word_is_wire_declared(1 << (8 * 3 + 2))
    assert not f.regime_word_is_wire_declared(f.REGIME_UNKNOWN_WORD)
    assert not f.regime_word_is_wire_declared(1 << 56)
    assert not f.regime_word_is_wire_declared(-1)
    assert not f.regime_word_is_wire_declared(1 << 64)
    # Unknown mark beside a value in the same byte is malformed.
    assert not f.regime_word_is_wire_declared(0x81)
