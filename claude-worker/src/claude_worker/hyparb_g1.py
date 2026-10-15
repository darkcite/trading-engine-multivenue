# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""hyparb_g1 — the HYPARB gate-G1 tally (slot 0, PAPER; operator 2026-10-15).

**The gate.** G1 (``docs/research/hyparb/hyparb-mainnet-plan-2026-09-24.md``
§2; the live plan's §4, ruling O-HL4 — real money waits on it): "≥ 200
judged AMM fills in live paper, net of the member's own P&L terms (pool fee
in the fill, hedge taker fees, gas p50 per attempt, funding), POSITIVE; and
positive again with the judge's Δ at TWO blocks."

**Why nothing tallied it before.** The nightly report's slot-0 row is
``audit-pnl``'s hedge-only re-model (no AMM judge), and ``hyparb_ladder`` is
a re-simulation no nightly job ran. The member's own level
(``pnl_session_usd_1e6``, ``strategy-hyparb`` ``mark_pnl``) lives on
``/state`` only while its session does, and every restart zeroes it. The
durable source is the run dir — ONE run dir is ONE engine session — whose
``engine-fills.pmlr`` / ``engine-orders.pmlr`` carry every slot-0 fill and
order. :func:`fold` re-folds them under the member's law, integer for
integer:

- an AMM fill (symbol venue byte 8) books token0 ``±qty`` and token1
  ``∓qty × px`` (``mul_div_i64``, truncating); a USD side is cash, any other
  side inventory in its coin. The pool fee is inside the fill's price;
- a Hyperliquid fill (venue byte 4) moves cash by ``∓qty × px`` less the
  taker fee (``perp_taker_bps_1e6`` / ``spot_taker_bps_1e6``), and the
  coin's inventory by ``±qty`` (a perp fill its position too). An order the
  member sent inside an AMM fill's callback is a HEDGE; any other is a
  FLATTEN from its 1 s timer — told apart by the submit time
  (:data:`HEDGE_WINDOW_NS` after an AMM fill);
- gas is ``gas_p50_usd_1e6`` per AMM ATTEMPT — every slot-0 AMM order,
  filled or not, as the member charges it at submit;
- funding is the member's 1 s pass (``accrue_funding``): ``−position × perp
  mid × rate × dt / 1 h`` per coin, each term truncated as its i128 math is;
- the level is ``cash + Σ inventory × mark + funding − gas``, the mark the
  member's ``last_mid_1e6``: the mid of whichever book (perp preferred) was
  sound most recently at the session's last record.

Mids and rates come from the run's own Hyperliquid tape (``hl-ticks``,
``hl-events``' ``activeAssetCtx``) when it is on disk, else ``candles.db``
(1 m closes, the hourly ``funding`` table); a tape without a coin's rates
takes them from ``candles.db`` (``tape+candles``). The row says which.
**Error rows, never a guessed number:** a fill the artifact cannot map (a
pool the universe does not list, a book no ``[[coin]]`` names); a
VENUE-origin slot-0 fill (G1 is paper; PAPER and VENUE are never summed,
plan §6.4); inventory with no mark, or a candle mark older than
:data:`CANDLE_MAX_AGE_NS` at the close; a perp position with no rate or
mid series.

**The artifact.** Every session is folded with TODAY's ``hyparb.toml`` (its
gas p50 and taker fees are the terms). Each row records the artifact's sha
and mtime, and a session that STARTED before that mtime is flagged
``artifact_may_differ=1`` — ``status`` counts them and tallies per hash.

**Ledgers** (worker state under ``~/multivenue/worker/hyparb/``; research
data never enters git):

- ``g1-sessions.tsv`` — one row per CLOSED session, keyed by run name. The
  live session is never stored: the newest run dir, and any run whose fill
  or order log changed in the last :data:`LIVE_QUIET_S`. A log that ends in
  a torn slot is stored flagged ``torn=1``. Idempotent: a re-fold adds
  nothing; an error row is replaced by an ok one, and a candle-marked one
  by a tape-marked one (``backfill --refold`` replaces every ok row).
- ``g1-replay.tsv`` — keyed by ``(unit, rung)``: each ≤ 2 h LEAN window of a
  session (``units`` per row; only ``hl-*`` / ``hyperevm-*`` keep their
  slots) through ``hyparb_ladder.run_rung`` at ``r3`` (the artifact as the
  member runs it, booted as paper) and ``r3-2blk`` (the judge's HyperEVM Δ
  at two blocks: ``--latency-ns-venue hyperevm:2000000000``; HZ-B — from
  this Mac a transaction lands in the next block ~56 % of the time). The
  measure is the member's ``pnl_session_usd``; a binary without that field
  gives ``oos_net + funding``, labelled ``oos_net+funding`` — a different
  law, never the gate's. A failed rung is an error row, never summed. Each
  rung is bounded by :data:`RUN_TIMEOUT_S` and the nightly replay by
  :data:`REPLAY_BUDGET_S`; units the budget did not reach stay PENDING (no
  row) for the next run.

**The verdict.** Condition 1: Σ judged AMM fills ≥ 200 and Σ pnl_session >
0 over the ok sessions. Condition 2 covers the SAME sample: every day of
those sessions has every unit's 2-block rung ok on the member's measure,
those units hold ≥ 200 judged fills, and their Σ is > 0 — anything less is
``INCOMPLETE``, never a pass. A fidelity line puts the r3 replay beside the
live fold over the sessions replayed at r3.

Lanes (``python -m claude_worker.hyparb_g1 <lane>``):

- ``accrue [--day D] [--no-replay] [--replay-budget-s S]`` — the 0020 slot;
- ``backfill --since D [--replay] [--refold]`` — local runs, then the
  archive's: only the two logs and the manifest are pulled per run
  (``archive.Archiver.pull_files``, ~2 MB), folded, and removed;
- ``status [--by-day]`` — the verdict, its evidence, the live session from
  ``/state`` beside it (never in a sum), and the sampling caveat;
- ``fold RUN_DIR`` — fold one run dir and print it; stores nothing.

**Sampling caveat.** ``cap_day_usd_1e6`` caps SUBMITTED AMM notional per UTC
day, but the counter lives in the member and every restart (3-5 a day)
zeroes it, so a session samples its first ~``cap_day / max_order`` arbs
(~20 at $2 000 / $100) — the dislocations that came first, not the day's.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import bisect
import datetime
import hashlib
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import time
import tomllib
import typing
import urllib.request

import numpy

import claude_worker.archive
import claude_worker.archive_config
import claude_worker.bin15_tape
import claude_worker.features
import claude_worker.fill_origin
import claude_worker.har_seed
import claude_worker.hyparb_ladder
import claude_worker.objstore
import claude_worker.pmlr
import claude_worker.pnl_report
import claude_worker.window_root

DEFAULT_ROOT: str = "~/multivenue/worker/hyparb"
SESSIONS_NAME: str = "g1-sessions.tsv"
REPLAY_NAME: str = "g1-replay.tsv"
DEFAULT_ARTIFACT: str = "~/multivenue/hyparb.toml"
DEFAULT_UNIVERSE: str = "~/multivenue/universe.toml"
DEFAULT_CANDLES: str = "~/multivenue/worker/candles.db"
DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"
DEFAULT_FEES: str = claude_worker.pnl_report.FEES_PATH_DEFAULT
STATE_URL: str = "http://127.0.0.1:9191/state"

#: The gate's fill count — for the live sessions and for the 2-block replay.
G1_MIN_FILLS: int = 200
#: ``strategy_set::SLOT_HYPARB`` (wire-stable).
SLOT_HYPARB: int = 0
#: ``core_types::VenueId`` bytes, a symbol's top byte.
VENUE_HYPERLIQUID: int = 4
VENUE_HYPEREVM: int = 8
#: ``core_fill::ORDER_KIND_AMM_SWAP``.
ORDER_KIND_AMM_SWAP: int = 2
#: ``strategy_hyparb::COIN_USD`` — a pool's numéraire side.
COIN_USD: int = 255
E6: int = 1_000_000
ONE_BPS_1E6: int = 10_000 * E6
#: The member's ``TIMER_NS``: one funding pass a second.
TIMER_NS: int = 1_000_000_000
HOUR_NS: int = 3_600 * 1_000_000_000
#: ``ChannelId::AssetCtx`` — HL ``activeAssetCtx``; ``v0`` = funding ×1e9.
CHANNEL_ASSET_CTX: int = 5
#: A Hyperliquid order accepted this soon after a slot-0 AMM fill is that
#: fill's HEDGE (the member sends it inside the fill's callback — µs apart
#: on the 2026-10-15 tape); any other is a FLATTEN from the 1 s timer, which
#: fires no sooner than ``lag_ns`` + 1 s grace after the hedge it replaces.
HEDGE_WINDOW_NS: int = 100_000_000
#: A candle mark older than this at the session's close is not a mark.
CANDLE_MAX_AGE_NS: int = 600 * 1_000_000_000
#: A run whose fill or order log changed this recently is live, newest or not.
LIVE_QUIET_S: float = 600.0
#: One replay rung may run this long before it is a failed rung.
RUN_TIMEOUT_S: int = 120
#: The nightly replay's wall budget (``accrue``): the 0020 slot shares the
#: night, and the engine's boot recommit waits on any live worker process.
REPLAY_BUDGET_S: float = 300.0
_I64_MAX: int = (1 << 63) - 1
_I64_MIN: int = -(1 << 63)
_MS_1M: int = 60_000
_MS_1H: int = 3_600_000

