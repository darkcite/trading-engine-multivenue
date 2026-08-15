"""pmlr.py against the Rust-writer golden fixtures (design §11: v1 + v2,
torn-tail tolerance). Fixture provenance: tests/fixtures/pmlr/README.md.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import struct

import pytest

import claude_worker.pmlr

FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "pmlr"

EPOCH_NS = 1_755_216_000_000_000_000
PM7 = 7
HL_YES = (4 << 24) + 10_810
HL_NO = (4 << 24) + 10_811


def _header(version: int, slot_kind: int) -> bytearray:
    buf = bytearray(claude_worker.pmlr.HEADER_SIZE)
    buf[0:4] = claude_worker.pmlr.MAGIC
    struct.pack_into("<H", buf, 4, version)
    buf[6] = slot_kind
    struct.pack_into("<Q", buf, 8, EPOCH_NS)
    return buf


# ---- golden v2 ticks -----------------------------------------------------


def test_ticks_v2_header_and_count() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr") as r:
        assert r.version == 2
        assert r.slot_kind == claude_worker.pmlr.SLOT_KIND_TICK
        assert r.epoch_ns == EPOCH_NS
        assert len(r) == 4
        assert r.torn is False


def test_ticks_v2_record_decode() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr") as r:
        t0 = r.tick(0)
        assert t0.ts_ns == 1_000_000_000
        assert t0.sym == PM7
        assert t0.venue_seq == 1
        assert t0.bid_px == 490_000
        assert t0.bid_qty == 100_000_000
        assert t0.ask_px == 510_000
        assert t0.ask_qty == 50_000_000
        assert t0.venue == 0  # Polymarket
        assert t0.mid() == 500_000
        t3 = r.tick(3)
        assert t3.sym == HL_NO
        assert t3.venue == 4  # Hyperliquid
        assert t3.mid() == 390_000


def test_ticks_v2_iterator_matches_indexing() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr") as r:
        ticks = list(r.ticks())
        assert len(ticks) == 4
        assert ticks[0] == r.tick(0)
        assert ticks[3] == r.tick(3)


# ---- golden v2 fills -----------------------------------------------------


def test_fills_v2_decode() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "fills_v2.pmlr") as r:
        assert r.slot_kind == claude_worker.pmlr.SLOT_KIND_FILL
        assert len(r) == 5
        f0 = r.fill(0)
        assert f0.ts_ns == 1_100_000_000
        assert f0.sym == PM7
        assert f0.side == 0  # Bid
        assert f0.px == 480_000
        assert f0.qty == 20_000_000
        assert f0.order_id == 101
        f2 = r.fill(2)
        assert f2.side == 1  # Ask
        assert f2.px == 520_000
        assert f2.qty == 15_000_000
        assert f2.order_id == 103
        f4 = r.fill(4)
        assert f4.sym == HL_NO
        assert f4.order_id == 105


# ---- golden v1 (patched writer bytes; garbage venue/tail bytes) -----------


def test_ticks_v1_reads_core_fields() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v1.pmlr") as r:
        assert r.version == 1
        assert len(r) == 4
        t0 = r.tick(0)
        assert t0.ts_ns == 1_000_000_000
        assert t0.sym == PM7
        assert t0.bid_px == 490_000
        assert t0.ask_px == 510_000
        # v1 has no venue byte — the raw garbage is surfaced verbatim;
        # consumers must gate on reader.version (features.py does).
        assert t0.venue == 0xAA


# ---- torn tail (engine mid-flush) -----------------------------------------


def test_torn_final_record_tolerated(tmp_path: pathlib.Path) -> None:
    whole = (FIXTURES / "fills_v2.pmlr").read_bytes()
    torn = tmp_path / "torn.pmlr"
    torn.write_bytes(whole[: 64 + 2 * 64 + 17])
    with claude_worker.pmlr.Reader(torn) as r:
        assert r.torn is True
        assert len(r) == 2
        assert r.fill(1).order_id == 102
        with pytest.raises(IndexError):
            r.fill(2)


# ---- container validation --------------------------------------------------


def test_empty_header_only_file(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "empty.pmlr"
    p.write_bytes(_header(2, claude_worker.pmlr.SLOT_KIND_FILL))
    with claude_worker.pmlr.Reader(p) as r:
        assert len(r) == 0
        assert r.torn is False
        assert list(r.fills()) == []


def test_bad_magic_rejected(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "badmagic.pmlr"
    p.write_bytes(bytes(64))
    with pytest.raises(claude_worker.pmlr.PmlrError, match="magic"):
        claude_worker.pmlr.Reader(p)


def test_truncated_header_rejected(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "short.pmlr"
    p.write_bytes(bytes(16))
    with pytest.raises(claude_worker.pmlr.PmlrError, match="truncated"):
        claude_worker.pmlr.Reader(p)


def test_future_version_rejected(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "v3.pmlr"
    p.write_bytes(_header(3, claude_worker.pmlr.SLOT_KIND_TICK))
    with pytest.raises(claude_worker.pmlr.PmlrError, match="version 3"):
        claude_worker.pmlr.Reader(p)


def test_unknown_slot_kind_rejected(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "kind9.pmlr"
    p.write_bytes(_header(2, 9))
    with pytest.raises(claude_worker.pmlr.PmlrError, match="slot kind"):
        claude_worker.pmlr.Reader(p)


def test_kind_mismatch_decode_refused() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr") as r:
        with pytest.raises(claude_worker.pmlr.PmlrError, match="fill decode refused"):
            r.fill(0)
        with pytest.raises(claude_worker.pmlr.PmlrError, match="ai_cmd decode refused"):
            r.ai_cmd(0)


def test_index_bounds() -> None:
    with claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr") as r:
        with pytest.raises(IndexError):
            r.tick(4)
        with pytest.raises(IndexError):
            r.tick(-1)


def test_close_is_idempotent_and_final() -> None:
    r = claude_worker.pmlr.Reader(FIXTURES / "ticks_v2.pmlr")
    assert len(r) == 4
    r.close()
    r.close()
    with pytest.raises(ValueError):
        r.tick(0)
