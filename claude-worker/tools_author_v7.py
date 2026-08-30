# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V7 one-shot: author the 3 migration + 3 generality-proof
ruleset artifacts into ~/multivenue/artifacts/rulesets/<hash128>.json
(hash128 = first 32 hex of sha256 over the file bytes — the stage
verb's recompute law). Deliberately a repo-side throwaway tool, not a
worker module: it writes operator artifacts once, under review.

Convention: full ``import x`` only.
"""

import hashlib
import json
import pathlib

OUT = pathlib.Path("~/multivenue/artifacts/rulesets").expanduser()

RISK = 9900.0
H = 60000  # horizon_ms re-evaluation gate


def row(**kw):
    base = {"horizon_ms": H, "max_risk_usd": RISK}
    base.update(kw)
    return base


# ---- migrations -----------------------------------------------------------

# xv_signal.py law: dev_bps = (mid_sym - mid_ref)/mid_ref * 1e4;
# enter |dev| >= 4.0 flat (SELL rich = ASK on positive signal), exit
# |dev| <= 1.0 or sign flip (the universal reversion law).
XV = {
    "rows": [
        row(
            name="xv-okx-bnspot", family="crypto",
            instrument="okx:BTC-USDT", ref="binance:btcusdt",
            feature="mid", combine="diff_bps",
            enter=4.0, abs=True, exit=1.0,
            max_risk_usd=4950.0,
        ),
        row(
            name="xv-hl-bnusdm", family="crypto",
            instrument="hyperliquid:BTC", ref="binance-usdm:btcusdt",
            feature="mid", combine="diff_bps",
            enter=4.0, abs=True, exit=1.0,
            max_risk_usd=4950.0,
        ),
    ]
}

# carry_signal.py CVFC law: enter apr24 spread >= 0.20 (short the
# higher-APR venue = ASK on positive diff), exit spread < 0 after the
# 96 h min hold, majors BTC/ETH excluded, MAX_POSITIONS 5 == the 5
# per-coin groups. 3 addressable venue pairs per coin, one group per
# coin (first-qualifying ~ the board's best-pair; delta documented in
# the V7 log). No confirm — the cron has none.
# 2 addressable pairs per coin (the deribit<->hl cross pair dropped):
# 10 rows x 2 legs x $4,950 = $99k -- inside the GROUP-BLIND static
# table cap (rule 7 sums every row; the cron's 5-position runtime
# exposure is the same $99k). Leg size is the documented delta.
_CVFC_VENUES = [
    ("binance-usdm:{b}usdt", "deribit:{c}_USDC-PERPETUAL"),
    ("binance-usdm:{b}usdt", "hyperliquid:{c}"),
]
_CVFC_COINS = ["sol", "xrp", "doge", "ada", "ltc"]
CVFC = {
    "rows": [
        row(
            name=f"cvfc-{coin}-{i}", family="crypto",
            instrument=a.format(b=coin, c=coin.upper()),
            ref=b.format(b=coin, c=coin.upper()),
            feature="apr24", combine="diff",
            enter=0.20, abs=True, exit=0.0,
            group=gi + 1, min_hold_s=345600, max_hold_s=1728000,
            max_risk_usd=4950.0,
        )
        for gi, coin in enumerate(_CVFC_COINS)
        for i, (a, b) in enumerate(_CVFC_VENUES)
    ]
}

# carry_signal.py S1 law: |apr24| >= 0.50 enter with |apr72| >= 0.30
# confirm (the cron's 3d window == 72 h), exit directional < 0.10,
# max age 240 h. Side derives from the signal sign (short the perp on
# positive funding). The cron's global 4-position cap is a cron
# artifact the grammar does not reproduce (documented delta).
_S1 = ["coti", "dexe", "bank", "era", "bless", "1000rats", "uai"]
S1 = {
    "rows": [
        row(
            name=f"s1-{n}", family="crypto",
            instrument=f"binance-usdm:{n}usdt",
            feature="apr24",
            enter=0.50, abs=True, exit=0.10,
            confirm_feature="apr72", confirm=0.30, confirm_abs=True,
            max_hold_s=864000,
        )
        for n in _S1
    ]
}

# ---- generality proofs (backtest-only; committed only on merit) -----------

# Spot<->perp basis: perp rich vs spot (bps) with live-funding
# confirm; NEW input pair class (two binance lanes, funding channel).
BASIS = {
    "rows": [
        row(
            name="basis-btc", family="crypto",
            instrument="binance-usdm:btcusdt", ref="binance:btcusdt",
            feature="mid", combine="diff_bps",
            enter=3.0, abs=True, exit=0.5,
            confirm_feature="apr24", confirm=0.05, confirm_abs=True,
            max_hold_s=86400,
        )
    ]
}

# IV spread: deribit vs okx BTC ~ATM same expiry/strike (Aug-30
# 77500 C); OptSummary channel + the D-7 mark-fill law end-to-end.
IV = {
    "rows": [
        row(
            name="iv-btc-atm", family="crypto",
            instrument="deribit:BTC-30AUG26-77500-C",
            ref="okx:BTC-USD-260830-77500-C",
            feature="mark_iv", combine="diff",
            enter=0.03, abs=True, exit=0.005,
            max_hold_s=21600, max_risk_usd=2000.0,
        )
    ]
}

# Depth imbalance: the WS10-B depth channel as a tradable feature.
DEPTH = {
    "rows": [
        row(
            name="imb-btc-okx", family="crypto",
            instrument="okx:BTC-USDT",
            feature="depth_imb",
            enter=0.6, abs=True, exit=0.1,
            max_hold_s=3600, max_risk_usd=5000.0,
        )
    ]
}


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    for tag, obj in (
        ("xv-v2", XV),
        ("cvfc-v2", CVFC),
        ("s1-v2", S1),
        ("basis-proof", BASIS),
        ("iv-spread-proof", IV),
        ("depth-imb-proof", DEPTH),
    ):
        blob = json.dumps(obj, separators=(",", ":")).encode()
        h = hashlib.sha256(blob).hexdigest()[:32]
        path = OUT / f"{h}.json"
        path.write_bytes(blob)
        print(f"{tag}\t{h}\trows={len(obj['rows'])}\t{path}")


if __name__ == "__main__":
    main()
