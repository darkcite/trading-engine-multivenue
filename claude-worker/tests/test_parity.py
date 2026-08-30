# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V8: the offline parity comparator.

Convention: full ``import x`` only.
"""

import pathlib

import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.parity
import claude_worker.pmlr
import tests.craft

_HDR = claude_worker.pmlr.HEADER_SIZE
_SLOT = claude_worker.pmlr.SLOT_SIZE

EPOCH_NS = 1_700_000_000_000_000_000
EPOCH_MS = EPOCH_NS // 1_000_000
ANCHOR_TS = 5_000_000_000

USDM_SYM = 0x0100_0200
S4 = claude_worker.frames.STRATEGY_SLOT_AI_EXEC
S5 = claude_worker.frames.STRATEGY_SLOT_VM
BID = claude_worker.frames.SIDE_BID
ASK = claude_worker.frames.SIDE_ASK

ENTRY = claude_worker.parity.ENTRY
EXIT = claude_worker.parity.EXIT


def write_orders(path, epoch_ns, recs):
    """(ts_ns, sym, side, px, qty, strategy_id) rows — the
    test_seeds.py fixture layout."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_ORDER, epoch_ns
    )
    blob = bytearray(header + bytes(_HDR - len(header)))
    for i, (ts, sym, side, px, qty, sid) in enumerate(recs):
        slot = claude_worker.pmlr._ORDER.pack(  # noqa: SLF001
            ts, sym, side, 0, px, qty, i + 1, 1, sid
        )
        blob.extend(slot + bytes(_SLOT - len(slot)))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(bytes(blob))


def make_root(tmp_path: pathlib.Path, orders) -> pathlib.Path:
    root = tmp_path / "logs"
    run = root / f"run-{EPOCH_NS}"
    run.mkdir(parents=True)
    tests.craft.write_ticks(run / "bn-ticks.pmlr", [ANCHOR_TS], EPOCH_NS)
    write_orders(run / "engine-orders.pmlr", EPOCH_NS, orders)
    (run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        f"{USDM_SYM}\tbinance-usdm:cotiusdt\n", encoding="utf-8"
    )
    return root


def _ts(seconds: int) -> int:
    return ANCHOR_TS + seconds * 1_000_000_000


def test_family_split():
    assert claude_worker.parity.family_of("okx:BTC-USDT") == "xv"
    assert claude_worker.parity.family_of("binance-usdm:cotiusdt") == "carry"


def test_fold_events_entry_exit_and_flip():
    orders = [
        (100, "d", BID, 2_000_000, S4),
        (200, "d", BID, 1_000_000, S4),  # scale-in: no event
        (300, "d", ASK, 3_000_000, S4),  # full exit
        (400, "d", ASK, 2_000_000, S4),  # fresh short entry
        (500, "d", BID, 5_000_000, S4),  # flip short->long: exit + entry
    ]
    rows = [(ts, "d", side, qty, sid) for ts, _d, side, qty, sid in orders]
    events, net = claude_worker.parity.fold_events(rows, S4)
    kinds = [(e.kind, e.direction) for e in events]
    assert kinds == [
        (ENTRY, 1),
        (EXIT, 1),
        (ENTRY, -1),
        (EXIT, -1),
        (ENTRY, 1),
    ]
    assert net == {"d": 3_000_000}


def test_matched_within_tolerance_is_green(tmp_path, capsys):
    orders = [
        (_ts(100), USDM_SYM, BID, 50_000, 2_000_000, S4),
        (_ts(150), USDM_SYM, BID, 50_000, 1_000_000, S5),  # 50s later: matched
    ]
    root = make_root(tmp_path, orders)
    rc = claude_worker.parity.main(
        ["--replay-dir", str(root), "--now-ms", str(EPOCH_MS + 3_600_000), "--window-h", "2"]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "MISSES=0" in out
    assert "-> GREEN" in out


def test_miss_beyond_tolerance_is_red(tmp_path, capsys):
    orders = [
        (_ts(100), USDM_SYM, BID, 50_000, 2_000_000, S4),
        (_ts(100 + 7_300), USDM_SYM, BID, 50_000, 1_000_000, S5),  # > 7200s
    ]
    root = make_root(tmp_path, orders)
    rc = claude_worker.parity.main(
        ["--replay-dir", str(root), "--now-ms", str(EPOCH_MS + 4 * 3_600_000), "--window-h", "5"]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "MISSES=1" in out
    assert "MISS carry: entry binance-usdm:cotiusdt" in out
    assert "-> RED" in out


def test_position_sign_disagreement_is_red(tmp_path, capsys):
    orders = [
        (_ts(100), USDM_SYM, BID, 50_000, 2_000_000, S4),  # cron long
        (_ts(150), USDM_SYM, ASK, 50_000, 1_000_000, S5),  # vm SHORT
    ]
    root = make_root(tmp_path, orders)
    claude_worker.parity.main(
        ["--replay-dir", str(root), "--now-ms", str(EPOCH_MS + 3_600_000), "--window-h", "2"]
    )
    out = capsys.readouterr().out
    assert "position-disagreements-total=1" in out
    assert "POSITION carry: binance-usdm:cotiusdt: cron=+1 vm=-1" in out
    assert "-> RED" in out


def test_vm_extras_stay_informational(tmp_path, capsys):
    orders = [
        (_ts(100), USDM_SYM, BID, 50_000, 1_000_000, S5),  # VM-only trade
        (_ts(200), USDM_SYM, ASK, 50_000, 1_000_000, S5),
    ]
    root = make_root(tmp_path, orders)
    claude_worker.parity.main(
        ["--replay-dir", str(root), "--now-ms", str(EPOCH_MS + 3_600_000), "--window-h", "2"]
    )
    out = capsys.readouterr().out
    assert "vm-extras=2" in out
    assert "-> GREEN" in out  # extras never fail parity
