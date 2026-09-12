# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""vrp_shadow (VRP P2.5): the engine's own book against the harness's.

The V8 acceptance instrument. Two independent accounts of one campaign
have to agree leg for leg, and the ways they can disagree are exactly
the ways a position stops being tracked.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import struct

import claude_worker.pmlr
import claude_worker.vrp_shadow

EPOCH: int = 1_788_000_000_000_000_000
OPT_SYM: int = (3 << 24) | 700
PERP_SYM: int = (3 << 24) | 1
OPT_DESC: str = "deribit:BTC-27MAR26-79000-C"
PERP_DESC: str = "deribit:BTC-PERPETUAL"


def _pack(kind: int, slots: list[bytes]) -> bytes:
    header = claude_worker.pmlr._HEADER.pack(claude_worker.pmlr.MAGIC, 3, kind, EPOCH)
    blob = bytearray(header + bytes(claude_worker.pmlr.HEADER_SIZE - len(header)))
    for s in slots:
        blob.extend(s + bytes(claude_worker.pmlr.SLOT_SIZE - len(s)))
    return bytes(blob)


def _fill(  # noqa: PLR0913, PLR0917 - one parameter per wire field, deliberately
    ts: int,
    sym: int,
    side: int,
    strategy_id: int,
    origin: int,
    px: int,
    qty: int,
    oid: int,
) -> bytes:
    return claude_worker.pmlr._FILL.pack(ts, sym, side, strategy_id, origin, px, qty, oid)


def _order(  # noqa: PLR0913, PLR0917 - one parameter per wire field, deliberately
    ts: int,
    sym: int,
    side: int,
    px: int,
    qty: int,
    oid: int,
    strategy_id: int,
) -> bytes:
    return claude_worker.pmlr._ORDER.pack(ts, sym, side, 1, px, qty, oid, 3, strategy_id)


def _run(tmp_path: pathlib.Path, fills: list[bytes], orders: list[bytes]) -> pathlib.Path:
    run = tmp_path / f"run-{EPOCH}"
    run.mkdir(parents=True, exist_ok=True)
    (run / "instrument-manifest.tsv").write_text(
        f"{PERP_SYM}\t{PERP_DESC}\n{OPT_SYM}\t{OPT_DESC}\n"
    )
    (run / claude_worker.vrp_shadow.FILLS_FILE).write_bytes(
        _pack(claude_worker.pmlr.SLOT_KIND_FILL, fills)
    )
    (run / claude_worker.vrp_shadow.ORDERS_FILE).write_bytes(
        _pack(claude_worker.pmlr.SLOT_KIND_ORDER, orders)
    )
    return run


def test_the_attribution_bytes_round_trip_through_the_reader(tmp_path):
    """X1: `strategy_id` at 13 and `origin` at 14 were zeroed padding, so
    every capture written before P1 reads as an unattributed venue fill
    — which is what makes the addition safe."""
    run = _run(
        tmp_path,
        [
            _fill(
                1,
                OPT_SYM,
                1,
                claude_worker.vrp_shadow.SLOT_VRP,
                claude_worker.pmlr.FILL_ORIGIN_PAPER,
                395_000_000,
                1_000_000,
                11,
            ),
            # A legacy slot: three zero bytes where the fields now live.
            struct.pack("<QIB3xqqQ", 2, PERP_SYM, 0, 79_000_000_000, 500_000, 12),
        ],
        [],
    )
    with claude_worker.pmlr.Reader(run / claude_worker.vrp_shadow.FILLS_FILE) as r:
        recs = list(r.fills())
    assert recs[0].strategy_id == claude_worker.vrp_shadow.SLOT_VRP
    assert recs[0].origin == claude_worker.pmlr.FILL_ORIGIN_PAPER
    assert recs[0].px == 395_000_000
    assert recs[0].order_id == 11
    assert recs[1].strategy_id == 0
    assert recs[1].origin == claude_worker.pmlr.FILL_ORIGIN_VENUE


def test_engine_legs_count_only_the_members_own_fills(tmp_path):
    paper = claude_worker.pmlr.FILL_ORIGIN_PAPER
    run = _run(
        tmp_path,
        [
            # The option leg: SOLD one contract.
            _fill(1, OPT_SYM, 1, 1, paper, 395_000_000, 1_000_000, 11),
            # The hedge: BOUGHT half a coin of perp.
            _fill(2, PERP_SYM, 0, 1, paper, 79_000_000_000, 500_000, 12),
            # Another member's fill on the same sym, and a venue fill
            # fanned out to everyone — neither is this member's book.
            _fill(3, PERP_SYM, 0, 2, paper, 79_000_000_000, 9_000_000, 13),
            _fill(
                4,
                PERP_SYM,
                0,
                claude_worker.pmlr.STRATEGY_ID_NONE,
                0,
                79_000_000_000,
                7_000_000,
                14,
            ),
        ],
        [
            _order(1, OPT_SYM, 1, 300_200_000, 1_000_000, 11, 1),
            _order(2, PERP_SYM, 0, 79_000_500_000, 500_000, 12, 1),
            _order(3, PERP_SYM, 0, 79_000_500_000, 9_000_000, 13, 2),
        ],
    )
    legs = claude_worker.vrp_shadow.engine_legs(run)
    assert legs[OPT_SYM] == claude_worker.vrp_shadow.Leg(1, -1_000_000)
    assert legs[PERP_SYM] == claude_worker.vrp_shadow.Leg(1, 500_000), "one leg, not three"
    assert claude_worker.vrp_shadow.engine_orders(run) == 2


