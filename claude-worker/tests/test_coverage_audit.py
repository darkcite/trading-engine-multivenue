# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6: the offline coverage audit.

Convention: full ``import x`` only.
"""

import sqlite3

import claude_worker.coverage_audit
import claude_worker.iv_digest
import tests.craft


def test_class_of_splits_options_out():
    cls = claude_worker.coverage_audit.class_of
    assert cls("123456789") == "polymarket"
    assert cls("okx:BTC-USDT-SWAP") == "okx"
    assert cls("okx:BTC-USD-260925-100000-C") == "okx/opt"
    assert cls("deribit:BTC-26SEP26-100000-P") == "deribit/opt"
    assert cls("binance-opt:BTC-260925-100000-C") == "binance-opt"
    assert cls("bybit-linear:BTCUSDT") == "bybit-linear"


def test_expectation_law():
    exp = claude_worker.coverage_audit.expectations_of
    # Perp: candles + funding + depth, no iv.
    assert exp("okx:BTC-USDT-SWAP") == {
        "candles": True, "funding": True, "iv": False, "depth": True,
    }
    # Pure option (okx): iv only — no candle lane for options.
    assert exp("okx:BTC-USD-260925-100000-C") == {
        "candles": False, "funding": False, "iv": True, "depth": False,
    }
    # Deribit option carries CAP_PRICE too — candles still NOT
    # expected (no fetch lane), iv is.
    assert exp("deribit:BTC-26SEP26-100000-P") == {
        "candles": False, "funding": False, "iv": True, "depth": False,
    }
    # PM binary: candles only.
    assert exp("123456789") == {
        "candles": True, "funding": False, "iv": False, "depth": False,
    }


def _db(tmp_path, candle_descs, funding_descs, now_ms):
    db = tmp_path / "candles.db"
    conn = sqlite3.connect(db)
    conn.execute(
        "CREATE TABLE candles (venue INTEGER, descriptor TEXT, tf TEXT,"
        " open_ts INTEGER, o REAL, h REAL, l REAL, c REAL, n INTEGER,"
        " source TEXT, fetched_ts INTEGER)"
    )
    conn.execute(
        "CREATE TABLE funding (venue INTEGER, descriptor TEXT, ts_ms INTEGER,"
        " rate REAL, fetched_ts INTEGER)"
    )
    for desc in candle_descs:
        conn.execute(
            "INSERT INTO candles VALUES (1,?, '1m', ?, 1,1,1,1,1,'rest',0)",
            (desc, now_ms - 1000),
        )
    for desc in funding_descs:
        conn.execute(
            "INSERT INTO funding VALUES (1,?,?,0.0001,0)", (desc, now_ms - 1000)
        )
    conn.commit()
    return db, conn


def test_audit_finds_hollow_lanes(tmp_path):
    now_ms = 1_700_000_000_000
    manifest = {
        (1, 0x0100_0200): "binance-usdm:btcusdt",  # funding present
        (1, 0x0100_0201): "binance-usdm:ethusdt",  # funding HOLLOW
        (0, 42): "123456789",  # candles HOLLOW
    }
    _path, conn = _db(
        tmp_path,
        candle_descs=["binance-usdm:btcusdt", "binance-usdm:ethusdt"],
        funding_descs=["binance-usdm:btcusdt"],
        now_ms=now_ms,
    )
    coverage = claude_worker.coverage_audit.audit(conn, manifest, now_ms - 3_600_000)
    conn.close()
    usdm = coverage["binance-usdm"]
    assert usdm.total == 2
    assert usdm.present["candles"] == 2 and usdm.expected["candles"] == 2
    assert usdm.present["funding"] == 1 and usdm.expected["funding"] == 2
    assert usdm.hollow["funding"] == ["binance-usdm:ethusdt"]
    pm = coverage["polymarket"]
    assert pm.present["candles"] == 0 and pm.expected["candles"] == 1
    lines = claude_worker.coverage_audit.render(coverage)
    assert any("HOLLOW binance-usdm/funding: 1 missing" in line for line in lines)
    assert any("HOLLOW polymarket/candles: 1 missing" in line for line in lines)
    assert lines[-1] == "hollow-lanes-total=2"


def test_audit_window_excludes_stale_rows(tmp_path):
    now_ms = 1_700_000_000_000
    manifest = {(1, 0x0100_0200): "binance-usdm:btcusdt"}
    _path, conn = _db(
        tmp_path, ["binance-usdm:btcusdt"], ["binance-usdm:btcusdt"], now_ms
    )
    # Window floor AFTER the rows ⇒ everything stale ⇒ hollow.
    coverage = claude_worker.coverage_audit.audit(conn, manifest, now_ms + 1)
    conn.close()
    usdm = coverage["binance-usdm"]
    assert usdm.present["candles"] == 0 and usdm.present["funding"] == 0


def test_main_end_to_end(tmp_path, capsys):
    now_ms = 1_700_000_000_000
    root = tmp_path / "logs"
    run = tests.craft.write_run(root, now_ms * 1_000_000, [1])
    (run / claude_worker.iv_digest.INSTRUMENT_MANIFEST_FILE).write_text(
        "42\t123456789\n", encoding="utf-8"
    )
    db, conn = _db(tmp_path, ["123456789"], [], now_ms)
    conn.close()
    rc = claude_worker.coverage_audit.main(
        [
            "--db", str(db),
            "--replay-dir", str(root),
            "--now-ms", str(now_ms),
        ]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "polymarket" in out
    assert "hollow-lanes-total=0" in out