#: Everything a session fold reads — all a pull fetches for it.
FOLD_FILES: tuple[str, ...] = (
    "engine-fills.pmlr",
    "engine-orders.pmlr",
    "instrument-manifest.tsv",
)
#: The capture prefixes a lean replay unit keeps (every other ``*.pmlr`` is
#: cut empty, ``window_root.cut_run(empty=…)``).
LEAN_PREFIXES: tuple[str, ...] = ("hl-", "hyperevm-")
#: What a replay pull fetches of an archived run.
LEAN_FILES: tuple[str, ...] = (
    *(
        f"{v}-{k}.pmlr"
        for v in ("hl", "hyperevm")
        for k in ("ticks", "depth", "events", "signals", "opt-summary")
    ),
    *claude_worker.window_root.MANIFESTS,
)

#: Replay rungs: the artifact as the member runs it, and the judge's Δ at
#: two blocks (one block = 1 000 ms, ``core_fill::ACTIVATION_NS_DEFAULT``).
RUNG_R3: str = "r3"
RUNG_R3_2BLK: str = "r3-2blk"
RUNG_LATENCY_NS: dict[str, int] = {RUNG_R3: 1_000_000_000, RUNG_R3_2BLK: 2_000_000_000}
RUNG_ARGV: dict[str, tuple[str, ...]] = {
    RUNG_R3: (),
    RUNG_R3_2BLK: ("--latency-ns-venue", "hyperevm:2000000000"),
}
MEASURE_PNL: str = "pnl_session"
MEASURE_PROXY: str = "oos_net+funding"

#: How good a session's marks are, for the merge: a tape mark beats a candle one.
_MARK_RANK: dict[str, int] = {"candles": 0, "tape+candles": 1, "tape": 2, "none": 2}

RunFn = typing.Callable[[list[str]], tuple[int, str, str]]
Report = typing.Callable[[str], None]


class G1Error(Exception):
    """A session or term the tally cannot fold honestly."""


def _quiet(_s: str) -> None:
    return None


# ---------------------------------------------------------------------
# The member's terms
# ---------------------------------------------------------------------


class Terms(typing.NamedTuple):
    """What the fold needs from ``hyparb.toml`` + ``universe.toml``."""

    gas_p50_usd_1e6: int
    perp_taker_bps_1e6: int
    spot_taker_bps_1e6: int
    cap_day_usd_1e6: int
    max_order_usd_1e6: int
    #: ``[[coin]]`` names in order — a coin's index is the member's.
    coins: tuple[str, ...]
    #: Per coin: (perp descriptor, spot descriptor or "").
    descs: tuple[tuple[str, str], ...]
    #: Book descriptor (``hyperliquid:HYPE``) -> (coin index, is_spot).
    books: dict[str, tuple[int, bool]]
    #: Pool symbol -> (coin0, coin1); :data:`COIN_USD` for the numéraire.
    pools: dict[int, tuple[int, int]]
    #: sha256 of the artifact's bytes (the engine's hash of it).
    sha256: str
    #: The artifact file's mtime, wall ns — a session that started before
    #: it may have run another artifact.
    mtime_ns: int


def terms(hyparb_toml: pathlib.Path, universe_toml: pathlib.Path) -> Terms:
    """The member's P&L terms and maps. A pool symbol is ``make_symbol_id(
    HyperEvm, i + 1)`` for ``i`` its index in the universe's APPEND-ONLY
    ``[hyperevm] pools`` (the boot's own resolver, ``hyparb_boot``)."""
    try:
        raw = hyparb_toml.read_bytes()
        mtime_ns = hyparb_toml.stat().st_mtime_ns
        art = tomllib.loads(raw.decode("utf-8"))
        uni = tomllib.loads(universe_toml.read_text(encoding="utf-8"))
        hp = art["hyparb"]
        coins = art.get("coin") or []
        names = tuple(str(c["name"]) for c in coins)
        descs = tuple((str(c.get("perp", "")), str(c.get("spot", ""))) for c in coins)
        books: dict[str, tuple[int, bool]] = {}
        for i, (perp, spot) in enumerate(descs):
            if perp:
                books[perp] = (i, False)
            if spot:
                books[spot] = (i, True)
        listed = [
            str(e).split(":", 1)[0].lower() for e in (uni.get("hyperevm") or {}).get("pools") or []
        ]
        pools: dict[int, tuple[int, int]] = {}
        for p in art.get("pool") or []:
            addr = str(p["address"]).lower()
            if addr not in listed:
                raise G1Error(f"pool {addr}: not in {universe_toml} [hyperevm] pools")
            sym = (VENUE_HYPEREVM << 24) | (listed.index(addr) + 1)
            pools[sym] = (_coin_index(names, p["coin0"]), _coin_index(names, p["coin1"]))
        return Terms(
            gas_p50_usd_1e6=int(hp["gas_p50_usd_1e6"]),
            perp_taker_bps_1e6=int(hp["perp_taker_bps_1e6"]),
            spot_taker_bps_1e6=int(hp["spot_taker_bps_1e6"]),
            cap_day_usd_1e6=int(hp.get("cap_day_usd_1e6", 0)),
            max_order_usd_1e6=int(hp.get("max_order_usd_1e6", 0)),
            coins=names,
            descs=descs,
            books=books,
            pools=pools,
            sha256=hashlib.sha256(raw).hexdigest(),
            mtime_ns=mtime_ns,
        )
    except (OSError, tomllib.TOMLDecodeError, UnicodeDecodeError, KeyError, TypeError) as exc:
        raise G1Error(f"{hyparb_toml} / {universe_toml}: {exc!r}") from exc


def _coin_index(names: tuple[str, ...], name: object) -> int:
    if name == "USD":
        return COIN_USD
    if name not in names:
        raise G1Error(f"pool coin {name!r} names no [[coin]]")
    return names.index(str(name))


# ---------------------------------------------------------------------
# Marks and rates, on the fill clock
# ---------------------------------------------------------------------


class Series(typing.NamedTuple):
    """A step function on the fill clock (monotonic ns, ascending)."""

    ts: list[int]
    val: list[int]

    def at(self, t: int) -> int | None:
        """The value in force at ``t``; None before the first step."""
        i = bisect.bisect_right(self.ts, t) - 1
        return self.val[i] if i >= 0 else None


class Marks(typing.NamedTuple):
    """Where a fold reads mids and funding: ``tape`` / ``candles`` /
    ``tape+candles`` (tape mids, candle rates) / ``none``."""

    source: str
    #: Coin -> mid ×1e6, 0 where the touch is unsound (the member's ``None``).
    perp: dict[int, Series]
    spot: dict[int, Series]
    #: Coin -> funding rate ×1e9 (hourly).
    funding: dict[int, Series]
    #: A close mark older than this is refused (candles); None = the tape,
    #: whose last mark is the member's own however old.
    max_age_ns: int | None = None


NO_MARKS: Marks = Marks("none", {}, {}, {})


def _usd_mid(marks: Marks, coin: int, t: int) -> int | None:
    """The member's ``usd_mid_1e6`` at ``t``: the perp's mid, else the spot's."""
    for book in (marks.perp, marks.spot):
        s = book.get(coin)
        v = s.at(t) if s is not None else None
        if v:
            return v
    return None


def _last_sound(s: Series, t: int) -> tuple[int, int, int] | None:
    """``(sound until, mid, step ts)`` of the last sound step at or before
    ``t`` — ``sound until`` is ``t`` itself while it still is."""
    i = bisect.bisect_right(s.ts, t) - 1
    if i < 0:
        return None
    if s.val[i]:
        return t, s.val[i], s.ts[i]
    j = i
    while j >= 0 and not s.val[j]:
        j -= 1
    if j < 0:
        return None
    return s.ts[j + 1], s.val[j], s.ts[j]


def _mark(marks: Marks, coin: int, t: int) -> tuple[int, int] | None:
    """``(mark, step ts)``: the member's ``last_mid_1e6`` at ``t``. Every
    pass sets it to ``usd_mid`` (the perp's mid, else the spot's) when that
    is sound, so it is the mid of whichever book was sound MOST RECENTLY,
    the perp on a tie (``lib.rs`` ``usd_mid_1e6`` / ``check_inventory``)."""
    best: tuple[int, int, int] | None = None
    for book in (marks.perp, marks.spot):
        s = book.get(coin)
        hit = _last_sound(s, t) if s is not None else None
        if hit is not None and (best is None or hit[0] > best[0]):
            best = hit
    return None if best is None else (best[1], best[2])