def test_harness_legs_are_scoped_to_their_strategys_header():
    """The per-sym rows are not tagged — the header above them is the
    only scope, so a second strategy's rows must not leak in."""
    lines = [
        "audit-pnl: paper view: fills=2 net=0.0 (cash=0.0 markout=0.0; fees none in paper)",
        "audit-pnl: strategy 1 (vrp): orders=2 fills=2 trades=1 days=1 net=-5.0 opt_settled=1",
        "audit-pnl:   ioc_fills=2 ioc_canceled=0 ttl_expired=0 | fee ladder"
        " (net, flat bps/side): 0=1 1=2 2=3 tier=0",
        f"audit-pnl:   {OPT_DESC}: fills=1 pos=-1000000 realized=12.5",
        f"audit-pnl:   {PERP_DESC}: fills=1 pos=500000 realized=-17.5",
        "audit-pnl: strategy 2 (xsd): orders=9 fills=9 trades=4 days=1 net=1.0 opt_settled=0",
        "audit-pnl:   okx:BTC-USDT-SWAP: fills=9 pos=123 realized=1.0",
    ]
    har = claude_worker.vrp_shadow.harness_legs(lines)
    assert set(har) == {OPT_DESC, PERP_DESC}
    assert har[OPT_DESC] == claude_worker.vrp_shadow.Leg(1, -1_000_000)
    assert har[PERP_DESC] == claude_worker.vrp_shadow.Leg(1, 500_000)
    assert claude_worker.vrp_shadow.harness_legs(lines, slot=2) == {
        "okx:BTC-USDT-SWAP": claude_worker.vrp_shadow.Leg(9, 123)
    }


def test_reconcile_agrees_and_then_catches_every_class_of_gap(tmp_path, monkeypatch):
    paper = claude_worker.pmlr.FILL_ORIGIN_PAPER
    run = _run(
        tmp_path,
        [
            _fill(1, OPT_SYM, 1, 1, paper, 395_000_000, 1_000_000, 11),
            _fill(2, PERP_SYM, 0, 1, paper, 79_000_000_000, 500_000, 12),
        ],
        [
            _order(1, OPT_SYM, 1, 300_200_000, 1_000_000, 11, 1),
            _order(2, PERP_SYM, 0, 79_000_500_000, 500_000, 12, 1),
        ],
    )
    agree = [
        "audit-pnl: strategy 1 (vrp): orders=2 fills=2 trades=1 days=1 net=-5.0 opt_settled=1",
        f"audit-pnl:   {OPT_DESC}: fills=1 pos=-1000000 realized=12.5",
        f"audit-pnl:   {PERP_DESC}: fills=1 pos=500000 realized=-17.5",
    ]

    def fake(_engine, _run, lines=agree, orders=2):
        return ({"strategies": [{"strategy_id": 1, "orders": orders, "fills": 2}]}, lines)

    monkeypatch.setattr(claude_worker.vrp_shadow, "run_audit_pnl", fake)
    assert claude_worker.vrp_shadow.reconcile(run, pathlib.Path("/nonexistent")) == []

    # F7's own shape: the engine booked a hedge the model never filled,
    # so the two books hold different perp positions.
    naked = [
        "audit-pnl: strategy 1 (vrp): orders=2 fills=1 trades=1 days=1 net=-5.0 opt_settled=1",
        f"audit-pnl:   {OPT_DESC}: fills=1 pos=-1000000 realized=12.5",
    ]
    monkeypatch.setattr(
        claude_worker.vrp_shadow, "run_audit_pnl", lambda e, r: fake(e, r, naked)
    )
    bad = claude_worker.vrp_shadow.reconcile(run, pathlib.Path("/nonexistent"))
    assert [(d.sym, d.field) for d in bad] == [
        (PERP_DESC, "fills"),
        (PERP_DESC, "pos_1e6"),
    ]
    assert "DISAGREE" in bad[0].line()

    # And a capture the replay is not even looking at the same way.
    monkeypatch.setattr(
        claude_worker.vrp_shadow, "run_audit_pnl", lambda e, r: fake(e, r, agree, 5)
    )
    bad = claude_worker.vrp_shadow.reconcile(run, pathlib.Path("/nonexistent"))
    assert [(d.sym, d.field, d.engine, d.harness) for d in bad] == [("*", "orders", 2, 5)]


def test_a_run_with_no_fill_log_is_an_error_not_a_disagreement(tmp_path):
    run = tmp_path / f"run-{EPOCH}"
    run.mkdir(parents=True)
    try:
        claude_worker.vrp_shadow.engine_legs(run)
    except claude_worker.vrp_shadow.ShadowError as exc:
        assert "engine-fills.pmlr" in str(exc)
    else:  # pragma: no cover - the guard above must fire
        raise AssertionError("a missing fill log must refuse, not reconcile")
