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


def test_mexc_namespaces_carry_one_class_each() -> None:
    """MX7 (plan D6): xStocks are ordinary spot rows and TradFi / equity /
    FX / metal perps ordinary perps — the name never changes the class."""
    f = claude_worker.instrument_class.class_of_descriptor
    for name in ("BTCUSDT", "AAPLXUSDT", "SPYXUSDT"):
        assert f(f"mexc:{name}") == "spot"
    for name in ("BTC_USDT", "XAU_USDT", "EUR_USDT", "AAPLSTOCK_USDT", "SPY_USDT"):
        assert f(f"mexc-perp:{name}") == "perp"
    assert f("mexc:") is None
    assert f("mexc-perp:") is None


def test_hyperevm_pools_are_spot() -> None:
    """HYPARB H3b: an AMM pool trades token0 against token1 outright."""
    f = claude_worker.instrument_class.class_of_descriptor
    assert f("hyperevm:0x6c9a33e3b592c0d65b3ba59355d5be0d38259285") == "spot"
    assert f("hyperevm:") is None


def test_coinm_class_reads_the_suffix() -> None:
    """BX2: `_perp` -> perp, six digits -> dated, anything else unknown —
    inverse is a discovery-row flag, never a class."""
    f = claude_worker.instrument_class.class_of_descriptor
    assert f("binance-coinm:btcusd_perp") == "perp"
    assert f("binance-coinm:1000shibusd_perp") == "perp"
    assert f("binance-coinm:btcusd_261225") == "dated"
    for bad in ("btcusd", "btcusd_2612", "_perp", "_261225"):
        assert f(f"binance-coinm:{bad}") is None, bad
    assert f("binance-coinm:") is None