def tape_marks(run_dir: pathlib.Path, t: Terms, syms: dict[str, int]) -> Marks | None:
    """Mids from ``hl-ticks.pmlr`` and rates from ``hl-events.pmlr``
    (``activeAssetCtx``), on the run's own clock; None without a tick."""
    path = run_dir / "hl-ticks.pmlr"
    if not path.is_file():
        return None
    with claude_worker.pmlr.Reader(path) as r:
        has_flags = r.has_venue_time
    arr = claude_worker.pmlr.tick_array(path)
    if len(arr) == 0:
        return None
    books: tuple[dict[int, Series], dict[int, Series]] = ({}, {})
    for c, pair in enumerate(t.descs):
        for side, desc in enumerate(pair):
            sym = syms.get(desc)
            if not desc or sym is None:
                continue
            sel = arr[arr["sym"] == sym]
            bid, ask = sel["bid"].astype(numpy.int64), sel["ask"].astype(numpy.int64)
            sound = (bid > 0) & (ask > bid) & (sel["bq"] > 0) & (sel["aq"] > 0)
            if has_flags:
                sound &= (sel["flags"] & claude_worker.pmlr.TICK_FLAG_STALE) == 0
            mid = numpy.where(sound, bid + (ask - bid) // 2, 0)
            books[side][c] = Series(sel["ts"].astype(numpy.int64).tolist(), mid.tolist())
    del arr
    funding: dict[int, Series] = {}
    events = run_dir / "hl-events.pmlr"
    if events.is_file():
        ev = claude_worker.pmlr.channel_event_array(events)
        ctx = ev[ev["channel"] == CHANNEL_ASSET_CTX]
        for c, (perp, _spot) in enumerate(t.descs):
            sym = syms.get(perp)
            sel = ctx[ctx["sym"] == sym] if sym is not None else ctx[:0]
            if len(sel):
                funding[c] = Series(sel["ts"].astype(numpy.int64).tolist(), sel["v0"].tolist())
        del ev, ctx
    return Marks("tape", books[0], books[1], funding)


def candle_marks(db: pathlib.Path, t: Terms, clock: tuple[int, int, int]) -> Marks:
    """Mids (1 m closes, in force from the bar's close) and rates (the hourly
    ``funding`` row, in force over the hour it pays) from ``candles.db``,
    put on the run's clock. ``clock`` = (run epoch wall ns, anchor ts, end ts)."""
    epoch, anchor, end = clock

    def mono(wall_ms: int) -> int:
        return anchor + wall_ms * E6 - epoch

    lo_ms = epoch // E6 - 2 * _MS_1H
    hi_ms = (epoch + end - anchor) // E6 + _MS_1H
    books: tuple[dict[int, Series], dict[int, Series]] = ({}, {})
    funding: dict[int, Series] = {}
    conn = claude_worker.har_seed.connect_store(db)
    try:
        for c, pair in enumerate(t.descs):
            for side, desc in enumerate(pair):
                if not desc:
                    continue
                rows = conn.execute(
                    "SELECT open_ts, c FROM candles WHERE venue=? AND descriptor=? AND tf='1m'"
                    " AND open_ts BETWEEN ? AND ? ORDER BY open_ts",
                    (VENUE_HYPERLIQUID, desc, lo_ms, hi_ms),
                ).fetchall()
                if rows:
                    books[side][c] = Series(
                        [mono(o + _MS_1M) for o, _ in rows],
                        [round(px * E6) if px else 0 for _, px in rows],
                    )
            if pair[0]:
                rows = conn.execute(
                    "SELECT ts_ms, rate FROM funding WHERE venue=? AND descriptor=?"
                    " AND ts_ms BETWEEN ? AND ? ORDER BY ts_ms",
                    (VENUE_HYPERLIQUID, pair[0], lo_ms, hi_ms + _MS_1H),
                ).fetchall()
                if rows:
                    funding[c] = Series(
                        [mono(ts - _MS_1H) for ts, _ in rows], [round(r * 1e9) for _, r in rows]
                    )
    finally:
        conn.close()
    return Marks("candles", books[0], books[1], funding, CANDLE_MAX_AGE_NS)


# ---------------------------------------------------------------------
# The fold — the member's law
# ---------------------------------------------------------------------


def _tdiv(n: int, d: int) -> int:
    """Rust integer division (toward zero) for ``d > 0``."""
    return n // d if n >= 0 else -((-n) // d)


def mul_div(a: int, b: int, d: int) -> int:
    """``strategy_hyparb::mul_div_i64``: an i128 product, truncated, saturating."""
    if d <= 0:
        return 0
    return max(_I64_MIN, min(_I64_MAX, _tdiv(a * b, d)))


def _amm_fill(
    pool: tuple[int, int], f: claude_worker.pmlr.FillRec, inv: list[int], marks: Marks
) -> tuple[int, int]:
    """``(cash, USD notional)`` of one AMM fill (``on_amm_fill``); ``inv``
    takes the non-USD legs in place."""
    coin0, coin1 = pool
    qty0 = f.qty
    qty1 = mul_div(qty0, f.px, E6)
    bought = f.side == 0
    cash = 0
    for coin, delta in ((coin0, qty0 if bought else -qty0), (coin1, -qty1 if bought else qty1)):
        if coin == COIN_USD:
            cash += delta
        else:
            inv[coin] += delta
    usd0 = E6 if coin0 == COIN_USD else _usd_mid(marks, coin0, f.ts_ns)
    return cash, (mul_div(qty0, usd0, E6) if usd0 else 0)


def _hedge_fill(
    t: Terms, coin: int, spot: bool, f: claude_worker.pmlr.FillRec, inv: list[int]
) -> tuple[int, int, int]:
    """``(cash, fee, signed qty)`` of one Hyperliquid fill (``on_hedge_fill``)."""
    signed = f.qty if f.side == 0 else -f.qty
    notional = mul_div(f.qty, f.px, E6)
    fee = mul_div(notional, t.spot_taker_bps_1e6 if spot else t.perp_taker_bps_1e6, ONE_BPS_1E6)
    inv[coin] += signed
    return (-notional if signed > 0 else notional) - fee, fee, signed


def funding(
    steps: dict[int, list[tuple[int, int]]],
    marks: Marks,
    end_ts: int,
    names: typing.Sequence[str] = (),
) -> int:
    """The member's ``accrue_funding`` summed over its 1 s passes from the
    first perp fill to ``end_ts``: per coin ``−pos × mid / 1e6 × rate × dt /
    (1e9 × 1 h)``, each division truncating as the member's i128 does. A pass
    with no position, no rate yet or an unsound perp touch accrues nothing —
    as the member's does. A coin that holds a perp position with NO rate or
    mid series at all is not a zero: :class:`G1Error`.
    ``steps`` = coin -> [(ts, perp position after the fill)]."""
    held = {c: s for c, s in steps.items() if any(p for _, p in s)}
    for c in held:
        if c not in marks.funding or c not in marks.perp:
            name = names[c] if c < len(names) else str(c)
            raise G1Error(f"{name}: a perp position with no rate or mid series ({marks.source})")
    if not held:
        return 0
    when = {c: [ts for ts, _ in s] for c, s in held.items()}
    pos = {c: [p for _, p in s] for c, s in held.items()}
    denom = 1_000_000_000 * HOUR_NS
    total = 0
    now = min(w[0] for w in when.values()) + TIMER_NS
    while now <= end_ts:
        earned = 0
        for c in held:
            i = bisect.bisect_right(when[c], now) - 1
            p = pos[c][i] if i >= 0 else 0
            rate = marks.funding[c].at(now) if p else None
            mid = marks.perp[c].at(now) if rate else None
            if mid:
                earned -= _tdiv(_tdiv(p * mid, E6) * rate * TIMER_NS, denom)
        total += max(_I64_MIN, min(_I64_MAX, earned))
        now += TIMER_NS
    return total


def _classify(
    orders: typing.Sequence[claude_worker.pmlr.OrderRec],
    amm_fill_ts: list[int],
) -> tuple[int, set[int], set[int]]:
    """``(AMM attempts, hedge oids, flatten oids)`` of slot 0's orders."""
    amm = 0
    hedges: set[int] = set()
    flattens: set[int] = set()
    for o in orders:
        if o.sym >> 24 == VENUE_HYPEREVM and o.kind == ORDER_KIND_AMM_SWAP:
            amm += 1
        elif o.sym >> 24 == VENUE_HYPERLIQUID:
            i = bisect.bisect_right(amm_fill_ts, o.ts_ns) - 1
            inside = i >= 0 and o.ts_ns - amm_fill_ts[i] <= HEDGE_WINDOW_NS
            (hedges if inside else flattens).add(o.client_oid)
        else:
            raise G1Error(f"slot-0 order on symbol {o.sym} kind {o.kind}: not the hyparb member")
    return amm, hedges, flattens


def fold(
    t: Terms,
    orders: typing.Sequence[claude_worker.pmlr.OrderRec],
    fills: typing.Sequence[claude_worker.pmlr.FillRec],
    books: dict[int, str],
    end_ts: int,
    marks: Marks,
) -> dict[str, int]:
    """One session's slot-0 orders and fills under the member's P&L law.
    ``books`` maps a Hyperliquid symbol to its descriptor (the run's
    instrument manifest). Raises :class:`G1Error` on anything it cannot map."""
    for f in fills:
        if f.origin != claude_worker.fill_origin.PAPER:
            raise G1Error(
                f"a {claude_worker.fill_origin.name_of(f.origin)} fill on slot 0: G1 is "
                "paper, and PAPER and VENUE are never summed"
            )
    trades = [f for f in fills if f.is_trade]
    amm_ts = sorted(f.ts_ns for f in trades if f.sym >> 24 == VENUE_HYPEREVM)
    amm_attempts, hedge_oids, flatten_oids = _classify(orders, amm_ts)
    inv = [0] * len(t.coins)
    perp = [0] * len(t.coins)
    steps: dict[int, list[tuple[int, int]]] = {}
    cash = fees = notional = amm_fills = 0
    filled: set[int] = set()
    for f in trades:
        if f.sym >> 24 == VENUE_HYPEREVM:
            pool = t.pools.get(f.sym)
            if pool is None:
                raise G1Error(f"an AMM fill on pool symbol {f.sym} the artifact does not map")
            dc, dn = _amm_fill(pool, f, inv, marks)
            cash, notional, amm_fills = cash + dc, notional + dn, amm_fills + 1
        elif f.sym >> 24 == VENUE_HYPERLIQUID:
            hit = t.books.get(books.get(f.sym, ""))
            if hit is None:
                raise G1Error(f"a hedge fill on {books.get(f.sym, f.sym)}: no [[coin]] names it")
            dc, fee, signed = _hedge_fill(t, hit[0], hit[1], f, inv)
            cash, fees = cash + dc, fees + fee
            if not hit[1]:
                perp[hit[0]] += signed
                steps.setdefault(hit[0], []).append((f.ts_ns, perp[hit[0]]))
            filled.add(f.order_id)
        else:
            raise G1Error(f"a slot-0 fill on symbol {f.sym}: not the hyparb member")
    marked = residual = 0
    for c, q in enumerate(inv):
        if q == 0:
            continue
        m = _mark(marks, c, end_ts)
        if m is None:
            raise G1Error(f"{t.coins[c]}: inventory {q} with no mark ({marks.source})")
        if marks.max_age_ns is not None and end_ts - m[1] > marks.max_age_ns:
            raise G1Error(
                f"{t.coins[c]}: the {marks.source} mark is {(end_ts - m[1]) // 60_000_000_000} "
                "min old at the close"
            )
        marked += mul_div(q, m[0], E6)
        residual += mul_div(abs(q), m[0], E6)
    gas = amm_attempts * t.gas_p50_usd_1e6
    fund = funding(steps, marks, end_ts, t.coins)
    return {
        "amm_attempts": amm_attempts,
        "amm_fills": amm_fills,
        "amm_notional_usd_1e6": notional,
        "hedge_attempts": len(hedge_oids),
        "hedge_fills": len(hedge_oids & filled),
        "hedge_unfilled": len(hedge_oids - filled),
        "flatten_attempts": len(flatten_oids),
        "flatten_fills": len(flatten_oids & filled),
        "taker_fees_usd_1e6": fees,
        "gas_usd_1e6": gas,
        "funding_usd_1e6": fund,
        "cash_usd_1e6": cash,
        "inventory_usd_1e6": marked,
        "pnl_session_usd_1e6": cash + marked + fund - gas,
        "residual_usd_1e6": residual,
    }


# ---------------------------------------------------------------------
# Sessions
# ---------------------------------------------------------------------


class Session(typing.NamedTuple):
    """One closed engine session (one run dir), folded. Dollars ×1e6."""

    run: str
    start_utc: str
    end_utc: str
    #: ``ok`` / ``error`` — an error row's numbers are zero and never summed.
    status: str
    #: ``local`` (a run dir on disk) / ``s3`` (pulled from the archive).
    source: str
    mark_source: str
    torn: int
    #: The session's ≤ 2 h replay windows (``window_root.windows_of``).
    units: int
    amm_attempts: int
    amm_fills: int
    amm_notional_usd_1e6: int
    hedge_attempts: int
    hedge_fills: int
    hedge_unfilled: int
    flatten_attempts: int
    flatten_fills: int
    #: Every Hyperliquid taker fee, flattens included (inside ``cash``).
    taker_fees_usd_1e6: int
    gas_usd_1e6: int
    funding_usd_1e6: int
    cash_usd_1e6: int
    inventory_usd_1e6: int
    pnl_session_usd_1e6: int
    residual_usd_1e6: int
    artifact_sha256: str
    artifact_mtime_utc: str
    #: 1 when the session started before the artifact file's mtime.
    artifact_may_differ: int
    error: str

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    @property
    def day(self) -> str:
        return self.start_utc[:10]


_SESSION_TEXT: frozenset[str] = frozenset(
    {
        "run",
        "start_utc",
        "end_utc",
        "status",
        "source",
        "mark_source",
        "artifact_sha256",
        "artifact_mtime_utc",
        "error",
    }
)

SESSIONS_HEADER: str = (
    "# hyparb G1 sessions — one row per CLOSED engine session (one run dir) of slot 0\n"
    "#   (HYPARB), PAPER only, folded from engine-fills/engine-orders under the member's\n"
    "#   own law: pnl_session = cash + inventory at marks + funding - gas (gas = p50 per\n"
    "#   AMM attempt; every taker fee is inside cash). Dollars x1e6. status=error rows\n"
    "#   carry zeros and are never summed. mark_source: tape (the run's hl-ticks/hl-events)\n"
    "#   | candles (candles.db 1 m closes + hourly funding) | tape+candles (tape mids,\n"
    "#   candle rates) | none (nothing to mark). Folded with TODAY's hyparb.toml:\n"
    "#   artifact_may_differ=1 when the session started before its mtime.\n"
    "# Worker state (claude_worker.hyparb_g1). Never git.\n"
    "# " + "\t".join(Session._fields) + "\n"
)


def _utc(epoch_ns: int) -> str:
    return datetime.datetime.fromtimestamp(epoch_ns / 1e9, datetime.UTC).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )


