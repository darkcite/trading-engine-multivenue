# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Resolutions and the scorecard — what the lane's claims were worth (§9.6).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Every claim the cascade makes is a prediction with a horizon, and this
module resolves it against the tape when the horizon elapses —
continuously, inside the 60 s cycle, so the scorecard on the dashboard is
a number you watch rather than a report you request.

Three claims get resolved, and the writer of each opens the row, so no
claim escapes:

* a **label** with a market — its direction if it has one, and its vol
  call if it made one;
* a **declaration** — did the MEASURED fast word actually reach
  `vol:high` inside the TTL;
* an **intent** — a candle-approximate fill and mark-out, labelled
  APPROXIMATE everywhere it is shown because the harness fill model is
  the authority and the N4 tool re-judges through it.

What this is for, stated plainly: the lane exists on the hypothesis that
news moves markets predictably enough to trade. The scorecard is the
measurement that can refute it. A `base_rate` sits beside every hit rate
precisely so "up was usually right in an uptrend" cannot masquerade as
edge, `by_model` keeps the session brain and the automated one from being
pooled silently, and the reliability buckets exist because a confidence
number that does not separate 0.9 from 0.7 is decoration. A scorecard
that sits at the base rate for thirty days is this lane's honest NO
DIRECTION, and writing it up as such is the job.

