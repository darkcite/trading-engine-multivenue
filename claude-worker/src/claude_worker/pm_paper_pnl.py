# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""pm_paper_pnl -- slots 9 (pm15) and 11 (pmj): paper P&L since start, on the venue's result.

The members book a window's P&L at its ``market_resolved``, but only inside
one engine session: ``engine_pm15_pnl_usd_1e6`` / ``engine_pmj_pnl_usd_1e6``
restart at every boot, and a window that ends before a boot and resolves
after it is never settled by the engine at all. This lane keeps the
since-start book across boots (rulings R-PMJ-S27..S30).

**One pass** (``scripts/pm-pnl.sh``, launchd ``com.multivenue.pm-pnl`` at
:06, :21, :36 and :51 -- a window resolves +116..218 s after its end, so a
pass finds the 15-minute windows that just closed resolved; a 5-minute
window that ended a minute before the pass may still be resolving, and
waits for the next one):

1. Reads the PAPER trade fills of slots 9 and 11 (``engine-fills.pmlr``,
   ``strategy_id`` 9 / 11, origin paper, ``FillRec.is_trade``) of every run
   booted at or after ``--from-ns`` (default :data:`FROM_NS`), plus the
   fills an earlier pass left open (``pm/open.tsv``) whose run directory is
   gone. A venue fill of either slot (an armed arm) is never booked here; it
   is counted. A run whose capture cannot be read is reported and skipped,
   never fatal to the pass.
2. Names each fill's window from its client order id -- the members'
   layout: bits 0..32 the instance, 32..48 the family, bit 48 the leg -- and
   its asset from the run's ``instrument-manifest.tsv``. The instance gives
   the window (15 m: ``window * 8 + family``; 5 m: the same with bit 31 set)
   and so the market's slug ``<asset>-updown-<15m|5m>-<open epoch>``. The
   sym's family, pair and leg (its ordinal in the slot pool), the
   descriptor's, the order id's and the instance's must all agree, and the
   pair must be the window's; any disagreement is reported ``unmapped``,
   never booked.
3. Labels the window on the venue's own result: the SETTLED roll row the
   ingress wrote at ``market_resolved`` in the fill's own run
   (``pm15-events`` / ``pm5-events``, kind byte 1, ``v0`` +1 Up / -1 Down).
   When that capture never saw it resolve (a boot between the window's end
   and its resolution; a retired window), Gamma's market by slug:
   ``closed``, ``umaResolutionStatus`` ``resolved``, ``outcomePrices``
   exactly 1 / 0. Gamma also re-checks each capture label once, after the
   windows that need it and within the pass's budget; a disagreement is
   counted, never absorbed. A window neither source labels stays OPEN at
   cost, retried at every pass, never guessed.
4. Prices the fill exactly as its member does, in integer micro-dollars:
   cost ``px * qty / 1e6``; the paper fee ``rate * px * (1 - px) * qty /
   1e18`` (rate 0.07 for slot 9, twice that for slot 11); payout ``qty`` on
   the winning leg, 0 on the other. Deliberate differences from the
   session counter: a window the capture retired unresolved is a full loss
   to the member (``settle`` with an unknown winner pays nothing) and waits
   for Gamma here; a late fill whose instance no market holds any more is
   not booked by the member (``unknown_fills``) and is booked here.
5. Writes every newly settled fill into the LEDGER (``pm/paper-pnl.tsv``
   under the worker dir, keyed ``(run, index)``, rewritten atomically in
   window order: a row is never modified or removed), rewrites
   ``pm/open.tsv``, and writes the summary ``pm/paper-pnl.json`` that the
   dashboard's pm15 and pmj blocks read.

**Segments.** Each fill carries the fill LAW its run ran for its slot
(:data:`SEGMENTS`, by run epoch and slot): ``touch`` (before J8: the touch
alone, capped by its size), ``j8`` (the FAK sweep behind the touch; slot 9's
0.90 floor) and, for slot 11 only, ``j11`` (the sweep judged 5 s after
submit, decisions at tau >= 6 s: R-PMJ-S47 / S55) and ``j12`` (law (iii): the
trapezoid strike and settlement with the vol-scaled residual, R-PMJ-S61 /
S63) -- slot 9 reaches ``j12`` from 10-28 00:00Z (R-PMJ-S68). The summary never sums
across segments. **Any change to slot 9's or 11's fill
or entry law -- a rollback included -- adds a row here in the same
change.**

**Laws.** Offline, off the hot path: it reads captures and writes three
files under ``worker/pm/``; never ``state.db``, never the engine, never a
secret. Its one socket is Gamma's public read. It refuses a root whose
``strategy.conf`` names neither ``pm15`` nor ``pmj``. It is outside the
worker serialization guard by design, as the dashboard is:
``scripts/pm-pnl.sh`` runs ``scripts/pm-pnl.py`` through the venv alias, so
no argv carries the worker's name (the CMDLINE LAW). A lock serializes
passes; an overlapping pass exits at once.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections
import dataclasses
import datetime
import fcntl
import json
import os
import pathlib
import sys
import time
import typing

import httpx

import claude_worker.bin15_tape
import claude_worker.features
import claude_worker.pmlr