def _day(epoch_ns: int) -> str:
    return _utc(epoch_ns)[:10]


def _epoch(run: pathlib.Path | str) -> int:
    return int(pathlib.Path(run).name[len("run-") :])


def _clean(text: str) -> str:
    return " ".join(text.split())[:300] or "-"


def _artifact_cols(t: Terms, run: str) -> dict[str, object]:
    return {
        "artifact_sha256": t.sha256,
        "artifact_mtime_utc": _utc(t.mtime_ns),
        "artifact_may_differ": int(_epoch(run) < t.mtime_ns),
    }


def _error_row(run: str, source: str, t: Terms, exc: object) -> Session:
    zeros = {f: 0 for f in Session._fields if f not in _SESSION_TEXT}
    return Session(
        **{
            **zeros,
            "run": run,
            "start_utc": _utc(_epoch(run)),
            "end_utc": "-",
            "status": "error",
            "source": source,
            "mark_source": "none",
            **_artifact_cols(t, run),
            "error": _clean(str(exc)),
        },
    )


def _slot0(
    run_dir: pathlib.Path,
) -> tuple[list[claude_worker.pmlr.OrderRec], list[claude_worker.pmlr.FillRec], bool]:
    """Slot 0's orders and fills, file order; and whether either log ends in a
    torn slot (the reader drops it — the row is flagged, not refused)."""
    with claude_worker.pmlr.Reader(run_dir / "engine-orders.pmlr") as r:
        orders = [o for o in r.orders() if o.strategy_id == SLOT_HYPARB]
        torn = r.torn
    with claude_worker.pmlr.Reader(run_dir / "engine-fills.pmlr") as r:
        fills = [f for f in r.fills() if f.strategy_id == SLOT_HYPARB]
        torn = torn or r.torn
    return orders, fills, torn


