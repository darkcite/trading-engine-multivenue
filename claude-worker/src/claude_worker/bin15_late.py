# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15 H90 -- the LATE-WINDOW entry's accrual stores and its report
(vault doc 33 §L3 and §3; evidence doc 32).

The harness (``backtest --member bin15 --emit-detail``) writes three
blocks when the artifact switches the late law on:

- ``bin15_late_entries``: one row per late entry the member submitted --
  the side, the ask, the size, the P1b belief it was priced at
  (``p_late_1e6``), the short-horizon sigma, the production pricer's belief at
  the same reprice (``ctl_p_1e6``, ``-1`` where it did not price) and the
  time to expiry (``tau_ms``);
- ``bin15_late_fires``: per instance, the two COUNTERFACTUALS -- the first
  reprice P1b's test held at persistence 1 (``ff_*``) and the first the
  PRODUCTION p̂ held the same test (``ctl_*``) -- logged, never traded;
- ``bin15_late_law``: the law the member ran under.

This module turns them into two append-only stores beside the entries'
(``late_entries.tsv``, ``late_fires.tsv``) and scores them against the
paper bars pre-registered in doc 33 §3:

- **KILL** -- EV per $ staked ≤ 0 (net of the venue's settlement fee) after
  100 settled late entries;
- **PASS** -- ≥ 200 settled with hit ≥ 95 %, EV/$ > 0 and day-t ≥ 2;
- always: the ≥ 0.97 / < 0.97 ask subsets, per family, per τ band, and
  the counterfactuals on the same instances.

On the VENUE's law and clock only: a block from a run without venue time
is dropped (counted), as the first fires are. Rows are keyed per law and
never scored across two laws. A PASS arms nothing.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections
import datetime
import json
import math
import os
import pathlib
import statistics
import typing

import claude_worker.bin15_ledger
import claude_worker.fill_origin

#: The stores, beside the entries'.
DEFAULT_LATE_ENTRIES: str = "~/multivenue/worker/bin15/late_entries.tsv"
DEFAULT_LATE_FIRES: str = "~/multivenue/worker/bin15/late_fires.tsv"

#: ``y`` for an instance the window could not settle.
Y_UNKNOWN: int = claude_worker.bin15_ledger.Y_UNKNOWN

#: The venue's settlement fee on the payout, bps (measured 14; 13.44 with
#: the referral discount -- the conservative one is used).
SETTLE_FEE_BPS: int = 14

#: Doc 33 §3's bars.
KILL_N: int = 100
PASS_N: int = 200
PASS_HIT: float = 0.95
PASS_DAY_T: float = 2.0

#: The ruled least belief (O-H90-5), x1e6 -- what "the production pricer
#: agreed" means in the report.
P_MIN_1E6: int = 900_000

#: A side's label at or above this settled ITM (``y`` x1e6).
HALF_1E6: int = 500_000

#: Day-t needs this many days.
MIN_DAYS: int = 2

#: The ask that splits the report's two subsets, x1e6.
ASK_SPLIT_1E6: int = 970_000

#: The τ bands the report splits on, ms: [lo, hi).
TAU_BANDS_MS: tuple[tuple[int, int], ...] = ((0, 8_000), (8_000, 15_000), (15_000, 60_000))

LATE_ENTRIES_HEADER: str = (
    "# bin15 LATE entries -- BIN15 H90 (vault doc 33): one row per late-window\n"
    "#   entry the member submitted, from `backtest --member bin15 --emit-detail`\n"
    "#   sidecars' `bin15_late_entries` block. On the VENUE's law and clock only.\n"
    "# ts_ns\toutcome\tfamily\texpiry_ns\ttau_ms\tis_yes\tpx_1e6\tqty_1e6\tp_late_1e6"
    "\tsigma_1e9\tctl_p_1e6\ty\ty_next_strike\torigin\tlaw\n"
    "# p_late_1e6: P1b's belief in the side bought; ctl_p_1e6: the production\n"
    "#   pricer's at the same reprice (-1: it did not price there).\n"
    "# y: 1000000 settled ITM, 0 OTM, -1 the window could not derive it.\n"
    "# law: the late law the member ran under (`bin15_late_law`), never mixed.\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)

LATE_FIRES_HEADER: str = (
    "# bin15 LATE first fires -- BIN15 H90: per instance, the first reprice\n"
    "#   P1b's test held at persistence 1 (ff_*) and the first the PRODUCTION\n"
    "#   p-hat held the same test (ctl_*), logged and NEVER traded.\n"
    "# outcome\tfamily\texpiry_ns\tff_ts_ns\tff_is_yes\tff_px_1e6\tctl_ts_ns"
    "\tctl_is_yes\tctl_px_1e6\tentered\ty\ty_next_strike\tlaw\n"
    "# A test that never held: ts 0, is_yes -1, px 0. entered 1: the member\n"
    "#   placed a late entry on the instance too.\n"
    "# Worker state. Never git; findings go to docs/research/outcome/.\n"
)


class LateEntry(typing.NamedTuple):
    """One late-window entry, as placed and (later) as settled."""

    ts_ns: int
    outcome: int
    family: int
    expiry_ns: int
    tau_ms: int
    is_yes: int
    px_1e6: int
    qty_1e6: int
    p_late_1e6: int
    sigma_1e9: int
    ctl_p_1e6: int
    y: int
    y_next_strike: int
    #: PAPER / VENUE -- never summed together (plan §6.4).
    origin: int
    law: str

    @property
    def settled(self) -> bool:
        return self.y != Y_UNKNOWN

    @property
    def won(self) -> bool:
        """Whether the side the member BOUGHT is the side that settled."""
        return (self.y >= HALF_1E6) == (self.is_yes == 1)

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"


class LateFire(typing.NamedTuple):
    """One instance's two late counterfactuals: logged, never traded."""

    outcome: int
    family: int
    expiry_ns: int
    ff_ts_ns: int
    ff_is_yes: int
    ff_px_1e6: int
    ctl_ts_ns: int
    ctl_is_yes: int
    ctl_px_1e6: int
    entered: int
    y: int
    y_next_strike: int
    law: str

    @property
    def settled(self) -> bool:
        return self.y != Y_UNKNOWN

    def view(self, which: str) -> tuple[int, int, int]:
        """``(ts_ns, is_yes, px_1e6)`` of ``"ff"`` (P1b) or ``"ctl"``
        (the production pricer); ``ts_ns == 0`` when it never held."""
        if which == "ff":
            return (self.ff_ts_ns, self.ff_is_yes, self.ff_px_1e6)
        if which == "ctl":
            return (self.ctl_ts_ns, self.ctl_is_yes, self.ctl_px_1e6)
        raise ValueError(f"which must be ff or ctl, got {which!r}")

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"


def late_entries_path(path: str | None = None) -> pathlib.Path:
    return pathlib.Path(os.path.expanduser(path or DEFAULT_LATE_ENTRIES))


def late_fires_path(path: str | None = None) -> pathlib.Path:
    return pathlib.Path(os.path.expanduser(path or DEFAULT_LATE_FIRES))


def law_of(block: dict | None) -> str | None:
    """The late law of a sidecar's ``bin15_late_law`` block as one key --
    ``None`` when the member ran without it (no block, or ``enabled`` 0).

    The BIN15-fills keys (2026-10-23) follow only when stated, so a block
    written before them and one that states them at 0 are the same law:
    ``_rf`` (re-fire only on a fresh touch), ``_age<ms>ms`` (the oldest
    quote it fires on), ``_bound`` (the IoC priced at p - e), ``_cap<px 1e6>``
    (PR-BIN15-P2: the dearest price it pays)."""
    if not block or int(block.get("enabled", 0)) != 1:
        return None
    law = (
        f"tau{int(block['tau_min_ns']) / 1e9:g}-{int(block['tau_max_ns']) / 1e9:g}s"
        f"_p{int(block['p_min_1e6'])}_e{int(block['e_1e6'])}"
        f"_n{int(block['persist_polls'])}_k{int(block['sigma_k_1e3'])}"
    )
    if int(block.get("refire_fresh", 0)) == 1:
        law += "_rf"
    age_ns = int(block.get("max_quote_age_ns", 0))
    if age_ns != 0:
        law += f"_age{age_ns // 1_000_000}ms"
    if int(block.get("limit_bound", 0)) == 1:
        law += "_bound"
    cap = int(block.get("limit_max_1e6", 0))
    if cap != 0:
        law += f"_cap{cap}"
    return law


def _int_or(v: typing.Any, default: int) -> int:
    return default if v is None else int(v)


def late_from_sidecar(text: str) -> tuple[list[LateEntry], list[LateFire], int]:
    """``(entries, fires, dropped)`` of one `--emit-detail` sidecar.

    Nothing without a ``bin15_late_law`` block (the law off, or a harness
    that predates it). A sidecar whose runs are not on the venue clock
    yields nothing and counts what it dropped -- the stores are on one
    clock. An entry row without ``origin`` is refused, not defaulted
    (plan §6.4)."""
    obj = json.loads(text)
    law = law_of(obj.get("bin15_late_law"))
    if law is None:
        return [], [], 0
    entries: list[LateEntry] = []
    for r in obj.get("bin15_late_entries") or []:
        if "origin" not in r:
            raise ValueError("bin15_late_entries row has no `origin` (plan §6.4) -- rebuild")
        entries.append(
            LateEntry(
                ts_ns=int(r["ts_ns"]),
                outcome=int(r["outcome"]),
                family=int(r["family"]),
                expiry_ns=int(r["expiry_ns"]),
                tau_ms=int(r["tau_ms"]),
                is_yes=int(r["is_yes"]),
                px_1e6=int(r["px_1e6"]),
                qty_1e6=int(r["qty_1e6"]),
                p_late_1e6=int(r["p_late_1e6"]),
                sigma_1e9=int(r["sigma_1e9"]),
                ctl_p_1e6=_int_or(r.get("ctl_p_1e6"), -1),
                y=_int_or(r.get("y"), Y_UNKNOWN),
                y_next_strike=_int_or(r.get("y_next_strike"), Y_UNKNOWN),
                origin=int(r["origin"]),
                law=law,
            )
        )
    fires: list[LateFire] = []
    for r in obj.get("bin15_late_fires") or []:
        f = LateFire(
            outcome=int(r["outcome"]),
            family=int(r["family"]),
            expiry_ns=int(r["expiry_ns"]),
            ff_ts_ns=_int_or(r.get("ff_ts_ns"), 0),
            ff_is_yes=_int_or(r.get("ff_is_yes"), -1),
            ff_px_1e6=_int_or(r.get("ff_px_1e6"), 0),
            ctl_ts_ns=_int_or(r.get("ctl_ts_ns"), 0),
            ctl_is_yes=_int_or(r.get("ctl_is_yes"), -1),
            ctl_px_1e6=_int_or(r.get("ctl_px_1e6"), 0),
            entered=int(r.get("entered", 0)),
            y=_int_or(r.get("y"), Y_UNKNOWN),
            y_next_strike=_int_or(r.get("y_next_strike"), Y_UNKNOWN),
            law=law,
        )
        if f.ff_ts_ns or f.ctl_ts_ns:
            fires.append(f)
    if (entries or fires) and not claude_worker.bin15_ledger.sidecar_on_venue(obj):
        return [], [], len(entries) + len(fires)
    return entries, fires, 0


def _read(path: pathlib.Path, width: int, make: typing.Callable[[list[str]], typing.Any]) -> list:
    if not path.is_file():
        return []
    out = []
    for line in path.read_text(encoding="utf-8").splitlines():
        s = line.strip()
        if not s or s.startswith("#"):
            continue
        f = s.split("\t")
        if len(f) != width:
            raise ValueError(f"{path}: want {width} columns, got {len(f)}: {s!r}")
        out.append(make(f))
    return out


def read_late_entries(path: pathlib.Path) -> list[LateEntry]:
    """Every late entry, oldest first. Empty when absent."""
    return _read(path, 15, lambda f: LateEntry(*(int(v) for v in f[:14]), law=f[14]))


def read_late_fires(path: pathlib.Path) -> list[LateFire]:
    """Every late first-fire row, oldest instance first. Empty when absent."""
    return _read(path, 13, lambda f: LateFire(*(int(v) for v in f[:12]), law=f[12]))


def _write(path: pathlib.Path, header: str, rows: typing.Sequence[typing.Any]) -> None:
    """Atomic write (tmp + rename): a merge never leaves a torn file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(header)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


def write_late_entries(path: pathlib.Path, rows: typing.Sequence[LateEntry]) -> None:
    _write(path, LATE_ENTRIES_HEADER, rows)


def write_late_fires(path: pathlib.Path, rows: typing.Sequence[LateFire]) -> None:
    _write(path, LATE_FIRES_HEADER, rows)


def merge_late_entries(
    existing: typing.Sequence[LateEntry], incoming: typing.Sequence[LateEntry]
) -> tuple[list[LateEntry], int]:
    """``(merged oldest first, added)``, one row per ``(OUTCOME, ORIGIN)``.

    A settled row upgrades an unsettled one of the SAME law (a window re-cut
    later derives the payout); otherwise the stored row wins -- a re-cut never
    un-settles the store, and a day re-accrued under another law never
    replaces the law it was scored under."""
    by: dict[tuple[int, int], LateEntry] = {(e.outcome, e.origin): e for e in existing}
    added = 0
    for e in incoming:
        key = (e.outcome, e.origin)
        old = by.get(key)
        if old is None:
            by[key] = e
            added += 1
        elif old.law == e.law and not old.settled and e.settled:
            by[key] = e
    return sorted(by.values(), key=lambda e: (e.ts_ns, e.outcome, e.origin)), added


def _earlier(a: tuple[int, int, int], b: tuple[int, int, int]) -> tuple[int, int, int]:
    """The earlier of two ``(ts, side, px)`` fires; ``0`` never held."""
    if b[0] and (not a[0] or b[0] < a[0]):
        return b
    return a


def merge_late_fires(
    existing: typing.Sequence[LateFire], incoming: typing.Sequence[LateFire]
) -> tuple[list[LateFire], int]:
    """``(merged oldest instance first, added)``, one row per OUTCOME: each
    counterfactual its EARLIEST stamp (an instance straddling a window cut is
    replayed twice), the label from whichever row has it, ``entered`` from
    either -- all within ONE law; a row of another law leaves the stored row
    as it is but for a label it lacked."""
    by: dict[int, LateFire] = {f.outcome: f for f in existing}
    added = 0
    for f in incoming:
        old = by.get(f.outcome)
        if old is None:
            by[f.outcome] = f
            added += 1
            continue
        lab = old if old.settled or not f.settled else f
        nxt = old.y_next_strike if old.y_next_strike != Y_UNKNOWN else f.y_next_strike
        if old.law != f.law:
            by[f.outcome] = old._replace(y=lab.y, y_next_strike=nxt)
            continue
        ff = _earlier(old.view("ff"), f.view("ff"))
        ctl = _earlier(old.view("ctl"), f.view("ctl"))
        by[f.outcome] = old._replace(
            ff_ts_ns=ff[0], ff_is_yes=ff[1], ff_px_1e6=ff[2],
            ctl_ts_ns=ctl[0], ctl_is_yes=ctl[1], ctl_px_1e6=ctl[2],
            entered=max(old.entered, f.entered), y=lab.y, y_next_strike=nxt,
        )
    return sorted(by.values(), key=lambda f: (f.expiry_ns, f.outcome)), added


def fill_labels(rows: list, labels: dict) -> tuple[list, int]:
    """Rows whose ``y`` is unknown take the captures' LAW E-11 label when
    there is one (``bin15_accrue relabel``); a known label is never touched."""
    out = []
    filled = 0
    for r in rows:
        lab = labels.get(r.outcome)
        if r.y == Y_UNKNOWN and lab is not None and lab.y != Y_UNKNOWN:
            nxt = r.y_next_strike if lab.y_next_strike == Y_UNKNOWN else lab.y_next_strike
            out.append(r._replace(y=lab.y, y_next_strike=nxt))
            filled += 1
        else:
            out.append(r)
    return out, filled


# ---------------------------------------------------------------------
# the report
# ---------------------------------------------------------------------


def _wilson(k: int, n: int, z: float = 1.96) -> tuple[float, float]:
    if n == 0:
        return (0.0, 1.0)
    den = n + z * z
    centre = (k + z * z / 2) / den
    half = (z / den) * math.sqrt(k * (n - k) / n + z * z / 4)
    return (max(0.0, centre - half), min(1.0, centre + half))


def _utc_day(epoch_ns: int) -> str:
    return datetime.datetime.fromtimestamp(epoch_ns / 1e9, datetime.UTC).strftime("%Y-%m-%d")


class Money(typing.NamedTuple):
    """A set of settled entries in dollars, AS TRADED (stake-weighted)."""

    n: int
    won: int
    cost: float
    payout: float
    fee: float

    @property
    def hit(self) -> float:
        return self.won / self.n if self.n else float("nan")

    @property
    def ev_gross(self) -> float:
        return (self.payout - self.cost) / self.cost if self.cost else float("nan")

    @property
    def ev_net(self) -> float:
        return (self.payout - self.fee - self.cost) / self.cost if self.cost else float("nan")


def money(rows: typing.Sequence[LateEntry], fee_bps: int = SETTLE_FEE_BPS) -> Money:
    """The settled rows' cost, payout and the settlement fee on the payout."""
    settled = [e for e in rows if e.settled]
    cost = sum(e.px_1e6 / 1e6 * e.qty_1e6 / 1e6 for e in settled)
    payout = sum(e.qty_1e6 / 1e6 for e in settled if e.won)
    won = sum(1 for e in settled if e.won)
    return Money(len(settled), won, cost, payout, payout * fee_bps / 1e4)


def day_t(rows: typing.Sequence[LateEntry], fee_bps: int = SETTLE_FEE_BPS) -> tuple[float, int]:
    """``(t, days)``: the per-UTC-day net EV/$ (each day stake-weighted), its
    mean over its standard error -- the day the unit, doc 32's law. ``nan``
    under two days or with no spread."""
    by: dict[str, list[LateEntry]] = collections.defaultdict(list)
    for e in rows:
        if e.settled:
            by[_utc_day(e.ts_ns)].append(e)
    evs = [money(v, fee_bps).ev_net for v in by.values()]
    if len(evs) < MIN_DAYS:
        return float("nan"), len(evs)
    sd = statistics.stdev(evs)
    if sd == 0:
        return float("nan"), len(evs)
    return statistics.fmean(evs) / (sd / math.sqrt(len(evs))), len(evs)


def verdict(rows: typing.Sequence[LateEntry], fee_bps: int = SETTLE_FEE_BPS) -> str:
    """Doc 33 §3: ``KILL``, ``PASS`` or ``ACCRUING``, with the reason."""
    m = money(rows, fee_bps)
    t, days = day_t(rows, fee_bps)
    if m.n >= KILL_N and not m.ev_net > 0:
        return f"KILL -- EV/$ {m.ev_net:+.4f} <= 0 after {m.n} settled (bar: {KILL_N})"
    if m.n >= PASS_N and m.hit >= PASS_HIT and m.ev_net > 0 and t >= PASS_DAY_T:
        return (
            f"PASS -- {m.n} settled, hit {100 * m.hit:.1f} % >= {100 * PASS_HIT:.0f} %, "
            f"EV/$ {m.ev_net:+.4f} > 0, day-t {t:.2f} >= {PASS_DAY_T:g} (arms nothing)"
        )
    return (
        f"ACCRUING -- {m.n}/{PASS_N} settled; hit {100 * m.hit:.1f} %, EV/$ {m.ev_net:+.4f}, "
        f"day-t {t:.2f} over {days} day(s)"
    )


def _line(name: str, rows: typing.Sequence[LateEntry], fee_bps: int) -> str:
    m = money(rows, fee_bps)
    if not m.n:
        return f"   {name}: none settled"
    lo, hi = _wilson(m.won, m.n)
    avg = statistics.fmean(e.px_1e6 / 1e6 for e in rows if e.settled)
    return (
        f"   {name}: {m.won}/{m.n} = {100 * m.hit:.1f} % [{100 * lo:.1f}, {100 * hi:.1f}] at a "
        f"mean ask {avg:.4f} (break-even {100 * avg / (1 - fee_bps / 1e4):.1f} %); EV/$ "
        f"{m.ev_gross:+.4f} gross, {m.ev_net:+.4f} net"
    )


def _fire_scores(
    fires: typing.Sequence[LateFire], which: str, fee_bps: int
) -> tuple[int, float, float]:
    """``(n, hit, equal-dollar net EV per $1)`` of one counterfactual on its
    settled instances (a counterfactual's size was never decided)."""
    rows = [(f.view(which), f.y) for f in fires if f.settled and f.view(which)[0]]
    if not rows:
        return 0, float("nan"), float("nan")
    won = [((y >= HALF_1E6) == (side == 1), px) for (_, side, px), y in rows]
    hit = sum(1 for w, _ in won if w) / len(won)
    keep = 1 - fee_bps / 1e4
    ev = statistics.fmean((keep * 1e6 / px - 1.0) if w else -1.0 for w, px in won if px > 0)
    return len(won), hit, ev


def render_late(
    entries: typing.Sequence[LateEntry],
    fires: typing.Sequence[LateFire],
    fee_bps: int = SETTLE_FEE_BPS,
) -> list[str]:
    """The late law's report, one block per law and accounting, never mixed:
    the bars' verdict, the hit and EV with their interval, the subsets, and
    the two counterfactuals -- P1b at persistence 1 and the production
    pricer under the same test -- on all their settled instances and on
    those the member entered."""
    out: list[str] = []
    laws = sorted({e.law for e in entries} | {f.law for f in fires})
    if not laws:
        return ["late law: no late entry or fire accrued yet"]
    for law in laws:
        group = [e for e in entries if e.law == law]
        lfires = [f for f in fires if f.law == law]
        for origin in sorted({e.origin for e in group}) or [claude_worker.fill_origin.PAPER]:
            rows = [e for e in group if e.origin == origin]
            name = claude_worker.fill_origin.NAMES.get(origin, f"origin-{origin}")
            settled = [e for e in rows if e.settled]
            out.append(
                f"== LATE law {law} -- {name} accounting: {len(rows)} entr(ies) over "
                f"{len({_utc_day(e.ts_ns) for e in rows})} UTC day(s), {len(settled)} settled "
                f"(settlement fee {fee_bps} bps of the payout)"
            )
            if not rows:
                continue
            out.append(f"   VERDICT (doc 33 §3): {verdict(rows, fee_bps)}")
            out.append(_line("all", rows, fee_bps))
            split = ASK_SPLIT_1E6 / 1e6
            subsets: list[tuple[str, list[LateEntry]]] = [
                (f"ask >= {split:.2f}", [e for e in rows if e.px_1e6 >= ASK_SPLIT_1E6]),
                (f"ask <  {split:.2f}", [e for e in rows if e.px_1e6 < ASK_SPLIT_1E6]),
            ]
            subsets += [
                (f"tau [{lo / 1e3:g}, {hi / 1e3:g}) s", [e for e in rows if lo <= e.tau_ms < hi])
                for lo, hi in TAU_BANDS_MS
            ]
            subsets += [
                (f"family {fam}", [e for e in rows if e.family == fam])
                for fam in sorted({e.family for e in rows})
            ]
            out.extend(_line(name, sub, fee_bps) for name, sub in subsets)
            ctl = [e for e in settled if e.ctl_p_1e6 >= 0]
            if ctl:
                agree = sum(1 for e in ctl if e.ctl_p_1e6 >= P_MIN_1E6)
                out.append(
                    f"   the production pricer at the same reprices: priced {len(ctl)} of "
                    f"{len(settled)}, its belief in the side bought >= 0.90 on {agree}"
                )
        if lfires:
            entered = {e.outcome for e in group}
            for which, title in (("ff", "P1b at persistence 1"), ("ctl", "the production p-hat")):
                n, hit, ev = _fire_scores(lfires, which, fee_bps)
                mine = [f for f in lfires if f.outcome in entered]
                n_s, hit_s, ev_s = _fire_scores(mine, which, fee_bps)
                out.append(
                    f"   counterfactual, {title} (logged, never traded): {n} settled instance(s) "
                    f"fired, hit {100 * hit:.1f} %, equal-dollar EV/$ {ev:+.4f} net; on the "
                    f"{n_s} the member entered: hit {100 * hit_s:.1f} %, EV/$ {ev_s:+.4f}"
                )
    return out
