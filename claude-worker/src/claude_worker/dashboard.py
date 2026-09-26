# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG6 §6.2 — the operator's dashboard page (``python -m claude_worker.dashboard``).

A stdlib ``http.server`` on ``127.0.0.1:9292`` (``$CLAUDE_WORKER_DASHBOARD_PORT``),
single-threaded, READ-ONLY. Routes:

- ``/``                    → ``dashboard/dashboard.html`` (one file, inline CSS+JS,
                             no CDN — the engine's offline law).
- ``/api/worker``          → the worker-side JSON (this module's ``worker_payload``):
                             the HAR H3 series (``har.toml``, each seed's sources,
                             the hourly ``har/drift.json``),
                             rulesets catalog, library + evidence, compositions,
                             regime history (24 h) + ``declared.json`` + the
                             ``regime.toml`` bands, the latest ``pnl-<day>.json``
                             (per strategy, per regime, per ruleset) + the day
                             series, candidates, the events ledger tail, positions
                             from the fills tail of the CURRENT run (the
                             ``positions`` verb's code path, marks carried at
                             cost), the config snapshot (``strategy.conf``,
                             ``fees.toml``, ``regime.toml``, ``xmm.toml`` hash +
                             quoted perps, ``universe.toml`` summary) and the Data
                             volume's free space. **Never ``.env``.**
- ``/api/engine/state``, ``/api/engine/metrics`` → same-origin proxies to the
                             engine's 9191 (no CORS, one page); 502 when the
                             engine is down.
- anything else            → 404.

Cadence is the page's: engine 2 s, worker 10 s. The worker payload is
cached ``CACHE_S`` seconds server-side so a second tab never doubles the
SQLite/fills reads. Write controls (enable/disable/declare/halt) are NOT
here — plan §10.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import dataclasses
import hashlib
import http.server
import json
import os
import pathlib
import shutil
import sqlite3
import sys
import time
import tomllib
import typing
import urllib.error
import urllib.request

import claude_worker.features
import claude_worker.frames
import claude_worker.har_config
import claude_worker.har_seed
import claude_worker.library
import claude_worker.news
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.cycle
import claude_worker.news.local_llm
import claude_worker.news.resolve
import claude_worker.news.store
import claude_worker.pnl_report
import claude_worker.regime
import claude_worker.state

HOST: str = "127.0.0.1"
PORT_ENV: str = "CLAUDE_WORKER_DASHBOARD_PORT"
PORT_DEFAULT: int = 9292
ENGINE_URL_ENV: str = "CLAUDE_WORKER_ENGINE_URL"
ENGINE_URL_DEFAULT: str = "http://127.0.0.1:9191"
DB_ENV: str = "CLAUDE_WORKER_DB"
DB_DEFAULT: str = "~/multivenue/worker/state.db"
MULTIVENUE_DIR_ENV: str = "CLAUDE_WORKER_MULTIVENUE_DIR"
MULTIVENUE_DIR_DEFAULT: str = "~/multivenue"
HTML_PATH: pathlib.Path = pathlib.Path(__file__).parent / "dashboard" / "dashboard.html"

CACHE_S: float = 5.0
POSITIONS_CACHE_S: float = 30.0
EVENTS_TAIL: int = 100
CANDIDATES_MAX: int = 50
PNL_DAYS: int = 14
PROXY_TIMEOUT_S: float = 2.0
PROXY_MAX_BYTES: int = 1 << 20
CONFIG_TEXT_MAX: int = 16 * 1024

# Engine paths the proxies accept (allow-list — the page asks for nothing else).
_ENGINE_ROUTES: dict[str, str] = {
    "/api/engine/state": "/state",
    "/api/engine/metrics": "/metrics",
}


@dataclasses.dataclass(frozen=True, slots=True)
class Inputs:
    """Everything ``worker_payload`` reads — resolved once at boot so tests
    point every path at a tmp dir and never touch the operator's files."""

    db_path: pathlib.Path
    reports_dir: pathlib.Path
    regime_dir: pathlib.Path
    candidates_dir: pathlib.Path
    replay_dir: pathlib.Path
    multivenue_dir: pathlib.Path
    news_dir: pathlib.Path
    #: The NEWS action policy. Its own field beside `news_dir` rather than a
    #: whole `NewsPaths` (the shape §15 names): this class is the ONE place
    #: the page's inputs are resolved, and a nested record here would let a
    #: reader take a path from it that `worker_payload` never resolved.
    #: Defaulted so the two pre-NEWS call sites keep working.
    news_policy_path: pathlib.Path = pathlib.Path(claude_worker.news.DEFAULT_POLICY_TOML)
    #: The local sidecar's artifact (doc 03 amendment d). Defaulted like
    #: `news_policy_path`, and never DIALLED from a test: `llm_section` only
    #: reaches the server when the config says one exists.
    news_llm_path: pathlib.Path = pathlib.Path(claude_worker.news.DEFAULT_LLM_TOML)
    engine_url: str = ""


def inputs_from_env(env: typing.Mapping[str, str] | None = None) -> Inputs:
    """The operator defaults: ``~/multivenue/worker/state.db`` and its
    siblings, ``~/multivenue/logs`` (``$CLAUDE_WORKER_REPLAY_DIR``),
    ``~/multivenue/worker/reports`` (``$CLAUDE_WORKER_REPORTS_DIR``),
    ``~/multivenue/*.toml`` (``$CLAUDE_WORKER_MULTIVENUE_DIR``)."""
    e = os.environ if env is None else env
    db = pathlib.Path(e.get(DB_ENV, "") or DB_DEFAULT).expanduser()
    return Inputs(
        db_path=db,
        reports_dir=claude_worker.pnl_report.resolve_reports_dir(e),
        regime_dir=claude_worker.regime.regime_dir_for(db),
        candidates_dir=claude_worker.library.candidates_dir_for(db),
        replay_dir=claude_worker.pnl_report.resolve_replay_dir(e),
        multivenue_dir=pathlib.Path(
            e.get(MULTIVENUE_DIR_ENV, "") or MULTIVENUE_DIR_DEFAULT
        ).expanduser(),
        news_dir=claude_worker.news.paths_from_env(e).news_dir,
        news_policy_path=claude_worker.news.paths_from_env(e).policy_path,
        news_llm_path=claude_worker.news.paths_from_env(e).llm_path,
        engine_url=(e.get(ENGINE_URL_ENV, "") or ENGINE_URL_DEFAULT).rstrip("/"),
    )


# ---- readers (each one fails soft: a missing/unreadable source is a
# ---- `None`/empty section, never a 500 — the page shows "n/a") ----


def _read_text(path: pathlib.Path, limit: int = CONFIG_TEXT_MAX) -> str | None:
    try:
        data = path.read_bytes()
    except OSError:
        return None
    return data[:limit].decode("utf-8", errors="replace")


def _sha256_file(path: pathlib.Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def _load_json(path: pathlib.Path) -> dict[str, object] | None:
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except OSError, ValueError:
        return None
    return obj if isinstance(obj, dict) else None


def rulesets_section(state: claude_worker.state.State) -> list[dict[str, object]]:
    return [dict(r._asdict()) for r in state.rulesets_all()]


def library_section(state: claude_worker.state.State) -> list[dict[str, object]]:
    out: list[dict[str, object]] = []
    for m in state.library_members():
        ev = state.evidence_for(m.member_id)
        out.append(
            {
                **m._asdict(),
                "evidence_n": len(ev),
                "evidence_fills": sum(r.n_fills for r in ev),
                "evidence_net_usd_0": round(sum(r.net_usd_0 for r in ev), 6),
                "evidence_net_usd_tier": round(sum(r.net_usd_tier for r in ev), 6),
                "evidence_judged": sum(1 for r in ev if r.judged),
                "evidence": [dict(r._asdict()) for r in ev],
            }
        )
    return out


def compositions_section(state: claude_worker.state.State) -> list[dict[str, object]]:
    return [dict(c._asdict()) for c in state.compositions()]


def events_tail(db_path: pathlib.Path, n: int = EVENTS_TAIL) -> list[dict[str, object]]:
    """The newest ``n`` ledger rows (read-only connection; the ``State``
    reader returns the whole table, which is the wrong shape at a 10 s
    cadence)."""
    if not db_path.is_file():
        return []
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
    except sqlite3.Error:
        return []
    try:
        cur = conn.execute("SELECT id, ts, kind, detail FROM events ORDER BY id DESC LIMIT ?", (n,))
        rows = cur.fetchall()
    except sqlite3.Error:
        return []
    finally:
        conn.close()
    rows.reverse()
    return [
        {"id": int(r[0]), "ts": int(r[1]), "kind": str(r[2]), "detail": str(r[3])} for r in rows
    ]


def regime_section(inputs: Inputs, now_ms: int) -> dict[str, object]:
    d = inputs.regime_dir
    history = claude_worker.regime.history_tail(d, now_ms)
    declared = claude_worker.regime.load_declared(d)
    params: dict[str, object] | None = None
    regime_toml = inputs.multivenue_dir / "regime.toml"
    try:
        art = claude_worker.regime.read_regime_params(regime_toml)
        params = {
            "btc": art.btc,
            "fund": art.fund,
            "members": list(art.members),
            "confirm_min": art.params.confirm_min,
            "profiles": {
                name: dataclasses.asdict(art.params.profiles[i])
                for i, name in enumerate(claude_worker.regime.PROFILE_NAMES)
                if i < len(art.params.profiles)
            },
        }
    except OSError, ValueError, TypeError, KeyError:
        params = None
    return {
        "dir": str(d),
        "history": history,
        "declared": declared,
        "params": params,
        "dims": claude_worker.frames.REGIME_DIMS,
        "values": claude_worker.frames.REGIME_VALUES,
    }


#: How far back the NEWS funnel counts (the page shows one day).
NEWS_WINDOW_S: int = 86_400
#: Rows the NEWS panel shows at most.
NEWS_EVENTS_MAX: int = 20
#: Instruments named inside a collapsed events row.
NEWS_EVENT_SAMPLE: int = 3
#: Open stories the board shows (spec §15).
NEWS_STORIES_MAX: int = 20
#: Markers drawn onto the regime timeline (spec §15).
NEWS_TIMELINE_MAX: int = 200
#: Chars of a story title carried into a timeline marker.
NEWS_TITLE_MAX: int = 120


def _news_events(
    store: claude_worker.news.store.Store,
    since_ts: int,
    limit: int = NEWS_EVENTS_MAX,
) -> list[dict[str, object]]:
    """The events tail, newest RECORDED last (the panel reverses it).

    Two corrections over a plain slice of `events_since`, both measured
    2026-09-19 on live payloads: that query orders by `at_ts`, so taking
    the last N returned the FURTHEST-FUTURE events rather than the newest
    ones; and one poll of the Deribit BTC option chain records 190 expiry
    events dated 18-72 h out, which would hold every row of a 20-row tail
    for days and hide every listing, delisting and maintenance event
    behind them. Events sharing (kind, venue, at_ts) therefore collapse to
    one row naming the count, exactly as `calendar.json` does.

    The rule itself lives in `store.recent_events`, because the tier-3
    analyst's context block wants the same tail and was wrong in the same
    two ways before it shared this one (2026-09-20).
    """
    return claude_worker.news.store.recent_events(
        store, since_ts, limit, NEWS_EVENT_SAMPLE
    )


def _news_funnel(
    store: claude_worker.news.store.Store, since_ts: int
) -> tuple[dict[str, int], int]:
    """Items by tier-0 verdict over the window, and the total."""
    by_verdict: dict[str, int] = {}
    rows = store.items_since(since_ts)
    for i in range(len(rows)):
        verdict = str(rows[i]["tier0"])
        by_verdict[verdict] = by_verdict.get(verdict, 0) + 1
    return by_verdict, len(rows)


def _news_stories(
    store: claude_worker.news.store.Store, since_ts: int
) -> list[dict[str, object]]:
    """The stories board (spec §15): one row per open story with its label,
    the resolution that will judge it, and the policy's verdict.

    Three tables joined in Python rather than SQL because each is a
    one-row lookup by primary key and the board is capped at twenty — and
    because a LEFT JOIN across labels, resolutions and actions would make
    a missing row indistinguishable from an empty one, which is exactly
    the distinction the operator is reading for.
    """
    out: list[dict[str, object]] = []
    stories = store.stories_open(since_ts)
    start = max(0, len(stories) - NEWS_STORIES_MAX)
    verdicts = _news_verdicts(store, since_ts)
    for i in range(start, len(stories)):
        story = stories[i]
        story_id = str(story["story_id"])
        row = dict(story)
        row["label"] = store.label(story_id)
        row["resolution"] = store.resolution(
            claude_worker.news.resolve.SUBJECT_LABEL, story_id
        )
        row["verdict"] = verdicts.get(story_id)
        out.append(row)
    return out


def _news_verdicts(
    store: claude_worker.news.store.Store, since_ts: int
) -> dict[str, dict[str, object]]:
    """The NEWEST bias verdict per story. A story whose label was refused
    and later emitted should read as emitted, not as both."""
    out: dict[str, dict[str, object]] = {}
    rows = store.actions_since(since_ts)
    for i in range(len(rows)):
        row = rows[i]
        if str(row["kind"]) != claude_worker.news.actions.KIND_SET_BIAS:
            continue
        out[str(row["story_id"])] = {
            "mode": row["mode"],
            "refused_reason": row["refused_reason"],
            "ts": row["ts"],
            "seq": row["seq"],
        }
    return out


def _news_actions(
    store: claude_worker.news.store.Store, since_ts: int
) -> list[dict[str, object]]:
    """Actions of the window counted by (kind, mode) — the whole record of
    what the policy let through, and what it did not."""
    counts: dict[tuple[str, str], int] = {}
    rows = store.actions_since(since_ts)
    for i in range(len(rows)):
        key = (str(rows[i]["kind"]), str(rows[i]["mode"]))
        counts[key] = counts.get(key, 0) + 1
    out: list[dict[str, object]] = []
    for kind, mode in sorted(counts):
        out.append({"kind": kind, "mode": mode, "count": counts[(kind, mode)]})
    return out


def _news_budget(
    store: claude_worker.news.store.Store, ceilings: typing.Mapping[str, int], now_ts: int
) -> dict[str, object]:
    out: dict[str, object] = {}
    for i in range(len(claude_worker.news.cascade.TIERS)):
        tier = claude_worker.news.cascade.TIERS[i]
        spent = store.budget_today(tier, now_ts)
        out[tier] = {
            "calls": spent["calls"],
            "skipped": spent["skipped"],
            "ceiling": int(ceilings.get(tier, 0)),
        }
    return out


def _news_timeline(
    store: claude_worker.news.store.Store, since_ts: int
) -> list[dict[str, object]]:
    """Markers for the existing regime timeline (spec §15).

    A marker is a CLAIM with what became of it: the arrow is its direction,
    the bar its horizon, the colour whether it is still pending, hit or
    missed. Claims with no direction are carried too — they are real
    answers about vol, and leaving them out would make the timeline look
    more directional than the lane is.
    """
    out: list[dict[str, object]] = []
    rows = store.resolutions_since(since_ts)
    for i in range(len(rows)):
        row = rows[i]
        kind = str(row["subject_kind"])
        marker: dict[str, object] = {
            "ts": int(typing.cast(int, row["t0"])) * 1000,
            "kind": kind,
            "story_id": row["subject_id"],
            "descriptor": row["descriptor"],
            "direction": row["direction"],
            "confidence": row["confidence"],
            "horizon_s": row["horizon_s"],
            "state": row["state"],
            "hit": row["hit"],
            "signed_bps": row["signed_bps"],
            "vol_reached_high": row["vol_reached_high"],
        }
        if kind == claude_worker.news.resolve.SUBJECT_LABEL:
            story = store.story(str(row["subject_id"]))
            if story is not None:
                marker["event_type"] = story["event_type"]
                marker["assets"] = story["assets"]
                items = store.story_items(str(row["subject_id"]), 1)
                if items:
                    marker["title"] = str(items[0]["title"])[:NEWS_TITLE_MAX]
        out.append(marker)
    return out[-NEWS_TIMELINE_MAX:]


def _news_red_rules(
    payload: typing.Mapping[str, object],
    policy: claude_worker.news.actions.NewsPolicy,
    registry_ok: bool,
) -> list[str]:
    """The four §15 red rules, as sentences the page prints verbatim.

    The maintenance rule is NOT recomputed here: `detect.maintenance_alerts`
    owns it and writes the ALERT file, which the panel already shows. Two
    implementations of "a member is enabled on a venue under maintenance"
    would eventually disagree, and the one on the page is the one the
    operator would trust.
    """
    out: list[str] = []
    if not policy.valid:
        out.append("policy invalid — every action mode is off")
    if not registry_ok:
        out.append("registry invalid — the lane is not aggregating")
    rows = payload.get("sources")
    if isinstance(rows, list):
        for i in range(len(rows)):
            row = rows[i]
            if not isinstance(row, dict) or not int(row.get("enabled") or 0):
                continue
            streak = int(row.get("err_streak") or 0)
            if streak >= claude_worker.news.cycle.ERR_STREAK_ALERT:
                out.append(f"source {row.get('name')} dead {streak} polls")
    return out


#: How long the page waits on the sidecar. The panel is a 10 s-cadence read;
#: a model still loading must not hold it.
NEWS_LLM_TIMEOUT_S: float = 1.5


def llm_section(
    inputs: Inputs, store: claude_worker.news.store.Store | None
) -> dict[str, object]:
    """The local sidecar's block (doc 03 amendment d).

    Fail-soft like every other section, with one extra rule: an absent
    `llm.toml` does NOT dial anything. A page that probed 127.0.0.1:9393 on
    every refresh of a box with no sidecar would be a 10 s-cadence connection
    error in the operator's log forever.
    """
    cfg = claude_worker.news.local_llm.load_config(inputs.news_llm_path)
    out: dict[str, object] = {
        "path": str(inputs.news_llm_path),
        "present": cfg.present,
        "valid": cfg.valid,
        "model": cfg.model_tag,
        "url": cfg.base_url if cfg.present else "",
        "health": False,
        "serving": "",
        "match": None,
        "metrics": {},
        "calls_24h": 0,
        "counters": {},
    }
    if store is not None:
        counters = store.counters()
        picked: dict[str, int] = {}
        for name in sorted(counters):
            if name.startswith("llm_"):
                picked[name] = counters[name]
        out["counters"] = picked
        spent = 0
        for tier in claude_worker.news.cascade.TIERS:
            spent += store.budget_today(
                claude_worker.news.local_llm.local_tier(tier), now_ts=None
            )["calls"]
        out["calls_24h"] = spent
    if not cfg.present or not cfg.valid:
        return out
    try:
        client = claude_worker.news.local_llm.LocalClient(
            cfg.base_url, timeout_s=NEWS_LLM_TIMEOUT_S
        )
    except (OSError, ValueError):
        return out
    try:
        out["health"] = client.health()
        if not out["health"]:
            return out
        serving = claude_worker.news.local_llm.props_model_stem(client.props())
        pinned = cfg.model_path.name.removesuffix(".gguf").lower()
        out["serving"] = serving
        out["match"] = None if not serving else serving == pinned
        metrics = client.metrics()
        keep: dict[str, float] = {}
        for name in sorted(metrics):
            if "llamacpp" in name:
                keep[name] = metrics[name]
        out["metrics"] = keep
    finally:
        client.close()
    return out


def news_section(inputs: Inputs, now_ms: int) -> dict[str, object]:
    """The NEWS lane panel (NEWS spec §15).

    Read-only and fail-soft like every other section: an absent ``news.db``
    (the lane was never installed) renders as ``present: false`` with empty
    tables, and an unreadable one renders the same way. The page never 500s
    because a lane the operator has not set up is missing.
    """
    db_path = inputs.news_dir / claude_worker.news.DB_FILENAME
    payload: dict[str, object] = {
        "dir": str(inputs.news_dir),
        "db": {"path": str(db_path), "present": db_path.is_file()},
        "sources": [],
        "funnel_24h": {},
        "items_24h": 0,
        "events": [],
        "counters": {},
        "alert": None,
        "calendar": None,
        "scorecard": None,
        "stories_open": [],
        "actions_24h": [],
        "budget_today": {},
        "timeline_24h": [],
        "red_rules": [],
        "policy": {"path": str(inputs.news_policy_path), "valid": False, "modes": {}},
        "llm": {},
    }
    payload["llm"] = llm_section(inputs, None)
    policy = claude_worker.news.actions.load_policy(inputs.news_policy_path)
    payload["policy"] = {
        "path": str(inputs.news_policy_path),
        "present": inputs.news_policy_path.is_file(),
        "valid": policy.valid,
        "modes": dict(policy.modes),
    }
    alert = inputs.news_dir / claude_worker.news.ALERT_FILE
    if alert.is_file():
        try:
            payload["alert"] = alert.read_text(encoding="utf-8").strip()[:CONFIG_TEXT_MAX]
        except OSError:
            payload["alert"] = None
    payload["calendar"] = _load_json(inputs.news_dir / claude_worker.news.CALENDAR_FILE)
    payload["scorecard"] = _load_json(inputs.news_dir / claude_worker.news.SCORECARD_FILE)
    if not db_path.is_file():
        return payload
    since = now_ms // 1000 - NEWS_WINDOW_S
    try:
        store = claude_worker.news.store.Store(db_path)
    except (OSError, claude_worker.news.store.StoreError, sqlite3.Error):
        return payload
    try:
        payload["sources"] = store.source_rows()
        by_verdict, total = _news_funnel(store, since)
        payload["funnel_24h"] = by_verdict
        payload["items_24h"] = total
        payload["events"] = _news_events(store, since)
        payload["counters"] = store.counters()
        payload["stories_open"] = _news_stories(store, since)
        payload["actions_24h"] = _news_actions(store, since)
        payload["budget_today"] = _news_budget(store, policy.ceilings, now_ms // 1000)
        payload["timeline_24h"] = _news_timeline(store, since)
        payload["red_rules"] = _news_red_rules(payload, policy, registry_ok=True)
        payload["llm"] = llm_section(inputs, store)
    except sqlite3.Error:
        pass
    finally:
        store.close()
    return payload


def pnl_section(reports_dir: pathlib.Path) -> dict[str, object]:
    latest = claude_worker.pnl_report.latest_report(reports_dir)
    latest_obj = _load_json(latest) if latest is not None else None
    series: list[dict[str, object]] = []
    if reports_dir.is_dir():
        for path in sorted(reports_dir.glob("pnl-*.json"))[-PNL_DAYS:]:
            obj = _load_json(path)
            if obj is None:
                continue
            paper = obj.get("paper") if isinstance(obj.get("paper"), dict) else {}
            per_strategy: dict[str, object] = {}
            for row in obj.get("strategies") or []:
                if isinstance(row, dict):
                    per_strategy[str(row.get("label", row.get("strategy_id")))] = {
                        "net_usd": row.get("net_usd"),
                        "fee_ladder_net_usd": row.get("fee_ladder_net_usd"),
                        "fills": row.get("fills"),
                    }
            series.append(
                {
                    "day": obj.get("day", path.stem.removeprefix("pnl-")),
                    "runs": obj.get("runs"),
                    "paper_fills": paper.get("fills"),
                    "paper_net_usd": paper.get("net_usd"),
                    "strategies": per_strategy,
                }
            )
    latest_slim: dict[str, object] | None = None
    if latest_obj is not None:
        # Everything but the per-run detail (the day merge is the view).
        latest_slim = {k: v for k, v in latest_obj.items() if k != "runs_detail"}
    return {
        "reports_dir": str(reports_dir),
        "latest_path": None if latest is None else str(latest),
        "latest": latest_slim,
        "series": series,
    }


def candidates_section(candidates_dir: pathlib.Path) -> list[dict[str, object]]:
    if not candidates_dir.is_dir():
        return []
    files = sorted(
        (p for p in candidates_dir.glob("*.json") if p.is_file()),
        key=lambda p: p.stat().st_mtime,
        reverse=True,
    )
    out: list[dict[str, object]] = []
    for p in files[:CANDIDATES_MAX]:
        try:
            st = p.stat()
        except OSError:
            continue
        out.append({"name": p.name, "size": st.st_size, "mtime_ms": int(st.st_mtime * 1000)})
    return out


def positions_section(replay_dir: pathlib.Path) -> dict[str, object]:
    """The ``positions`` verb's law over the CURRENT run's fills tail,
    marks carried at cost (no tick scan at a 30 s cadence — the engine's
    ``/state`` carries the live marks the page needs for a quote)."""
    run_dir = claude_worker.features.latest_run_dir(replay_dir)
    if run_dir is None:
        return {"run_dir": None, "positions": [], "fills": 0, "fills_torn": False}
    fills, torn = claude_worker.features.read_fills(run_dir)
    reconstructed = claude_worker.features.reconstruct_positions(fills)
    views = claude_worker.features.position_views(reconstructed, {})
    to_usd = claude_worker.features.to_usd
    scale = 1_000_000
    rows: list[dict[str, object]] = []
    for sym in sorted(views):
        v = views[sym]
        rows.append(
            {
                "sym": v.sym,
                "net_qty": v.net_qty / scale,
                "avg_px": v.avg_px / scale,
                "realized_usd": to_usd(v.realized),
                "exposure_usd": to_usd(v.exposure),
            }
        )
    return {
        "run_dir": str(run_dir),
        "positions": rows,
        "fills": len(fills),
        "fills_torn": torn,
        "realized_usd": round(sum(float(r["realized_usd"]) for r in rows), 6),
    }


def _universe_summary(path: pathlib.Path) -> dict[str, object] | None:
    text = _read_text(path, limit=1 << 20)
    if text is None:
        return None
    try:
        obj = tomllib.loads(text)
    except ValueError:
        return {"parse": "failed"}
    out: dict[str, object] = {}
    for venue, section in obj.items():
        if isinstance(section, dict):
            out[venue] = {k: len(v) for k, v in section.items() if isinstance(v, list)}
    return out


def _xmm_summary(path: pathlib.Path) -> dict[str, object]:
    """``xmm.toml`` (slot 6 since XMM XH1): its hash and the perps it quotes
    (``quote_<coin> = 1``, upper-cased, sorted). Absent or unparseable → no
    perps; the hash says which (``None`` = absent)."""
    text = _read_text(path, limit=1 << 20)
    quoted: list[str] = []
    if text is not None:
        try:
            section = tomllib.loads(text).get("xmm", {})
        except ValueError:
            section = {}
        if isinstance(section, dict):
            quoted = sorted(
                k[len("quote_"):].upper() for k, v in section.items() if k.startswith("quote_") and v == 1
            )
    return {"hash": _sha256_file(path), "quoted": quoted}


def config_section(inputs: Inputs) -> dict[str, object]:
    d = inputs.multivenue_dir
    return {
        "strategy_conf": _read_text(d / "strategy.conf"),
        "fees_toml": _read_text(d / "fees.toml"),
        "regime_toml": _read_text(d / "regime.toml"),
        "xmm": _xmm_summary(d / "xmm.toml"),
        "universe": _universe_summary(d / "universe.toml"),
        "retention_conf": _read_text(d / "retention.conf"),
    }


# ---- HAR H3.6: the worker half of the "Volatility (HAR, long)" panel ----

#: The §7 amber rule: a fallback whose median ``|ln vol ratio|`` against the
#: source before it exceeds this over the drift window (~22 % in ``sum r^2``).
HAR_DRIFT_AMBER: float = 0.10
#: The §7 red rule: a newest closed day older than this is "not
#: recalibrating" (the engine's ``day_age_s``), seconds.
HAR_DAY_AGE_RED_S: int = 26 * 3600
#: Bytes of a seed's head read for its ``span`` lines (the header is < 1 KiB;
#: a fitted seed is ~350 KiB of rows that are never read here).
_HAR_SEED_HEAD: int = 8 * 1024
#: ``# span <descriptor> <first_ms> <last_ms> <minutes>``: six fields.
_HAR_SPAN_FIELDS: int = 6


def _seed_spans(path: pathlib.Path) -> list[dict[str, object]] | None:
    """The ``# span <descriptor> <first_ms> <last_ms> <minutes>`` lines a seed
    opens with (``har_seed.seed_header``), oldest first -- which venues the
    series' history came from. ``None``: no readable seed."""
    try:
        with path.open("r", encoding="utf-8") as f:
            head = f.read(_HAR_SEED_HEAD)
    except (OSError, UnicodeDecodeError):
        return None
    spans: list[dict[str, object]] = []
    for line in head.splitlines():
        if not line.startswith("#"):
            break
        parts = line.split()
        if len(parts) != _HAR_SPAN_FIELDS or parts[1] != "span":
            continue
        try:
            first, last, minutes = int(parts[3]), int(parts[4]), int(parts[5])
        except ValueError:
            continue
        spans.append(
            {"descriptor": parts[2], "first_ms": first, "last_ms": last, "minutes": minutes}
        )
    return spans


def _file_age_s(path: pathlib.Path, now_ms: int) -> int | None:
    """Seconds since ``path`` was written; ``None`` when it is absent."""
    try:
        return max(0, now_ms // 1000 - int(path.stat().st_mtime))
    except OSError:
        return None


def _har_drift(path: pathlib.Path) -> dict[str, object] | None:
    """``har/drift.json`` (``har_seed compare --json-out``), or ``None`` when
    it is absent, torn or another version -- never a guess."""
    try:
        doc = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, ValueError):
        return None
    if (
        not isinstance(doc, dict)
        or doc.get("v") != claude_worker.har_seed.DRIFT_VERSION
        or not isinstance(doc.get("pairs"), list)
    ):
        return None
    return typing.cast(dict[str, object], doc)


def har_section(inputs: Inputs, now_ms: int) -> dict[str, object]:
    """HAR H3.6: what the engine's ``/state.har`` cannot say -- the series
    ``har.toml`` names with their feed and ordered fallbacks, each seed's
    sources and age, each engine state file's age, and ``drift.json``, the
    hourly ``compare`` of every feed against its fallbacks (the ruling "keep
    fallbacks live"). The page joins it with ``/state.har`` by name and
    applies the thresholds carried here. No ``har.toml`` = not configured."""
    toml = inputs.multivenue_dir / "har.toml"
    har_dir = inputs.multivenue_dir / "har"
    doc: dict[str, object] = {
        "configured": False,
        "error": None,
        "amber_drift": HAR_DRIFT_AMBER,
        "red_day_age_s": HAR_DAY_AGE_RED_S,
        "series": [],
        "drift": None,
        "drift_age_s": None,
    }
    if not toml.is_file():
        return doc
    try:
        series = claude_worker.har_config.read(toml)
    except (OSError, claude_worker.har_config.HarConfigError) as e:
        doc["error"] = str(e)
        return doc
    drift_path = har_dir / "drift.json"
    doc["configured"] = True
    doc["series"] = [
        {
            "name": s.name,
            "feed": s.feed,
            "fallback": list(s.fallback),
            "spans": _seed_spans(har_dir / f"seed-{s.name}.tsv"),
            "seed_age_s": _file_age_s(har_dir / f"seed-{s.name}.tsv", now_ms),
            "state_age_s": _file_age_s(har_dir / f"state-{s.name}.tsv", now_ms),
        }
        for s in series
    ]
    doc["drift"] = _har_drift(drift_path)
    doc["drift_age_s"] = _file_age_s(drift_path, now_ms)
    return doc


def disk_section(path: pathlib.Path) -> dict[str, object] | None:
    probe = path if path.exists() else path.parent
    try:
        u = shutil.disk_usage(probe)
    except OSError:
        return None
    return {"path": str(probe), "free_bytes": u.free, "total_bytes": u.total}


def worker_payload(
    inputs: Inputs,
    now_ms: int | None = None,
    positions: dict[str, object] | None = None,
) -> dict[str, object]:
    """The ``/api/worker`` document. ``positions`` lets the server pass
    its 30 s-cached section; ``None`` computes it here."""
    ts = int(time.time() * 1000) if now_ms is None else now_ms
    db_present = inputs.db_path.is_file()
    rulesets: list[dict[str, object]] = []
    library: list[dict[str, object]] = []
    compositions: list[dict[str, object]] = []
    if db_present:
        state = claude_worker.state.State(inputs.db_path)
        try:
            rulesets = rulesets_section(state)
            library = library_section(state)
            compositions = compositions_section(state)
        finally:
            state.close()
    return {
        "v": 1,
        "now_ms": ts,
        "db": {"path": str(inputs.db_path), "present": db_present},
        "engine_url": inputs.engine_url,
        "rulesets": rulesets,
        "library": library,
        "compositions": compositions,
        "regime": regime_section(inputs, ts),
        "pnl": pnl_section(inputs.reports_dir),
        "candidates": candidates_section(inputs.candidates_dir),
        "events": events_tail(inputs.db_path),
        "positions": positions_section(inputs.replay_dir) if positions is None else positions,
        "config": config_section(inputs),
        "disk": disk_section(inputs.replay_dir),
        "news": news_section(inputs, ts),
        "har": har_section(inputs, ts),
    }


# ---- the server ----


class _Cache:
    """Server-side memo of the worker document + its positions part."""

    def __init__(self, inputs: Inputs) -> None:
        self.inputs = inputs
        self._doc: bytes | None = None
        self._doc_at: float = 0.0
        self._positions: dict[str, object] | None = None
        self._positions_at: float = 0.0

    def worker_json(self) -> bytes:
        now = time.monotonic()
        if self._doc is not None and now - self._doc_at < CACHE_S:
            return self._doc
        if self._positions is None or now - self._positions_at >= POSITIONS_CACHE_S:
            self._positions = positions_section(self.inputs.replay_dir)
            self._positions_at = now
        doc = worker_payload(self.inputs, positions=self._positions)
        self._doc = json.dumps(doc, separators=(",", ":"), default=str).encode("utf-8")
        self._doc_at = now
        return self._doc


def proxy_engine(engine_url: str, path: str) -> tuple[int, bytes, str]:
    """``GET engine_url + path`` → ``(status, body, content_type)``; 502
    when the engine does not answer."""
    req = urllib.request.Request(engine_url + path, method="GET")
    try:
        # Loopback only: `_ENGINE_ROUTES` is the allow-list, `engine_url` the boot config.
        with urllib.request.urlopen(req, timeout=PROXY_TIMEOUT_S) as resp:
            body = resp.read(PROXY_MAX_BYTES)
            ctype = resp.headers.get("Content-Type", "application/octet-stream")
            return int(resp.status), body, ctype
    except urllib.error.HTTPError as exc:
        return int(exc.code), exc.read(PROXY_MAX_BYTES), "text/plain"
    except OSError:  # URLError, ConnectionRefused, timeouts
        return 502, b"engine unreachable\n", "text/plain"


def make_handler(cache: _Cache, html: bytes) -> type[http.server.BaseHTTPRequestHandler]:
    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = "claude-worker-dashboard/1"

        def log_message(self, fmt: str, *args: object) -> None:
            # Quiet by default (launchd log hygiene); opt in per process.
            if os.environ.get("CLAUDE_WORKER_DASHBOARD_LOG"):
                sys.stderr.write("dashboard: " + fmt % args + "\n")

        def _send(self, status: int, body: bytes, ctype: str) -> None:
            self.send_response(status)
            self.send_header("Content-Type", ctype)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:  # http.server's dispatch name
            path = self.path.split("?", 1)[0]
            if path in ("/", "/index.html"):
                self._send(200, html, "text/html; charset=utf-8")
                return
            if path == "/api/worker":
                try:
                    body = cache.worker_json()
                except Exception as exc:  # a reader bug must not kill the page
                    self._send(500, f"worker payload failed: {exc}\n".encode(), "text/plain")
                    return
                self._send(200, body, "application/json")
                return
            engine_path = _ENGINE_ROUTES.get(path)
            if engine_path is not None:
                status, body, ctype = proxy_engine(cache.inputs.engine_url, engine_path)
                self._send(status, body, ctype)
                return
            self._send(404, b"not found\n", "text/plain")

    return Handler


def serve(inputs: Inputs, port: int, html_path: pathlib.Path = HTML_PATH) -> None:
    """Block serving until interrupted (the launchd job's body)."""
    html = html_path.read_bytes()
    handler = make_handler(_Cache(inputs), html)
    with http.server.HTTPServer((HOST, port), handler) as srv:
        sys.stderr.write(f"dashboard: serving http://{HOST}:{port}/ (engine {inputs.engine_url})\n")
        srv.serve_forever()


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="python -m claude_worker.dashboard")
    p.add_argument("--port", type=int, default=int(os.environ.get(PORT_ENV, "") or PORT_DEFAULT))
    p.add_argument(
        "--engine-url", default=None, help=f"default ${ENGINE_URL_ENV} or {ENGINE_URL_DEFAULT}"
    )
    p.add_argument(
        "--db", default=None, help=f"worker state.db (default ${DB_ENV} or {DB_DEFAULT})"
    )
    p.add_argument("--once", action="store_true", help="print /api/worker JSON and exit")
    args = p.parse_args(argv)
    env = dict(os.environ)
    if args.db:
        env[DB_ENV] = args.db
    if args.engine_url:
        env[ENGINE_URL_ENV] = args.engine_url
    inputs = inputs_from_env(env)
    if args.once:
        sys.stdout.write(json.dumps(worker_payload(inputs), indent=2, sort_keys=True, default=str))
        sys.stdout.write("\n")
        return 0
    try:
        serve(inputs, args.port)
    except KeyboardInterrupt:
        pass
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