def units_of(anchor: int, end: int) -> int:
    """How many ≤ 2 h windows ``window_root.windows_of`` cuts a span into."""
    return int(((end - anchor) / 1e9) // claude_worker.window_root.WINDOW_MAX_S) + 1


def unit_names(run: str, units: int) -> list[str]:
    """The replay ledger's unit keys of a session (``replay_units``' names)."""
    return [f"{run}+{k * claude_worker.window_root.WINDOW_MAX_S:.0f}s" for k in range(units)]


def _marks_for(
    run_dir: pathlib.Path,
    t: Terms,
    books: dict[int, str],
    fills: list[claude_worker.pmlr.FillRec],
    clock: tuple[int, int, int],
    candles: pathlib.Path | None,
) -> Marks:
    """The tape when it is on disk, else ``candles.db``; a tape without a
    hedged coin's rates takes that coin's rates from ``candles.db``."""
    if not fills:
        return NO_MARKS
    has_db = candles is not None and candles.is_file()
    marks = tape_marks(run_dir, t, {name: sym for sym, name in books.items()})
    if marks is None:
        return candle_marks(typing.cast(pathlib.Path, candles), t, clock) if has_db else NO_MARKS
    hedged = {
        t.books[books[f.sym]][0]
        for f in fills
        if books.get(f.sym) in t.books and not t.books[books[f.sym]][1]
    }
    missing = hedged - set(marks.funding)
    if missing and has_db:
        rates = candle_marks(typing.cast(pathlib.Path, candles), t, clock).funding
        got = {c: rates[c] for c in missing if c in rates}
        if got:
            marks = marks._replace(source="tape+candles", funding={**marks.funding, **got})
    return marks


def fold_session(
    run_dir: pathlib.Path,
    t: Terms,
    candles: pathlib.Path | None,
    *,
    source: str = "local",
    span: tuple[int, int] | None = None,
) -> Session:
    """One run dir folded into a row; an error row when it cannot be.
    ``span`` = (anchor, last ts) — the archive manifest's ``span_ns`` for a
    partial pull, else ``window_root.run_span`` (the harness's anchor law)."""
    try:
        if span is None:
            span = claude_worker.window_root.run_span(run_dir)
        if span is None:
            raise G1Error("no tick to anchor the session")
        anchor, end = int(span[0]), int(span[1])
        orders, fills, torn = _slot0(run_dir)
        books = claude_worker.bin15_tape.read_manifest(run_dir)
        marks = _marks_for(run_dir, t, books, fills, (_epoch(run_dir), anchor, end), candles)
        vals = fold(t, orders, fills, books, end, marks)
    except (G1Error, claude_worker.pmlr.PmlrError, OSError, ValueError, sqlite3.Error) as exc:
        return _error_row(run_dir.name, source, t, exc)
    epoch = _epoch(run_dir)
    return Session(
        run=run_dir.name,
        start_utc=_utc(epoch),
        end_utc=_utc(epoch + end - anchor),
        status="ok",
        source=source,
        mark_source=marks.source,
        torn=int(torn),
        units=units_of(anchor, end),
        **vals,
        **_artifact_cols(t, run_dir.name),
        error="-",
    )


def read_sessions(path: pathlib.Path) -> list[Session]:
    """Every row; empty when absent. A line of the wrong width is a file this
    reader does not understand — refused, never guessed at."""
    if not path.is_file():
        return []
    out: list[Session] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        f = line.split("\t")
        if len(f) != len(Session._fields):
            raise G1Error(f"{path}: want {len(Session._fields)} columns, got {len(f)}: {line!r}")
        out.append(
            Session(
                *(
                    v if k in _SESSION_TEXT else int(v)
                    for k, v in zip(Session._fields, f, strict=True)
                )
            )
        )
    return out


def _better(new: Session, old: Session, refold: bool) -> bool:
    """Whether ``new`` replaces ``old``: an error never replaces an ok row;
    an ok row replaces an error, a better-marked ok row (tape over candles)
    an ok one, and with ``refold`` any ok row an ok one."""
    if not new.ok:
        return False
    if not old.ok or refold:
        return True
    return _MARK_RANK.get(new.mark_source, 0) > _MARK_RANK.get(old.mark_source, 0)


def merge_sessions(
    existing: typing.Iterable[Session],
    incoming: typing.Iterable[Session],
    *,
    refold: bool = False,
) -> tuple[list[Session], int, int]:
    """``(merged by run, added, replaced)``."""
    by = {s.run: s for s in existing}
    added = replaced = 0
    for s in incoming:
        old = by.get(s.run)
        if old is None:
            by[s.run] = s
            added += 1
        elif s != old and _better(s, old, refold):
            by[s.run] = s
            replaced += 1
    return sorted(by.values(), key=lambda s: _epoch(s.run)), added, replaced


def _write(path: pathlib.Path, header: str, rows: typing.Iterable[typing.Any]) -> None:
    """Atomic (tmp + rename): a merge never leaves a torn file."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w", encoding="utf-8") as f:
        f.write(header)
        for r in rows:
            f.write(r.tsv())
    os.replace(tmp, path)


# ---------------------------------------------------------------------
# The replay ledger
# ---------------------------------------------------------------------


class Replay(typing.NamedTuple):
    """One lean unit through one rung. Dollars ×1e6; ``value`` is the
    ``measure`` (``pnl_session``, or ``oos_net+funding`` from a binary that
    does not print it — then ``pnl_session_usd_1e6`` is 0)."""

    unit: str
    rung: str
    day: str
    status: str
    measure: str
    latency_ns: int
    arbs: int
    amm_judged_fills: int
    gas_usd_1e6: int
    funding_usd_1e6: int
    oos_net_usd_1e6: int
    pnl_session_usd_1e6: int
    value_usd_1e6: int
    elapsed_ms: int
    error: str

    def tsv(self) -> str:
        return "\t".join(str(v) for v in self) + "\n"

    @property
    def ok(self) -> bool:
        return self.status == "ok"

    @property
    def on_member_law(self) -> bool:
        return self.ok and self.measure == MEASURE_PNL


_REPLAY_TEXT: frozenset[str] = frozenset({"unit", "rung", "day", "status", "measure", "error"})

REPLAY_HEADER: str = (
    "# hyparb G1 replay — each <= 2 h LEAN window (hl-*, hyperevm-* only) of a closed\n"
    "#   session through `backtest --member hyparb` (hyparb_ladder.run_rung, booted as\n"
    "#   paper): r3 = the artifact as the member runs it (judge Δ one block), r3-2blk =\n"
    "#   the judge's HyperEVM Δ at two blocks. value = measure: pnl_session (the member's\n"
    "#   law) or oos_net+funding (a binary without the field — never the gate's law).\n"
    "#   status=error rows are never summed. Dollars x1e6. Worker state. Never git.\n"
    "# " + "\t".join(Replay._fields) + "\n"
)


def read_replay(path: pathlib.Path) -> list[Replay]:
    if not path.is_file():
        return []
    out: list[Replay] = []
    for line in path.read_text(encoding="utf-8").splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        f = line.split("\t")
        if len(f) != len(Replay._fields):
            raise G1Error(f"{path}: want {len(Replay._fields)} columns, got {len(f)}: {line!r}")
        out.append(
            Replay(
                *(
                    v if k in _REPLAY_TEXT else int(v)
                    for k, v in zip(Replay._fields, f, strict=True)
                )
            )
        )
    return out


def merge_replay(
    existing: typing.Iterable[Replay], incoming: typing.Iterable[Replay]
) -> tuple[list[Replay], int]:
    """``(merged, added or upgraded)`` keyed by ``(unit, rung)``: an ok row
    beats an error one, and the member's measure beats the proxy."""
    by = {(r.unit, r.rung): r for r in existing}
    n = 0
    for r in incoming:
        old = by.get((r.unit, r.rung))
        if old is None or (r.ok and not old.ok) or (r.on_member_law and not old.on_member_law):
            by[(r.unit, r.rung)] = r
            n += 1
    return sorted(by.values(), key=lambda r: (r.unit, r.rung)), n


def _replay_error(key: tuple[str, str, str], elapsed_ms: int, why: str) -> Replay:
    unit, rung, day = key
    zeros = {f: 0 for f in Replay._fields if f not in _REPLAY_TEXT}
    return Replay(
        **{
            **zeros,
            "unit": unit,
            "rung": rung,
            "day": day,
            "status": "error",
            "measure": "-",
            "latency_ns": RUNG_LATENCY_NS[rung],
            "elapsed_ms": elapsed_ms,
            "error": _clean(why),
        },
    )


def _settled(r: Replay | None) -> bool:
    """A stored rung the replay need not run again."""
    return r is not None and r.on_member_law


def _default_run(argv: list[str]) -> tuple[int, str, str]:
    proc = subprocess.run(argv, capture_output=True, text=True, timeout=RUN_TIMEOUT_S, check=False)
    return proc.returncode, proc.stdout, proc.stderr


def lean_empty(run_dir: pathlib.Path) -> frozenset[str]:
    """Every capture a lean unit cuts EMPTY: all but ``hl-*`` / ``hyperevm-*``."""
    return frozenset(p.name for p in run_dir.glob("*.pmlr") if not p.name.startswith(LEAN_PREFIXES))


def _usd_1e6(v: object) -> int:
    return round(float(typing.cast(float, v)) * E6)


def replay_rung(
    unit_dir: pathlib.Path,
    key: tuple[str, str, str],
    artifact: pathlib.Path,
    fee_flags: list[str],
    run_fn: RunFn,
) -> Replay:
    """``key`` = (unit, rung, day). One rung of one unit; an error row when
    the replay fails (a timeout included)."""
    unit, rung, day = key
    t0 = time.monotonic()
    try:
        row = claude_worker.hyparb_ladder.run_rung(
            unit_dir, artifact, fee_flags, run_fn, extra_argv=RUNG_ARGV[rung]
        )
    except (ValueError, OSError, subprocess.SubprocessError) as exc:
        return _replay_error(key, round((time.monotonic() - t0) * 1000), str(exc))
    has = "pnl_session_usd" in row
    pnl = _usd_1e6(row["pnl_session_usd"]) if has else 0
    net, fund = _usd_1e6(row["oos_net_usd"]), _usd_1e6(row["funding_earned_usd"])
    return Replay(
        unit=unit,
        rung=rung,
        day=day,
        status="ok",
        measure=MEASURE_PNL if has else MEASURE_PROXY,
        latency_ns=RUNG_LATENCY_NS[rung],
        arbs=int(typing.cast(int, row["arbs"])),
        amm_judged_fills=int(typing.cast(int, row["amm_judged_fills"])),
        gas_usd_1e6=_usd_1e6(row["gas_usd"]),
        funding_usd_1e6=fund,
        oos_net_usd_1e6=net,
        pnl_session_usd_1e6=pnl,
        value_usd_1e6=pnl if has else net + fund,
        elapsed_ms=round((time.monotonic() - t0) * 1000),
        error="-",
    )


def replay_units(
    runs: typing.Sequence[pathlib.Path],
    artifact_text: str,
    fee_flags: list[str],
    stored: dict[tuple[str, str], Replay],
    scratch: pathlib.Path,
    run_fn: RunFn | None = None,
    report: Report = _quiet,
    deadline: float | None = None,
) -> tuple[list[Replay], int]:
    """``(rows to merge, units left PENDING)``: every ≤ 2 h lean window of
    ``runs`` through both rungs. A (unit, rung) already stored on the
    member's measure is skipped; one unit is on disk at a time (cut, replay,
    delete). Past ``deadline`` (``time.monotonic``) no further unit starts:
    those stay pending — no row, the next run takes them."""
    fn = run_fn or _default_run
    scratch.mkdir(parents=True, exist_ok=True)
    artifact = scratch / "hyparb-replay.toml"
    bad_artifact = ""
    try:
        artifact.write_text(
            claude_worker.hyparb_ladder.replay_artifact(artifact_text, {}), encoding="utf-8"
        )
    except claude_worker.hyparb_ladder.LadderError as exc:
        bad_artifact = f"artifact refused: {exc}"
    out: list[Replay] = []
    pending = 0
    for run in runs:
        try:
            windows = claude_worker.window_root.windows_of(run)
        except claude_worker.window_root.WindowError as exc:
            report(f"hyparb-g1: {run.name}: cannot window ({exc}) — skipped")
            continue
        empty = lean_empty(run)
        for lo, hi in windows:
            unit = f"{run.name}+{lo:.0f}s"
            day = _day(_epoch(run) + int(lo * 1e9))
            todo = [g for g in RUNG_ARGV if not _settled(stored.get((unit, g)))]
            if not todo:
                continue
            if bad_artifact:
                out.extend(_replay_error((unit, g, day), 0, bad_artifact) for g in todo)
                continue
            if deadline is not None and time.monotonic() >= deadline:
                pending += 1
                continue
            how = (artifact, fee_flags, fn)
            out.extend(_replay_unit(run, (lo, hi, empty), (unit, day), todo, how, scratch))
    return out, pending


def _replay_unit(
    run: pathlib.Path,
    window: tuple[float, float, frozenset[str]],
    key: tuple[str, str],
    todo: list[str],
    how: tuple[pathlib.Path, list[str], RunFn],
    scratch: pathlib.Path,
) -> list[Replay]:
    lo, hi, empty = window
    unit, day = key
    artifact, fee_flags, fn = how
    unit_dir = scratch / "unit"
    shutil.rmtree(unit_dir, ignore_errors=True)
    unit_dir.mkdir(parents=True)
    try:
        claude_worker.window_root.cut_run(run, unit_dir, lo, hi, empty=empty)
        return [replay_rung(unit_dir, (unit, g, day), artifact, fee_flags, fn) for g in todo]
    except (claude_worker.window_root.WindowError, OSError) as exc:
        return [_replay_error((unit, g, day), 0, f"cut failed: {exc}") for g in todo]
    finally:
        shutil.rmtree(unit_dir, ignore_errors=True)


# ---------------------------------------------------------------------
# The lanes
# ---------------------------------------------------------------------


class Paths(typing.NamedTuple):
    replay_dir: pathlib.Path
    root: pathlib.Path
    artifact: pathlib.Path
    universe: pathlib.Path
    candles: pathlib.Path
    fees: pathlib.Path

    @property
    def sessions(self) -> pathlib.Path:
        return self.root / SESSIONS_NAME

    @property
    def replay(self) -> pathlib.Path:
        return self.root / REPLAY_NAME


def _quiet_for(run: pathlib.Path, now: float) -> bool:
    """Neither the fill nor the order log changed in the last :data:`LIVE_QUIET_S`."""
    for name in ("engine-fills.pmlr", "engine-orders.pmlr"):
        try:
            if now - (run / name).stat().st_mtime < LIVE_QUIET_S:
                return False
        except FileNotFoundError:
            continue
    return True


def closed_runs(
    replay_dir: pathlib.Path, report: Report = _quiet, now: float | None = None
) -> list[pathlib.Path]:
    """Every run dir but the live ones: the NEWEST (the engine only ever
    writes the newest), and any whose fill or order log changed in the last
    :data:`LIVE_QUIET_S` — a newer run alone does not close one."""
    runs = claude_worker.features.run_dirs(replay_dir)[:-1]
    clock = time.time() if now is None else now
    out = []
    for r in runs:
        if _quiet_for(r, clock):
            out.append(r)
        else:
            report(f"hyparb-g1: {r.name}: logs changed < {LIVE_QUIET_S:.0f} s ago — live, skipped")
    return out


def _fee_flags(path: pathlib.Path) -> list[str]:
    return claude_worker.pnl_report.load_fee_flags(path) if path.is_file() else []


def _store_sessions(paths: Paths, rows: list[Session], refold: bool = False) -> tuple[int, int]:
    merged, added, replaced = merge_sessions(read_sessions(paths.sessions), rows, refold=refold)
    _write(paths.sessions, SESSIONS_HEADER, merged)
    return added, replaced


def _store_replay(paths: Paths, rows: list[Replay]) -> int:
    merged, n = merge_replay(read_replay(paths.replay), rows)
    if merged:
        _write(paths.replay, REPLAY_HEADER, merged)
    return n


def accrue(
    day: str,
    paths: Paths,
    *,
    replay: bool,
    run_fn: RunFn | None = None,
    report: Report = _quiet,
    budget_s: float | None = REPLAY_BUDGET_S,
) -> tuple[int, int, int]:
    """``(sessions folded, session rows added or replaced, replay rows)`` for
    the closed ``day``. Sessions: the day's closed runs, plus any closed
    local run of an earlier day without an OK row (a missed night, or an
    error row, is retried here). Replay: the day's runs, within ``budget_s``
    (units it does not reach stay pending and are reported)."""
    t = terms(paths.artifact, paths.universe)
    ok = {s.run for s in read_sessions(paths.sessions) if s.ok}
    closed = closed_runs(paths.replay_dir, report)
    runs = [
        r for r in closed if _day(_epoch(r)) == day or (_day(_epoch(r)) < day and r.name not in ok)
    ]
    rows = [fold_session(r, t, paths.candles) for r in runs]
    added, replaced = _store_sessions(paths, rows)
    for s in rows:
        if not s.ok:
            report(f"hyparb-g1: {s.run}: ERROR {s.error}")
    n_replay = 0
    if replay:
        deadline = None if budget_s is None else time.monotonic() + budget_s
        day_runs = [r for r in closed if _day(_epoch(r)) == day]
        stored = {(r.unit, r.rung): r for r in read_replay(paths.replay)}
        paths.root.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=paths.root, prefix=".replay-") as tmp:
            new, pending = replay_units(
                day_runs,
                paths.artifact.read_text(encoding="utf-8"),
                _fee_flags(paths.fees),
                stored,
                pathlib.Path(tmp),
                run_fn,
                report,
                deadline,
            )
        n_replay = _store_replay(paths, new)
        if pending:
            report(
                f"hyparb-g1: replay budget {budget_s:.0f} s spent — {pending} unit(s) of {day} "
                "left PENDING (no row; the next accrue or `backfill --replay` takes them)"
            )
    return len(rows), added + replaced, n_replay


