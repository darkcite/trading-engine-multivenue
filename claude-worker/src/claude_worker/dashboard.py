# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG6 §6.2 — the operator's dashboard page (``python -m claude_worker.dashboard``).

A stdlib ``http.server`` on ``127.0.0.1:9292`` (``$CLAUDE_WORKER_DASHBOARD_PORT``),
single-threaded, READ-ONLY. Routes:

- ``/``                    → ``dashboard/dashboard.html`` (one file, inline CSS+JS,
                             no CDN — the engine's offline law).
- ``/api/worker``          → the worker-side JSON (this module's ``worker_payload``):
                             rulesets catalog, library + evidence, compositions,
                             regime history (24 h) + ``declared.json`` + the
                             ``regime.toml`` bands, the latest ``pnl-<day>.json``
                             (per strategy, per regime, per ruleset) + the day
                             series, candidates, the events ledger tail, positions
                             from the fills tail of the CURRENT run (the
                             ``positions`` verb's code path, marks carried at
                             cost), the config snapshot (``strategy.conf``,
                             ``fees.toml``, ``regime.toml``, ``icdp.toml`` hash +
                             instruments, ``universe.toml`` summary) and the Data
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
import claude_worker.library
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
    engine_url: str


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


def config_section(inputs: Inputs) -> dict[str, object]:
    d = inputs.multivenue_dir
    icdp = d / "icdp.toml"
    icdp_text = _read_text(icdp, limit=1 << 20)
    icdp_instruments = icdp_text.count("[[instrument]]") if icdp_text else 0
    return {
        "strategy_conf": _read_text(d / "strategy.conf"),
        "fees_toml": _read_text(d / "fees.toml"),
        "regime_toml": _read_text(d / "regime.toml"),
        "icdp": {"hash": _sha256_file(icdp), "instruments": icdp_instruments},
        "universe": _universe_summary(d / "universe.toml"),
        "retention_conf": _read_text(d / "retention.conf"),
    }


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