#: ``1e6``: one share, one dollar.
ONE_1E6: int = 1_000_000

#: ``rate_1e6 * px_1e6 * (1e6 - px_1e6) * qty_1e6`` is scaled 1e24; the fee is 1e6.
FEE_DEN: int = 1_000_000_000_000_000_000

#: The first local run of the ruling's start (R-PMJ-S27: "since 00:11Z",
#: 2026-10-19, run-1792368684525363000).
FROM_NS: int = 1_792_368_684_525_363_000

#: The first boot that reads slot 11's 5 s paper delay and 6 s lead (J11,
#: R-PMJ-S47 / S55): ``pmj.toml`` changed before 2026-10-19 16:00Z, and the
#: 16:05Z daily restart is the first boot after it. No run boots between the
#: edit and 16:00Z (no restart is scheduled then).
J11_FROM_NS: int = 1_792_425_600_000_000_000

#: The first boot that reads slot 11's law (iii) (J12, R-PMJ-S61 / S63):
#: ``pmj.toml`` gets ``strike_law = "trap62"`` before 2026-10-23 16:00Z, and
#: the 16:05Z daily restart is the first boot after it. No run boots between
#: the edit and 16:00Z (no restart is scheduled then).
J12_FROM_NS: int = 1_792_771_200_000_000_000

#: Slot 9's law (iii) (R-PMJ-S61 / S68): P0's confirmatory window closes at
#: 2026-10-28 00:00Z; ``pm15.toml`` gets ``strike_law = "trap62"`` between
#: 00:00Z and the 0010 drain, so the 0010 daily restart is the first boot
#: after it and no run boots between 00:00Z and the edit.
J12_SLOT9_FROM_NS: int = 1_793_145_600_000_000_000

#: Both rolling members.
BOTH_SLOTS: frozenset[int] = frozenset({9, 11})

#: The members' entry floor on the ask (`price_floor_1e6` in `pm15.toml` and
#: `pmj.toml`, 0.90 since J8). A fill BELOW it was bought after the price fell
#: during the judge delay -- the losses' signature (R-PMJ-S62): each segment
#: books those fills apart (`below_floor`) from the rest (`at_or_above`). The
#: pre-J8 `touch` segment had no floor; it is split at the same 0.90 so the
#: segments read alike.
PRICE_FLOOR_1E6: int = 900_000

#: ``(segment, first run epoch, the fill law, the slots it applies to)``, oldest
#: first: a fill belongs to the last segment of ITS slot whose epoch its run's is
#: at or after. A change that touches one slot's law adds a row for that slot only.
SEGMENTS: tuple[tuple[str, int, str, frozenset[int]], ...] = (
    ("touch", 0, "the touch alone, capped by its size (before J8)", BOTH_SLOTS),
    (
        "j8",
        1_792_399_727_031_380_000,
        "the FAK sweep behind the touch; slot 9's 0.90 floor (J8)",
        BOTH_SLOTS,
    ),
    (
        "j11",
        J11_FROM_NS,
        "the J8 sweep judged 5 s after submit (Jupiter's keeper); decisions at tau >= 6 s (J11)",
        frozenset({11}),
    ),
    (
        "j12",
        J12_FROM_NS,
        "J11's fill law; law (iii): the trapezoid strike/settlement + vol-scaled residual (J12)",
        frozenset({11}),
    ),
    (
        "j12",
        J12_SLOT9_FROM_NS,
        "J8's fill law; law (iii): the trapezoid strike/settlement + vol-scaled residual (J12)",
        frozenset({9}),
    ),
)

#: core-types ``PM_ROLLING_ORDINAL_BASE`` / ``PM_ROLLING_STRIDE`` (15 m) and
#: ``PM5_ROLLING_ORDINAL_BASE`` / ``PM5_ROLLING_STRIDE`` (5 m): family ``f``
#: owns ``stride`` ordinals from ``base + f * stride`` -- up0, down0, up1,
#: down1, (up2, down2,) mark.
PM_ROLLING_ORDINAL_BASE: int = 4096
PM_ROLLING_STRIDE: int = 5
PM5_ROLLING_ORDINAL_BASE: int = 4160
PM5_ROLLING_STRIDE: int = 7
#: core-types ``PM5_INSTANCE_FLAG``: a 5-minute instance id carries bit 31.
PM5_INSTANCE_FLAG: int = 1 << 31
#: core-types ``PM_ROLLING_MAX`` = ``PM5_ROLLING_MAX``: families per pool.
PM_ROLLING_MAX: int = 8
#: core-types ``OID_INSTANCE_MASK`` and the members' client-id layout.
OID_INSTANCE_MASK: int = 0xFFFF_FFFF
OID_FAMILY_SHIFT: int = 32
OID_LEG_SHIFT: int = 48
#: core-types ``pack_roll_seq``: the kind byte (1 = settled, read strictly as
#: ``roll_kind_strict`` does), and ``PM_SETTLED_UP`` / ``_DOWN``.
ROLL_KIND_SHIFT: int = 56
ROLL_KIND_SETTLED: int = 1
SETTLED_UP: int = 1
SETTLED_DOWN: int = -1

DESCRIPTOR_PREFIX: str = "polymarket-rolling:updown:"

