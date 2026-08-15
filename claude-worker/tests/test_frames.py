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
    assert kinds == set(range(10)), "one golden vector per AiCmdKind"


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
