# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15 accrual — turn each closed UTC day's capture into ledger rows
and entry rows, so the desk gate accrues instead of ageing into S3.

The BIN15 lane's evidence is a SAMPLE SIZE problem, not an analysis
problem: 12.5 h of tape gave 32 settled entries and a 95 % interval on
the hit rate spanning 51 %-82 %. Detecting even a 10-point edge needs
~86 settled entries and a 5-point edge ~344. Run dirs are archived to
object storage and removed locally after a day (``PROTECT_DAYS=1``), so
a lane that only analyses on request analyses the same fortnight of tape
over and over. This module makes the day's evidence PERMANENT and small:
append-only TSVs under the worker's own state root -- the ledger, the
entries, and (BIN15 S5, ruling O-4; S5b) the COUNTERFACTUALS: on every
instance, the first reprice the artifact's own price test held and the
first today's law (the control) held, logged by the harness and never
traded -- what the persistence law is scored against on the same
instances.

**What it does.** For every run of the closed day that carries a HIP-4
instance: cut it into ≤ 2 h windows (the capture-window law, absolute),
cut a POINT-IN-TIME seed for each from ``candles.db``, drive
``backtest --member bin15 --emit-detail`` through it, and merge the
sidecar's ``bin15_ledger``, ``bin15_entries`` and ``bin15_first_fires``
blocks into the stores.
Idempotent: both merges dedupe, so re-running a day adds nothing -- on ONE
clock law. A day accrued on the anchor clock, relabelled onto the venue's
(BIN15 S4), and then re-accrued by a venue-clock binary is NOT deduped
(the ledger's key is the stamp): never re-accrue a relabelled day into
the same store.

**Why the seed is the whole method.** ``bin15_boot``'s seed loader does
NOT filter by time (xsd's does — it drops rows at or after the boot
hour). Pointing a replay of an old window at the LIVE seed hands the
forecast hours of the future, and every ``d`` inherits it. Filtering the
live seed FILE instead starves the HAR — it holds ~25 h of minute
returns and the forecast needs 1 440 before it speaks at all. Only
re-cutting from ``candles.db`` at the window's own first instant is both
honest and warm, and that is what :func:`seed_for` does, through the
same ``bin15_seed seed-all --now-ms`` call the engine wrapper makes at
boot.

Lanes (``python -m claude_worker.bin15_accrue <lane>``):

- ``accrue [--day YYYY-MM-DD] [--replay-dir …]`` — drive the day and
  merge. ``--day`` absent = the closed UTC day, which is what the 0020
  slot wants.
- ``report`` — the three operator questions (prediction success at the
  price paid, entry timing, entry price) with a Wilson interval and the
  sample size each conclusion would need — on the VENUE's label first
  (LAW E-11), the label each entry was accrued with beside it — and,
  once first fires exist, the artifact's unpersisted trigger and today's
  law (the control) on the SAME instances beside the member's (BIN15
  S5/S5b).
- ``relabel [--pull DIR …] [--runs DIR …]`` — BIN15 S4: correct both
  stores IN PLACE onto the venue's law and clock from the captures on
  disk (``claude_worker.bin15_tape``): every row not yet on it gets
  LAW E-11's ``y`` (the old one kept as ``y_engine``), the venue-published
  ``y_next_strike``, and its stamp moved onto the venue clock. Rows no
  capture covers stay as they are, counted. Idempotent: a second run
  converts nothing. The first-fire store (BIN15 S5) has its unknown labels
  filled the same way.
- ``status`` — rows, instances, settled entries, days covered.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections
import dataclasses
import datetime
import json
import math
import os
import pathlib
import shutil
import statistics
import subprocess
import sys
import tempfile
import typing

import claude_worker.backtest
import claude_worker.bin15_ledger
import claude_worker.bin15_ref
import claude_worker.bin15_tape
import claude_worker.fill_origin
import claude_worker.hip4
import claude_worker.window_root

#: The entry store, beside the calibration ledger.
DEFAULT_ENTRIES: str = "~/multivenue/worker/bin15/entries.tsv"