GAMMA_URL: str = "https://gamma-api.polymarket.com/markets"
GAMMA_TIMEOUT_S: float = 5.0
#: Gamma reads one pass may make (each market at most once a pass).
GAMMA_MAX_PER_PASS: int = 64
#: Consecutive failed Gamma reads after which a pass stops reading Gamma.
GAMMA_MAX_ERRORS: int = 3

#: A window this long past its end with no label is ``unlabelled``, not ``awaiting label``.
UNLABELLED_AFTER_NS: int = 3_600_000_000_000

#: Settled rows the dashboard lists per slot.
RECENT_ROWS: int = 8

LEDGER_FILE: str = "pm/paper-pnl.tsv"
OPEN_FILE: str = "pm/open.tsv"
SUMMARY_FILE: str = "pm/paper-pnl.json"
LOCK_FILE: str = "pm/paper-pnl.lock"
SOURCE: str = "pm-paper-pnl"
SUMMARY_VERSION: int = 1


@dataclasses.dataclass(frozen=True, slots=True)
class Slot:
    """One member's settlement law."""

    sid: int
    name: str
    period: str
    window_s: int
    events_file: str
    #: The member's paper fee rate ×1e6 (strategy-pm15 / strategy-pmj ``FEE_RATE_1E6``).
    fee_rate_1e6: int
    pm5: bool
    ordinal_base: int
    stride: int
    #: Slot pairs per family: window ``w`` rides pair ``w % pairs``.
    pairs: int


SLOTS: dict[int, Slot] = {
    9: Slot(9, "pm15", "15m", 900, "pm15-events.pmlr", 70_000, False,
            PM_ROLLING_ORDINAL_BASE, PM_ROLLING_STRIDE, 2),
    11: Slot(11, "pmj", "5m", 300, "pm5-events.pmlr", 140_000, True,
             PM5_ROLLING_ORDINAL_BASE, PM5_ROLLING_STRIDE, 3),
}


class RootRefused(Exception):
    """The root carries neither member."""


#: A fill, named: everything a later pass needs to settle it without its run.
FILL_COLUMNS: tuple[str, ...] = (
    "run", "idx", "fill_ts", "strategy_id", "sym", "asset", "period", "leg", "side",
    "px_1e6", "qty_1e6", "order_id", "instance", "window_end_ns", "slug", "segment",
)
LEDGER_COLUMNS: tuple[str, ...] = (
    *FILL_COLUMNS, "label_src", "outcome", "gamma", "cost_usd_1e6", "payout_usd_1e6",
    "fee_usd_1e6", "pnl_usd_1e6",
)
OPEN_COLUMNS: tuple[str, ...] = (*FILL_COLUMNS, "status")
_TEXT: frozenset[str] = frozenset(
    {"run", "asset", "period", "leg", "slug", "segment", "label_src", "outcome", "gamma", "status"}
)


@dataclasses.dataclass(frozen=True, slots=True)
class Fill:
    """A slot 9 / 11 trade fill with its window named."""

    run: str
    idx: int
    #: The fill's engine timestamp (ordering only; the book's clock is the window's).
    fill_ts: int
    strategy_id: int
    sym: int
    asset: str
    period: str
    #: ``up`` | ``down``: the leg bought.
    leg: str
    side: int
    px_1e6: int
    qty_1e6: int
    order_id: int
    instance: int
    window_end_ns: int
    slug: str
    segment: str

    @property
    def key(self) -> tuple[str, int]:
        return (self.run, self.idx)

    @property
    def family(self) -> str:
        return f"{self.asset.upper()} {self.period}"

    @property
    def cost_usd_1e6(self) -> int:
        return self.px_1e6 * self.qty_1e6 // ONE_1E6


@dataclasses.dataclass(frozen=True, slots=True)
class Row:
    """One settled fill: the ledger's line."""

    fill: Fill
    #: ``capture`` (the settled roll row) | ``gamma``.
    label_src: str
    #: ``up`` | ``down``: the window's result.
    outcome: str
    #: Gamma's result at settlement (``up`` | ``down``), ``-`` when it had none.
    gamma: str
    cost_usd_1e6: int
    payout_usd_1e6: int
    fee_usd_1e6: int
    pnl_usd_1e6: int

    @property
    def won(self) -> bool:
        return self.outcome == self.fill.leg

    def cells(self) -> list[str]:
        f = self.fill
        head = [str(getattr(f, c)) for c in FILL_COLUMNS]
        return head + [str(getattr(self, c)) for c in LEDGER_COLUMNS[len(FILL_COLUMNS):]]


@dataclasses.dataclass(frozen=True, slots=True)
class Open:
    """A fill not yet settled, and why."""

    fill: Fill
    #: ``open`` | ``awaiting label`` | ``unlabelled``.
    status: str

    def cells(self) -> list[str]:
        return [str(getattr(self.fill, c)) for c in FILL_COLUMNS] + [self.status]


@dataclasses.dataclass(slots=True)
class PassResult:
    """What one pass did, for its log line."""

    settled_new: int = 0
    ledger_rows: int = 0
    open: int = 0
    unmapped: int = 0
    unreadable: int = 0
    gamma_lookups: int = 0
    gamma_errors: int = 0
    net_usd_1e6: dict[int, dict[str, int]] = dataclasses.field(default_factory=dict)


