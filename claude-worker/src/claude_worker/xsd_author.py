# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""xsd_author -- the xsd member's table and boot seed (XSD-4).

Two artifacts the engine's slot-2 member (``crates/strategy-xsd``) reads at
boot, both written here from ``~/multivenue/worker/candles.db`` and nothing
else -- derived data, never a capture window:

* ``xsd-table.tsv`` -- ``target \\t partner \\t beta_1e9 \\t rank \\t t_adf_1e6``:
  the research screen (statarb doc 07 §3.2, lifted from the vault's
  ``sa_screen.py`` with the same arithmetic) over ONE formation window of
  ``form_h`` hourly log closes -- OLS hedge ratio per ordered pair, the
  Engle-Granger residual ADF (batched normal equations, one solve per pair),
  the admissibility law (return correlation, half-life band, spread sigma,
  beta band, a median dollar-volume floor) and the K most negative ADF
  statistics per target.  Two laws the engine forced on top of the research:
  every target and partner must close **>= $0.005 over the whole window**
  (the engine's x1e6 ``Price`` quantises cheaper names' logs -- BOME broke
  parity at $0.00035), and a target with fewer than K admissible partners is
  left OUT (the research's ``len(g) < top_k`` skip).  The file's sha256 is the
  table identity: the engine restores positions only under the same hash and
  flattens them under a new one, so writing a new table IS the rotation.

* ``xsd-seed.tsv`` -- ``descriptor \\t open_ms \\t close_1e6``: the trailing
  ``hours`` hourly closes of every descriptor the table names, so the z
  windows are warm at the first roll instead of 30 days later.  Rows at or
  after the boot hour are refused by the engine; this lane never writes them
  (it ends at the last COMPLETE hour candles.db holds).

Lanes (``python -m claude_worker.xsd_author <lane>``):

- ``author --universe <tsv> --out <xsd-table.tsv> [--db] [--end-ms] [screen knobs]``
- ``seed-out --table <xsd-table.tsv> --out <xsd-seed.tsv> [--db] [--hours 800]``
- ``status [--table]`` -- age, hash, row counts (the rotation lane's question).

Full imports only; numpy is a real dependency of this module (the screen is
N^2 x W einsum work, seconds in numpy and minutes in pure Python).
"""

import argparse
import hashlib
import os
import pathlib
import sqlite3
import sys
import time
import typing

import numpy

HOUR_MS: int = 3_600_000
DEFAULT_DB: str = "~/multivenue/worker/candles.db"
DEFAULT_UNIVERSE: str = "~/multivenue/xsd-universe.tsv"
DEFAULT_TABLE: str = "~/multivenue/xsd-table.tsv"
DEFAULT_SEED: str = "~/multivenue/xsd-seed.tsv"
#: Seed depth: the operating point's z window (720 h) plus slack for holes.
DEFAULT_SEED_HOURS: int = 800
#: The rotation cadence (doc 08 §3.7): the research's 30-day trading window.
ROTATION_DAYS: float = 30.0
#: A table row is `target \t partner \t beta_1e9` at least (rank / t_adf optional).
TABLE_MIN_FIELDS: int = 3
#: Fewer alive names than this cannot form a pair.
MIN_ALIVE: int = 2

TABLE_HEADER: str = (
    "# xsd-table.tsv — target\tpartner\tbeta_1e9\trank\tt_adf_1e6 (claude_worker.xsd_author)\n"
)
SEED_HEADER: str = (
    "# xsd-seed.tsv — descriptor\topen_ms\tclose_1e6 (claude_worker.xsd_author seed-out)\n"
)


class ScreenParams(typing.NamedTuple):
    """The screen's knobs — defaults are the research operating point."""

    form_h: int = 2160
    lags: int = 1
    top_k: int = 3
    min_corr: float = 0.30
    hl_min_h: float = 2.0
    hl_max_h: float = 240.0
    min_sigma_bps: float = 25.0
    min_qv_daily_usd: float = 5e6
    max_beta: float = 4.0
    min_px_usd: float = 0.005
    #: candles.db gap tolerance: a descriptor missing at most this many bars
    #: of the window is filled (forward, leading bars backward) instead of
    #: dropped — a REST-lane artefact (the w-z names' backfill started one
    #: hour after the a-v names' on this host), not a market gap; Binance
    #: perps never close. More missing bars ⇒ the research's rule: OUT.
    max_gap_bars: int = 3


class PairRow(typing.NamedTuple):
    target: str
    partner: str
    beta: float
    rank: int
    t_adf: float
    corr: float
    halflife: float
    sigma: float


class AuthorResult(typing.NamedTuple):
    rows: list[PairRow]
    alive: list[str]
    skipped: dict[str, list[str]]
    window: tuple[int, int]


# ---------------------------------------------------------------------------
# inputs
# ---------------------------------------------------------------------------

def read_universe(path: pathlib.Path) -> list[str]:
    """One descriptor per line (first tab-separated column); ``#`` comments;
    duplicates collapse in first-seen order."""
    out: list[str] = []
    seen: set[str] = set()
    for raw in path.read_text().splitlines():
        line: str = raw.strip()
        if not line or line.startswith("#"):
            continue
        d: str = line.split("\t", 1)[0].strip()
        if d and d not in seen:
            seen.add(d)
            out.append(d)
    return out


def table_descriptors(text: str) -> list[str]:
    """Targets + partners of a table, first-seen order (the seed's universe)."""
    out: list[str] = []
    seen: set[str] = set()
    for raw in text.splitlines():
        line: str = raw.strip()
        if not line or line.startswith("#"):
            continue
        f: list[str] = [x.strip() for x in line.split("\t")]
        if len(f) < TABLE_MIN_FIELDS:
            continue
        for d in (f[0], f[1]):
            if d not in seen:
                seen.add(d)
                out.append(d)
    return out


def hour_floor_ms(ms: int) -> int:
    return (ms // HOUR_MS) * HOUR_MS


def last_complete_hour_end_ms(now_ms: int) -> int:
    """Exclusive end of the newest COMPLETE hour: the current hour's open
    (the in-progress hour is never a close)."""
    return hour_floor_ms(now_ms)


def load_matrix(
    db: sqlite3.Connection, descriptors: list[str], end_ms: int, hours: int
) -> tuple[numpy.ndarray, numpy.ndarray, numpy.ndarray]:
    """``(open_ms [W], close [W, N], qv [W, N])`` for the ``hours`` hourly bars
    ending just before ``end_ms``; absent bars are NaN. ``qv`` is the bar's
    dollar volume proxy ``c · v`` (candles.db carries base volume)."""
    start_ms: int = end_ms - hours * HOUR_MS
    hours_ms: numpy.ndarray = numpy.arange(start_ms, end_ms, HOUR_MS, dtype=numpy.int64)
    n: int = len(descriptors)
    close: numpy.ndarray = numpy.full((hours, n), numpy.nan, dtype=numpy.float64)
    qv: numpy.ndarray = numpy.full((hours, n), numpy.nan, dtype=numpy.float64)
    j: int = 0
    while j < n:
        rows = db.execute(
            "select open_ts, c, v from candles where descriptor = ? and tf = '1h' "
            "and open_ts >= ? and open_ts < ? and c is not null and c > 0",
            (descriptors[j], start_ms, end_ms),
        ).fetchall()
        for ts, c, v in rows:
            i: int = (int(ts) - start_ms) // HOUR_MS
            if 0 <= i < hours:
                close[i, j] = float(c)
                qv[i, j] = float(c) * float(v) if v is not None else numpy.nan
        j += 1
    return hours_ms, close, qv


# ---------------------------------------------------------------------------
# the screen (sa_screen.py, unchanged arithmetic)
# ---------------------------------------------------------------------------

def batched_adf(eps: numpy.ndarray, lags: int) -> tuple[numpy.ndarray, numpy.ndarray]:
    """Batched augmented Dickey-Fuller over the columns of ``eps`` [W, P]:
    ``(t_stat [P], rho [P])``; degenerate columns come back NaN."""
    W: int = eps.shape[0]
    P: int = eps.shape[1]
    d: numpy.ndarray = numpy.diff(eps, axis=0)
    n: int = W - 1 - lags
    if n <= lags + 3:
        return (numpy.full(P, numpy.nan), numpy.full(P, numpy.nan))
    y: numpy.ndarray = d[lags:, :]
    ncol: int = lags + 2
    X: numpy.ndarray = numpy.empty((n, ncol, P), dtype=numpy.float64)
    X[:, 0, :] = eps[lags:W - 1, :]
    k: int = 1
    while k <= lags:
        X[:, k, :] = d[lags - k:W - 1 - k, :]
        k += 1
    X[:, ncol - 1, :] = 1.0
    xtx: numpy.ndarray = numpy.einsum("tkp,tlp->pkl", X, X, optimize=True)
    xty: numpy.ndarray = numpy.einsum("tkp,tp->pk", X, y, optimize=True)
    xtx = xtx + numpy.eye(ncol)[None, :, :] * 1e-12
    try:
        beta: numpy.ndarray = numpy.linalg.solve(xtx, xty[:, :, None])[:, :, 0]
        inv: numpy.ndarray = numpy.linalg.inv(xtx)
    except numpy.linalg.LinAlgError:
        return (numpy.full(P, numpy.nan), numpy.full(P, numpy.nan))
    fit: numpy.ndarray = numpy.einsum("tkp,pk->tp", X, beta, optimize=True)
    resid: numpy.ndarray = y - fit
    dof: int = n - ncol
    s2: numpy.ndarray = (resid * resid).sum(axis=0) / max(dof, 1)
    var_rho: numpy.ndarray = s2 * inv[:, 0, 0]
    with numpy.errstate(invalid="ignore", divide="ignore"):
        t: numpy.ndarray = beta[:, 0] / numpy.sqrt(var_rho)
    return (t, beta[:, 0])


def screen_window(logp: numpy.ndarray, rets: numpy.ndarray, lags: int) -> dict[str, numpy.ndarray]:
    """One formation window: ``logp`` [W, N] log prices, ``rets`` [W-1, N]."""
    N: int = logp.shape[1]
    mu: numpy.ndarray = logp.mean(axis=0)
    c: numpy.ndarray = logp - mu[None, :]
    var: numpy.ndarray = (c * c).sum(axis=0)
    cov: numpy.ndarray = c.T @ c
    with numpy.errstate(invalid="ignore", divide="ignore"):
        beta: numpy.ndarray = cov / var[None, :]
    numpy.fill_diagonal(beta, numpy.nan)
    rc: numpy.ndarray = rets - rets.mean(axis=0)[None, :]
    rsd: numpy.ndarray = numpy.sqrt((rc * rc).sum(axis=0))
    with numpy.errstate(invalid="ignore", divide="ignore"):
        corr: numpy.ndarray = (rc.T @ rc) / (rsd[:, None] * rsd[None, :])
    numpy.fill_diagonal(corr, numpy.nan)
    t_adf: numpy.ndarray = numpy.full((N, N), numpy.nan)
    rho: numpy.ndarray = numpy.full((N, N), numpy.nan)
    sigma: numpy.ndarray = numpy.full((N, N), numpy.nan)
    i: int = 0
    while i < N:
        eps: numpy.ndarray = c[:, i][:, None] - beta[i, :][None, :] * c
        eps[:, i] = numpy.nan
        good: numpy.ndarray = numpy.isfinite(eps).all(axis=0)
        sigma[i, good] = eps[:, good].std(axis=0, ddof=1)
        if good.any():
            tt, rr = batched_adf(eps[:, good], lags)
            t_adf[i, good] = tt
            rho[i, good] = rr
        i += 1
    with numpy.errstate(invalid="ignore", divide="ignore"):
        phi: numpy.ndarray = 1.0 + rho
        halflife: numpy.ndarray = numpy.where(
            (phi > 0.0) & (phi < 1.0), -numpy.log(2.0) / numpy.log(phi), numpy.nan,
        )
    return {"beta": beta, "corr": corr, "t_adf": t_adf, "halflife": halflife, "sigma": sigma}


def fill_short_gaps(close: numpy.ndarray, max_bars: int) -> tuple[numpy.ndarray, list[int]]:
    """A copy of ``close`` [W, N] with every column missing ``1..=max_bars``
    bars filled — each hole takes the previous finite close, a leading hole
    the first finite one. Columns missing more bars are left as they are.
    Returns the filled matrix and the indices of the columns it touched."""
    out: numpy.ndarray = close.copy()
    touched: list[int] = []
    n: int = out.shape[1]
    j: int = 0
    while j < n:
        holes: numpy.ndarray = numpy.flatnonzero(~numpy.isfinite(out[:, j]))
        if 0 < holes.shape[0] <= max_bars:
            finite_idx: numpy.ndarray = numpy.flatnonzero(numpy.isfinite(out[:, j]))
            if finite_idx.shape[0] > 0:
                first: int = int(finite_idx[0])
                for h in holes:
                    src: int = int(h) - 1
                    while src >= 0 and not numpy.isfinite(out[src, j]):
                        src -= 1
                    out[h, j] = out[src, j] if src >= 0 else out[first, j]
                touched.append(j)
        j += 1
    return out, touched


def author(
    close: numpy.ndarray, qv: numpy.ndarray, descriptors: list[str], p: ScreenParams,
    window: tuple[int, int],
) -> AuthorResult:
    """The table from one formation window (``close``/``qv`` [W, N])."""
    skipped: dict[str, list[str]] = {
        "holes": [], "price": [], "liquidity": [], "fewer_than_k": [], "filled": [],
    }
    close, touched = fill_short_gaps(close, p.max_gap_bars)
    skipped["filled"] = [descriptors[j] for j in touched]
    finite: numpy.ndarray = numpy.isfinite(close).all(axis=0)
    with numpy.errstate(invalid="ignore"):
        min_px: numpy.ndarray = numpy.nanmin(
            numpy.where(numpy.isfinite(close), close, numpy.inf), axis=0,
        )
        medqv: numpy.ndarray = numpy.nanmedian(qv, axis=0)
    min_qv_bar: float = p.min_qv_daily_usd / 24.0
    alive: numpy.ndarray = finite.copy()
    j: int = 0
    while j < len(descriptors):
        if not finite[j]:
            skipped["holes"].append(descriptors[j])
        elif not (min_px[j] >= p.min_px_usd):
            skipped["price"].append(descriptors[j])
            alive[j] = False
        elif not (medqv[j] >= min_qv_bar):
            skipped["liquidity"].append(descriptors[j])
            alive[j] = False
        j += 1
    idx: numpy.ndarray = numpy.flatnonzero(alive)
    if idx.shape[0] < MIN_ALIVE:
        return AuthorResult([], [descriptors[int(i)] for i in idx], skipped, window)
    sub: numpy.ndarray = numpy.log(numpy.ascontiguousarray(close[:, idx]))
    rets: numpy.ndarray = numpy.diff(sub, axis=0)
    st: dict[str, numpy.ndarray] = screen_window(sub, rets, p.lags)
    sig_bps: numpy.ndarray = st["sigma"] * 1e4
    ok: numpy.ndarray = (
        numpy.isfinite(st["t_adf"])
        & (numpy.abs(st["corr"]) >= p.min_corr)
        & (st["halflife"] >= p.hl_min_h)
        & (st["halflife"] <= p.hl_max_h)
        & (sig_bps >= p.min_sigma_bps)
        & (numpy.abs(st["beta"]) <= p.max_beta)
        & (numpy.abs(st["beta"]) >= 1.0 / p.max_beta)
    )
    scored: numpy.ndarray = numpy.where(ok, st["t_adf"], numpy.inf)
    rows: list[PairRow] = []
    i: int = 0
    while i < idx.shape[0]:
        order: numpy.ndarray = numpy.argsort(scored[i, :], kind="stable")
        chosen: list[int] = []
        for jj in order[: p.top_k * 3]:
            if len(chosen) >= p.top_k:
                break
            if not numpy.isfinite(scored[i, jj]):
                break
            chosen.append(int(jj))
        target: str = descriptors[int(idx[i])]
        if len(chosen) < p.top_k:
            skipped["fewer_than_k"].append(target)
            i += 1
            continue
        r: int = 1
        for jj in chosen:
            rows.append(PairRow(
                target, descriptors[int(idx[jj])], float(st["beta"][i, jj]), r,
                float(st["t_adf"][i, jj]), float(st["corr"][i, jj]),
                float(st["halflife"][i, jj]), float(st["sigma"][i, jj]),
            ))
            r += 1
        i += 1
    return AuthorResult(rows, [descriptors[int(i)] for i in idx], skipped, window)


# ---------------------------------------------------------------------------
# rendering
# ---------------------------------------------------------------------------

def render_table(res: AuthorResult, p: ScreenParams, universe_n: int) -> str:
    """The engine's grammar plus a fixed header — no timestamp, so the same
    window and universe render the same bytes (the hash is the identity)."""
    lines: list[str] = [
        TABLE_HEADER.rstrip("\n"),
        "# window_ms %d %d  form_h %d  lags %d  top_k %d  min_corr %.2f  hl_h [%.1f, %.1f]  "
        "min_sigma_bps %.1f  min_qv_daily_usd %.0f  beta [1/%.1f, %.1f]  min_px_usd %.4f  "
        "max_gap_bars %d"
        % (res.window[0], res.window[1], p.form_h, p.lags, p.top_k, p.min_corr, p.hl_min_h,
           p.hl_max_h, p.min_sigma_bps, p.min_qv_daily_usd, p.max_beta, p.max_beta, p.min_px_usd,
           p.max_gap_bars),
        "# universe %d  alive %d  targets %d  skipped holes %d price %d liquidity %d"
        "  fewer_than_k %d  gap_filled %d"
        % (universe_n, len(res.alive), len({r.target for r in res.rows}),
           len(res.skipped["holes"]), len(res.skipped["price"]), len(res.skipped["liquidity"]),
           len(res.skipped["fewer_than_k"]), len(res.skipped["filled"])),
    ]
    for r in res.rows:
        lines.append("%s\t%s\t%d\t%d\t%d" % (r.target, r.partner, round(r.beta * 1e9), r.rank,
                                              round(r.t_adf * 1e6)))
    return "\n".join(lines) + "\n"


def seed_rows(
    db: sqlite3.Connection, descriptors: list[str], end_ms: int, hours: int
) -> list[tuple[str, int, int]]:
    """``(descriptor, open_ms, close_1e6)`` for the ``hours`` complete hours
    before ``end_ms`` — closes the engine can use (``> 1`` raw unit)."""
    start_ms: int = end_ms - hours * HOUR_MS
    out: list[tuple[str, int, int]] = []
    for d in descriptors:
        rows = db.execute(
            "select open_ts, c from candles where descriptor = ? and tf = '1h' "
            "and open_ts >= ? and open_ts < ? and c is not null and c > 0 order by open_ts",
            (d, start_ms, end_ms),
        ).fetchall()
        for ts, c in rows:
            close_1e6: int = round(float(c) * 1e6)
            if close_1e6 > 1:
                out.append((d, int(ts), close_1e6))
    return out


def render_seed(rows: list[tuple[str, int, int]]) -> str:
    return SEED_HEADER + "".join("%s\t%d\t%d\n" % r for r in rows)


def write_atomic(path: pathlib.Path, text: str) -> None:
    tmp: pathlib.Path = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(text)
    os.replace(tmp, path)


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def table_age_days(path: pathlib.Path, now_s: float) -> typing.Optional[float]:
    if not path.exists():
        return None
    return (now_s - path.stat().st_mtime) / 86_400.0


def rotation_due(path: pathlib.Path, now_s: float, max_age_days: float = ROTATION_DAYS) -> bool:
    age: typing.Optional[float] = table_age_days(path, now_s)
    return age is None or age >= max_age_days


# ---------------------------------------------------------------------------
# lanes
# ---------------------------------------------------------------------------

def _log(m: str) -> None:
    sys.stderr.write(m + "\n")
    sys.stderr.flush()


def lane_author(args: argparse.Namespace) -> int:
    p: ScreenParams = ScreenParams(
        form_h=args.form_h, lags=args.lags, top_k=args.top_k, min_corr=args.min_corr,
        hl_min_h=args.hl_min_h, hl_max_h=args.hl_max_h, min_sigma_bps=args.min_sigma_bps,
        min_qv_daily_usd=args.min_qv_daily_usd, max_beta=args.max_beta, min_px_usd=args.min_px_usd,
        max_gap_bars=args.max_gap_bars,
    )
    universe: list[str] = read_universe(pathlib.Path(os.path.expanduser(args.universe)))
    if not universe:
        _log("xsd-author: empty universe " + args.universe)
        return 2
    db: sqlite3.Connection = sqlite3.connect(os.path.expanduser(args.db))
    end_ms: int = args.end_ms if args.end_ms else last_complete_hour_end_ms(int(time.time() * 1000))
    t0: float = time.time()
    _, close, qv = load_matrix(db, universe, end_ms, p.form_h)
    res: AuthorResult = author(close, qv, universe, p, (end_ms - p.form_h * HOUR_MS, end_ms))
    text: str = render_table(res, p, len(universe))
    out: pathlib.Path = pathlib.Path(os.path.expanduser(args.out))
    if args.dry_run:
        sys.stdout.write(text)
    elif not res.rows:
        # Never replace a working table with an empty one: the engine
        # would refuse the boot on it. The old file stays; exit 3 says so.
        _log("xsd-author: NO rows — table not written (%s kept)" % out)
    else:
        out.parent.mkdir(parents=True, exist_ok=True)
        write_atomic(out, text)
    _log("xsd-author: universe=%d alive=%d targets=%d rows=%d skipped holes=%d price=%d "
         "liquidity=%d fewer_than_k=%d gap_filled=%d window_end_ms=%d hash=%s out=%s %.1fs"
         % (len(universe), len(res.alive), len({r.target for r in res.rows}), len(res.rows),
            len(res.skipped["holes"]), len(res.skipped["price"]), len(res.skipped["liquidity"]),
            len(res.skipped["fewer_than_k"]), len(res.skipped["filled"]), end_ms,
            sha256_hex(text.encode()), "-" if args.dry_run else str(out), time.time() - t0))
    for law in ("holes", "price", "liquidity", "fewer_than_k", "filled"):
        if res.skipped[law]:
            _log("xsd-author: %s: %s" % (law, ", ".join(res.skipped[law])))
    return 0 if res.rows else 3


def lane_seed_out(args: argparse.Namespace) -> int:
    table: pathlib.Path = pathlib.Path(os.path.expanduser(args.table))
    if not table.exists():
        _log("xsd-author: no table at " + str(table) + " — nothing to seed")
        return 2
    descriptors: list[str] = table_descriptors(table.read_text())
    db: sqlite3.Connection = sqlite3.connect(os.path.expanduser(args.db))
    end_ms: int = args.end_ms if args.end_ms else last_complete_hour_end_ms(int(time.time() * 1000))
    rows: list[tuple[str, int, int]] = seed_rows(db, descriptors, end_ms, args.hours)
    out: pathlib.Path = pathlib.Path(os.path.expanduser(args.out))
    out.parent.mkdir(parents=True, exist_ok=True)
    write_atomic(out, render_seed(rows))
    newest: int = max((r[1] for r in rows), default=0)
    _log("xsd-author: seed-out descriptors=%d rows=%d hours=%d end_ms=%d newest_open_ms=%d out=%s"
         % (len(descriptors), len(rows), args.hours, end_ms, newest, out))
    return 0


def lane_status(args: argparse.Namespace) -> int:
    table: pathlib.Path = pathlib.Path(os.path.expanduser(args.table))
    now_s: float = time.time()
    age: typing.Optional[float] = table_age_days(table, now_s)
    if age is None:
        sys.stdout.write("xsd: table absent at %s — rotation due\n" % table)
        return 0
    text: str = table.read_text()
    descriptors: list[str] = table_descriptors(text)
    rows: int = sum(1 for ln in text.splitlines() if ln.strip() and not ln.startswith("#"))
    sys.stdout.write("xsd: table %s age_days=%.2f rows=%d descriptors=%d hash=%s rotation_due=%s\n"
                     % (table, age, rows, len(descriptors), sha256_hex(text.encode()),
                        "yes" if rotation_due(table, now_s, args.max_age_days) else "no"))
    return 0


def main(argv: typing.Optional[list[str]] = None) -> int:
    ap: argparse.ArgumentParser = argparse.ArgumentParser(prog="claude_worker.xsd_author")
    sub = ap.add_subparsers(dest="lane", required=True)
    a = sub.add_parser("author", help="write xsd-table.tsv from the screen over candles.db")
    a.add_argument("--db", default=DEFAULT_DB)
    a.add_argument("--universe", default=DEFAULT_UNIVERSE)
    a.add_argument("--out", default=DEFAULT_TABLE)
    a.add_argument("--end-ms", type=int, default=0,
                   help="exclusive window end (default: the current hour's open)")
    a.add_argument("--dry-run", action="store_true", help="print the table instead of writing it")
    d: ScreenParams = ScreenParams()
    a.add_argument("--form-h", type=int, default=d.form_h)
    a.add_argument("--lags", type=int, default=d.lags)
    a.add_argument("--top-k", type=int, default=d.top_k)
    a.add_argument("--min-corr", type=float, default=d.min_corr)
    a.add_argument("--hl-min-h", type=float, default=d.hl_min_h)
    a.add_argument("--hl-max-h", type=float, default=d.hl_max_h)
    a.add_argument("--min-sigma-bps", type=float, default=d.min_sigma_bps)
    a.add_argument("--min-qv-daily-usd", type=float, default=d.min_qv_daily_usd)
    a.add_argument("--max-beta", type=float, default=d.max_beta)
    a.add_argument("--min-px-usd", type=float, default=d.min_px_usd)
    a.add_argument("--max-gap-bars", type=int, default=d.max_gap_bars)
    a.set_defaults(fn=lane_author)
    s = sub.add_parser("seed-out", help="write xsd-seed.tsv for the table's descriptors")
    s.add_argument("--db", default=DEFAULT_DB)
    s.add_argument("--table", default=DEFAULT_TABLE)
    s.add_argument("--out", default=DEFAULT_SEED)
    s.add_argument("--hours", type=int, default=DEFAULT_SEED_HOURS)
    s.add_argument("--end-ms", type=int, default=0)
    s.set_defaults(fn=lane_seed_out)
    st = sub.add_parser("status", help="table age / hash / rotation due")
    st.add_argument("--table", default=DEFAULT_TABLE)
    st.add_argument("--max-age-days", type=float, default=ROTATION_DAYS)
    st.set_defaults(fn=lane_status)
    args: argparse.Namespace = ap.parse_args(argv)
    return int(args.fn(args))


if __name__ == "__main__":
    raise SystemExit(main())
