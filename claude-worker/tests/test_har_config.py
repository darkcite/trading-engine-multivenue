# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""har.toml -- the worker's reader of the HAR H3 series list (H3.1).

The worker must never cut seeds from a file the engine would refuse, so
the reader is the engine's line grammar and every refusal
``core_config::har`` makes is pinned here too, message for message.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.har_config

_GOOD = """
# two series
[[series]]
name     = "BTC"
feed     = "binance-usdm:btcusdt"
fallback = ["binance:btcusdt"]      # spot before the USDⓈ-M listing

[[series]]
name = "SP500"
feed = "binance-usdm:spyusdt"
fallback = ["okx:SPY-USDT-SWAP", "hyperliquid:xyz:SP500",]

[[series]]
name = "BABA"
feed = "binance-usdm:babausdt"
fallback = []
"""


def _refused(text: str) -> str:
    with pytest.raises(claude_worker.har_config.HarConfigError) as e:
        claude_worker.har_config.parse(text)
    return str(e.value)


def _one(body: str) -> str:
    return _refused("[[series]]\n" + body)


def test_the_good_file_parses_in_file_order() -> None:
    got = claude_worker.har_config.parse(_GOOD)
    assert [s.name for s in got] == ["BTC", "SP500", "BABA"]
    assert got[0] == claude_worker.har_config.Series(
        "BTC", "binance-usdm:btcusdt", ("binance:btcusdt",)
    )
    assert got[1].sources() == (
        "binance-usdm:spyusdt",
        "okx:SPY-USDT-SWAP",
        "hyperliquid:xyz:SP500",
    )
    assert got[2].fallback == ()