# ---- the root and the runs -----------------------------------------------------


def check_root(root: pathlib.Path) -> None:
    """Refuse a root whose ``strategy.conf`` runs neither member."""
    conf = root / "strategy.conf"
    try:
        text = conf.read_text(encoding="utf-8")
    except OSError as e:
        raise RootRefused(f"{conf}: {e}") from e
    names: set[str] = set()
    for line in text.splitlines():
        if line.strip().startswith("STRATEGY="):
            names.update(n.strip() for n in line.split("=", 1)[1].split("+"))
    if not names & {s.name for s in SLOTS.values()}:
        raise RootRefused(f"{conf} runs {sorted(names)}: neither pm15 nor pmj")


def run_epoch(run: str) -> int | None:
    try:
        return int(run.split("-", 1)[1])
    except IndexError, ValueError:
        return None


def segment_of(epoch: int, sid: int) -> str:
    name = SEGMENTS[0][0]
    for seg, start, _law, slots in SEGMENTS:
        if sid in slots and epoch >= start:
            name = seg
    return name


# ---- naming a fill ---------------------------------------------------------------


def parse_descriptor(desc: str) -> tuple[str, str, str, int] | None:
    """``polymarket-rolling:updown:btc:15m[up1]`` -> ``("btc", "15m", "up", 1)``."""
    if not desc.startswith(DESCRIPTOR_PREFIX):
        return None
    parts = desc[len(DESCRIPTOR_PREFIX):].split(":")
    if len(parts) != 2 or "[" not in parts[1] or not parts[1].endswith("]"):
        return None
    period, tag = parts[1][:-1].split("[", 1)
    leg = "up" if tag.startswith("up") else "down" if tag.startswith("down") else ""
    pair = tag.removeprefix(leg)
    if not parts[0] or not leg or pair not in ("0", "1", "2"):
        return None
    return parts[0], period, leg, int(pair)


