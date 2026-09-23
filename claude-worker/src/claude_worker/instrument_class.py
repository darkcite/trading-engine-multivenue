# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""instrument_class — the DESCRIPTOR LAW, mirrored byte for byte from
``crates/core-config/src/instrument_class.rs`` (XSD-F, statarb doc 08
§4): an instrument's fee class from its §9.4 worker map-name
descriptor. Pinned to the Rust law by the shared fixture
``tests/fixtures/fees/descriptor-classes.tsv``.

Classes are the ``fees.toml`` ``[fees.<venue>]`` keys and the harness's
``--fee-bps <venue>.<class>`` grammar: ``spot`` · ``perp`` · ``dated`` ·
``option`` · ``prediction``. ``None`` = a shape the law does not know
(the harness then charges the venue's dearest class and counts the leg).

Convention: full ``import x`` only.
"""

CLASSES: tuple[str, ...] = ("spot", "perp", "dated", "option", "prediction")

_MONTHS: frozenset[str] = frozenset(
    ("JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC")
)


def _is_digits(s: str, n: int) -> bool:
    return len(s) == n and s.isascii() and s.isdigit()


def _is_ddmmmyy(s: str) -> bool:
    """``26SEP26`` / ``1OCT26`` — 1–2 day digits, upper-case month, 2 year digits."""
    if len(s) not in (6, 7):
        return False
    day_len = len(s) - 5
    day, mon, yy = s[:day_len], s[day_len : day_len + 3], s[day_len + 3 :]
    return day.isascii() and day.isdigit() and mon in _MONTHS and yy.isascii() and yy.isdigit()


def _bn_delivery_suffix(name: str) -> bool:
    base, sep, tail = name.rpartition("_")
    return bool(sep) and bool(base) and _is_digits(tail, 6)


def _okx(name: str) -> str | None:
    segs = name.split("-")
    if len(segs) == 2 and segs[0] and segs[1]:
        return "spot"
    if len(segs) == 3 and segs[2] == "SWAP":
        return "perp"
    if len(segs) == 3 and _is_digits(segs[2], 6):
        return "dated"
    if len(segs) == 5 and segs[4] in ("C", "P") and _is_digits(segs[2], 6) and segs[3]:
        return "option"
    return None


def _deribit(name: str) -> str | None:
    segs = name.split("-")
    if len(segs) == 1:
        base, sep, quote = name.partition("_")
        return "spot" if sep and base and quote else None
    if len(segs) == 2 and segs[1] == "PERPETUAL" and segs[0]:
        return "perp"
    if len(segs) == 2 and _is_ddmmmyy(segs[1]) and segs[0]:
        return "dated"
    if len(segs) == 4 and segs[3] in ("C", "P") and _is_ddmmmyy(segs[1]) and segs[2]:
        return "option"
    return None


def _hyperliquid(name: str) -> str:
    """Hyperliquid coins: ``#<enc>`` (a HIP-4 outcome leg) and the
    ``out:``/``native:`` rolling-family slot descriptors are prediction
    markets; everything else is a perp.

    ``@<idx>`` (spot) returns ``perp`` — a known mis-class, mirrored
    from the Rust law deliberately so both sides stay bit-identical.
    """
    if name.startswith("#") or name.startswith("out:") or name.startswith("native:"):
        return "prediction"
    return "perp"


def _dash_ddmmmyy(name: str) -> bool:
    base, sep, tail = name.partition("-")
    return bool(sep) and bool(base) and _is_ddmmmyy(tail)


#: Namespaces whose every descriptor is ONE class, whatever the name.
#: MX7: MEXC xStocks (`AAPLXUSDT`) are ordinary spot rows and its TradFi /
#: equity / FX / metal perps (`XAU_USDT`, `AAPLSTOCK_USDT`) ordinary perps
#: (plan D6: no new class); MEXC lists no dated futures.
_FIXED_CLASS_OF_NS: dict[str, str] = {
    "binance": "spot",
    "binance-opt": "option",
    "bybit": "spot",
    "mexc": "spot",
    "mexc-perp": "perp",
}


def class_of_descriptor(descriptor: str) -> str | None:
    """Fee class of a §9.4 descriptor, ``None`` for an unknown shape."""
    if not descriptor:
        return None
    ns, sep, name = descriptor.partition(":")
    if not sep:
        return "prediction" if descriptor.isascii() and descriptor.isdigit() else None
    if not name:
        return None
    fixed = _FIXED_CLASS_OF_NS.get(ns)
    if fixed is not None:
        return fixed
    if ns == "binance-usdm":
        return "dated" if _bn_delivery_suffix(name) else "perp"
    if ns == "okx":
        return _okx(name)
    if ns == "deribit":
        return _deribit(name)
    if ns == "hyperliquid":
        return _hyperliquid(name)
    if ns == "bybit-linear":
        return "dated" if _dash_ddmmmyy(name) else "perp"
    return None
