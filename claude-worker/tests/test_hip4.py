# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)

"""BIN15 O2: the HIP-4 description mirror and the roll sidecar."""

import pathlib
import struct

import claude_worker.hip4
import claude_worker.pmlr

FIXTURE = pathlib.Path(__file__).parent / "fixtures" / "hip4" / "descriptions.tsv"

GRAMMARS: dict[str, int] = {
    "unknown": claude_worker.hip4.GRAMMAR_UNKNOWN,
    "out_binary_price": claude_worker.hip4.GRAMMAR_OUT_BINARY_PRICE,
    "out_price_touch": claude_worker.hip4.GRAMMAR_OUT_PRICE_TOUCH,
    "native_price_binary": claude_worker.hip4.GRAMMAR_NATIVE_PRICE_BINARY,
}


def test_shared_description_fixture_rows_agree() -> None:
    """The SHARED fixture, also read by the Rust grammar's own test. If
    these two ever disagree the engine and every offline consumer are
    pricing different instruments."""
    rows = 0
    for line in FIXTURE.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        oid, desc, grammar, und, strike, expiry, twap, period = line.split("\t")
        got = claude_worker.hip4.parse_outcome_spec(int(oid), desc)
        assert got.outcome == int(oid)
        assert got.grammar == GRAMMARS[grammar], desc
        assert got.underlying == und, desc
        assert got.strike_1e6 == int(strike), desc
        assert got.expiry_ns == int(expiry), desc
        assert got.twap_s == int(twap), desc
        assert got.period_s == int(period), desc
        rows += 1
    assert rows >= 12


def test_hl_time_is_integer_civil_arithmetic() -> None:
    assert claude_worker.hip4.parse_hl_time_ns("20260912-0630") == 1_789_194_600_000_000_000
    assert claude_worker.hip4.parse_hl_time_ns("20261001-0000") == 1_790_812_800_000_000_000
    assert claude_worker.hip4.parse_hl_time_ns("19700101-0000") == 0
    # A real 29 Feb parses; the same date in a non-leap year does not.
    assert claude_worker.hip4.parse_hl_time_ns("20240229-1200") == 1_709_208_000_000_000_000
    assert claude_worker.hip4.parse_hl_time_ns("20260229-0000") is None
    for bad in (
        "",
        "2026091-0630",
        "20260912_0630",
        "2026091a-0630",
        "20261301-0000",
        "20260012-0000",
        "20260912-2400",
        "20260912-0660",
        "19690101-0000",
    ):
        assert claude_worker.hip4.parse_hl_time_ns(bad) is None, bad


def test_roll_seq_unpacks_like_the_engine_packs() -> None:
    for outcome, twap, family, settled in (
        (2649, 60, 0, False),
        (2638, 60, 0, True),
        (4_294_967_295, 65_535, 255, True),
        (1, 0, 7, False),
    ):
        seq = (
            outcome
            | ((twap & 0xFFFF) << 32)
            | ((family & 0xFF) << 48)
            | (int(settled) << 56)
        )
        assert claude_worker.hip4.unpack_roll_seq(seq) == (outcome, twap, family, settled)


def _write_events(path: pathlib.Path, events: list[tuple[int, int, int, int, int, int]]) -> None:
    """A kind-5 PMLR file of ChannelEvent slots: the engine's own
    layout, written by hand so the sidecar is tested against bytes
    rather than against a mock."""
    header = struct.pack(
        "<4sHBxQ",
        claude_worker.pmlr.MAGIC,
        claude_worker.pmlr.VERSION_MAX,
        claude_worker.pmlr.SLOT_KIND_CHANNEL_EVENT,
        0,
    ).ljust(claude_worker.pmlr.HEADER_SIZE, b"\x00")
    body = b""
    for ts, sym, channel, seq, v0, v1 in events:
        body += struct.pack("<QIBB2xQQqq16x", ts, sym, 4, channel, seq, 0, v0, v1)
    path.write_bytes(header + body)


