# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``Fill.origin`` — which ACCOUNTING a number belongs to (plan §6.4).

The Rust side is ``core_types::FILL_ORIGIN_VENUE`` / ``FILL_ORIGIN_PAPER``
(``crates/core-types/src/lib.rs``), a byte on every ``Fill``. This module
is the Python mirror of those two integers and nothing else.

**It exists so there is exactly one of them.** Two readers now depend on
the split — ``bin15_accrue`` (the entry store) and ``pnl_report`` (the
nightly line) — and a second copy of "paper is 1" is a second thing to
keep in agreement with a crate that cannot see either. The values are
pinned on the Rust side by
``backtest::member::tests::a_replay_entry_is_stamped_paper_in_the_sidecar``
and on this side by ``tests/test_bin15_accrue.py``.

**The law these serve:** a PAPER figure and a VENUE figure are never
summed into one number. A mixed total is meaningless, and every BIN15
dollar figure must be quoted with the word that says which it is.

Convention: full ``import x`` only. No ``from x import y``.
"""

#: A real fill report from a venue.
VENUE: int = 0

#: A fill the paper dispatcher MODELLED through the matcher.
PAPER: int = 1

#: What to call each in a number an operator reads.
NAMES: dict[int, str] = {VENUE: "VENUE", PAPER: "PAPER"}


def name_of(origin: int) -> str:
    """The word §6.4 requires beside every BIN15 dollar figure.

    An unknown value renders as ``origin-<n>`` rather than raising: a
    report that refuses to print because it met a byte it did not
    recognise tells an operator less than one that prints the byte.
    """
    return NAMES.get(origin, f"origin-{origin}")