def name_fill(
    run: str, idx: int, f: claude_worker.pmlr.FillRec, manifest: dict[int, str]
) -> Fill | str:
    """The fill with its window named, or why it cannot be."""
    slot = SLOTS[f.strategy_id]
    parsed = parse_descriptor(manifest.get(f.sym, ""))
    if parsed is None:
        return f"sym {f.sym} is not a rolling leg in the run's manifest"
    asset, period, leg, pair = parsed
    if period != slot.period:
        return f"sym {f.sym} is a {period} leg, slot {slot.sid} trades {slot.period}"
    if f.side != 0:
        return "a sell (neither member sells)"
    if f.qty <= 0 or not 0 < f.px < ONE_1E6:
        return f"px {f.px} / qty {f.qty}: no fill the member books"
    rel = f.sym - slot.ordinal_base
    fam, off = divmod(rel, slot.stride)
    if rel < 0 or fam >= PM_ROLLING_MAX or off >= 2 * slot.pairs:
        return f"sym {f.sym} is no leg of slot {slot.sid}'s pool"
    if (off // 2, off % 2) != (pair, 0 if leg == "up" else 1):
        return f"sym {f.sym} is pair {off // 2} leg {off % 2}, its descriptor {leg}{pair}"
    inst = f.order_id & OID_INSTANCE_MASK
    if bool(inst & PM5_INSTANCE_FLAG) != slot.pm5 or inst == 0:
        return f"order id {f.order_id:#x} names no {slot.period} instance"
    window, inst_fam = divmod(inst & ~PM5_INSTANCE_FLAG, PM_ROLLING_MAX)
    oid_fam = (f.order_id >> OID_FAMILY_SHIFT) & 0xFFFF
    if not inst_fam == oid_fam == fam:
        return f"family: sym {fam}, order id {oid_fam}, instance {inst_fam}"
    if ((f.order_id >> OID_LEG_SHIFT) & 1) != off % 2:
        return f"order id {f.order_id:#x} names the other leg of sym {f.sym}"
    if window % slot.pairs != pair:
        return f"window {window} rides pair {window % slot.pairs}, sym {f.sym} is pair {pair}"
    open_s = window * slot.window_s
    epoch = run_epoch(run) or 0
    return Fill(
        run=run, idx=idx, fill_ts=f.ts_ns, strategy_id=f.strategy_id, sym=f.sym, asset=asset,
        period=period, leg=leg, side=f.side, px_1e6=f.px, qty_1e6=f.qty, order_id=f.order_id,
        instance=inst, window_end_ns=(open_s + slot.window_s) * 1_000_000_000,
        slug=f"{asset}-updown-{period}-{open_s}", segment=segment_of(epoch, f.strategy_id),
    )


# ---- labels ------------------------------------------------------------------------


def capture_labels(path: pathlib.Path) -> dict[int, int]:
    """``instance -> v0`` of every SETTLED roll row in one events capture
    (+1 Up, -1 Down, 0 retired unresolved); empty when the file is absent."""
    if not path.is_file():
        return {}
    a = claude_worker.pmlr.channel_event_array(path)
    a = a[a["channel"] == claude_worker.pmlr.CHANNEL_INSTRUMENT_ROLL]
    out: dict[int, int] = {}
    for seq, v0 in zip(a["seq"].tolist(), a["v0"].tolist(), strict=True):
        if (seq >> ROLL_KIND_SHIFT) & 0xFF == ROLL_KIND_SETTLED:
            out[seq & OID_INSTANCE_MASK] = v0
    return out


def parse_gamma(slug: str, obj: object) -> str | None:
    """``up`` / ``down`` when Gamma's market ``slug`` is resolved, else ``None``."""
    if not isinstance(obj, list):
        return None
    for m in obj:
        if not isinstance(m, dict) or m.get("slug") != slug:
            continue
        if m.get("closed") is not True or m.get("umaResolutionStatus") != "resolved":
            return None
        outcomes = m.get("outcomes")
        prices = m.get("outcomePrices")
        outcomes = json.loads(outcomes) if isinstance(outcomes, str) else outcomes
        prices = json.loads(prices) if isinstance(prices, str) else prices
        if outcomes != ["Up", "Down"] or not isinstance(prices, list) or len(prices) != 2:
            return None
        p = [float(x) for x in prices]
        if p == [1.0, 0.0]:
            return "up"
        if p == [0.0, 1.0]:
            return "down"
        return None
    return None


Fetch = typing.Callable[[str], object]


def http_fetch(client: httpx.Client) -> Fetch:
    """Gamma's markets by slug, closed ones included (the default reader)."""

    def get(slug: str) -> object:
        r = client.get(GAMMA_URL, params={"slug": slug, "closed": "true"}, timeout=GAMMA_TIMEOUT_S)
        r.raise_for_status()
        out: object = r.json()
        return out

    return get


class Gamma:
    """Gamma's results for one pass: each slug read at most once, at most
    :data:`GAMMA_MAX_PER_PASS` reads, none after :data:`GAMMA_MAX_ERRORS`
    failures in a row; a failed read is ``None`` (retried next pass)."""

    def __init__(self, fetch: Fetch | None, budget: int = GAMMA_MAX_PER_PASS) -> None:
        self._fetch = fetch
        self._budget = budget
        self._seen: dict[str, str | None] = {}
        self._run = 0
        self.lookups = 0
        self.errors = 0

    def outcome(self, slug: str) -> str | None:
        if slug in self._seen:
            return self._seen[slug]
        if self._fetch is None or self.lookups >= self._budget or self._run >= GAMMA_MAX_ERRORS:
            return None
        self.lookups += 1
        got: str | None
        try:
            got = parse_gamma(slug, self._fetch(slug))
            self._run = 0
        except (httpx.HTTPError, ValueError, TypeError) as e:
            print(f"pm-pnl: gamma {slug}: {type(e).__name__}: {e}", file=sys.stderr)
            self.errors += 1
            self._run += 1
            got = None
        self._seen[slug] = got
        return got


# ---- pricing ---------------------------------------------------------------------------


def price(slot: Slot, px_1e6: int, qty_1e6: int, won: bool) -> tuple[int, int, int, int]:
    """``(cost, payout, fee, pnl)`` in micro-USD for a BUY of ``qty`` shares
    at ``px``: the member's own arithmetic (strategy-pm15 / -pmj ``book_fill``
    and ``settle``), truncating as its i128 divisions do."""
    cost = px_1e6 * qty_1e6 // ONE_1E6
    fee = slot.fee_rate_1e6 * px_1e6 * (ONE_1E6 - px_1e6) * qty_1e6 // FEE_DEN
    payout = qty_1e6 if won else 0
    return cost, payout, fee, payout - cost - fee


# ---- the files ---------------------------------------------------------------------------


def write_atomic(path: pathlib.Path, text: str) -> bool:
    """Write ``text`` through a temp file and a rename; ``False`` (and no
    write) when the file already holds exactly that."""
    try:
        if path.read_text(encoding="utf-8") == text:
            return False
    except OSError:
        pass
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    with tmp.open("w", encoding="utf-8") as fh:
        fh.write(text)
        fh.flush()
        os.fsync(fh.fileno())
    os.replace(tmp, path)
    dfd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dfd)
    finally:
        os.close(dfd)
    return True