def _archiver(report: Report) -> typing.Any:
    """The archive, or None (off, or unconfigured — local runs only)."""
    cfg = claude_worker.archive_config.load()
    if not claude_worker.archive_config.is_enabled(cfg):
        report(f"hyparb-g1: archive off ({cfg.reason}) — local runs only")
        return None
    return claude_worker.archive.build(cfg)


_PULL_ERRORS = (claude_worker.archive.ArchiveError, claude_worker.objstore.ObjStoreError, OSError)


def _fold_remote(archiver: typing.Any, run: str, t: Terms, paths: Paths) -> Session:
    with tempfile.TemporaryDirectory(dir=paths.root, prefix=".pull-") as tmp:
        try:
            run_dir, manifest = archiver.pull_files(run, FOLD_FILES, pathlib.Path(tmp))
        except _PULL_ERRORS as exc:
            return _error_row(run, "s3", t, f"pull failed: {exc}")
        span = manifest.get("span_ns")
        return fold_session(
            run_dir,
            t,
            paths.candles,
            source="s3",
            span=(int(span[0]), int(span[1])) if span else None,
        )


def backfill(
    since: str,
    paths: Paths,
    *,
    replay: bool,
    refold: bool = False,
    archiver: typing.Any = None,
    run_fn: RunFn | None = None,
    report: Report = _quiet,
) -> tuple[int, int, int]:
    """``(sessions folded, session rows added or replaced, replay rows)``
    from ``since`` (YYYY-MM-DD): every closed local run, then every archived
    run not on disk and without an ok row (``refold``: every one, and every
    ok row is replaced). ``replay`` also replays every one of those runs'
    units — the archived ones from a lean pull — with no budget."""
    t = terms(paths.artifact, paths.universe)
    since_ns = (
        int(datetime.datetime.fromisoformat(since).replace(tzinfo=datetime.UTC).timestamp())
        * 1_000_000_000
    )
    paths.root.mkdir(parents=True, exist_ok=True)
    local = [r for r in closed_runs(paths.replay_dir, report) if _epoch(r) >= since_ns]
    rows = [fold_session(r, t, paths.candles) for r in local]
    ok = {s.run for s in read_sessions(paths.sessions) if s.ok}
    on_disk = {r.name for r in claude_worker.features.run_dirs(paths.replay_dir)}
    remote = [] if archiver is None else [r for r in archiver.run_ids(since_ns) if r not in on_disk]
    for i, run in enumerate(remote):
        if run in ok and not refold:
            continue
        rows.append(_fold_remote(archiver, run, t, paths))
        report(f"hyparb-g1: {run} ({i + 1}/{len(remote)}) {rows[-1].status}")
    added, replaced = _store_sessions(paths, rows, refold=refold)
    n_replay = 0
    if replay:
        n_replay = _backfill_replay(local, remote, paths, archiver, run_fn, report)
    return len(rows), added + replaced, n_replay