#: Header; `#` lines are skipped on read.
ENTRIES_HEADER: str = (
    "# bin15 entries — one row per COVERAGE ENTRY the member placed,\n"
    "#   merged from `backtest --member bin15 --emit-detail` sidecars.\n"
    "# ts_ns\toutcome\tfamily\tstart_ns\texpiry_ns\toffset_s\tis_yes\tpx_1e6\tqty_1e6"
    "\tp_hat_1e6\ty\torigin[\ty_engine\ty_next_strike]\n"
    "# 14 columns (BIN15 S4): on the VENUE's law and clock — `y` is LAW E-11's\n"
    "#   label (TWAP[T-W, T] >= strike), `ts_ns`/`offset_s` venue-clocked,\n"
    "#   `y_engine` the label accrued before `relabel` corrected it (-1: none —\n"
    "#   accrued on the venue law, or the engine could not settle it; from an\n"
    "#   S1+ binary on an anchor-clocked run it is E-11 read on that clock),\n"
    "#   `y_next_strike` the venue-published label\n"
    "#   (-1 unknown or a tie). 12 columns: the engine's label and the anchor\n"
    "#   clock, not yet relabelled — never scored as the venue's.\n"
    "# origin: 1 PAPER (a modelled fill), 0 VENUE (a real one). NEVER\n"
    "#   summed together — a mixed total is meaningless (plan §6.4). An\n"
    "#   11-column row predates the split and reads as PAPER, which is\n"
    "#   true by construction: no live fill had ever reached this file.\n"
    "# offset_s: seconds from the INSTANCE's start (expiry - 900 s), not\n"
    "#   from our receipt of the roll, which lags the venue a second or two.\n"
    "# y: 1000000 settled ITM, 0 settled OTM, -1 the window could not derive\n"
    "#   the payout (its expiry or TWAP falls outside the cut).\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)

#: `y` for an entry whose instance the window could not settle.
Y_UNKNOWN: int = claude_worker.bin15_ledger.Y_UNKNOWN

#: ``origin`` values, from the one module that defines them.
ORIGIN_VENUE: int = claude_worker.fill_origin.VENUE
ORIGIN_PAPER: int = claude_worker.fill_origin.PAPER

#: What to call each in a number an operator reads.
ORIGIN_NAMES: dict[int, str] = claude_worker.fill_origin.NAMES

#: Columns a pre-§6.4 entries file has. Every one of them is paper by
#: construction: the harness models every fill, and no live path had
#: written here when they were produced.
LEGACY_COLUMNS: int = 11

#: Columns of an engine-label row (§6.4) and of a venue-law row (BIN15 S4).
ENGINE_COLUMNS: int = 12
VENUE_COLUMNS: int = 14

#: The 15 m tenor, ns.
TAU_15M_NS: int = 900_000_000_000


class Entry(typing.NamedTuple):
    """One coverage entry, as placed and (later) as settled."""

    ts_ns: int
    outcome: int
    family: int
    start_ns: int
    expiry_ns: int
    offset_s: int
    is_yes: int
    px_1e6: int
    qty_1e6: int
    p_hat_1e6: int
    y: int
    #: Which ACCOUNTING this entry belongs to. **No default**, on
    #: purpose: a default is the one place a future producer could get
    #: PAPER without saying so, which is the silent mislabelling the
    #: whole split exists to prevent.
    origin: int
    #: BIN15 S4: the label the entry was accrued with before the relabel;
    #: ``Y_UNKNOWN`` = none (accrued on the venue law, or the engine could
    #: not settle it).
    y_engine: int = Y_UNKNOWN
    #: BIN15 S4: the venue-published label (``Y_UNKNOWN`` unknown / a tie).
    y_next_strike: int = Y_UNKNOWN
    #: BIN15 S4: ``y`` is the VENUE's label and the stamps venue-clocked
    #: (a 14-column row); ``False`` = the engine's, not yet relabelled.
    venue: bool = False

    @property
    def settled(self) -> bool:
        return self.y != Y_UNKNOWN

    @property
    def won(self) -> bool:
        """Whether the side the member BOUGHT is the side that settled."""
        return (self.y >= 500_000) == (self.is_yes == 1)

    @property
    def accounting(self) -> str:
        """``PAPER`` / ``VENUE`` — the word §6.4 says must appear beside
        every BIN15 dollar figure."""
        return ORIGIN_NAMES.get(self.origin, f"origin-{self.origin}")

    def tsv(self) -> str:
        cols = self[:ENGINE_COLUMNS]
        tail = f"\t{self.y_engine}\t{self.y_next_strike}" if self.venue else ""
        return "\t".join(str(v) for v in cols) + tail + "\n"


def entries_path(path: str | None = None) -> pathlib.Path:
    return pathlib.Path(os.path.expanduser(path or DEFAULT_ENTRIES))


def entries_from_sidecar(text: str) -> list[Entry]:
    """The `bin15_entries` block of one `--emit-detail` sidecar."""
    obj = json.loads(text)
    block = obj.get("bin15_entries")
    if not block:
        return []
    venue = claude_worker.bin15_ledger.sidecar_on_venue(obj)
    out: list[Entry] = []
    for r in block:
        y = r.get("y")
        y_next = r.get("y_next_strike")
        if "origin" not in r:
            # The harness stamps it (`Bin15EntryRow::origin`). A sidecar
            # without it came from a binary that predates §6.4, and
            # defaulting it here is exactly the silent mislabelling the
            # split exists to prevent — so this is an error, not a
            # fallback. Rebuild the release binary.
            raise ValueError(
                "bin15_entries row has no `origin`: this sidecar predates the "
                "PAPER/VENUE split (plan §6.4). Rebuild the harness."
            )
        out.append(
            Entry(
                ts_ns=int(r["ts_ns"]),
                outcome=int(r["outcome"]),
                family=int(r["family"]),
                start_ns=int(r["start_ns"]),
                expiry_ns=int(r["expiry_ns"]),
                offset_s=int(r["offset_s"]),
                is_yes=int(r["is_yes"]),
                px_1e6=int(r["px_1e6"]),
                qty_1e6=int(r["qty_1e6"]),
                p_hat_1e6=int(r["p_hat_1e6"]),
                y=Y_UNKNOWN if y is None else int(y),
                origin=int(r["origin"]),
                y_next_strike=Y_UNKNOWN if y_next is None else int(y_next),
                venue=venue,
            )
        )
    return out


def read_entries(path: pathlib.Path) -> list[Entry]:
    """Every entry, oldest first. Empty when absent."""
    if not path.is_file():
        return []
    out: list[Entry] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        f = s.split("\t")
        if len(f) == LEGACY_COLUMNS:
            # Pre-§6.4. PAPER is not an assumption here: the harness
            # models every fill, so nothing else could have written
            # these. Anything BUT 11, 12 or 14 is a file this reader does
            # not understand, and a reader that guesses at a column
            # count produces a number nobody can defend.
            out.append(Entry(*(int(v) for v in f), origin=ORIGIN_PAPER))
        elif len(f) == ENGINE_COLUMNS:
            out.append(Entry(*(int(v) for v in f)))
        elif len(f) == VENUE_COLUMNS:
            out.append(Entry(*(int(v) for v in f), venue=True))
        else:
            raise ValueError(
                f"{path}: want {ENGINE_COLUMNS} or {VENUE_COLUMNS} columns "
                f"(or {LEGACY_COLUMNS} pre-§6.4), got {len(f)}: {s!r}"
            )
    return out


def merge_entries(
    existing: typing.Sequence[Entry], incoming: typing.Sequence[Entry]
) -> tuple[list[Entry], int]:
    """``(merged oldest first, added)``, deduped by ``(OUTCOME, ORIGIN)``.

    One instance is entered at most once **per accounting**, so the
    outcome id alone is NOT the identity. The same instance can carry a
    PAPER entry (what the model would have done, from a replay of that
    day) and a VENUE entry (what the account actually did) — two
    different facts about one market, and keying on the outcome alone
    would let whichever arrived second silently overwrite the other.
    That is how a mixed number gets built out of two honest halves
    (plan §6.4).

    A SETTLED row wins over an unsettled one whichever way round they
    arrive: a window re-cut later can carry the same entry with its
    payout now derivable, and that is an upgrade, not a duplicate.
    Otherwise existing wins, so a re-cut can never un-settle the store.
    Above both (BIN15 S4): a VENUE-law row beats an engine-label row,
    settled or not — the law is corrected, never reverted.
    """
    by: dict[tuple[int, int], Entry] = {(e.outcome, e.origin): e for e in existing}
    added = 0
    for e in incoming:
        key = (e.outcome, e.origin)
        old = by.get(key)
        if old is None:
            by[key] = e
            added += 1
        elif e.venue != old.venue:
            if e.venue:
                by[key] = e
        elif not old.settled and e.settled:
            by[key] = e
    return (
        sorted(by.values(), key=lambda e: (e.ts_ns, e.outcome, e.origin)),
        added,
    )


def write_entries(path: pathlib.Path, rows: typing.Sequence[Entry]) -> None:
    """Atomic write (tmp + rename): a merge never leaves a torn file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(ENTRIES_HEADER)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


# ---------------------------------------------------------------------
# BIN15 S5 (ruling O-4): the counterfactual first fires
# ---------------------------------------------------------------------

#: The counterfactual store, beside the entries.
DEFAULT_FIRST_FIRES: str = "~/multivenue/worker/bin15/first_fires.tsv"

#: Header; `#` lines are skipped on read.
FIRST_FIRES_HEADER: str = (
    "# bin15 first fires -- BIN15 S5 (ruling O-4) + S5b: the COUNTERFACTUALS.\n"
    "#   One row per instance on which either of two price tests held, each\n"
    "#   the first reprice it held at persist 1 with no ceiling, logged by the\n"
    "#   harness and NEVER traded: ts_ns .. px_1e6 the ARTIFACT's own test (its\n"
    "#   floor and e_entry -- the unpersisted trigger), ctl_* TODAY'S law (the\n"
    "#   floor and the compiled default bound -- the control, doc 27 §5).\n"
    "#   From the `bin15_entries` rows' `first_fire_*` / `ctl_fire_*` columns\n"
    "#   (entered 1: the member bought the instance too) and the\n"
    "#   `bin15_first_fires` block (entered 0: the persistence law or the\n"
    "#   elapsed ceiling declined it).\n"
    "# ts_ns\toutcome\tfamily\tstart_ns\texpiry_ns\toffset_s\tis_yes\tpx_1e6"
    "\ty\ty_next_strike\tentered\tpersist_polls\telapsed_max_ns"
    "\tctl_ts_ns\tctl_offset_s\tctl_is_yes\tctl_px_1e6\n"
    "# A test that never held: ts 0, offset 0, is_yes -1, px 0. A row accrued\n"
    "#   before S5b (13 columns) recorded no control and reads as one.\n"
    "# persist_polls / elapsed_max_ns: the entry law the member ran under when\n"
    "#   the row was accrued (the sidecar's `bin15_entry_law`). 1 / 0 is the old\n"
    "#   law itself: its entries ARE its unpersisted trigger, not a comparison.\n"
    "# On the VENUE's law and clock only: a first fire exists only from an S5\n"
    "#   harness, and one from a run without venue time is dropped (counted).\n"
    "# y: 1000000 settled ITM, 0 OTM, -1 the window could not derive it.\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)

#: Columns of a first-fire row: 17 since S5b (the control's four) ...
FIRST_FIRE_COLUMNS: int = 17
#: ... and 13 before it, still read (no control recorded).
FIRST_FIRE_COLUMNS_PRE_S5B: int = 13

#: The entry law with neither S5 key set: the old law itself.
OLD_LAW: tuple[int, int] = (1, 0)

#: The sidecar keys of one counterfactual: (stamp, ask, side).
_ENTRY_FIRE: tuple[str, str, str] = ("first_fire_ts_ns", "first_fire_px_1e6", "first_fire_is_yes")
_ENTRY_CTL: tuple[str, str, str] = ("ctl_fire_ts_ns", "ctl_fire_px_1e6", "ctl_fire_is_yes")
_BLOCK_FIRE: tuple[str, str, str] = ("ts_ns", "px_1e6", "is_yes")
_BLOCK_CTL: tuple[str, str, str] = ("ctl_ts_ns", "ctl_px_1e6", "ctl_is_yes")

#: One counterfactual that never held: (ts_ns, offset_s, is_yes, px_1e6).
_NO_FIRE: tuple[int, int, int, int] = (0, 0, -1, 0)


class FirstFire(typing.NamedTuple):
    """One instance's two counterfactuals: logged, never traded."""

    #: The ARTIFACT's own test at persist 1, no ceiling -- the unpersisted
    #: trigger. ``ts_ns == 0``: it never held (``is_yes`` -1).
    ts_ns: int
    outcome: int
    family: int
    start_ns: int
    expiry_ns: int
    offset_s: int
    is_yes: int
    px_1e6: int
    y: int
    #: The venue-published label (``Y_UNKNOWN`` unknown / a tie).
    y_next_strike: int
    #: ``1``: the member entered this instance too, so the laws are scored
    #: on the SAME instance; ``0``: the persistence law declined it.
    entered: int
    #: The entry law the member ran under (``entry_persist_polls``,
    #: ``entry_elapsed_max_ns``) -- :data:`OLD_LAW` is the old law itself.
    persist_polls: int
    elapsed_max_ns: int
    #: BIN15 S5b: TODAY'S law, the control -- the same record at the
    #: compiled default bound. ``ctl_ts_ns == 0``: it never held, or the row
    #: predates S5b and recorded none.
    ctl_ts_ns: int = 0
    ctl_offset_s: int = 0
    ctl_is_yes: int = -1
    ctl_px_1e6: int = 0

    @property
    def law(self) -> tuple[int, int]:
        return (self.persist_polls, self.elapsed_max_ns)

    @property
    def settled(self) -> bool:
        return self.y != Y_UNKNOWN

    @property
    def fired(self) -> bool:
        """Whether the artifact's own test held on the instance."""
        return self.ts_ns != 0

    @property
    def ctl_fired(self) -> bool:
        """Whether today's law held on the instance."""
        return self.ctl_ts_ns != 0

    @property
    def won(self) -> bool:
        """Whether the side the artifact's test would have bought settled ITM."""
        return (self.y >= 500_000) == (self.is_yes == 1)

    def control(self) -> "FirstFire":
        """The control as a first fire of its own -- what the scoring reads."""
        return self._replace(
            ts_ns=self.ctl_ts_ns,
            offset_s=self.ctl_offset_s,
            is_yes=self.ctl_is_yes,
            px_1e6=self.ctl_px_1e6,
        )

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"


def first_fires_path(path: str | None = None) -> pathlib.Path:
    return pathlib.Path(os.path.expanduser(path or DEFAULT_FIRST_FIRES))


def _fire(r: dict, keys: tuple[str, str, str], start: int) -> tuple[int, int, int, int]:
    """``(ts_ns, offset_s, is_yes, px_1e6)`` of one counterfactual on a
    sidecar row -- :data:`_NO_FIRE` when its test never held (``null``) or
    the harness predates it (absent)."""
    ts_key, px_key, side_key = keys
    ts = r.get(ts_key)
    if ts is None:
        return _NO_FIRE
    ts = int(ts)
    return (ts, max(ts - start, 0) // 1_000_000_000, int(r[side_key]), int(r[px_key]))


def _first_fire(
    r: dict,
    keys: tuple[tuple[str, str, str], tuple[str, str, str]],
    entered: int,
    law: tuple[int, int],
) -> FirstFire:
    y = r.get("y")
    y_next = r.get("y_next_strike")
    start = int(r["start_ns"])
    ts, offset, side, px = _fire(r, keys[0], start)
    cts, coffset, cside, cpx = _fire(r, keys[1], start)
    return FirstFire(
        ts_ns=ts,
        outcome=int(r["outcome"]),
        family=int(r["family"]),
        start_ns=start,
        expiry_ns=int(r["expiry_ns"]),
        offset_s=offset,
        is_yes=side,
        px_1e6=px,
        y=Y_UNKNOWN if y is None else int(y),
        y_next_strike=Y_UNKNOWN if y_next is None else int(y_next),
        entered=entered,
        persist_polls=law[0],
        elapsed_max_ns=law[1],
        ctl_ts_ns=cts,
        ctl_offset_s=coffset,
        ctl_is_yes=cside,
        ctl_px_1e6=cpx,
    )


def entry_law_of(obj: dict) -> tuple[int, int]:
    """The sidecar's ``bin15_entry_law`` -- the entry law its member ran
    under; :data:`OLD_LAW` for a pre-S5 sidecar, which has none."""
    law = obj.get("bin15_entry_law") or {}
    return (int(law.get("persist_polls", 1)), int(law.get("elapsed_max_ns", 0)))


def first_fires_from_sidecar(text: str) -> tuple[list[FirstFire], int]:
    """``(first fires, dropped)`` of one `--emit-detail` sidecar: the
    entries' own ``first_fire_*`` / ``ctl_fire_*`` columns (``entered`` 1)
    and the ``bin15_first_fires`` block (``entered`` 0), each stamped with
    the sidecar's ``bin15_entry_law``. A pre-S5 sidecar carries none and
    yields nothing; a pre-S5b one yields rows without a control. One whose
    runs are not on the venue clock yields nothing either and counts what it
    dropped -- the store is on one clock, and a first fire on the anchor
    clock is not a fact about it."""
    obj = json.loads(text)
    law = entry_law_of(obj)
    rows = [(r, (_ENTRY_FIRE, _ENTRY_CTL), 1) for r in obj.get("bin15_entries") or []]
    rows += [(r, (_BLOCK_FIRE, _BLOCK_CTL), 0) for r in obj.get("bin15_first_fires") or []]
    out: list[FirstFire] = []
    for r, keys, entered in rows:
        f = _first_fire(r, keys, entered, law)
        # Neither test recorded: a pre-S5 row, or an entry whose first fire
        # the harness lost (the gate cannot pass without one) -- nothing to
        # pair, and nothing is guessed.
        if f.fired or f.ctl_fired:
            out.append(f)
    if out and not claude_worker.bin15_ledger.sidecar_on_venue(obj):
        return [], len(out)
    return out, 0


def read_first_fires(path: pathlib.Path) -> list[FirstFire]:
    """Every first fire, oldest instance first. Empty when absent."""
    if not path.is_file():
        return []
    out: list[FirstFire] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        f = s.split("\t")
        if len(f) not in (FIRST_FIRE_COLUMNS, FIRST_FIRE_COLUMNS_PRE_S5B):
            raise ValueError(
                f"{path}: want {FIRST_FIRE_COLUMNS} columns "
                f"({FIRST_FIRE_COLUMNS_PRE_S5B} before S5b), got {len(f)}: {s!r}"
            )
        out.append(FirstFire(*(int(v) for v in f)))
    return out


def _earliest(a: FirstFire, b: FirstFire) -> FirstFire:
    """``a`` with each counterfactual the EARLIER of the two rows' -- each on
    its own, since the two tests fire apart; a test that never held (``0``)
    is later than any that did, and a tie keeps ``a``'s."""
    if b.fired and (not a.fired or b.ts_ns < a.ts_ns):
        a = a._replace(ts_ns=b.ts_ns, offset_s=b.offset_s, is_yes=b.is_yes, px_1e6=b.px_1e6)
    if b.ctl_fired and (not a.ctl_fired or b.ctl_ts_ns < a.ctl_ts_ns):
        a = a._replace(
            ctl_ts_ns=b.ctl_ts_ns,
            ctl_offset_s=b.ctl_offset_s,
            ctl_is_yes=b.ctl_is_yes,
            ctl_px_1e6=b.ctl_px_1e6,
        )
    return a


def merge_first_fires(
    existing: typing.Sequence[FirstFire], incoming: typing.Sequence[FirstFire]
) -> tuple[list[FirstFire], int]:
    """``(merged oldest instance first, added)``, one row per OUTCOME.

    An instance that straddles a window cut is replayed twice, and each
    window's member records its own counterfactuals. The EARLIEST of each
    is the law's -- a later one is a re-bound member firing again -- so its
    stamp, price and side are kept, each counterfactual on its own; the
    label is taken from whichever row has it (settled beats unsettled, a
    known venue label beats -1); and ``entered`` from either, because the
    member entering the instance in any window is what makes it a
    same-instance pair.

    A row accrued under ANOTHER entry law (a day re-accrued after the
    artifact changed) is a different fact: the stored row keeps its fires,
    law and ``entered`` -- existing wins, as in the entries' merge -- and
    only takes a label it lacked. With :func:`same_law_entries` on the
    entries' side, a stored pair never mixes two laws.
    """
    by: dict[int, FirstFire] = {f.outcome: f for f in existing}
    added = 0
    for f in incoming:
        old = by.get(f.outcome)
        if old is None:
            by[f.outcome] = f
            added += 1
            continue
        same_law = old.law == f.law
        lab = old if old.settled or not f.settled else f
        by[f.outcome] = (_earliest(old, f) if same_law else old)._replace(
            y=lab.y,
            y_next_strike=(
                old.y_next_strike if old.y_next_strike != Y_UNKNOWN else f.y_next_strike
            ),
            entered=max(old.entered, f.entered) if same_law else old.entered,
        )
    return sorted(by.values(), key=lambda f: (f.start_ns, f.outcome)), added


def same_law_entries(
    incoming: typing.Sequence[tuple[Entry, tuple[int, int]]],
    stored: typing.Sequence[FirstFire],
) -> list[Entry]:
    """The incoming entries (each with its sidecar's entry law) that may
    merge: an instance whose first fire is stored under ANOTHER law keeps
    its stored entry, because the pair is scored under that law."""
    law_of = {f.outcome: f.law for f in stored}
    return [e for e, law in incoming if law_of.get(e.outcome, law) == law]


def write_first_fires(path: pathlib.Path, rows: typing.Sequence[FirstFire]) -> None:
    """Atomic write (tmp + rename), as the entries."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(FIRST_FIRES_HEADER)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


# ---------------------------------------------------------------------
# BIN15 S4: the relabel — both stores onto the venue's law and clock
# ---------------------------------------------------------------------


class RelabelCount(typing.NamedTuple):
    """What one store's relabel did."""

    rows: int
    converted: int
    already: int
    #: Left on the engine's label: no capture on disk covers their stamp.
    uncovered: int
    #: A converted row whose venue stamp another row already held.
    collided: int
    #: Venue rows whose unknown label (-1) the captures now settle.
    filled: int = 0


def _label_of(
    labels: dict[int, claude_worker.bin15_tape.Label], outcome: int
) -> claude_worker.bin15_tape.Label:
    return labels.get(outcome) or claude_worker.bin15_tape.Label(Y_UNKNOWN, Y_UNKNOWN, -1)


def _fill_unknown(rows: list, labels: dict, fill: typing.Callable) -> tuple[list, int]:
    """Venue rows whose ``y`` is unknown take the captures' LAW E-11 label
    when there is one now (a later pull can hold the evidence an earlier
    one lacked). A known label is never touched: idempotent."""
    out = []
    filled = 0
    for r in rows:
        lab = labels.get(r.outcome)
        if r.y == Y_UNKNOWN and lab is not None and lab.y != Y_UNKNOWN:
            out.append(fill(r, lab))
            filled += 1
        else:
            out.append(r)
    return out, filled


def _keep_next(row: typing.Any, lab: claude_worker.bin15_tape.Label) -> int:
    """The captures' venue-published label, or the row's own when the
    captures have none — a known ``y_next_strike`` is never downgraded."""
    return row.y_next_strike if lab.y_next_strike == Y_UNKNOWN else lab.y_next_strike


def _stat(path: pathlib.Path) -> tuple[int, int] | None:
    try:
        st = path.stat()
    except FileNotFoundError:
        return None
    return (st.st_size, st.st_mtime_ns)


def relabel_ledger(
    rows: typing.Sequence[claude_worker.bin15_ledger.Row],
    tapes: typing.Sequence[claude_worker.bin15_tape.VenueTape],
    labels: dict[int, claude_worker.bin15_tape.Label],
) -> tuple[list[claude_worker.bin15_ledger.Row], RelabelCount]:
    """Every ENGINE row a capture covers onto the venue's law and clock:
    ``ts_ns`` moved by its run's :attr:`~claude_worker.bin15_tape.VenueTape.delta_ns`,
    ``y`` = LAW E-11's label, the old one kept as ``y_engine``. Venue rows
    are left exactly as they are, and so is an engine row no capture
    covers (counted). A converted row whose new key a venue row already
    holds is dropped (the venue row wins — merge's law)."""
    venue, filled = _fill_unknown(
        [r for r in rows if r.venue], labels,
        lambda r, lab: dataclasses.replace(r, y=lab.y, y_next_strike=_keep_next(r, lab)),
    )
    keys = {r.key for r in venue}
    out = list(venue)
    converted = uncovered = collided = 0
    for r in rows:
        if r.venue:
            continue
        tape = claude_worker.bin15_tape.covering(tapes, r.ts_ns)
        if tape is None:
            out.append(r)
            uncovered += 1
            continue
        lab = _label_of(labels, r.outcome)
        new = dataclasses.replace(
            r, ts_ns=r.ts_ns + tape.delta_ns, y=lab.y, y_engine=r.y,
            y_next_strike=lab.y_next_strike, venue=True,
        )
        if new.key in keys:
            collided += 1
            continue
        keys.add(new.key)
        out.append(new)
        converted += 1
    out.sort(key=lambda r: (r.ts_ns, r.family, r.outcome))
    return out, RelabelCount(len(rows), converted, len(venue), uncovered, collided, filled)


def relabel_entries(
    rows: typing.Sequence[Entry],
    tapes: typing.Sequence[claude_worker.bin15_tape.VenueTape],
    labels: dict[int, claude_worker.bin15_tape.Label],
) -> tuple[list[Entry], RelabelCount]:
    """:func:`relabel_ledger` for the entry store: ``offset_s`` follows the
    stamp (``(ts − start) // 1 s``, never negative — the harness's own
    arithmetic); one entry per ``(outcome, origin)`` as ever, a venue row
    winning a collision."""
    venue, filled = _fill_unknown(
        [e for e in rows if e.venue], labels,
        lambda e, lab: e._replace(y=lab.y, y_next_strike=_keep_next(e, lab)),
    )
    keys = {(e.outcome, e.origin) for e in venue}
    out = list(venue)
    converted = uncovered = collided = 0
    for e in rows:
        if e.venue:
            continue
        tape = claude_worker.bin15_tape.covering(tapes, e.ts_ns)
        if tape is None:
            out.append(e)
            uncovered += 1
            continue
        if (e.outcome, e.origin) in keys:
            collided += 1
            continue
        lab = _label_of(labels, e.outcome)
        ts = e.ts_ns + tape.delta_ns
        out.append(
            e._replace(
                ts_ns=ts, offset_s=max(ts - e.start_ns, 0) // 1_000_000_000,
                y=lab.y, y_engine=e.y, y_next_strike=lab.y_next_strike, venue=True,
            )
        )
        keys.add((e.outcome, e.origin))
        converted += 1
    out.sort(key=lambda e: (e.ts_ns, e.outcome, e.origin))
    return out, RelabelCount(len(rows), converted, len(venue), uncovered, collided, filled)


def label_changes_by_day(
    rows: typing.Iterable[tuple[int, int, int, int]],
) -> dict[str, tuple[int, int]]:
    """``{expiry UTC day: (instances whose accrued label differs from the
    venue's, instances with both labels known)}`` over ``(outcome,
    expiry_ns, y, y_engine)`` rows, each instance counted once."""
    seen: dict[int, tuple[int, int, int]] = {}
    for outcome, expiry_ns, y, y_engine in rows:
        seen.setdefault(outcome, (expiry_ns, y, y_engine))
    out: dict[str, tuple[int, int]] = {}
    for expiry_ns, y, y_engine in seen.values():
        if Y_UNKNOWN in (y, y_engine):
            continue
        d = day_of(expiry_ns)
        changed, n = out.get(d, (0, 0))
        out[d] = (changed + int(y != y_engine), n + 1)
    return dict(sorted(out.items()))


def tape_dirs(pulls: typing.Sequence[str], runs: typing.Sequence[str]) -> list[pathlib.Path]:
    """The capture directories: ``part-*`` under each pull root, ``run-*``
    under each runs root (the logs root when neither is given)."""
    if not pulls and not runs:
        runs = ["~/multivenue/logs"]
    out: list[pathlib.Path] = []
    for root in pulls:
        out += sorted(pathlib.Path(os.path.expanduser(root)).glob("part-*"))
    for root in runs:
        out += sorted(pathlib.Path(os.path.expanduser(root)).glob("run-*"))
    return out


def relabel(
    ledger: pathlib.Path,
    entries: pathlib.Path,
    dirs: typing.Sequence[pathlib.Path],
    report: typing.Callable[[str], None],
    dry_run: bool = False,
    first_fires: pathlib.Path | None = None,
) -> tuple[RelabelCount, RelabelCount]:
    """The ``relabel`` lane: load the captures, label every instance on
    them on LAW E-11, correct both stores (a timestamped backup of each
    changed file first), and say what moved. BIN15 S5: the first-fire
    store, when given, has its unknown labels filled the same way (its
    rows are venue-clocked by construction, so nothing else moves)."""
    tapes = claude_worker.bin15_tape.load_all(dirs, report)
    if tapes:
        deltas = sorted(t.delta_ns / 1e9 for t in tapes)
        report(
            f"bin15-accrue relabel: the anchor law ran early by {deltas[0]:.1f} .. "
            f"{deltas[len(deltas) // 2]:.1f} (median) .. {deltas[-1]:.1f} s against the venue"
        )
    insts = claude_worker.bin15_tape.instances(tapes)
    marks = claude_worker.bin15_tape.marks_by_underlying(tapes)
    labels = {o: claude_worker.bin15_tape.label(i, marks) for o, i in insts.items()}
    settled = sum(1 for lab in labels.values() if lab.y != Y_UNKNOWN)
    checked = [
        lab for lab in labels.values() if Y_UNKNOWN not in (lab.y, lab.y_next_strike)
    ]
    disagree = sum(1 for lab in checked if lab.y != lab.y_next_strike)
    report(
        f"bin15-accrue relabel: {len(tapes)} capture(s) of {len(dirs)}; "
        f"{len(insts)} instance(s), {settled} settled on LAW E-11 "
        f"(TWAP[T-W, T] >= strike, strict evidence); venue-published check: "
        f"{len(checked)} checked, {disagree} disagree"
    )
    # The stores are read ONCE and replaced whole; an accrual that lands in
    # between would be lost. Their stat at read time is checked again just
    # before each replace, and a change aborts that write (rerun).
    seen = (_stat(ledger), _stat(entries), None if first_fires is None else _stat(first_fires))
    led_rows = claude_worker.bin15_ledger.read_ledger(ledger)
    ent_rows = read_entries(entries)
    ff_rows = [] if first_fires is None else read_first_fires(first_fires)
    new_led, cl = relabel_ledger(led_rows, tapes, labels)
    new_ent, ce = relabel_entries(ent_rows, tapes, labels)
    new_ff, filled_ff = _fill_unknown(
        ff_rows, labels, lambda r, lab: r._replace(y=lab.y, y_next_strike=_keep_next(r, lab))
    )
    cf = RelabelCount(len(ff_rows), 0, len(ff_rows), 0, 0, filled_ff)
    if first_fires is not None:
        report(f"  first fires: rows {cf.rows}: unknown labels filled {cf.filled}")
    for name, c in (("ledger", cl), ("entries", ce)):
        report(
            f"  {name}: rows {c.rows}: converted {c.converted}, already on the venue "
            f"law {c.already} (unknown labels filled {c.filled}), left on the engine "
            f"label (no capture covers them) {c.uncovered}, collided {c.collided}"
        )
    by_day = label_changes_by_day(
        (r.outcome, insts[r.outcome].expiry_ns, r.y, r.y_engine)
        for r in new_led
        if r.venue and r.outcome in insts
    )
    if by_day:
        report(
            "  ledger instances whose accrued label differs from the venue's, by expiry day: "
            + "  ".join(f"{d[5:]} {c}/{n}" for d, (c, n) in by_day.items())
            + f"  (total {sum(c for c, _ in by_day.values())}/{sum(n for _, n in by_day.values())})"
        )
    both = [e for e in new_ent if e.venue and Y_UNKNOWN not in (e.y, e.y_engine)]
    report(
        f"  entries settled on both labels: {len(both)}, flipped by the venue's law: "
        f"{sum(1 for e in both if e.y != e.y_engine)}"
    )
    if dry_run:
        report("  --dry-run: nothing written")
        return cl, ce
    stamp = datetime.datetime.now(datetime.UTC).strftime("%Y%m%dT%H%M%SZ")
    stores = [
        (ledger, cl, seen[0], lambda p: claude_worker.bin15_ledger.write_ledger(p, new_led)),
        (entries, ce, seen[1], lambda p: write_entries(p, new_ent)),
    ]
    if first_fires is not None:
        stores.append((first_fires, cf, seen[2], lambda p: write_first_fires(p, new_ff)))
    for path, count, before, write in stores:
        if not (count.converted or count.collided or count.filled):
            continue
        if _stat(path) != before:
            raise RuntimeError(
                f"{path} changed while the relabel was reading the captures (an accrual?) "
                "— nothing written to it; rerun the relabel"
            )
        if path.is_file():
            shutil.copyfile(path, path.with_name(f"{path.name}.bak-relabel-{stamp}"))
        write(path)
    return cl, ce


# ---------------------------------------------------------------------
# driving the harness
# ---------------------------------------------------------------------


def by_origin(
    entries: typing.Sequence[Entry],
) -> list[tuple[int, list[Entry]]]:
    """``[(origin, entries)]``, VENUE first, each non-empty.

    VENUE leads because it is what the account actually did; PAPER is
    the model's opinion about the same market. An origin with no
    entries is absent rather than rendered as a row of zeroes — a
    reader must be able to tell "no live entries yet" from "live
    entries that netted nothing".
    """
    groups: dict[int, list[Entry]] = {}
    for e in entries:
        groups.setdefault(e.origin, []).append(e)
    return [(o, groups[o]) for o in sorted(groups)]


def day_of(epoch_ns: int) -> str:
    return datetime.datetime.fromtimestamp(
        epoch_ns / 1e9, datetime.UTC
    ).strftime("%Y-%m-%d")


def hip4_runs(replay_dir: pathlib.Path, day: str | None) -> list[pathlib.Path]:
    """Run dirs of ``day`` (all days when None) that carry a HIP-4 roll.

    A run with no rolling family holds no instance and is skipped rather
    than audited for nothing.
    """
    out: list[pathlib.Path] = []
    for p in sorted(replay_dir.glob("run-*")):
        try:
            epoch = int(p.name.split("-")[1])
        except (IndexError, ValueError):
            continue
        if day is not None and day_of(epoch) != day:
            continue
        try:
            if claude_worker.hip4.read_rolls(p):
                out.append(p)
        except Exception:  # a torn or unreadable events file is not a run
            continue
    return out


def cutoff_ms_of(run: pathlib.Path, lo_s: float) -> int:
    """A window's first wall instant, ms — the seed cutoff.

    From the run dir's NAME (`run-<epoch_ns>` is its start wall time)
    plus the cut's offset, not from the PMLR stamps: those are MONOTONIC
    and turning them into wall needs the anchor, which is one more thing
    to get wrong for a number the filename already states exactly.
    """
    return (int(run.name.split("-")[1]) + int(lo_s * 1_000_000_000)) // 1_000_000


def seed_for(bin15: pathlib.Path, dst: pathlib.Path, cutoff_ms: int) -> bool:
    """Cut a seed AS OF ``cutoff_ms`` from candles.db — the same
    ``bin15_seed seed-all --now-ms`` the engine wrapper runs at boot.
    See the module docstring for why nothing cheaper is honest."""
    dst.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        [
            sys.executable, "-m", "claude_worker.bin15_seed", "seed-all",
            "--bin15", str(bin15), "--dir", str(dst), "--now-ms", str(cutoff_ms),
        ],
        capture_output=True, text=True, check=False,
    )
    if proc.returncode != 0:
        print(
            f"bin15-accrue: seed-all @{cutoff_ms} exit {proc.returncode}: "
            f"{proc.stderr.strip()[-200:]}",
            file=sys.stderr,
        )
        return False
    return True


def drive(
    unit: pathlib.Path,
    bin15: pathlib.Path,
    seed_dir: pathlib.Path,
    detail: pathlib.Path,
    fee_flag: str,
    run_fn: typing.Callable[[list[str]], int] | None = None,
) -> bool:
    """One ≤ 2 h window through `backtest --member bin15`."""
    argv = [
        claude_worker.backtest.ENGINE_BINARY, "backtest",
        "--replay-dir", str(unit),
        # All-OOS: there is no ruleset being fitted here, so an IS half
        # would only hide instances from the count.
        "--split", "0/100",
        "--member", "bin15",
        "--bin15", str(bin15),
        "--bin15-seed-dir", str(seed_dir),
        "--fee-bps", fee_flag,
        "--emit-detail", str(detail),
    ]
    if run_fn is not None:
        return run_fn(argv) == 0
    proc = subprocess.run(argv, capture_output=True, text=True, check=False)
    if proc.returncode != 0:
        print(
            f"bin15-accrue: {unit.name}: harness exit {proc.returncode}: "
            f"{proc.stderr.strip()[-300:]}",
            file=sys.stderr,
        )
        return False
    return True


def accrue(
    replay_dir: pathlib.Path,
    bin15: pathlib.Path,
    day: str | None,
    ledger: pathlib.Path,
    entries: pathlib.Path,
    first_fires: pathlib.Path,
    fee_flag: str,
    report: typing.Callable[[str], None] = lambda s: None,
) -> tuple[int, int, int, int]:
    """``(windows driven, ledger rows added, entries added, first fires
    added)``."""
    runs = hip4_runs(replay_dir, day)
    if not runs:
        report(f"bin15-accrue: no HIP-4 run for {day or 'any day'}")
        return (0, 0, 0, 0)
    new_ledger: list = []
    new_entries: list[tuple[Entry, tuple[int, int]]] = []
    new_fires: list[FirstFire] = []
    dropped_fires = 0
    units = 0
    with tempfile.TemporaryDirectory() as tmp:
        root = pathlib.Path(tmp)
        for run in runs:
            try:
                cuts = claude_worker.window_root.windows_of(run)
            except claude_worker.window_root.WindowError as exc:
                report(f"bin15-accrue: {run.name}: cannot window ({exc}) — skipped")
                continue
            for lo, hi in cuts:
                unit = root / f"unit-{run.name}-{lo:.0f}"
                shutil.rmtree(unit, ignore_errors=True)
                unit.mkdir(parents=True)
                try:
                    claude_worker.window_root.cut_run(run, unit, lo, hi)
                except claude_worker.window_root.WindowError as exc:
                    report(f"bin15-accrue: {run.name}@{lo:.0f}s: cut failed ({exc})")
                    shutil.rmtree(unit, ignore_errors=True)
                    continue
                seed_dir = root / f"seed-{unit.name}"
                detail = root / f"{unit.name}.json"
                units += 1
                if seed_for(bin15, seed_dir, cutoff_ms_of(run, lo)) and drive(
                    unit, bin15, seed_dir, detail, fee_flag
                ) and detail.is_file():
                    text = detail.read_text(encoding="utf-8")
                    new_ledger.extend(
                        claude_worker.bin15_ledger.rows_from_sidecar(text)
                    )
                    law = entry_law_of(json.loads(text))
                    new_entries.extend((e, law) for e in entries_from_sidecar(text))
                    fires, dropped = first_fires_from_sidecar(text)
                    new_fires.extend(fires)
                    dropped_fires += dropped
                shutil.rmtree(unit, ignore_errors=True)
                shutil.rmtree(seed_dir, ignore_errors=True)
    merged_l, added_l = claude_worker.bin15_ledger.merge(
        claude_worker.bin15_ledger.read_ledger(ledger), new_ledger
    )
    claude_worker.bin15_ledger.write_ledger(ledger, merged_l)
    # BIN15 S5: an instance whose first fire is stored under another entry
    # law (a day re-accrued after the artifact changed) keeps its stored
    # entry -- merging this law's would pair two laws in one number.
    kept = same_law_entries(new_entries, read_first_fires(first_fires))
    if len(kept) < len(new_entries):
        report(
            f"bin15-accrue: {len(new_entries) - len(kept)} entr(ies) of instances accrued "
            "under another entry law — not merged (never accrue one day under two laws)"
        )
    merged_e, added_e = merge_entries(read_entries(entries), kept)
    write_entries(entries, merged_e)
    if dropped_fires:
        report(
            f"bin15-accrue: {dropped_fires} first fire(s) dropped — their run has no "
            "venue clock (the counterfactual store is on the venue's law only)"
        )
    merged_f, added_f = merge_first_fires(read_first_fires(first_fires), new_fires)
    if merged_f:
        write_first_fires(first_fires, merged_f)
    return (units, added_l, added_e, added_f)


# ---------------------------------------------------------------------
# the report
# ---------------------------------------------------------------------


def wilson(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    """Wilson score interval on ``k/n`` — the one that stays inside
    [0, 1] and does not lie at the small n this lane has."""
    if n == 0:
        return (0.0, 1.0)
    den = n + z * z
    centre = (k + z * z / 2) / den
    half = (z / den) * math.sqrt(k * (n - k) / n + z * z / 4)
    return (max(0.0, centre - half), min(1.0, centre + half))


def needed_n(p: float, edge: float, z: float = 2.0) -> int:
    """Settled entries to resolve ``edge`` at ``z`` SE, at hit rate p."""
    if edge <= 0:
        return 0
    return int(math.ceil(z * z * p * (1 - p) / (edge * edge)))


def on_label(entries: typing.Sequence[Entry], label: str) -> tuple[list[Entry], int]:
    """``(entries scored on ONE label, entries that carry none)``.

    ``venue``: LAW E-11's ``y`` — relabelled or venue-accrued rows only;
    an engine row is not the venue's outcome and is left out (counted).
    ``engine``: the label each entry was accrued with — ``y_engine`` on a
    relabelled row, ``y`` on an engine row; a venue-accrued row has none.
    The report prints the two side by side until every entry is the
    venue's (BIN15 S4); they are never mixed in one number.
    """
    if label not in ("venue", "engine"):
        raise ValueError(f"label must be venue or engine, got {label!r}")
    out: list[Entry] = []
    missing = 0
    for e in entries:
        if label == "venue":
            if e.venue:
                out.append(e)
            else:
                missing += 1
        elif not e.venue:
            out.append(e)
        elif e.y_engine != Y_UNKNOWN:
            out.append(e._replace(y=e.y_engine))
        else:
            missing += 1
    return out, missing


def render(entries: typing.Sequence[Entry], fee_bps: int) -> list[str]:
    """The three operator questions, with the error bar that decides
    whether any of them is an answer yet.

    **One accounting per call.** §6.4: PAPER and VENUE are never mixed
    in one number. That is enforced here rather than left to the
    caller, because every figure below — the hit rate, the cost, the
    payout, the Wilson interval — is a sum over whatever it is handed,
    and a function that silently averages a modelled fill with a real
    one produces a number that looks exactly like the ones that are
    true. :func:`by_origin` is how a caller splits a store.

    Raises:
        ValueError: the sequence carries more than one ``origin``.
    """
    origins = sorted({e.origin for e in entries})
    if len(origins) > 1:
        raise ValueError(
            "render() was handed a MIXED set ("
            + ", ".join(ORIGIN_NAMES.get(o, str(o)) for o in origins)
            + "). PAPER and VENUE are never summed into one number "
            "(plan §6.4) — split with by_origin() and render each."
        )
    out: list[str] = []
    settled = [e for e in entries if e.settled]
    days = {day_of(e.ts_ns) for e in entries}
    out.append(
        f"entries {len(entries)} over {len(days)} UTC day(s); "
        f"{len(settled)} settled, {len(entries)-len(settled)} straddled a window "
        "edge or a restart gap (counted, never guessed)"
    )
    if not settled:
        return out + ["no settled entry yet: nothing to score"]

    n = len(settled)
    k = sum(1 for e in settled if e.won)
    p = k / n
    avg_px = statistics.fmean(e.px_1e6 / 1e6 for e in settled)
    cost = sum(e.px_1e6 / 1e6 * e.qty_1e6 / 1e6 for e in settled)
    payout = sum(e.qty_1e6 / 1e6 for e in settled if e.won)
    fee = payout * fee_bps / 10_000.0
    lo, hi = wilson(k, n)
    se = math.sqrt(p * (1 - p) / n)

    out.append("")
    out.append(f"1. PREDICTION SUCCESS: {k}/{n} = {100*p:.1f} %")
    out.append(f"   average price paid {avg_px:.4f} => break-even hit rate {100*avg_px:.1f} %")
    # DOLLARS are the verdict, not the hit rate and not a ratio of
    # averages: a win at 0.96 returns 4 c on 96 c staked while a win at
    # 0.48 returns 52 c on 48 c, so `p/a - 1` at the MEAN price can
    # disagree in sign with what the account did. It has, on this lane.
    # `+$150.40`, not `$+150.40`: the sign belongs to the amount.
    def usd(v: float) -> str:
        return f"{'-' if v < 0 else '+'}${abs(v):,.2f}"

    out.append(
        f"   AS TRADED: cost ${cost:,.2f}  payout ${payout:,.2f}  "
        f"gross {usd(payout-cost)} ({100*(payout-cost)/cost:+.2f} %)  "
        f"net of the {fee_bps} bps exit leg {usd(payout-cost-fee)}"
    )
    # Doc 27 §1: EV per dollar staked is p/a − 1 per entry, and the
    # account's number is its STAKE-weighted mean — which is exactly the
    # gross over the cost. Named, so nobody reads the hit rate for it.
    out.append(
        f"   EV per $ staked (p/a - 1, stake-weighted): {(payout-cost)/cost:+.4f} gross, "
        f"{(payout-cost-fee)/cost:+.4f} net"
    )
    out.append(
        f"   Wilson 95 % CI on the hit rate [{100*lo:.1f} %, {100*hi:.1f} %] — "
        f"break-even {100*avg_px:.1f} % is "
        f"{'INSIDE (nothing is settled yet)' if lo <= avg_px <= hi else 'OUTSIDE'}"
    )
    if se > 0:
        out.append(
            f"   edge over break-even {100*(p-avg_px):+.1f} pts = {(p-avg_px)/se:.2f} SD; "
            "settled entries needed to resolve, at 2 SE:"
        )
        for e_ in (0.05, 0.08, 0.10):
            need = needed_n(p, e_)
            more = max(0, need - n)
            out.append(
                f"     {100*e_:>3.0f} pts -> n = {need:5d}  ({more} more, "
                f"~{more/96:.1f} days at 96 instances/day on one family)"
            )
    # Phases keyed as the ENGINE keys its recalibration since BIN15 S3: on
    # the pricing horizon of the venue's window, not the raw time left.
    for ph in range(claude_worker.bin15_ref.PHASES):
        sub = [
            e for e in settled
            if claude_worker.bin15_ref.phase_of(
                claude_worker.bin15_ref.pricing_horizon_ns(
                    max(e.expiry_ns - e.ts_ns, 0), claude_worker.bin15_ledger.TWAP_15M_NS
                )
            ) == ph
        ]
        if sub:
            sk = sum(1 for e in sub if e.won)
            spx = statistics.fmean(e.px_1e6 / 1e6 for e in sub)
            flag = "  <-- under its price" if sk / len(sub) < spx else ""
            out.append(
                f"   phase {claude_worker.bin15_ledger.PHASE_NAMES[ph]:<5}: "
                f"{sk}/{len(sub)} = {100*sk/len(sub):5.1f} %  "
                f"break-even {100*spx:5.1f} %{flag}"
            )

    offs = sorted(e.offset_s for e in entries)
    out.append("")
    out.append("2. ENTRY TIMING, seconds from the instance's start (0 .. 900):")
    out.append(
        f"   min {offs[0]}  p25 {offs[len(offs)//4]}  median {offs[len(offs)//2]}  "
        f"p75 {offs[3*len(offs)//4]}  max {offs[-1]}  mean {statistics.fmean(offs):.0f}"
    )
    buckets = collections.Counter(min(o // 60, 14) for o in offs)
    for b in range(15):
        if buckets[b]:
            out.append(f"   [{b*60:3d}-{b*60+59:3d}s] {buckets[b]:4d} {'#' * min(buckets[b], 50)}")

    px = [e.px_1e6 / 1e6 for e in entries]
    qty = [e.qty_1e6 / 1e6 for e in entries]
    out.append("")
    out.append("3. ENTRY PRICE (all entries, settled or not):")
    out.append(
        f"   mean {statistics.fmean(px):.4f}  median {statistics.median(px):.4f}  "
        f"min {min(px):.4f}  max {max(px):.4f}"
    )
    out.append(
        f"   size mean {statistics.fmean(qty):.0f} contracts, mean notional "
        f"${statistics.fmean(p_ * q for p_, q in zip(px, qty)):.2f}"
    )
    yes = sum(1 for e in entries if e.is_yes == 1)
    out.append(f"   side {yes} YES / {len(entries)-yes} NO")
    return out


def ev_per_dollar(won: bool, px_1e6: int) -> float:
    """One dollar staked at ``px``: ``1/px - 1`` when the side won, ``-1``
    when it lost -- doc 27 §1's ``p/a - 1`` with the outcome for ``p``."""
    return (1_000_000 / px_1e6 - 1.0) if won else -1.0


def render_counterfactual(
    entries: typing.Sequence[Entry], fires: typing.Sequence[FirstFire]
) -> list[str]:
    """BIN15 S5 (ruling O-4) + S5b: the member's entries beside the two
    counterfactuals -- each the first passing reprice at persist 1 with no
    ceiling, logged, never traded -- on the VENUE's label, one block per
    entry law the rows were accrued under, never mixed:

    * the ARTIFACT's own test, unpersisted: what the persistence law and the
      ceiling alone changed;
    * TODAY'S law, the control: doc 27 §5's CONFIRM bar "≥ +5 pts over
      today's law on the same instances".

    Each is paired with the entries by outcome on the instances the member
    entered (the fire's own ``entered``: an entry its law declined -- a day
    re-accrued under another law -- is not a pair), and scored alone on the
    instances it would have bought and the member declined. Both sides at an
    equal dollar per instance (a counterfactual's size was never decided),
    so the EV is a mean of ``won/px - 1``, not the stake-weighted figure
    above it. A pairing identical by construction -- the artifact's test
    under the old law itself, or the control wherever it IS the artifact's
    test (the artifact's bound is today's) -- is counted, not compared. A
    control that is absent is either one that never held or a row accrued
    before S5b, which recorded none; the report does not guess which. The
    §5 fill bar is not measurable in the paper model: ruling 2026-09-24
    gates it at the first live step.
    """
    out: list[str] = []
    entered = sum(f.entered for f in fires)
    out.append(
        "counterfactuals (the first passing reprice at persist 1, no ceiling -- logged, not "
        f"traded): {len(fires)} instance(s), {sum(f.settled for f in fires)} settled; "
        f"{entered} the member entered, {len(fires) - entered} it declined; the artifact's "
        f"test held on {sum(f.fired for f in fires)}, today's law (the control) on "
        f"{sum(f.ctl_fired for f in fires)}"
    )
    fire_of = {f.outcome: f for f in fires}
    ours = [e for e in entries if e.venue]
    unpaired = sum(1 for e in ours if e.outcome not in fire_of)
    if unpaired:
        out.append(f"   {unpaired} entr(ies) with no first fire (accrued before S5) -- not paired")
    for law in sorted({f.law for f in fires}):
        persist, ceiling = law
        name = f"persist {persist}, " + (
            f"ceiling {ceiling / 1e9:g} s" if ceiling else "no ceiling"
        )
        group = [f for f in fires if f.law == law]
        mine = [
            e for e in ours
            if e.settled and e.outcome in fire_of
            and fire_of[e.outcome].law == law and fire_of[e.outcome].entered
        ]
        if law == OLD_LAW:
            out.append(
                f"   -- entry law {name}: the old law itself, {len(group)} instance(s) -- its "
                "entries ARE its unpersisted trigger, not a comparison"
            )
        else:
            out.append(f"   -- entry law {name}: {len(group)} instance(s)")
            out.extend(_pairing(mine, fire_of, group, "the artifact's unpersisted trigger", False))
        fired = [f for f in group if f.fired]
        ctl_only = sum(1 for f in group if f.ctl_fired and not f.fired)
        if not any(f.ctl_fired for f in group):
            out.append(
                "      today's law (the control): no fire recorded (accrued before S5b, or it "
                "never held) -- not paired"
            )
        elif not ctl_only and all(f.control() == f for f in fired):
            out.append(
                f"      today's law (the control): the artifact's own test on all {len(fired)} "
                "instance(s) (its bound is today's) -- the same fires, not a second comparison"
            )
        else:
            out.extend(_pairing(mine, fire_of, group, "today's law (the control)", True))
    out.append(
        "   IoC fills (doc 27 §5's fill bar): not measurable in the paper model -- the harness "
        "models every fill; gated at the first live step (ruling 2026-09-24)"
    )
    return out


def _pairing(
    mine: typing.Sequence[Entry],
    fire_of: dict[int, FirstFire],
    group: typing.Sequence[FirstFire],
    name: str,
    control: bool,
) -> list[str]:
    """One counterfactual against the member's settled entries ``mine``:
    the pairs on the SAME instances, then the instances it would have bought
    that the member declined."""
    def view(f: FirstFire) -> FirstFire:
        return f.control() if control else f

    out: list[str] = []
    fired = [(e, view(fire_of[e.outcome])) for e in mine if view(fire_of[e.outcome]).fired]
    if len(fired) < len(mine):
        why = "or accrued before S5b" if control else "the harness lost it"
        out.append(
            f"      {len(mine) - len(fired)} settled entr(ies) {name} never bought ({why}) "
            "-- not paired"
        )
    pairs = [(e, f) for e, f in fired if f.settled]
    if len(pairs) < len(fired):
        out.append(
            f"      {len(fired) - len(pairs)} settled entr(ies) whose counterfactual is unsettled "
            "(run `relabel`) -- left out"
        )
    if pairs:
        hit_new, ev_new, px_new = _scored([e for e, _ in pairs])
        hit_old, ev_old, px_old = _scored([f for _, f in pairs])
        out.append(
            f"      vs {name}: the SAME {len(pairs)} instance(s): hit {100*hit_new:.1f} % vs "
            f"{100*hit_old:.1f} % ({100*(hit_new-hit_old):+.1f} pts); EV per $1 {ev_new:+.4f} "
            f"vs {ev_old:+.4f} ({100*(ev_new-ev_old):+.1f} pts); mean price {px_new:.4f} vs "
            f"{px_old:.4f}"
        )
    declined = [view(f) for f in group if f.settled and not f.entered and view(f).fired]
    if declined:
        hit, ev, px = _scored(declined)
        out.append(
            f"      the {len(declined)} instance(s) it declined that {name} would have bought: "
            f"hit {100*hit:.1f} % at a mean {px:.4f}, EV per $1 {ev:+.4f}"
        )
    return out


def _scored(rows: typing.Sequence[Entry | FirstFire]) -> tuple[float, float, float]:
    """``(hit rate, equal-dollar EV per $1, mean price)`` of settled rows."""
    return (
        sum(r.won for r in rows) / len(rows),
        statistics.fmean(ev_per_dollar(r.won, r.px_1e6) for r in rows),
        statistics.fmean(r.px_1e6 for r in rows) / 1e6,
    )


def main(argv: list[str] | None = None) -> int:
    """CLI entry point; the verb surface stays frozen once published."""
    ap = argparse.ArgumentParser(prog="claude_worker.bin15_accrue")
    sub = ap.add_subparsers(dest="lane", required=True)

    ac = sub.add_parser("accrue", help="drive the day and merge into the stores")
    ac.add_argument("--day", default=None, help="YYYY-MM-DD; absent = the closed UTC day")
    ac.add_argument("--replay-dir", default=None)
    ac.add_argument("--bin15", default=None)
    ac.add_argument("--ledger", default=None)
    ac.add_argument("--entries", default=None)
    ac.add_argument("--first-fires", default=None)
    ac.add_argument("--fee-bps", default="hl.prediction:0:0")

    rp = sub.add_parser("report", help="the three questions, with the error bar")
    rp.add_argument("--entries", default=None)
    rp.add_argument("--first-fires", default=None)
    rp.add_argument("--fee-bps-exit", type=int, default=0)

    st = sub.add_parser("status", help="rows, instances, settled, days")
    st.add_argument("--entries", default=None)
    st.add_argument("--first-fires", default=None)

    rl = sub.add_parser(
        "relabel", help="BIN15 S4: correct both stores in place onto the venue's law and clock"
    )
    rl.add_argument(
        "--pull", action="append", default=[], help="a root of part-* pulls (repeatable)"
    )
    rl.add_argument("--runs", action="append", default=[], help="a root of run-* dirs (repeatable)")
    rl.add_argument("--ledger", default=None)
    rl.add_argument("--entries", default=None)
    rl.add_argument("--first-fires", default=None)
    rl.add_argument("--dry-run", action="store_true")

    args = ap.parse_args(argv)

    if args.lane == "relabel":
        dirs = tape_dirs(args.pull, args.runs)
        relabel(
            claude_worker.bin15_ledger.ledger_path(args.ledger),
            entries_path(args.entries),
            dirs,
            print,
            dry_run=args.dry_run,
            first_fires=first_fires_path(args.first_fires),
        )
        return 0

    if args.lane == "accrue":
        replay = pathlib.Path(
            os.path.expanduser(args.replay_dir or "~/multivenue/logs")
        )
        bin15 = pathlib.Path(os.path.expanduser(args.bin15 or "~/multivenue/bin15.toml"))
        if not bin15.is_file():
            print(f"bin15-accrue: {bin15}: no such artifact", file=sys.stderr)
            return 2
        day = args.day
        if day is None:
            now = datetime.datetime.now(datetime.UTC)
            day = (now - datetime.timedelta(days=1)).strftime("%Y-%m-%d")
        led = claude_worker.bin15_ledger.ledger_path(args.ledger)
        ent = entries_path(args.entries)
        ff = first_fires_path(args.first_fires)
        units, dl, de, df = accrue(
            replay, bin15, day, led, ent, ff, args.fee_bps, lambda s: print(s, file=sys.stderr)
        )
        print(
            f"bin15-accrue: day {day}: {units} window(s) <= 2 h driven; "
            f"+{dl} ledger row(s) -> {led}; +{de} entry(ies) -> {ent}; "
            f"+{df} first fire(s) -> {ff}"
        )
        return 0

    ent = entries_path(args.entries)
    rows = read_entries(ent)
    fires = read_first_fires(first_fires_path(args.first_fires))
    if args.lane == "status":
        print(f"bin15-accrue: {ent} entries={len(rows)} first_fires={len(fires)}")
        for origin, group in by_origin(rows):
            settled = [e for e in group if e.settled]
            venue = sum(1 for e in group if e.venue)
            print(
                f"  {ORIGIN_NAMES.get(origin, origin)}: entries={len(group)} "
                f"settled={len(settled)} "
                f"days={len({day_of(e.ts_ns) for e in group})} "
                f"families={len({e.family for e in group})} "
                f"venue_label={venue} engine_label={len(group) - venue}"
            )
        return 0

    print(f"bin15-accrue: {ent}")
    groups = by_origin(rows)
    if not groups:
        print("  no entries yet")
        return 0
    for origin, group in groups:
        # The word §6.4 requires beside every BIN15 dollar figure, on
        # its own line above the figures rather than buried in one of
        # them, so that quoting a number without it takes effort.
        print(f"  == {ORIGIN_NAMES.get(origin, origin)} accounting — {len(group)} entr(ies)")
        # BIN15 S4: the VENUE's label first — LAW E-11, what the venue
        # pays on — and the label each entry was accrued with beside it,
        # until every entry is on the venue's. Never one mixed number.
        venue, engine_only = on_label(group, "venue")
        print(
            f"    -- VENUE label (LAW E-11: TWAP[T-60 s, T] >= strike): {len(venue)} entr(ies); "
            f"{engine_only} still on the engine label (run `relabel`)"
        )
        lines = render(venue, args.fee_bps_exit) if venue else ["no entry on the venue label yet"]
        for line in lines:
            print(f"    {line}")
        if origin == ORIGIN_PAPER and fires:
            # BIN15 S5: first fires come from the harness, so they sit
            # beside the PAPER accounting and nowhere else.
            for line in render_counterfactual(venue, fires):
                print(f"    {line}")
        engine, _ = on_label(group, "engine")
        if engine:
            print(
                f"    -- ENGINE label, as accrued (the label before the relabel), beside it: "
                f"{len(engine)} entr(ies)"
            )
            for line in render(engine, args.fee_bps_exit):
                print(f"    {line}")
    return 0


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