def _read_tsv(path: pathlib.Path, columns: tuple[str, ...]) -> list[dict[str, str | int]]:
    """Rows of a ledger-shaped file; empty when absent. A malformed line is
    refused, never skipped: a ledger that silently loses a settled fill
    under-states the total it exists to keep."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        return []
    if not lines:
        return []
    if tuple(lines[0].split("\t")) != columns:
        raise ValueError(f"{path}: header is not {columns}")
    out: list[dict[str, str | int]] = []
    for n, line in enumerate(lines[1:], start=2):
        cells = line.split("\t")
        if len(cells) != len(columns):
            raise ValueError(f"{path}:{n}: {len(cells)} fields, want {len(columns)}")
        out.append({c: v if c in _TEXT else int(v) for c, v in zip(columns, cells, strict=True)})
    return out


def _fill_of(d: dict[str, str | int]) -> Fill:
    return Fill(**{c: d[c] for c in FILL_COLUMNS})  # type: ignore[arg-type]


def read_ledger(path: pathlib.Path) -> dict[tuple[str, int], Row]:
    out: dict[tuple[str, int], Row] = {}
    for d in _read_tsv(path, LEDGER_COLUMNS):
        rest = {c: d[c] for c in LEDGER_COLUMNS[len(FILL_COLUMNS):]}
        row = Row(fill=_fill_of(d), **rest)  # type: ignore[arg-type]
        if row.fill.key in out:
            raise ValueError(f"{path}: {row.fill.key} twice")
        out[row.fill.key] = row
    return out


def read_open(path: pathlib.Path) -> dict[tuple[str, int], Open]:
    out: dict[tuple[str, int], Open] = {}
    for d in _read_tsv(path, OPEN_COLUMNS):
        o = Open(fill=_fill_of(d), status=str(d["status"]))
        if o.fill.key in out:
            raise ValueError(f"{path}: {o.fill.key} twice")
        out[o.fill.key] = o
    return out


def _render(columns: tuple[str, ...], lines: typing.Iterable[list[str]]) -> str:
    return "\n".join(["\t".join(columns), *("\t".join(c) for c in lines)]) + "\n"


def _order(f: Fill) -> tuple[int, int, str, int]:
    return (f.window_end_ns, f.fill_ts, f.run, f.idx)


# ---- one pass ------------------------------------------------------------------------------


@dataclasses.dataclass(slots=True)
class Seen:
    """What a pass could not book: fills it cannot name, venue fills of
    either slot, and captures it cannot read."""

    unmapped: list[dict[str, object]] = dataclasses.field(default_factory=list)
    venue_fills: collections.Counter[int] = dataclasses.field(default_factory=collections.Counter)
    unreadable: list[dict[str, str]] = dataclasses.field(default_factory=list)


def candidates(
    runs: list[pathlib.Path],
    done: dict[tuple[str, int], Row],
    carried: dict[tuple[str, int], Open],
    seen: Seen,
) -> list[Fill]:
    """Every slot 9 / 11 paper trade fill not yet in the ledger -- read from
    the runs on disk, plus the carried open fills whose run is gone. What
    cannot be booked lands in ``seen``."""
    out: dict[tuple[str, int], Fill] = {}
    for run_dir in runs:
        try:
            fills, _torn = claude_worker.features.read_fills(run_dir)
        except (claude_worker.pmlr.PmlrError, OSError, ValueError) as e:
            seen.unreadable.append({"run": run_dir.name, "file": "engine-fills", "why": str(e)})
            continue
        manifest: dict[int, str] | None = None
        for idx, f in enumerate(fills):
            if f.strategy_id not in SLOTS or not f.is_trade or (run_dir.name, idx) in done:
                continue
            if f.origin != claude_worker.pmlr.FILL_ORIGIN_PAPER:
                seen.venue_fills[f.strategy_id] += 1
                continue
            if manifest is None:
                try:
                    manifest = claude_worker.bin15_tape.read_manifest(run_dir)
                except (OSError, ValueError) as e:  # a UnicodeDecodeError is a ValueError
                    seen.unreadable.append(
                        {"run": run_dir.name, "file": "instrument-manifest", "why": str(e)}
                    )
                    manifest = {}
            got = name_fill(run_dir.name, idx, f, manifest)
            if isinstance(got, str):
                seen.unmapped.append(
                    {"run": run_dir.name, "idx": idx, "slot": f.strategy_id, "why": got}
                )
            else:
                out[got.key] = got
    for key, o in carried.items():
        if key not in done and key not in out:
            out[key] = o.fill
    return sorted(out.values(), key=_order)


def settle(
    fills: list[Fill], logs: pathlib.Path, gamma: Gamma, now_ns: int, seen: Seen
) -> tuple[list[Row], list[Open]]:
    """New settled rows, and the fills still open. The windows that need
    Gamma are read before the capture labels' re-checks, so a spent budget
    costs a re-check, never a label."""
    labels: dict[tuple[str, str], dict[int, int]] = {}
    rows: list[Row] = []
    opens: list[Open] = []
    need: list[Fill] = []
    have: list[tuple[Fill, str]] = []
    for f in fills:
        slot = SLOTS[f.strategy_id]
        if now_ns < f.window_end_ns:
            opens.append(Open(f, "open"))
            continue
        cache = (f.run, slot.events_file)
        if cache not in labels:
            path = logs / f.run / slot.events_file
            try:
                labels[cache] = capture_labels(path)
            except (claude_worker.pmlr.PmlrError, OSError, ValueError) as e:
                seen.unreadable.append({"run": f.run, "file": path.stem, "why": str(e)})
                labels[cache] = {}
        v0 = labels[cache].get(f.instance, 0)
        if v0 in (SETTLED_UP, SETTLED_DOWN):
            have.append((f, "up" if v0 == SETTLED_UP else "down"))
        else:
            need.append(f)
    for f in need:
        got = gamma.outcome(f.slug)
        if got is None:
            late = now_ns - f.window_end_ns >= UNLABELLED_AFTER_NS
            opens.append(Open(f, "unlabelled" if late else "awaiting label"))
            continue
        rows.append(_priced(f, "gamma", got, got))
    for f, outcome in have:
        rows.append(_priced(f, "capture", outcome, gamma.outcome(f.slug) or "-"))
    return rows, opens


def _priced(f: Fill, src: str, outcome: str, check: str) -> Row:
    cost, payout, fee, pnl = price(SLOTS[f.strategy_id], f.px_1e6, f.qty_1e6, outcome == f.leg)
    return Row(f, src, outcome, check, cost, payout, fee, pnl)


# ---- the summary ---------------------------------------------------------------------------


def utc_iso(ns: int) -> str:
    return datetime.datetime.fromtimestamp(ns / 1e9, datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def utc_day(ns: int) -> str:
    return datetime.datetime.fromtimestamp(ns / 1e9, datetime.UTC).strftime("%Y-%m-%d")


def usd(v_1e6: int) -> float:
    return round(v_1e6 / ONE_1E6, 6)


def book(rows: list[Row]) -> dict[str, typing.Any]:
    """Totals of a set of settled rows."""
    stake = sum(r.cost_usd_1e6 for r in rows)
    fees = sum(r.fee_usd_1e6 for r in rows)
    net = sum(r.pnl_usd_1e6 for r in rows)
    cum = peak = dd = 0
    for r in sorted(rows, key=lambda r: _order(r.fill)):
        cum += r.pnl_usd_1e6
        peak = max(peak, cum)
        dd = max(dd, peak - cum)
    return {
        "settled": len(rows),
        "won": sum(1 for r in rows if r.won),
        "lost": sum(1 for r in rows if not r.won),
        "shares": usd(sum(r.fill.qty_1e6 for r in rows)),
        "stake_usd": usd(stake),
        "fees_usd": usd(fees),
        "net_usd": usd(net),
        "ev_pct": round(100.0 * net / stake, 4) if stake else None,
        "max_drawdown_usd": usd(dd),
    }


def floor_split(rows: list[Row]) -> dict[str, dict[str, typing.Any]]:
    """The rows' book split at :data:`PRICE_FLOOR_1E6` (R-PMJ-S62)."""
    keys = ("settled", "won", "lost", "stake_usd", "fees_usd", "net_usd")
    below = book([r for r in rows if r.fill.px_1e6 < PRICE_FLOOR_1E6])
    above = book([r for r in rows if r.fill.px_1e6 >= PRICE_FLOOR_1E6])
    return {
        "below_floor": {k: below[k] for k in keys},
        "at_or_above": {k: above[k] for k in keys},
    }