def test_read_rolls_is_the_sidecar_for_a_slots_meaning(tmp_path: pathlib.Path) -> None:
    """A slot sym means different instruments at different times; the
    roll events are the only record of which, and `instance_at` is how
    a consumer asks."""
    sym_yes = (4 << 24) | 4096
    other = (4 << 24) | 1

    def seq(outcome: int, settled: bool) -> int:
        return outcome | (60 << 32) | (0 << 48) | (int(settled) << 56)

    _write_events(
        tmp_path / "hl-events.pmlr",
        [
            # A mark row and a lifecycle row the sidecar must ignore.
            (1_000, other, claude_worker.pmlr.CHANNEL_MARK, 0, 77_180_000_000, 0),
            (1_100, 0xFFFFFFFF, claude_worker.pmlr.CHANNEL_OUTCOME_META, 0, 0, 0),
            # 2649 created, then settled, then 2650 created.
            (
                2_000,
                sym_yes,
                claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL,
                seq(2649, False),
                77_177_000_000,
                1_789_194_600_000_000_000,
            ),
            (
                3_000,
                sym_yes,
                claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL,
                seq(2649, True),
                77_177_000_000,
                1_789_194_600_000_000_000,
            ),
            (
                3_001,
                sym_yes,
                claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL,
                seq(2650, False),
                77_201_000_000,
                1_789_195_500_000_000_000,
            ),
        ],
    )

    rolls = claude_worker.hip4.read_rolls(tmp_path)
    assert len(rolls) == 3, "the mark and lifecycle rows are not rolls"
    assert [r.outcome for r in rolls] == [2649, 2649, 2650]
    assert [r.settled for r in rolls] == [False, True, False]
    assert rolls[0].sym == sym_yes
    assert rolls[0].family == 0
    assert rolls[0].strike_1e6 == 77_177_000_000
    assert rolls[0].expiry_ns == 1_789_194_600_000_000_000
    assert rolls[2].twap_s == 60

    # Before any roll: nothing was bound.
    assert claude_worker.hip4.instance_at(rolls, sym_yes, 1_999) is None
    # Between the created and the settle: 2649.
    at = claude_worker.hip4.instance_at(rolls, sym_yes, 2_500)
    assert at is not None and at.outcome == 2649
    # A SETTLED instance is still what the slot holds until its
    # successor is created — the ingress keeps the coins bound.
    at = claude_worker.hip4.instance_at(rolls, sym_yes, 3_000)
    assert at is not None and at.outcome == 2649
    at = claude_worker.hip4.instance_at(rolls, sym_yes, 3_001)
    assert at is not None and at.outcome == 2650
    # A different slot never resolves to this family's instance.
    assert claude_worker.hip4.instance_at(rolls, other, 9_999) is None


def test_read_rolls_on_a_run_with_no_events_is_empty(tmp_path: pathlib.Path) -> None:
    """A run with no rolling families simply has no rolls — never an
    exception on the nightly path."""
    assert claude_worker.hip4.read_rolls(tmp_path) == []
    # A file of the WRONG slot kind is not a roll source either.
    header = struct.pack(
        "<4sHBxQ",
        claude_worker.pmlr.MAGIC,
        claude_worker.pmlr.VERSION_MAX,
        claude_worker.pmlr.SLOT_KIND_TICK,
        0,
    ).ljust(claude_worker.pmlr.HEADER_SIZE, b"\x00")
    (tmp_path / "hl-events.pmlr").write_bytes(header)
    assert claude_worker.hip4.read_rolls(tmp_path) == []


def test_the_mirror_refuses_what_the_engine_refuses() -> None:
    """The three laws worth naming: tokenising (free text is not a
    field), the demotion on an unparseable required key, and the fixed
    underlying field."""
    spec = claude_worker.hip4.parse_outcome_spec(
        1, "perp:BTC|priceDescription:threshold:99 is not the strike|target:5"
    )
    assert spec.strike_1e6 == 5_000_000
    spec = claude_worker.hip4.parse_outcome_spec(
        2, "perp:BTC|seconds:60|threshold:1.2345678|time:20260912-0630"
    )
    assert spec.strike_1e6 == 1_234_567, "truncates past six decimals"
    spec = claude_worker.hip4.parse_outcome_spec(3, "perp:" + "A" * 16 + "|target:1")
    assert spec.underlying == ""
    assert spec.grammar == claude_worker.hip4.GRAMMAR_UNKNOWN
    # Total on any string, including junk and the empty one.
    for junk in ("", "|", "::", "a:b|c", "perp:", "\x00\xff", "x" * 4096):
        assert claude_worker.hip4.parse_outcome_spec(4, junk).outcome == 4
