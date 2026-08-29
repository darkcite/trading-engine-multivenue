# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6: the channel map — python/Rust caps mirror pin + TSV render.

The fixture table below MIRRORS the Rust ``caps_of_descriptor_law``
cases (crates/ingress-ai/src/ruleset.rs tests) — change either side
only with the other in the same commit.

Convention: full ``import x`` only.
"""

import pathlib

import claude_worker.channel_map
import claude_worker.iv_digest
import tests.craft

P = claude_worker.channel_map.CAP_PRICE
F = claude_worker.channel_map.CAP_FUNDING
D = claude_worker.channel_map.CAP_DEPTH
O = claude_worker.channel_map.CAP_OPT  # noqa: E741 — mirrors the Rust pin table

#: The cross-language pin table (Rust `caps_of_descriptor_law`).
LAW: list[tuple[str, int]] = [
    ("123456789", P),  # bare PM token id
    ("binance:btcusdt", P),
    ("binance-usdm:btcusdt", P | F),
    ("okx:BTC-USDT-SWAP", P | F | D),
    ("okx:BTC-USDT", P | D),
    ("okx:BTC-USD-260925-100000-C", O),
    ("deribit:BTC-PERPETUAL", P | F | D),
    ("deribit:BTC-26SEP26", P | D),
    ("deribit:BTC-26SEP26-100000-P", O | P),
    ("binance-opt:BTC-260925-100000-C", O | P),
    ("hyperliquid:BTC", P | F),
    ("hyperliquid:#NVDA", P),
    ("bybit:BTCUSDT", P),
    ("bybit-linear:BTCUSDT", P | F),
]


def test_caps_mirror_the_rust_offline_string_law():
    for desc, want in LAW:
        got = claude_worker.channel_map.caps_of_descriptor(desc)
        assert got == want, f"{desc}: caps {got} != {want}"


def test_channel_names_bit_order():
    assert claude_worker.channel_map.channel_names(P | F | D) == "price+funding+depth"
    assert claude_worker.channel_map.channel_names(O) == "opt_summary"
    assert claude_worker.channel_map.channel_names(O | P) == "price+opt_summary"


def test_render_map_is_sym_sorted_tsv():
    manifest = {
        (2, 0x0200_0002): "okx:BTC-USDT-SWAP",
        (0, 42): "123456789",
    }
    lines = claude_worker.channel_map.render_map(manifest)
    assert lines == [
        "42\t123456789\t1\tprice",
        f"{0x0200_0002}\tokx:BTC-USDT-SWAP\t7\tprice+funding+depth",
    ]


def test_main_writes_tsv_from_newest_run(tmp_path, capsys):
    root = tmp_path / "logs"
    run = tests.craft.write_run(root, 1_700_000_000_000_000_000, [1])
    (run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        "42\t123456789\n", encoding="utf-8"
    )
    out = tmp_path / "channel-map.tsv"
    rc = claude_worker.channel_map.main(
        ["--replay-dir", str(root), "--out", str(out)]
    )
    assert rc == 0
    assert out.read_text(encoding="utf-8") == "42\t123456789\t1\tprice\n"
    assert "1 descriptors" in capsys.readouterr().err


def test_main_stdout_mode_and_missing_manifest(tmp_path, capsys):
    root = tmp_path / "logs"
    tests.craft.write_run(root, 1_700_000_000_000_000_000, [1])
    rc = claude_worker.channel_map.main(["--replay-dir", str(root), "--out", "-"])
    assert rc == 1
    assert "no instrument manifest" in capsys.readouterr().err