def test_the_file_is_read_from_its_path(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "har.toml"
    p.write_text(_GOOD, encoding="utf-8")
    assert len(claude_worker.har_config.read(p)) == 3
    with pytest.raises(OSError):
        claude_worker.har_config.read(tmp_path / "absent.toml")
    p.write_bytes(b'[[series]]\nname = "\xff"\n')
    with pytest.raises(claude_worker.har_config.HarConfigError, match="not UTF-8"):
        claude_worker.har_config.read(p)


def test_the_grammar_is_the_engines_not_standard_toml() -> None:
    """Standard TOML accepts all of these; the engine refuses them."""
    assert _one('name = "A"\nfeed = "x:y"\nfallback = [\n  "x:z",\n]\n') == (
        "line 4: unterminated array"
    )
    assert _one("name = 'A'\n") == "line 2: bad integer `'A'`"
    assert _one('name = "A\\"B"\n') == "line 2: strings carry no escapes"
    assert _refused('[[ series ]]\nname = "A"\n') == "line 1: unknown section `[[ series ]]`"
    assert _refused('series = [{ name = "A" }]\n') == "line 1: key before any section header"
    assert _one('name = "A"\nfeed = "x:y"\nfeed = "x:z"\n') == "line 4: duplicate key `feed`"
    assert _one('series.name = "A"\n') == "line 2: bad key `series.name`"
    assert _one('name "A"\n') == "line 2: expected `key = value`"
    assert _one('name = "A"\nfeed = "x:y"\nfallback = ["x:z", 1]\n') == (
        "line 4: expected a quoted string"
    )
    assert _one("name = 01\n") == "line 2: leading zero in `01`"
    assert _one("name = 99999999999999999999\n") == "line 2: bad integer `99999999999999999999`"
    assert _one("name = 9223372036854775808\n") == "line 2: integer overflow `9223372036854775808`"


def test_unknown_keys_and_sections_are_refused() -> None:
    assert _one('name = "BTC"\nfeed = "binance:btcusdt"\ntau = 1\n') == (
        "line 4: unknown [[series]] key `tau`"
    )
    assert _refused('[har]\nname = "A"\n') == "line 1: unknown section `[har]`"
    assert _refused('mode = 1\n[[series]]\nname = "A"\nfeed = "x:y"\n') == (
        "line 1: key before any section header"
    )


def test_a_name_is_one_to_twelve_of_upper_alnum() -> None:
    for bad in ('""', '"btc"', '"SP-500"', '"ABCDEFGHIJKLM"', '"É"', "7"):
        assert "`name` must be 1..=12 of [A-Z0-9]" in _one(f'name = {bad}\nfeed = "x:y"\n')
    assert _one('feed = "x:y"\n') == "[[series]] at line 1: `name` is required"
    assert _one('name = "btc"\n') == (
        '[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got "btc")'
    )
    assert _one("name = 7\n") == "[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got 7)"
    assert _one('name = ["A"]\n') == (
        "[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got an array)"
    )
    ok = claude_worker.har_config.parse('[[series]]\nname = "ABCDEFGHIJ12"\nfeed = "x:y"\n')
    assert ok[0].name == "ABCDEFGHIJ12"


def test_feed_and_fallbacks_are_descriptors() -> None:
    assert _one('name = "A"\n') == "[[series]] at line 1: `feed` is required"
    for bad in ('"btcusdt"', '"Binance:btc"', '"binance:"', '":btc"', '"1x:btc"', '"x:a b"', "1"):
        assert "is not a `<venue>:<instrument>` descriptor" in _one(f'name = "A"\nfeed = {bad}\n')
    long = "x:" + "a" * 63
    assert "descriptor" in _one(f'name = "A"\nfeed = "{long}"\n')
    assert claude_worker.har_config.valid_descriptor("x:" + "a" * 62)
    assert _one('name = "A"\nfeed = "x:y"\nfallback = "x:z"\n') == (
        "[[series]] at line 1: `fallback` must be a one-line array of descriptors"
    )
    assert _one('name = "A"\nfeed = "x:y"\nfallback = [1]\n') == (
        "[[series]] at line 1: `fallback` must be a one-line array of descriptors"
    )
    assert _one('name = "A"\nfeed = "x:y"\nfallback = ["z"]\n') == (
        '[[series]] at line 1: fallback "z" is not a `<venue>:<instrument>` descriptor'
    )
    assert _one('name = "A"\nfeed = "Binance:btc"\n') == (
        '[[series]] at line 1: `feed` "Binance:btc" is not a `<venue>:<instrument>` descriptor'
    )
    for good in ("okx:MU-USDT-SWAP", "hyperliquid:xyz:SP500", "mexc-perp:SPY_USDT", "a1-b:x.y/z"):
        assert claude_worker.har_config.valid_descriptor(good)


def test_a_fallback_never_repeats_the_feed_or_itself() -> None:
    assert _one('name = "A"\nfeed = "x:y"\nfallback = ["x:y"]\n') == (
        '[[series]] at line 1: `fallback` repeats "x:y"'
    )
    assert 'repeats "x:z"' in _one('name = "A"\nfeed = "x:y"\nfallback = ["x:z", "x:z"]\n')
    many = ", ".join(f'"x:f{i}"' for i in range(claude_worker.har_config.MAX_FALLBACKS + 1))
    assert _one(f'name = "A"\nfeed = "x:y"\nfallback = [{many}]\n') == (
        "[[series]] at line 1: at most 4 fallbacks (got 5)"
    )


def test_names_and_feeds_are_unique_and_the_count_bounded() -> None:
    assert (
        _refused('[[series]]\nname = "A"\nfeed = "x:y"\n[[series]]\nname = "A"\nfeed = "x:z"\n')
        == 'series name "A" appears twice'
    )
    assert (
        _refused('[[series]]\nname = "A"\nfeed = "x:y"\n[[series]]\nname = "B"\nfeed = "x:y"\n')
        == 'feed "x:y" appears twice'
    )
    assert _refused("# nothing\n") == "at least one [[series]] is required"
    blocks = "".join(f'[[series]]\nname = "S{i}"\nfeed = "x:s{i}"\n' for i in range(13))
    assert _refused(blocks) == "line 37: more than 12 series"
    twelve = "".join(f'[[series]]\nname = "S{i}"\nfeed = "x:s{i}"\n' for i in range(12))
    assert len(claude_worker.har_config.parse(twelve)) == 12


def test_the_default_path_honours_the_env(
    monkeypatch: pytest.MonkeyPatch, tmp_path: pathlib.Path
) -> None:
    monkeypatch.delenv(claude_worker.har_config.PATH_ENV, raising=False)
    assert claude_worker.har_config.default_path() == (
        pathlib.Path("~/multivenue/har.toml").expanduser()
    )
    monkeypatch.setenv(claude_worker.har_config.PATH_ENV, str(tmp_path / "h.toml"))
    assert claude_worker.har_config.default_path() == tmp_path / "h.toml"


def test_the_example_is_read_as_the_engine_reads_it() -> None:
    """``har.toml.example`` is its parser's contract: the engine's test
    parses it, and so must the worker's, to the same twelve series."""
    example = pathlib.Path(__file__).resolve().parents[2] / "har.toml.example"
    got = claude_worker.har_config.read(example)
    assert [s.name for s in got] == [
        "SP500",
        "SPCX",
        "MU",
        "NVDA",
        "MSFT",
        "META",
        "AAPL",
        "BABA",
        "SNDK",
        "BOT",
        "BTC",
        "ETH",
    ]
    assert all(s.feed.startswith("binance-usdm:") for s in got)
    assert got[0].fallback == ("okx:SPY-USDT-SWAP",)
    assert got[7].fallback == ()
    assert got[9].fallback == ("bybit-linear:BOTUSDT",)