Prices come from `candles.db` — the 1 m lane, gap-filled hourly, so a bar
can lag. A missing bar leaves the row `pending` for the next cycle rather
than guessing, and only after 48 h does it become `unresolvable`.
"""

import dataclasses
import json
import math
import pathlib
import sqlite3
import typing

import claude_worker.frames
import claude_worker.news
import claude_worker.news.store
import claude_worker.regime

# ---------------------------------------------------------------- constants

SUBJECT_LABEL: str = "label"
SUBJECT_DECLARE: str = "declare"
SUBJECT_INTENT: str = "intent"

STATE_PENDING: str = "pending"
STATE_RESOLVED: str = "resolved"
STATE_UNRESOLVABLE: str = "unresolvable"

#: A label's half-life is clamped into a window the tape can actually
#: judge: under a minute there is no bar, over a day the claim is about
#: something else.
HORIZON_MIN_S: int = 60
HORIZON_MAX_S: int = 86_400
#: After this long without both bars, stop waiting and say so.
GIVE_UP_S: int = 48 * 3_600

PRICE_CANDLES: str = "candles"
PRICE_TICKS: str = "ticks"

BPS: float = 1e4
MS_PER_S: int = 1_000
#: Realised-variance ratio needs this many 1 m bars on BOTH sides or it is
#: not a ratio, it is noise.
RV_MIN_BARS: int = 10
#: Rows resolved per cycle. A backlog drains over several passes rather
#: than making one slot long.
RESOLVE_BATCH: int = 200

#: 95 % Wilson interval.
Z_95: float = 1.959_963_985

WINDOW_7D: str = "7d"
WINDOW_30D: str = "30d"
WINDOW_ALL: str = "all"
WINDOWS: tuple[tuple[str, int], ...] = (
    (WINDOW_7D, 7 * 86_400),
    (WINDOW_30D, 30 * 86_400),
    (WINDOW_ALL, 0),
)

#: Confidence buckets for the reliability curve.
RELIABILITY_EDGES: tuple[tuple[float, float], ...] = (
    (0.5, 0.7),
    (0.7, 0.8),
    (0.8, 0.9),
    (0.9, 1.000_001),
)


def clamp_horizon(seconds: float) -> int:
    return max(HORIZON_MIN_S, min(int(seconds), HORIZON_MAX_S))


def wilson(hits: int, n: int, z: float = Z_95) -> tuple[float, float]:
    """The 95 % Wilson score interval.

    Not the normal approximation: at the sample sizes this lane will have
    for months, the normal interval is wrong in the direction that
    flatters a small number of lucky calls.
    """
    if n <= 0:
        return 0.0, 0.0
    phat = hits / n
    denom = 1.0 + z * z / n
    centre = (phat + z * z / (2 * n)) / denom
    margin = z * math.sqrt((phat * (1 - phat) + z * z / (4 * n)) / n) / denom
    return max(0.0, centre - margin), min(1.0, centre + margin)


@dataclasses.dataclass(slots=True)
class ResolveStats:
    """What one resolution pass did."""

    due: int = 0
    resolved: int = 0
    still_pending: int = 0
    unresolvable: int = 0


def open_resolution(  # noqa: PLR0913 — one row of the resolutions table
    store: claude_worker.news.store.Store,
    subject_kind: str,
    subject_id: str,
    *,
    t0: int,
    horizon_s: int,
    descriptor: str = "",
    venue: int = 0,
    direction: str = "",
    confidence: float = 0.0,
    vol_claimed: int = 0,
    detail: str = "",
) -> None:
    """Open a pending resolution. Called by the WRITER of the claim, so no
    claim escapes measurement — that is the whole design.

    ``descriptor`` is copied from the claim rather than looked up later: a
    ``SymbolId`` is meaningless across boots and the descriptor is not.
    """
    horizon = clamp_horizon(horizon_s)
    store.put_resolution(
        {
            "subject_kind": subject_kind,
            "subject_id": subject_id,
            "t0": t0,
            "t1": t0 + horizon,
            "horizon_s": horizon,
            "state": STATE_PENDING,
            "descriptor": descriptor,
            "venue": venue,
            "direction": direction,
            "confidence": confidence,
            "vol_claimed": vol_claimed,
            "detail": detail,
        }
    )


# ------------------------------------------------------------------- prices


class Bars(typing.NamedTuple):
    """The two closes a resolution needs, and where they came from."""

    px0_1e6: int
    px1_1e6: int
    source: str


def _open_candles(path: pathlib.Path) -> sqlite3.Connection | None:
    """Open `candles.db` for reading, and REFUSE to write to it.

    Not ``?mode=ro``. MEASURED on the operator's live databases
    2026-09-20: a read-only URI connection cannot open a WAL database that
    has un-checkpointed content and no live ``-shm``, because attaching to
    the WAL means creating that shared-memory file and a read-only
    connection may not. It fails with "unable to open database file" —
    intermittently, depending on whether a writer happens to be holding
    the database at that instant.

    That failure is silent in the worst way: `bars_for` would return None,
    every resolution would stay pending, and after 48 h the whole
    scorecard would quietly become `unresolvable`. It was invisible in
    tests because a tmp database built by `sqlite3.connect` uses a
    rollback journal, not WAL.

    ``PRAGMA query_only`` is the stronger guarantee anyway: SQLite itself
    refuses a write on this connection ("attempt to write a readonly
    database"), and it works whatever the journal mode.
    """
    try:
        conn = sqlite3.connect(str(path))
        conn.execute("PRAGMA query_only = 1")
    except sqlite3.Error:
        return None
    return conn


def close_at(
    conn: sqlite3.Connection, venue: int, descriptor: str, at_ts: int, tf: str = "1m"
) -> float | None:
    """The close of the 1 m bar CONTAINING ``at_ts``.

    ``open_ts`` is milliseconds and a closed 1 m bar is immutable, so the
    newest bar at or before the instant is the one that contains it.
    """
    try:
        row = conn.execute(
            """
            SELECT c FROM candles
            WHERE venue = ? AND descriptor = ? AND tf = ? AND open_ts <= ?
            ORDER BY open_ts DESC LIMIT 1
            """,
            (venue, descriptor, tf, at_ts * MS_PER_S),
        ).fetchone()
    except sqlite3.Error:
        return None
    if row is None or row[0] is None:
        return None
    return float(row[0])


def bars_for(
    conn: sqlite3.Connection | None, row: typing.Mapping[str, object]
) -> Bars | None:
    """Both closes, or ``None`` — which leaves the row pending. Never a
    single-sided guess: a forward return needs two prices."""
    if conn is None:
        return None
    descriptor = str(row["descriptor"])
    venue = int(typing.cast(int, row["venue"]))
    if not descriptor:
        return None
    px0 = close_at(conn, venue, descriptor, int(typing.cast(int, row["t0"])))
    px1 = close_at(conn, venue, descriptor, int(typing.cast(int, row["t1"])))
    if px0 is None or px1 is None or px0 <= 0:
        return None
    return Bars(px0_1e6=round(px0 * 1e6), px1_1e6=round(px1 * 1e6), source=PRICE_CANDLES)


def closes_between(
    conn: sqlite3.Connection, venue: int, descriptor: str, lo_ts: int, hi_ts: int
) -> list[float]:
    try:
        rows = conn.execute(
            """
            SELECT c FROM candles
            WHERE venue = ? AND descriptor = ? AND tf = '1m'
              AND open_ts >= ? AND open_ts <= ?
            ORDER BY open_ts
            """,
            (venue, descriptor, lo_ts * MS_PER_S, hi_ts * MS_PER_S),
        ).fetchall()
    except sqlite3.Error:
        return []
    out: list[float] = []
    for i in range(len(rows)):
        if rows[i][0] is not None:
            out.append(float(rows[i][0]))
    return out


def realised_variance(closes: list[float]) -> float | None:
    """Variance of 1 m log returns. ``None`` below `RV_MIN_BARS` — a ratio
    of two noisy estimates is not a measurement."""
    if len(closes) < RV_MIN_BARS:
        return None
    total = 0.0
    count = 0
    for i in range(1, len(closes)):
        if closes[i - 1] <= 0 or closes[i] <= 0:
            continue
        ret = math.log(closes[i] / closes[i - 1])
        total += ret * ret
        count += 1
    if count < RV_MIN_BARS - 1:
        return None
    return total / count


def rv_ratio(
    conn: sqlite3.Connection | None, row: typing.Mapping[str, object]
) -> float:
    """Realised variance over the horizon, divided by the same length
    before it. 0 when either side is too thin to say anything."""
    if conn is None:
        return 0.0
    descriptor = str(row["descriptor"])
    if not descriptor:
        return 0.0
    venue = int(typing.cast(int, row["venue"]))
    t0 = int(typing.cast(int, row["t0"]))
    t1 = int(typing.cast(int, row["t1"]))
    horizon = max(1, t1 - t0)
    after = realised_variance(closes_between(conn, venue, descriptor, t0, t1))
    before = realised_variance(closes_between(conn, venue, descriptor, t0 - horizon, t0))
    if after is None or before is None or before <= 0.0:
        return 0.0
    return after / before


# ---------------------------------------------------------------- the vol call


def reached_vol_high(
    regime_dir: pathlib.Path, t0: int, t1: int
) -> tuple[int, str]:
    """Did the MEASURED fast word reach `vol:high` inside the window?

    Returns ``(reached, which)``. The engine's own EFFECTIVE word is
    preferred when the history entry carries it, because that is what
    actually gated the strategies; the worker's measured word is the
    fallback. Which one answered is recorded, so a scorecard number can
    never quietly change meaning.
    """
    try:
        entries = claude_worker.regime.history_tail(regime_dir, t1 * MS_PER_S)
    except (OSError, ValueError):
        return 0, "unavailable"
    which = "none"
    for i in range(len(entries)):
        entry = entries[i]
        ts_ms = int(typing.cast(int, entry.get("ts_ms", 0)))
        if ts_ms < t0 * MS_PER_S or ts_ms > t1 * MS_PER_S:
            continue
        word, source = _fast_word(entry)
        if word is None:
            continue
        which = source
        if claude_worker.frames.regime_word_dims(word).get("vol") == "high":
            return 1, source
    return 0, which


def _fast_word(entry: typing.Mapping[str, object]) -> tuple[int | None, str]:
    engine = entry.get("engine")
    if isinstance(engine, dict):
        raw = engine.get("fast_effective", engine.get("fast"))
        word = _hex_word(raw)
        if word is not None:
            return word, "engine_effective"
    return _hex_word(entry.get("fast")), "worker_measured"


def _hex_word(raw: object) -> int | None:
    if isinstance(raw, int) and not isinstance(raw, bool):
        return int(raw)
    if not isinstance(raw, str) or not raw:
        return None
    try:
        return int(raw, 16)
    except ValueError:
        return None


# -------------------------------------------------------------- the intents


def intent_fill(
    conn: sqlite3.Connection | None, row: typing.Mapping[str, object], px_1e6: int, side: int
) -> tuple[int, float]:
    """A conservative candle proxy of the harness's strict-cross rule.

    A BID fills iff some 1 m bar in the window trades BELOW it; an ASK iff
    some bar trades above. APPROXIMATE, and labelled so everywhere it is
    shown — `crates/cli/src/backtest/fill.rs` is the authority and the N4
    tool re-judges every intent through it.
    """
    if conn is None or px_1e6 <= 0:
        return 0, 0.0
    descriptor = str(row["descriptor"])
    venue = int(typing.cast(int, row["venue"]))
    t0 = int(typing.cast(int, row["t0"]))
    t1 = int(typing.cast(int, row["t1"]))
    column = "l" if side == claude_worker.frames.SIDE_BID else "h"
    try:
        hit = conn.execute(
            f"""
            SELECT COUNT(*) FROM candles
            WHERE venue = ? AND descriptor = ? AND tf = '1m'
              AND open_ts >= ? AND open_ts <= ?
              AND {column} {"<" if side == claude_worker.frames.SIDE_BID else ">"} ?
            """,
            (venue, descriptor, t0 * MS_PER_S, t1 * MS_PER_S, px_1e6 / 1e6),
        ).fetchone()
    except sqlite3.Error:
        return 0, 0.0
    if hit is None or int(hit[0]) <= 0:
        return 0, 0.0
    px1 = close_at(conn, venue, descriptor, t1)
    if px1 is None:
        return 1, 0.0
    sign = 1.0 if side == claude_worker.frames.SIDE_BID else -1.0
    return 1, (px1 / (px_1e6 / 1e6) - 1.0) * BPS * sign


# ------------------------------------------------------------- the resolver


def resolve_due(
    store: claude_worker.news.store.Store,
    paths: claude_worker.news.NewsPaths,
    now_ts: int,
    max_rows: int = RESOLVE_BATCH,
) -> ResolveStats:
    """Resolve every pending claim whose horizon has elapsed.

    A row whose bars have not arrived stays PENDING — the 1 m lane is
    gap-filled hourly, so waiting is the correct answer, not a zero. Only
    after 48 h does it become `unresolvable`, counted, with the reason.
    """
    stats = ResolveStats()
    rows = store.resolutions_due(now_ts)[:max_rows]
    stats.due = len(rows)
    conn = _open_candles(paths.candles_db_path)
    try:
        for i in range(len(rows)):
            _resolve_one(store, paths, conn, rows[i], now_ts, stats)
    finally:
        if conn is not None:
            conn.close()
    return stats


def _resolve_one(  # noqa: PLR0913, PLR0917 — one row, every collaborator it needs
    store: claude_worker.news.store.Store,
    paths: claude_worker.news.NewsPaths,
    conn: sqlite3.Connection | None,
    row: dict[str, object],
    now_ts: int,
    stats: ResolveStats,
) -> None:
    bars = bars_for(conn, row)
    if bars is None:
        if now_ts - int(typing.cast(int, row["t1"])) > GIVE_UP_S:
            merged = dict(row)
            merged["state"] = STATE_UNRESOLVABLE
            merged["resolved_ts"] = now_ts
            merged["detail"] = _no_bars_reason(row)
            store.put_resolution(merged)
            stats.unresolvable += 1
            return
        stats.still_pending += 1
        return
    merged = dict(row)
    merged.update(_judged(store, paths, conn, row, bars))
    merged["state"] = STATE_RESOLVED
    merged["resolved_ts"] = now_ts
    store.put_resolution(merged)
    stats.resolved += 1


def _no_bars_reason(row: typing.Mapping[str, object]) -> str:
    descriptor = str(row["descriptor"])
    if not descriptor:
        return "no descriptor on the claim"
    return f"no 1m bars for {descriptor} within 48h"


def _judged(
    store: claude_worker.news.store.Store,
    paths: claude_worker.news.NewsPaths,
    conn: sqlite3.Connection | None,
    row: typing.Mapping[str, object],
    bars: Bars,
) -> dict[str, object]:
    del store
    fwd_bps = (bars.px1_1e6 / bars.px0_1e6 - 1.0) * BPS
    direction = str(row["direction"])
    sign = 1.0 if direction == "up" else (-1.0 if direction == "down" else 0.0)
    signed = fwd_bps * sign
    out: dict[str, object] = {
        "px0_1e6": bars.px0_1e6,
        "px1_1e6": bars.px1_1e6,
        "fwd_bps": fwd_bps,
        "signed_bps": signed,
        # A tie is a MISS. Rounding a zero move into a win is how a null
        # becomes a result.
        "hit": 1 if (sign != 0.0 and signed > 0.0) else 0,
        "price_source": bars.source,
        "rv_ratio": rv_ratio(conn, row),
    }
    if int(typing.cast(int, row["vol_claimed"])) or str(row["subject_kind"]) == SUBJECT_DECLARE:
        reached, which = reached_vol_high(
            paths.regime_dir, int(typing.cast(int, row["t0"])), int(typing.cast(int, row["t1"]))
        )
        out["vol_reached_high"] = reached
        out["detail"] = f"vol from {which}"
    return out


# ---------------------------------------------------------------- scorecard


def _mean(values: list[float]) -> float:
    return sum(values) / len(values) if values else 0.0


def _median(values: list[float]) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def _direction_score(rows: list[dict[str, object]]) -> dict[str, object]:
    """Hit rate with the two numbers that stop it lying: a Wilson lower
    bound instead of a point estimate, and the direction-AGNOSTIC base
    rate beside it — "up was usually right in an uptrend" is not edge."""
    judged: list[dict[str, object]] = []
    ups = 0
    for i in range(len(rows)):
        if str(rows[i]["direction"]) in ("up", "down"):
            judged.append(rows[i])
        if float(typing.cast(float, rows[i]["fwd_bps"])) > 0.0:
            ups += 1
    hits = 0
    signed: list[float] = []
    by_model: dict[str, list[int]] = {}
    for i in range(len(judged)):
        row = judged[i]
        hit = int(typing.cast(int, row["hit"]))
        hits += hit
        signed.append(float(typing.cast(float, row["signed_bps"])))
        model = str(row.get("model", "")) or "unattributed"
        bucket = by_model.setdefault(model, [0, 0])
        bucket[0] += 1
        bucket[1] += hit
    n = len(judged)
    lo, hi = wilson(hits, n)
    models: dict[str, object] = {}
    for model in sorted(by_model):
        count, won = by_model[model]
        models[model] = {"n": count, "hits": won, "hit_rate": won / count if count else 0.0}
    return {
        "n": n,
        "hits": hits,
        "hit_rate": hits / n if n else 0.0,
        "wilson_lo": lo,
        "wilson_hi": hi,
        "base_rate": ups / len(rows) if rows else 0.0,
        "mean_signed_bps": _mean(signed),
        "median_signed_bps": _median(signed),
        "by_model": models,
    }


def _reliability(rows: list[dict[str, object]]) -> list[dict[str, object]]:
    """Does a 0.9 beat a 0.7? If not, the confidence number is decoration
    and any policy floor written against it is arbitrary."""
    out: list[dict[str, object]] = []
    for lo, hi in RELIABILITY_EDGES:
        n = 0
        hits = 0
        for i in range(len(rows)):
            row = rows[i]
            if str(row["direction"]) not in ("up", "down"):
                continue
            confidence = float(typing.cast(float, row["confidence"]))
            if lo <= confidence < hi:
                n += 1
                hits += int(typing.cast(int, row["hit"]))
        out.append(
            {
                "lo": lo,
                "hi": min(hi, 1.0),
                "n": n,
                "hit_rate": hits / n if n else 0.0,
            }
        )
    return out


def _vol_score(rows: list[dict[str, object]]) -> dict[str, object]:
    split: dict[str, list[dict[str, object]]] = {SUBJECT_LABEL: [], SUBJECT_DECLARE: []}
    for i in range(len(rows)):
        row = rows[i]
        kind = str(row["subject_kind"])
        claimed = int(typing.cast(int, row["vol_claimed"]))
        if kind == SUBJECT_DECLARE or (kind == SUBJECT_LABEL and claimed):
            split.setdefault(kind, []).append(row)
    out: dict[str, object] = {}
    every: list[dict[str, object]] = []
    for kind in (SUBJECT_LABEL, SUBJECT_DECLARE):
        out[kind] = _vol_slice(split.get(kind, []))
        every.extend(split.get(kind, []))
    merged = typing.cast(dict[str, object], _vol_slice(every))
    merged.update(out)
    return merged


def _vol_slice(rows: list[dict[str, object]]) -> dict[str, object]:
    reached = 0
    ratios: list[float] = []
    for i in range(len(rows)):
        reached += int(typing.cast(int, rows[i]["vol_reached_high"]))
        ratios.append(float(typing.cast(float, rows[i]["rv_ratio"])))
    n = len(rows)
    return {
        "n": n,
        "precision": reached / n if n else 0.0,
        "mean_rv_ratio": _mean(ratios),
        "median_rv_ratio": _median(ratios),
    }


def _intent_score(rows: list[dict[str, object]]) -> dict[str, object]:
    intents: list[dict[str, object]] = []
    for i in range(len(rows)):
        if str(rows[i]["subject_kind"]) == SUBJECT_INTENT:
            intents.append(rows[i])
    filled = 0
    markouts: list[float] = []
    for i in range(len(intents)):
        hit = int(typing.cast(int, intents[i]["filled"]))
        filled += hit
        if hit:
            markouts.append(float(typing.cast(float, intents[i]["markout_bps"])))
    n = len(intents)
    return {
        "n": n,
        "fill_rate": filled / n if n else 0.0,
        "mean_markout_bps": _mean(markouts),
        "approximate": True,
    }


DAY_S: int = 86_400
LEAD_MATCH_S: int = 86_400
#: What counts as corroborated for the per-source statistics: a second
#: INDEPENDENT origin. One outlet is one claim however loud it is.
CORROBORATED_ORIGINS: int = 2


def _lead_time(store: claude_worker.news.store.Store, now_ts: int) -> dict[str, object]:
    """Did the prose beat the venue's machine state, or follow it?

    For stories whose (venue, event_type) matches a class-A event within
    24 h: ``story.first_ts - events.created_ts``. NEGATIVE means the
    reporting arrived before the venue's own instrument set moved, which
    is the only thing that would make a prose lane worth its model bill.
    """
    stories = store.stories_open(now_ts - 30 * DAY_S)
    events = store.events_since(now_ts - 30 * DAY_S)
    leads: list[float] = []
    for i in range(len(stories)):
        story = stories[i]
        first_ts = int(typing.cast(int, story["first_ts"]))
        for j in range(len(events)):
            event = events[j]
            if str(event["kind"]).split("_")[0] not in str(story["event_type"]):
                continue
            if str(event["venue"]) not in str(story["venues"]):
                continue
            created = int(typing.cast(int, event["created_ts"]))
            if abs(first_ts - created) <= LEAD_MATCH_S:
                leads.append(float(first_ts - created))
                break
    ordered = sorted(leads)
    return {
        "n": len(ordered),
        "median_s": _median(ordered),
        "p25_s": ordered[len(ordered) // 4] if ordered else 0.0,
        "p75_s": ordered[(3 * len(ordered)) // 4] if ordered else 0.0,
    }


def _sources(store: claude_worker.news.store.Store, now_ts: int) -> list[dict[str, object]]:
    """Per source: how much it produced, how much survived tier 0, how
    often it was FIRST to a story, and how often that story was later
    corroborated. Being first only matters if somebody confirms it."""
    funnel = store.source_funnel_since(now_ts - DAY_S)
    firsts = store.story_first_sources(now_ts - 30 * DAY_S)
    first_count: dict[str, int] = {}
    corroborated: dict[str, int] = {}
    for i in range(len(firsts)):
        row = firsts[i]
        source = str(row["first_source"] or "")
        if not source:
            continue
        first_count[source] = first_count.get(source, 0) + 1
        if int(typing.cast(int, row["origins"])) >= CORROBORATED_ORIGINS:
            corroborated[source] = corroborated.get(source, 0) + 1
    out: list[dict[str, object]] = []
    for i in range(len(funnel)):
        row = funnel[i]
        source = str(row["source"])
        items = int(typing.cast(int, row["items"]))
        passed = int(typing.cast(int, row["passed"] or 0))
        out.append(
            {
                "source": source,
                "items_24h": items,
                "pass_rate": passed / items if items else 0.0,
                "stories_first": first_count.get(source, 0),
                "corroborated_first": corroborated.get(source, 0),
                "calls_attributed": passed,
            }
        )
    return out


def _cost(
    store: claude_worker.news.store.Store,
    ceilings: typing.Mapping[str, int],
    now_ts: int,
) -> dict[str, object]:
    out: dict[str, object] = {}
    tiers = ("tier1", "tier2", "tier3")
    for i in range(len(tiers)):
        spent = store.budget_today(tiers[i], now_ts)
        out[tiers[i]] = {
            "calls": spent["calls"],
            "skipped": spent["skipped"],
            "input_tokens": spent["input_tokens"],
            "output_tokens": spent["output_tokens"],
            "ceiling": int(ceilings.get(tiers[i], 0)),
        }
    return out


def build_scorecard(
    store: claude_worker.news.store.Store,
    now_ts: int,
    ceilings: typing.Mapping[str, int] | None = None,
) -> dict[str, object]:
    """The whole measurement, in the shape the dashboard renders (§4.4).

    The dashboard READS this file and never recomputes it, so the number
    an operator sees and the number the N4 gate reads are the same number.
    """
    windows: dict[str, object] = {}
    all_rows: list[dict[str, object]] = []
    for name, span in WINDOWS:
        rows = store.resolutions_resolved(0 if span == 0 else now_ts - span)
        if span == 0:
            all_rows = rows
        windows[name] = {
            "direction": _direction_score(rows),
            "vol": _vol_score(rows),
            "intents": _intent_score(rows),
        }
    states = store.resolution_states()
    reasons: list[dict[str, object]] = []
    for detail, count in store.unresolvable_reasons():
        reasons.append({"detail": detail, "n": count})
    return {
        "v": claude_worker.news.SCHEMA_VERSION,
        "generated_ts": now_ts,
        "windows": windows,
        "reliability": _reliability(all_rows),
        "sources": _sources(store, now_ts),
        "lead_time": _lead_time(store, now_ts),
        "funnel_24h": store.funnel_since(now_ts - DAY_S),
        "cost_24h": _cost(store, ceilings or {}, now_ts),
        "unresolvable": {"n": states.get(STATE_UNRESOLVABLE, 0), "reasons": reasons},
        "pending": states.get(STATE_PENDING, 0),
        # The operator's contract, carried in the file so the dashboard's
        # legend and the gate can never drift apart.
        "legend": (
            "direction earns live when wilson_lo > base_rate on >= 100 resolved labels"
            " AND the N4 tool agrees — never on hit_rate alone; vol earns live at"
            " precision >= 0.6 over 8 windows of declarations; intents are"
            " APPROXIMATE (candle proxy), the harness fill model is the authority."
        ),
    }


def write_scorecard(path: pathlib.Path, doc: dict[str, object]) -> bool:
    try:
        claude_worker.news.write_atomic(
            path, json.dumps(doc, sort_keys=True, separators=(",", ":")) + "\n"
        )
    except OSError:
        return False
    return True
