# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_tape -- a HIP-4 capture read on the VENUE clock (BIN15 S4).

One capture directory: a ``run-<epoch_ns>/`` under the logs root, or a
``part-<epoch_ns>/`` pulled from the archive with its ``anchor.json``
(``{"epoch_ns", "anchor_ns"}``: the run's name and its first tick,
because a pull carries only some of the run's files). What the offline
readers of the lane need from it, and nothing else:

* the underlying MARKS of every rolling family in the manifest, per
  underlying descriptor (``hyperliquid:BTC``), stamped on the venue
  clock -- ``ts + claude_worker.hip4.venue_wall_offset_ns`` (the median of
  ``venue_time_ms - ts`` over the tick file);
* the CREATED rolls, venue-clocked, with the slot named by its
  descriptor (a sym means nothing across runs);
* the two numbers the accrual stores' clock correction needs:
  :attr:`VenueTape.old_span` -- where the harness's pre-S2 ANCHOR law
  (``epoch + (ts - first_tick)``) put the run -- and
  :attr:`VenueTape.delta_ns`, what that law was off by against the venue
  (``offset - (epoch - first_tick)``, 22-42 s on the lane's captures).

A capture without venue time (no v3 ``venue_time_ms``) cannot be put on
the venue clock; it is not a tape (:func:`load` returns ``None``) rather
than a guess -- and neither is one whose fit would put its first tick
more than :data:`EPOCH_SKEW_TOLERANCE_NS` BEFORE its own directory's
epoch (impossible for a true clock: the directory is named at boot; the
harness refuses the same fit, ``cli::backtest::clock``).

Offline tool -- allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import numpy

import claude_worker.hip4
import claude_worker.pmlr
import claude_worker.window_root

#: ``ChannelEvent`` -- ``claude_worker.pmlr``'s ``<QIBB2xQQqq16x``, as a
#: numpy dtype so a multi-hour events file is one ``fromfile``.
EV_DT: numpy.dtype = numpy.dtype(
    [
        ("ts", "<u8"), ("sym", "<u4"), ("venue", "u1"), ("channel", "u1"), ("pad", "V2"),
        ("seq", "<u8"), ("vt", "<u8"), ("v0", "<i8"), ("v1", "<i8"), ("pad2", "V16"),
    ]
)
assert EV_DT.itemsize == claude_worker.pmlr.SLOT_SIZE

#: The events file of the Hyperliquid lane.
EVENTS_FILE: str = "hl-events.pmlr"

#: The pull's anchor record.
ANCHOR_FILE: str = "anchor.json"

#: The manifest: ``sym<TAB>descriptor``.
MANIFEST_FILE: str = "instrument-manifest.tsv"

#: ``cli::backtest::clock::EPOCH_SKEW_TOLERANCE_NS`` -- the most a venue
#: fit may put a run's first tick before the run's epoch.
EPOCH_SKEW_TOLERANCE_NS: int = 2_000_000_000


def underlying_of(slot: str) -> str | None:
    """``cli::backtest::binary::underlying_descriptor_of``:
    ``hyperliquid:out:BTC:15m[yes]`` -> ``hyperliquid:BTC``; ``None`` for
    anything that is not a rolling family's Yes leg."""
    if not slot.startswith("hyperliquid:") or not slot.endswith("[yes]"):
        return None
    key = slot[len("hyperliquid:") : -len("[yes]")]
    if not (key.startswith("out:") or key.startswith("native:")):
        return None
    parts = key.split(":")
    if len(parts) != 3 or not parts[1] or not parts[2]:
        return None
    return f"hyperliquid:{parts[1]}"


def read_manifest(run_dir: pathlib.Path) -> dict[int, str]:
    """``sym -> descriptor``; empty when the run has none."""
    out: dict[int, str] = {}
    path = run_dir / MANIFEST_FILE
    if not path.is_file():
        return out
    for line in path.read_text(encoding="utf-8").splitlines():
        cols = line.split("\t")
        if len(cols) >= 2 and cols[0].isdigit():
            out[int(cols[0])] = cols[1]
    return out


def _events(path: pathlib.Path) -> numpy.ndarray:
    """The file's ChannelEvent slots, memory-mapped read-only (no copy of a
    multi-hour file) after the house reader has checked its header."""
    with claude_worker.pmlr.Reader(path) as reader:
        if reader.slot_kind != claude_worker.pmlr.SLOT_KIND_CHANNEL_EVENT:
            raise claude_worker.pmlr.PmlrError(f"{path}: not a ChannelEvent file")
        n = len(reader)
    if n == 0:
        return numpy.zeros(0, dtype=EV_DT)
    return numpy.memmap(
        path, dtype=EV_DT, mode="r", offset=claude_worker.pmlr.HEADER_SIZE, shape=(n,)
    )


class VenueTape(typing.NamedTuple):
    """One capture on the venue clock. See the module docstring."""

    name: str
    epoch_ns: int
    anchor_ns: int
    first_ts: int
    last_ts: int
    off_ns: int
    #: underlying descriptor -> (venue ts, mark ×1e6), file order.
    marks: dict[str, tuple[numpy.ndarray, numpy.ndarray]]
    #: CREATED rolls, venue-clocked, file order: (roll, slot descriptor).
    rolls: list[tuple[claude_worker.hip4.Roll, str]]

    @property
    def old_span(self) -> tuple[int, int]:
        """Where the pre-S2 anchor law put this run's first and last record."""
        return (
            self.epoch_ns + (self.first_ts - self.anchor_ns),
            self.epoch_ns + (self.last_ts - self.anchor_ns),
        )

    @property
    def delta_ns(self) -> int:
        """Venue wall minus anchor-law wall, for any record of the run."""
        return self.off_ns - (self.epoch_ns - self.anchor_ns)


def load(run_dir: pathlib.Path) -> VenueTape | None:
    """The tape, or ``None`` when the directory is not one: no events or
    ticks, no run name, no venue time, or no rolling family in its
    manifest."""
    events = run_dir / EVENTS_FILE
    if not events.is_file() or not (run_dir / "hl-ticks.pmlr").is_file():
        return None
    try:
        epoch_ns = int(run_dir.name.split("-", 1)[1])
    except (IndexError, ValueError):
        return None
    span = claude_worker.window_root.run_span(run_dir)
    if span is None:
        return None
    anchor_ns = span[0]
    anchor = run_dir / ANCHOR_FILE
    if anchor.is_file():
        meta = json.loads(anchor.read_text(encoding="utf-8"))
        epoch_ns = int(meta["epoch_ns"])
        anchor_ns = int(meta["anchor_ns"])
    off = claude_worker.hip4.venue_wall_offset_ns(run_dir)
    if off is None or off - (epoch_ns - anchor_ns) < -EPOCH_SKEW_TOLERANCE_NS:
        return None
    manifest = read_manifest(run_dir)
    slots = {sym: d for sym, d in manifest.items() if underlying_of(d) is not None}
    if not slots:
        return None
    by_desc = {d: sym for sym, d in manifest.items()}
    perps = {u: by_desc[u] for u in {underlying_of(d) for d in slots.values()} if u in by_desc}
    try:
        ev = _events(events)
    except (claude_worker.pmlr.PmlrError, OSError, ValueError):
        return None
    marks: dict[str, tuple[numpy.ndarray, numpy.ndarray]] = {}
    # A non-positive mark is not a price: the harness drops it too
    # (`cli::backtest::binary::marks_by_sym`).
    is_mark = (ev["channel"] == claude_worker.pmlr.CHANNEL_MARK) & (ev["v0"] > 0)
    for under, sym in perps.items():
        m = ev[is_mark & (ev["sym"] == sym)]
        marks[under] = (m["ts"].astype(numpy.int64) + off, m["v0"].astype(numpy.int64))
    rolls: list[tuple[claude_worker.hip4.Roll, str]] = []
    for row in ev[ev["channel"] == claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL]:
        sym = int(row["sym"])
        if sym not in slots:
            continue
        outcome, twap_s, family, settled = claude_worker.hip4.unpack_roll_seq(int(row["seq"]))
        if settled or outcome == 0:
            continue
        rolls.append(
            (
                claude_worker.hip4.Roll(
                    ts_ns=int(row["ts"]) + off, sym=sym, family=family, outcome=outcome,
                    strike_1e6=int(row["v0"]), expiry_ns=int(row["v1"]),
                    twap_s=twap_s, settled=False,
                ),
                slots[sym],
            )
        )
    return VenueTape(
        name=run_dir.name, epoch_ns=epoch_ns, anchor_ns=anchor_ns,
        first_ts=span[0], last_ts=span[1], off_ns=off, marks=marks, rolls=rolls,
    )


def load_all(
    dirs: typing.Iterable[pathlib.Path], report: typing.Callable[[str], None] = lambda s: None
) -> list[VenueTape]:
    """Every loadable tape among ``dirs``, oldest (by epoch) first."""
    out: list[VenueTape] = []
    for d in dirs:
        t = load(d)
        if t is None:
            report(f"bin15-tape: {d.name}: not a venue-clocked HIP-4 capture — skipped")
            continue
        out.append(t)
    return sorted(out, key=lambda t: t.epoch_ns)


def covering(tapes: typing.Sequence[VenueTape], old_wall_ns: int) -> VenueTape | None:
    """The tape whose anchor-law span holds ``old_wall_ns`` (a stamp the
    pre-S2 harness wrote), or ``None``. Runs never overlap, so at most one
    does; a stamp in a gap between runs belongs to no tape we have."""
    for t in tapes:
        lo, hi = t.old_span
        if lo <= old_wall_ns <= hi:
            return t
    return None


def instances(tapes: typing.Sequence[VenueTape]) -> dict[int, claude_worker.hip4.Instance]:
    """Every created instance, ONE per outcome (the earliest created row
    on the venue clock: a boot re-announces the live instance), with each
    one's successor strike linked (``hip4.link_successors``)."""
    out: dict[int, claude_worker.hip4.Instance] = {}
    for t in tapes:
        for roll, slot in t.rolls:
            old = out.get(roll.outcome)
            if old is not None and old.created_ns <= roll.ts_ns:
                continue
            out[roll.outcome] = claude_worker.hip4.Instance(
                outcome=roll.outcome, slot=slot, strike_1e6=roll.strike_1e6,
                expiry_ns=roll.expiry_ns, twap_ns=roll.twap_s * 1_000_000_000,
                created_ns=roll.ts_ns,
            )
    claude_worker.hip4.link_successors(list(out.values()))
    return out


def marks_by_underlying(
    tapes: typing.Sequence[VenueTape],
) -> dict[str, tuple[numpy.ndarray, numpy.ndarray]]:
    """Every tape's marks per underlying, concatenated and STABLY sorted
    by venue time (the harness's ``marks_by_sym`` law)."""
    acc: dict[str, list[tuple[numpy.ndarray, numpy.ndarray]]] = {}
    for t in tapes:
        for under, pair in t.marks.items():
            acc.setdefault(under, []).append(pair)
    out: dict[str, tuple[numpy.ndarray, numpy.ndarray]] = {}
    for under, pairs in acc.items():
        ts = numpy.concatenate([p[0] for p in pairs])
        px = numpy.concatenate([p[1] for p in pairs])
        order = numpy.argsort(ts, kind="stable")
        out[under] = (ts[order], px[order])
    return out


class Label(typing.NamedTuple):
    """One instance's labels on the venue's law."""

    #: ``1e6`` / ``0`` on LAW E-11, ``-1`` when the evidence is not there.
    y: int
    #: The venue-published label (``hip4.y_next_strike``), ``-1`` unknown.
    y_next_strike: int
    #: The TWAP the label was read from, ``-1`` with ``y``.
    settle_px_1e6: int


def label(
    inst: claude_worker.hip4.Instance,
    marks: dict[str, tuple[numpy.ndarray, numpy.ndarray]],
) -> Label:
    """``inst`` settled on LAW E-11 against its underlying's marks."""
    under = underlying_of(inst.slot)
    pair = marks.get(under) if under is not None else None
    ref = None
    if pair is not None:
        ref = claude_worker.hip4.settle_reference_1e6(
            pair[0], pair[1], inst.expiry_ns, inst.twap_ns
        )
    y = -1 if ref is None else claude_worker.hip4.payout_1e6(ref, inst.strike_1e6)
    return Label(
        y=y,
        y_next_strike=claude_worker.hip4.y_next_strike(inst.strike_1e6, inst.next_strike_1e6),
        settle_px_1e6=-1 if ref is None else ref,
    )