def _backfill_replay(
    local: list[pathlib.Path],
    remote: list[str],
    paths: Paths,
    archiver: typing.Any,
    run_fn: RunFn | None,
    report: Report,
) -> int:
    text = paths.artifact.read_text(encoding="utf-8")
    flags = _fee_flags(paths.fees)
    n = 0
    with tempfile.TemporaryDirectory(dir=paths.root, prefix=".replay-") as tmp:
        scratch = pathlib.Path(tmp)
        stored = {(r.unit, r.rung): r for r in read_replay(paths.replay)}
        new, _ = replay_units(local, text, flags, stored, scratch, run_fn, report)
        n += _store_replay(paths, new)
        for run in remote:
            pull = scratch / "pull"
            shutil.rmtree(pull, ignore_errors=True)
            pull.mkdir()
            try:
                run_dir, _ = archiver.pull_files(run, LEAN_FILES, pull)
            except _PULL_ERRORS as exc:
                report(f"hyparb-g1: {run}: lean pull failed ({exc}) — not replayed")
                continue
            stored = {(r.unit, r.rung): r for r in read_replay(paths.replay)}
            new, _ = replay_units([run_dir], text, flags, stored, scratch / "r", run_fn, report)
            n += _store_replay(paths, new)
            shutil.rmtree(pull, ignore_errors=True)
    return n


# ---------------------------------------------------------------------
# Status
# ---------------------------------------------------------------------


def _usd(v_1e6: int) -> str:
    sign = "-" if v_1e6 < 0 else ""
    return f"{sign}${abs(v_1e6) // E6:,}.{abs(v_1e6) % E6:06d}"


def live_state(url: str = STATE_URL, timeout_s: float = 2.0) -> tuple[str, dict[str, typing.Any]]:
    """``(state, hyparb block)``: ``unreachable`` (no engine answered),
    ``not-configured`` (slot 0 is not running) or ``ok``."""
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as resp:
            obj = json.loads(resp.read())
    except OSError, ValueError:
        return "unreachable", {}
    h = obj.get("hyparb") if isinstance(obj, dict) else None
    if not isinstance(h, dict) or not h.get("configured"):
        return "not-configured", {}
    return "ok", h


def _rung_line(rows: list[Replay], rung: str) -> str:
    mine = [r for r in rows if r.rung == rung]
    ok = [r for r in mine if r.ok]
    label = "/".join(sorted({r.measure for r in ok})) or "-"
    days = sorted({r.day for r in ok})
    return (
        f"  replay {rung:<8} units={len(ok)} failed={len(mine) - len(ok)} "
        f"days={days[0] + '..' + days[-1] if days else '-'} "
        f"judged_fills={sum(r.amm_judged_fills for r in ok)} arbs={sum(r.arbs for r in ok)} "
        f"gas={_usd(sum(r.gas_usd_1e6 for r in ok))} "
        f"Σ[{label}]={_usd(sum(r.value_usd_1e6 for r in ok))}"
    )


class Coverage(typing.NamedTuple):
    """Condition 2 over condition 1's sample."""

    days: int
    covered: int
    #: 2-block rows of the sample's units when every day is covered.
    rows: list[Replay]
    proxy_units: int


def coverage(sample: list[Session], replays: list[Replay], rung: str) -> Coverage:
    """A day is covered when every unit of every one of its sessions has
    ``rung`` ok on the member's measure."""
    got = {r.unit: r for r in replays if r.rung == rung}
    days = sorted({s.day for s in sample})
    covered = 0
    rows: list[Replay] = []
    proxy = 0
    for d in days:
        units = [u for s in sample if s.day == d for u in unit_names(s.run, s.units)]
        hits = [got.get(u) for u in units]
        proxy += sum(1 for h in hits if h is not None and h.ok and not h.on_member_law)
        if units and all(h is not None and h.on_member_law for h in hits):
            covered += 1
            rows.extend(typing.cast(list[Replay], hits))
    return Coverage(len(days), covered, rows if covered == len(days) else [], proxy)


def _fidelity(sample: list[Session], replays: list[Replay]) -> str:
    """The r3 replay beside the live fold, over sessions replayed whole at r3."""
    got = {r.unit: r for r in replays if r.rung == RUNG_R3 and r.on_member_law}
    whole = [s for s in sample if all(u in got for u in unit_names(s.run, s.units))]
    if not whole:
        return (
            "  fidelity (r3 replay vs live fold): no session replayed whole at r3 on the "
            "member's law yet"
        )
    rows = [got[u] for s in whole for u in unit_names(s.run, s.units)]
    return (
        f"  fidelity (r3 replay vs live fold, {len(whole)} session(s)): judged fills "
        f"{sum(r.amm_judged_fills for r in rows)} vs {sum(s.amm_fills for s in whole)}; "
        f"pnl_session {_usd(sum(r.value_usd_1e6 for r in rows))} vs "
        f"{_usd(sum(s.pnl_session_usd_1e6 for s in whole))}"
    )


def _verdict2(cov: Coverage) -> tuple[bool, str]:
    if not cov.days or cov.covered < cov.days:
        hint = (
            f"; {cov.proxy_units} unit(s) only on the oos_net+funding PROXY (rebuild the release "
            "binary for pnl_session_usd, then re-run the replay)"
            if cov.proxy_units
            else ""
        )
        return False, f"INCOMPLETE ({cov.covered} of {cov.days} days replayed{hint})"
    fills = sum(r.amm_judged_fills for r in cov.rows)
    total = sum(r.value_usd_1e6 for r in cov.rows)
    if fills < G1_MIN_FILLS:
        return False, f"NOT PASSED — 2-block judged fills {fills} < {G1_MIN_FILLS}"
    if total <= 0:
        return False, f"NOT PASSED — Σ 2-block {_usd(total)} <= 0"
    return True, f"PASS — {fills} judged fills, Σ {_usd(total)}"


def _artifact_lines(sample: list[Session]) -> list[str]:
    lines = []
    for sha in sorted({s.artifact_sha256 for s in sample}):
        mine = [s for s in sample if s.artifact_sha256 == sha]
        lines.append(
            f"  artifact {sha[:12]} (mtime {mine[0].artifact_mtime_utc}): sessions={len(mine)} "
            f"fills={sum(s.amm_fills for s in mine)} "
            f"pnl_session={_usd(sum(s.pnl_session_usd_1e6 for s in mine))} "
            "started before its mtime (artifact may differ)="
            f"{sum(s.artifact_may_differ for s in mine)}"
        )
    return lines


