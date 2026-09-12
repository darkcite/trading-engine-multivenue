# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""The descriptor law's Python mirror against the SHARED fixture — the
same rows crates/core-config/src/instrument_class.rs asserts."""

import pathlib

import claude_worker.instrument_class

FIXTURE = pathlib.Path(__file__).parent / "fixtures" / "fees" / "descriptor-classes.tsv"


def test_shared_fixture_rows_agree() -> None:
    rows = 0
    for line in FIXTURE.read_text(encoding="utf-8").splitlines():
        if not line or line.startswith("#"):
            continue
        desc, want = line.split("\t")
        expect = None if want == "none" else want
        assert expect is None or expect in claude_worker.instrument_class.CLASSES
        got = claude_worker.instrument_class.class_of_descriptor(desc)
        assert got == expect, (desc, got, expect)
        rows += 1
    assert rows >= 30


def test_unknown_shapes_are_none() -> None:
    f = claude_worker.instrument_class.class_of_descriptor
    assert f("") is None
    assert f("okx:BTC") is None
    assert f("deribit:BTC-FS-26SEP26_PERP") is None
    assert f("12ab") is None
    assert f("binance-usdm:btcusdt_2603") == "perp"  # a short tail is not a delivery suffix