def _row_view(r: Row) -> dict[str, typing.Any]:
    f = r.fill
    return {
        "window_end_utc": utc_iso(f.window_end_ns),
        "family": f.family,
        "leg": f.leg,
        "px": usd(f.px_1e6),
        "qty": usd(f.qty_1e6),
        "won": r.won,
        "label": r.label_src,
        "fee_usd": usd(r.fee_usd_1e6),
        "pnl_usd": usd(r.pnl_usd_1e6),
        "segment": f.segment,
    }


def _open_view(o: Open) -> dict[str, typing.Any]:
    f = o.fill
    return {
        "run": f.run, "idx": f.idx, "family": f.family, "leg": f.leg, "px": usd(f.px_1e6),
        "qty": usd(f.qty_1e6), "cost_usd": usd(f.cost_usd_1e6),
        "window_end_utc": utc_iso(f.window_end_ns), "status": o.status, "segment": f.segment,
    }


def slot_summary(
    slot: Slot, rows: list[Row], opens: list[Open], seen: Seen, from_ns: int
) -> dict[str, typing.Any]:
    rows = sorted((r for r in rows if r.fill.strategy_id == slot.sid), key=lambda r: _order(r.fill))
    opens = [o for o in opens if o.fill.strategy_id == slot.sid]
    segments: list[dict[str, typing.Any]] = []
    mine = [s for s in SEGMENTS if slot.sid in s[3]]
    for i, (seg, start, law, _slots) in enumerate(mine):
        rs = [r for r in rows if r.fill.segment == seg]
        os_ = [o for o in opens if o.fill.segment == seg]
        current = i == len(mine) - 1
        if not rs and not os_ and not current:
            continue
        by_fam: dict[str, list[Row]] = collections.defaultdict(list)
        by_day: dict[str, list[Row]] = collections.defaultdict(list)
        for r in rs:
            by_fam[r.fill.family].append(r)
            by_day[utc_day(r.fill.window_end_ns)].append(r)
        b = book(rs)
        b.update(
            {
                "segment": seg,
                "law": law,
                "from_utc": utc_iso(max(start, from_ns)),
                "open": len(os_),
                "open_cost_usd": usd(sum(o.fill.cost_usd_1e6 for o in os_)),
                "by_family": [dict(book(v), family=k) for k, v in sorted(by_fam.items())],
                "days": [dict(book(v), day=k) for k, v in sorted(by_day.items())],
                **floor_split(rs),
            }
        )
        segments.append(b)
    return {
        "slot": slot.sid,
        "name": slot.name,
        "segments": segments,
        "recent": [_row_view(r) for r in rows[-RECENT_ROWS:]],
        "open": [_open_view(o) for o in opens],
        "counts": {
            "label_capture": sum(1 for r in rows if r.label_src == "capture"),
            "label_gamma": sum(1 for r in rows if r.label_src == "gamma"),
            "gamma_disagree": sum(
                1 for r in rows if r.label_src == "capture" and r.gamma not in ("-", r.outcome)
            ),
            "gamma_unchecked": sum(1 for r in rows if r.gamma == "-"),
            "awaiting": sum(1 for o in opens if o.status == "awaiting label"),
            "unlabelled": sum(1 for o in opens if o.status == "unlabelled"),
            "unmapped": sum(1 for u in seen.unmapped if u["slot"] == slot.sid),
            "venue_fills": seen.venue_fills[slot.sid],
        },
        "unmapped": [u for u in seen.unmapped if u["slot"] == slot.sid],
    }