def _live_line(live: tuple[str, dict[str, typing.Any]]) -> str:
    state, h = live
    if state == "unreachable":
        return "  live session: /state unreachable (engine down?)"
    if state == "not-configured":
        return "  live session: the engine answers, but slot 0 (hyparb) is not configured"
    return (
        "  live session (not stored, never summed): "
        f"pnl_session={_usd(int(h.get('pnl_session_usd_1e6', 0)))} "
        f"arbs={h.get('arbs_submitted', 0)} amm_fills={h.get('amm_fills', 0)} "
        f"gas={_usd(int(h.get('gas_charged_usd_1e6', 0)))} "
        f"funding={_usd(int(h.get('funding_earned_usd_1e6', 0)))}"
    )


def render_status(
    sessions: list[Session],
    replays: list[Replay],
    t: Terms | None,
    live: tuple[str, dict[str, typing.Any]],
    by_day: bool = False,
) -> list[str]:
    """The gate, then its evidence, the live session beside it, the caveat."""
    ok = [s for s in sessions if s.ok]
    err = [s for s in sessions if not s.ok]
    fills = sum(s.amm_fills for s in ok)
    pnl = sum(s.pnl_session_usd_1e6 for s in ok)
    c1 = fills >= G1_MIN_FILLS and pnl > 0
    why1 = (
        f"judged fills {fills} < {G1_MIN_FILLS}" if fills < G1_MIN_FILLS else "Σ pnl_session <= 0"
    )
    cov = coverage(ok, replays, RUNG_R3_2BLK)
    c2, why2 = _verdict2(cov)
    days = sorted({s.day for s in ok})

    def tot(field: str) -> int:
        return sum(int(getattr(s, field)) for s in ok)

    lines = [
        f"hyparb-g1: G1 = {'PASS' if c1 and c2 else 'NOT PASSED'}  (>= {G1_MIN_FILLS} judged "
        "AMM fills in live PAPER, Σ pnl_session > 0, and > 0 again at the judge's 2-block Δ "
        "over the same sessions)",
        f"  sessions={len(sessions)} ok={len(ok)} error={len(err)} torn={tot('torn')} "
        f"with_attempts={sum(1 for s in ok if s.amm_attempts)} "
        f"days={days[0] + '..' + days[-1] if days else '-'}",
        f"  PAPER, the member's law: judged AMM fills={fills}/{G1_MIN_FILLS} "
        f"attempts={tot('amm_attempts')} notional={_usd(tot('amm_notional_usd_1e6'))} "
        f"Σ pnl_session={_usd(pnl)}",
        f"    = cash {_usd(tot('cash_usd_1e6'))} (taker fees {_usd(tot('taker_fees_usd_1e6'))} "
        f"inside) + inventory {_usd(tot('inventory_usd_1e6'))} + funding "
        f"{_usd(tot('funding_usd_1e6'))} - gas {_usd(tot('gas_usd_1e6'))}",
        f"    hedges {tot('hedge_fills')}/{tot('hedge_attempts')} filled, flattens "
        f"{tot('flatten_fills')}/{tot('flatten_attempts')} filled; residual at close "
        f"{_usd(tot('residual_usd_1e6'))}",
        "  condition 1 (fills and Σ pnl_session > 0): "
        + ("PASS" if c1 else f"NOT PASSED — {why1}"),
        _rung_line(replays, RUNG_R3),
        _rung_line(replays, RUNG_R3_2BLK),
        _fidelity(ok, replays),
        f"  condition 2 (the same sessions at the 2-block Δ, the member's law): {why2}",
        *_artifact_lines(ok),
    ]
    if by_day:
        got = {r.unit: r for r in replays if r.rung == RUNG_R3_2BLK and r.on_member_law}
        for d in days:
            ds = [s for s in ok if s.day == d]
            units = [u for s in ds for u in unit_names(s.run, s.units)]
            lines.append(
                f"    {d}: sessions={len(ds)} fills={sum(s.amm_fills for s in ds)} "
                f"pnl_session={_usd(sum(s.pnl_session_usd_1e6 for s in ds))} "
                f"2-block units={sum(1 for u in units if u in got)}/{len(units)}"
            )
    lines.extend(f"  error {s.run} ({s.source}): {s.error}" for s in err[:5])
    lines.append(_live_line(live))
    if t is not None and t.max_order_usd_1e6 > 0:
        per = t.cap_day_usd_1e6 // t.max_order_usd_1e6
        lines.append(
            f"  caveat: cap_day {_usd(t.cap_day_usd_1e6)} of SUBMITTED notional restarts with "
            f"every session (3-5 a day), so a session samples its first ~{per} arbs at "
            f"{_usd(t.max_order_usd_1e6)} — the dislocations that came first, not the day's"
        )
    return lines


# ---------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------


def _paths(args: argparse.Namespace) -> Paths:
    def p(v: str) -> pathlib.Path:
        return pathlib.Path(os.path.expanduser(v))

    return Paths(
        replay_dir=p(args.replay_dir),
        root=p(args.root),
        artifact=p(args.hyparb),
        universe=p(args.universe),
        candles=p(args.candles),
        fees=p(args.fees),
    )


def _common(sp: argparse.ArgumentParser) -> None:
    sp.add_argument("--replay-dir", default=DEFAULT_REPLAY_DIR)
    sp.add_argument("--root", default=DEFAULT_ROOT, help="where the two ledgers live")
    sp.add_argument("--hyparb", default=DEFAULT_ARTIFACT)
    sp.add_argument("--universe", default=DEFAULT_UNIVERSE)
    sp.add_argument("--candles", default=DEFAULT_CANDLES)
    sp.add_argument("--fees", default=DEFAULT_FEES)


def main(argv: list[str] | None = None) -> int:
    """CLI entry point."""
    ap = argparse.ArgumentParser(prog="claude_worker.hyparb_g1")
    sub = ap.add_subparsers(dest="lane", required=True)
    ac = sub.add_parser("accrue", help="fold the closed day's sessions and replay its units")
    ac.add_argument("--day", default=None, help="YYYY-MM-DD; absent = the closed UTC day")
    ac.add_argument("--no-replay", action="store_true", help="sessions only")
    ac.add_argument(
        "--replay-budget-s",
        type=float,
        default=REPLAY_BUDGET_S,
        help="the replay's wall budget; units past it stay pending",
    )
    bf = sub.add_parser("backfill", help="local runs, then the archive's, from --since")
    bf.add_argument("--since", required=True, help="YYYY-MM-DD")
    bf.add_argument("--replay", action="store_true", help="also replay every unit (no budget)")
    bf.add_argument("--refold", action="store_true", help="re-fold and replace every ok row")
    st = sub.add_parser("status", help="the gate's tally")
    st.add_argument("--by-day", action="store_true")
    fo = sub.add_parser("fold", help="fold one run dir and print it (stores nothing)")
    fo.add_argument("run_dir")
    for sp in (ac, bf, st, fo):
        _common(sp)
    args = ap.parse_args(argv)
    paths = _paths(args)

    def err(s: str) -> None:
        print(s, file=sys.stderr, flush=True)

    try:
        if args.lane == "accrue":
            day = args.day or (
                datetime.datetime.now(datetime.UTC) - datetime.timedelta(days=1)
            ).strftime("%Y-%m-%d")
            n, s, r = accrue(
                day, paths, replay=not args.no_replay, report=err, budget_s=args.replay_budget_s
            )
            print(
                f"hyparb-g1: day {day}: {n} session(s) folded, {s} row(s) -> "
                f"{paths.sessions}; {r} replay row(s) -> {paths.replay}"
            )
            return 0
        if args.lane == "backfill":
            n, s, r = backfill(
                args.since,
                paths,
                replay=args.replay,
                refold=args.refold,
                archiver=_archiver(err),
                report=err,
            )
            print(
                f"hyparb-g1: since {args.since}: {n} session(s) folded, {s} row(s) -> "
                f"{paths.sessions}; {r} replay row(s)"
            )
            return 0
        if args.lane == "fold":
            t = terms(paths.artifact, paths.universe)
            row = fold_session(pathlib.Path(os.path.expanduser(args.run_dir)), t, paths.candles)
            print(json.dumps(row._asdict(), indent=1))
            return 0 if row.ok else 1
        try:
            t_or_none: Terms | None = terms(paths.artifact, paths.universe)
        except G1Error:
            t_or_none = None
        for line in render_status(
            read_sessions(paths.sessions),
            read_replay(paths.replay),
            t_or_none,
            live_state(),
            by_day=args.by_day,
        ):
            print(line)
        return 0
    except G1Error as exc:
        err(f"hyparb-g1: {exc}")
        return 2


if __name__ == "__main__":  # pragma: no cover - CLI
    raise SystemExit(main())
