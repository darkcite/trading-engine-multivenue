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


def _dash_ddmmmyy(name: str) -> bool:
    base, sep, tail = name.partition("-")
    return bool(sep) and bool(base) and _is_ddmmmyy(tail)


def class_of_descriptor(descriptor: str) -> str | None:
    """Fee class of a §9.4 descriptor, ``None`` for an unknown shape."""
    if not descriptor:
        return None
    ns, sep, name = descriptor.partition(":")
    if not sep:
        return "prediction" if descriptor.isascii() and descriptor.isdigit() else None
    if not name:
        return None
    if ns == "binance":
        return "spot"
    if ns == "binance-usdm":
        return "dated" if _bn_delivery_suffix(name) else "perp"
    if ns == "binance-opt":
        return "option"
    if ns == "okx":
        return _okx(name)
    if ns == "deribit":
        return _deribit(name)
    if ns == "hyperliquid":
        return "perp"
    if ns == "bybit":
        return "spot"
    if ns == "bybit-linear":
        return "dated" if _dash_ddmmmyy(name) else "perp"
    return None