# ---- the pass ------------------------------------------------------------------------------


def run_pass(
    root: pathlib.Path,
    from_ns: int = FROM_NS,
    now_ns: int | None = None,
    fetch: Fetch | None = None,
) -> PassResult:
    """One pass over ``root`` (main's ``~/multivenue``). ``fetch`` reads
    Gamma (``None``: no network, a window the capture never saw resolve
    stays open)."""
    check_root(root)
    now = time.time_ns() if now_ns is None else now_ns
    worker = root / "worker"
    lock_path = worker / LOCK_FILE
    lock_path.parent.mkdir(parents=True, exist_ok=True)
    with lock_path.open("a+") as lock:
        try:
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("pm-pnl: another pass holds the lock — skipped", file=sys.stderr)
            return PassResult()
        return _locked_pass(root, worker, from_ns, now, Gamma(fetch))


def _locked_pass(
    root: pathlib.Path, worker: pathlib.Path, from_ns: int, now: int, gamma: Gamma
) -> PassResult:
    ledger_path = worker / LEDGER_FILE
    done = read_ledger(ledger_path)
    carried = {
        k: o for k, o in read_open(worker / OPEN_FILE).items()
        if (run_epoch(o.fill.run) or 0) >= from_ns
    }
    logs = root / "logs"
    runs = [
        d for d in claude_worker.features.run_dirs(logs) if (run_epoch(d.name) or 0) >= from_ns
    ]
    seen = Seen()
    fills = candidates(runs, done, carried, seen)
    new, opens = settle(fills, logs, gamma, now, seen)
    for r in new:
        done[r.fill.key] = r
    if new:
        ordered = sorted(done.values(), key=lambda r: _order(r.fill))
        write_atomic(ledger_path, _render(LEDGER_COLUMNS, (r.cells() for r in ordered)))
    write_atomic(worker / OPEN_FILE, _render(OPEN_COLUMNS, (o.cells() for o in opens)))
    rows = [r for r in done.values() if (run_epoch(r.fill.run) or 0) >= from_ns]
    summary = {
        "source": SOURCE,
        "version": SUMMARY_VERSION,
        "generated_utc": utc_iso(now),
        "from_utc": utc_iso(from_ns),
        "gamma": {"lookups": gamma.lookups, "errors": gamma.errors},
        "unreadable": seen.unreadable,
        "slots": {str(s.sid): slot_summary(s, rows, opens, seen, from_ns) for s in SLOTS.values()},
    }
    write_atomic(worker / SUMMARY_FILE, json.dumps(summary, indent=1, sort_keys=True) + "\n")
    net: dict[int, dict[str, int]] = {s: collections.defaultdict(int) for s in SLOTS}
    for r in rows:
        net[r.fill.strategy_id][r.fill.segment] += r.pnl_usd_1e6
    return PassResult(
        settled_new=len(new), ledger_rows=len(rows), open=len(opens),
        unmapped=len(seen.unmapped), unreadable=len(seen.unreadable),
        gamma_lookups=gamma.lookups, gamma_errors=gamma.errors,
        net_usd_1e6={s: dict(v) for s, v in net.items()},
    )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(
        prog="pm-pnl",
        description="Slots 9 and 11: the paper P&L since start, settled on the venue's result.",
    )
    ap.add_argument("--root", required=True, help="main's multivenue dir (~/multivenue)")
    ap.add_argument("--from-ns", type=int, default=FROM_NS,
                    help="count runs booted at or after this epoch (default: R-PMJ-S27's start)")
    ap.add_argument("--no-gamma", action="store_true",
                    help="no network: windows the capture never saw resolve stay open")
    args = ap.parse_args(argv)
    root = pathlib.Path(args.root).expanduser()
    try:
        if args.no_gamma:
            res = run_pass(root, args.from_ns)
        else:
            with httpx.Client(headers={"User-Agent": "multivenue-pm-pnl"}) as client:
                res = run_pass(root, args.from_ns, fetch=http_fetch(client))
    except RootRefused as e:
        print(f"pm-pnl: REFUSING: {e}", file=sys.stderr)
        return 78
    nets = " ".join(
        f"{SLOTS[s].name}[{','.join(f'{k} ${usd(v):+.2f}' for k, v in sorted(d.items()))}]"
        for s, d in sorted(res.net_usd_1e6.items())
    )
    print(
        f"pm-pnl: {utc_iso(time.time_ns())} settled +{res.settled_new} (ledger {res.ledger_rows}, "
        f"open {res.open}, unmapped {res.unmapped}, unreadable {res.unreadable}) {nets}; "
        f"gamma {res.gamma_lookups} "
        f"read(s), {res.gamma_errors} error(s)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
