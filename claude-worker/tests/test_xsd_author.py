# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""xsd_author: the screen on a synthetic cointegrated universe, the two laws
the engine forced (price floor, K partners or out), the seed cut, the
rendering the engine parses, and the rotation clock."""

import math
import pathlib
import random
import sqlite3

import numpy
import pytest

import claude_worker.xsd_author as xa

HOUR_MS: int = 3_600_000
END_MS: int = 1_789_171_200_000   # 2026-09-12 00:00:00Z


Series = dict[str, list[float]]


def _make_db(path: pathlib.Path, hours: int = 900) -> tuple[sqlite3.Connection, Series]:
    """Six perps over ``hours`` complete hours ending at END_MS: A/B/C/D share
    a random-walk factor with loadings and AR(1) noise (cointegrated), CHEAP
    trades below $0.005, THIN has no volume, HOLEY misses bars."""
    rng: random.Random = random.Random(4)
    names: list[str] = ["binance-usdm:aaausdt", "binance-usdm:bbbusdt", "binance-usdm:cccusdt",
                        "binance-usdm:dddusdt", "binance-usdm:cheapusdt", "binance-usdm:thinusdt",
                        "binance-usdm:holeyusdt"]
    bases: list[float] = [math.log(50.0), math.log(2.0), math.log(300.0), math.log(0.8),
                          math.log(0.0004), math.log(5.0), math.log(9.0)]
    loads: list[float] = [1.0, 1.1, 0.9, 1.05, 1.0, 1.0, 1.0]
    factor: float = 0.0
    eps: list[float] = [0.0] * len(names)
    series: dict[str, list[float]] = {n: [] for n in names}
    for _ in range(hours):
        factor += rng.gauss(0.0, 0.005)
        for i, n in enumerate(names):
            eps[i] = 0.9 * eps[i] + rng.gauss(0.0, 0.004)
            series[n].append(math.exp(bases[i] + loads[i] * factor + eps[i]))
    db: sqlite3.Connection = sqlite3.connect(path)
    db.execute(
        "CREATE TABLE candles (venue INTEGER NOT NULL, descriptor TEXT NOT NULL, tf TEXT NOT NULL, "
        "open_ts INTEGER NOT NULL, o REAL, h REAL, l REAL, c REAL, v REAL, source TEXT NOT NULL, "
        "fetched_ts INTEGER NOT NULL, n INTEGER, "
        "PRIMARY KEY (venue, descriptor, tf, open_ts)) WITHOUT ROWID"
    )
    start: int = END_MS - hours * HOUR_MS
    for n in names:
        for k, c in enumerate(series[n]):
            if n.endswith("holeyusdt") and k % 50 == 7:
                continue
            vol: float = 0.0 if n.endswith("thinusdt") else 5e6 / c   # ≈ $5M per bar
            db.execute("INSERT INTO candles VALUES (1, ?, '1h', ?, ?, ?, ?, ?, ?, 'rest', 0, 60)",
                       (n, start + k * HOUR_MS, c, c, c, c, vol))
    # A stray in-progress hour AT END_MS must never be read as a close.
    db.execute("INSERT INTO candles VALUES (1, ?, '1h', ?, 1, 1, 1, 999.0, 1, 'rest', 0, 60)",
               ("binance-usdm:aaausdt", END_MS))
    db.commit()
    return db, series


@pytest.fixture()
def synthetic(tmp_path: pathlib.Path) -> tuple[sqlite3.Connection, Series, pathlib.Path]:
    p: pathlib.Path = tmp_path / "candles.db"
    db, series = _make_db(p)
    return db, series, p


def test_read_universe_and_table_descriptors(tmp_path: pathlib.Path) -> None:
    u: pathlib.Path = tmp_path / "u.tsv"
    u.write_text("# c\nbinance-usdm:aaausdt\nbinance-usdm:bbbusdt\textra\n\nbinance-usdm:aaausdt\n")
    assert xa.read_universe(u) == ["binance-usdm:aaausdt", "binance-usdm:bbbusdt"]
    text: str = "# h\na\tb\t1\t1\t-1\na\tc\t2\t2\t-2\nd\tb\t3\t1\t-3\nbad\n"
    assert xa.table_descriptors(text) == ["a", "b", "c", "d"]


def test_load_matrix_shapes_holes_and_never_the_in_progress_hour(synthetic) -> None:
    db, series, _ = synthetic
    descs: list[str] = ["binance-usdm:aaausdt", "binance-usdm:holeyusdt", "binance-usdm:thinusdt"]
    hours_ms, close, qv = xa.load_matrix(db, descs, END_MS, 120)
    assert hours_ms.shape == (120,) and close.shape == (120, 3) and qv.shape == (120, 3)
    assert hours_ms[-1] == END_MS - HOUR_MS
    assert numpy.isfinite(close[:, 0]).all()
    assert close[-1, 0] == pytest.approx(series["binance-usdm:aaausdt"][-1])
    assert not numpy.isfinite(close[:, 1]).all(), "holey has holes"
    assert (qv[:, 2] == 0.0).all(), "thin has zero dollar volume"
    assert close[:, 0].max() < 999.0, "the END_MS row is outside the window"


def test_adf_separates_a_reverting_spread_from_a_random_walk() -> None:
    rng: numpy.random.Generator = numpy.random.default_rng(1)
    n: int = 1500
    ar: numpy.ndarray = numpy.zeros(n)
    for t in range(1, n):
        ar[t] = 0.9 * ar[t - 1] + rng.normal(0.0, 1.0)
    rw: numpy.ndarray = numpy.cumsum(rng.normal(0.0, 1.0, n))
    t_stat, rho = xa.batched_adf(numpy.column_stack([ar, rw]), 1)
    assert t_stat[0] < -3.9, t_stat
    assert t_stat[1] > -3.0, t_stat
    assert -0.2 < rho[0] < -0.05
    # Degenerate input never raises, and a live column beside it still scores.
    t2, _ = xa.batched_adf(numpy.column_stack([numpy.ones(200), ar[:200]]), 1)
    assert numpy.isfinite(t2[1]) and t2[1] < -2.0
    short, _ = xa.batched_adf(ar[:4, None], 1)
    assert not numpy.isfinite(short[0]), "too short ⇒ NaN"


def test_author_applies_the_two_engine_laws_and_ranks_by_adf(synthetic) -> None:
    db, _, _ = synthetic
    descs: list[str] = ["binance-usdm:aaausdt", "binance-usdm:bbbusdt", "binance-usdm:cccusdt",
                        "binance-usdm:dddusdt", "binance-usdm:cheapusdt", "binance-usdm:thinusdt",
                        "binance-usdm:holeyusdt"]
    p: xa.ScreenParams = xa.ScreenParams(form_h=720, min_sigma_bps=1.0, hl_max_h=1000.0)
    _, close, qv = xa.load_matrix(db, descs, END_MS, p.form_h)
    res: xa.AuthorResult = xa.author(close, qv, descs, p, (END_MS - p.form_h * HOUR_MS, END_MS))
    assert res.skipped["price"] == ["binance-usdm:cheapusdt"], "the ≥ $0.005 law"
    assert res.skipped["liquidity"] == ["binance-usdm:thinusdt"]
    assert res.skipped["holes"] == ["binance-usdm:holeyusdt"]
    assert set(res.alive) == set(descs[:4])
    targets: set[str] = {r.target for r in res.rows}
    assert targets == set(res.alive), "four cointegrated names, three partners each"
    for t in targets:
        mine: list[xa.PairRow] = [r for r in res.rows if r.target == t]
        assert [r.rank for r in mine] == [1, 2, 3]
        assert mine[0].t_adf <= mine[1].t_adf <= mine[2].t_adf, "ranked by the most negative ADF"
        assert all(r.t_adf < -3.0 for r in mine)
        assert all(0.25 <= abs(r.beta) <= 4.0 for r in mine)
        assert t not in {r.partner for r in mine}
    # β recovers the loading ratio within the AR noise: A on B ≈ 1.0 / 1.1.
    ab: xa.PairRow = next(r for r in res.rows if r.target == descs[0] and r.partner == descs[1])
    assert 0.8 < ab.beta < 1.05, ab
    # Fewer than K admissible partners ⇒ the target is OUT (K = 3 needs 3 others).
    res3: xa.AuthorResult = xa.author(close[:, :3], qv[:, :3], descs[:3], p, (0, 0))
    assert res3.rows == [] and set(res3.skipped["fewer_than_k"]) == set(descs[:3])


def test_short_gaps_are_filled_and_long_ones_stay_holes(synthetic) -> None:
    db, _, _ = synthetic
    descs: list[str] = ["binance-usdm:aaausdt", "binance-usdm:bbbusdt", "binance-usdm:cccusdt",
                        "binance-usdm:dddusdt", "binance-usdm:holeyusdt"]
    p: xa.ScreenParams = xa.ScreenParams(form_h=720, min_sigma_bps=1.0, hl_max_h=1000.0)
    _, close, qv = xa.load_matrix(db, descs, END_MS, p.form_h)
    # The live artefact: a leading hole (the backfill started one hour late)
    # and one hole in the middle — both within the 3-bar tolerance.
    close[0, 0] = numpy.nan
    close[300, 1] = numpy.nan
    close[301, 1] = numpy.nan
    filled, touched = xa.fill_short_gaps(close, p.max_gap_bars)
    assert touched == [0, 1]
    assert filled[0, 0] == close[1, 0], "a leading hole takes the first finite close"
    assert filled[300, 1] == close[299, 1] and filled[301, 1] == close[299, 1], "forward-filled"
    assert numpy.isfinite(filled[:, :4]).all()
    assert not numpy.isfinite(filled[:, 4]).all(), "14 holes in 720 h stay holes"
    assert numpy.isnan(close[0, 0]), "the input is never mutated"
    res: xa.AuthorResult = xa.author(close, qv, descs, p, (END_MS - p.form_h * HOUR_MS, END_MS))
    assert res.skipped["filled"] == ["binance-usdm:aaausdt", "binance-usdm:bbbusdt"]
    assert res.skipped["holes"] == ["binance-usdm:holeyusdt"]
    assert {r.target for r in res.rows} == set(descs[:4])
    strict: xa.AuthorResult = xa.author(close, qv, descs, p._replace(max_gap_bars=0), res.window)
    assert set(strict.skipped["holes"]) == {"binance-usdm:aaausdt", "binance-usdm:bbbusdt",
                                            "binance-usdm:holeyusdt"}
    assert "gap_filled 2" in xa.render_table(res, p, len(descs))


def test_render_table_is_the_engine_grammar_and_deterministic(synthetic) -> None:
    db, _, _ = synthetic
    descs: list[str] = ["binance-usdm:aaausdt", "binance-usdm:bbbusdt", "binance-usdm:cccusdt",
                        "binance-usdm:dddusdt"]
    p: xa.ScreenParams = xa.ScreenParams(form_h=720, min_sigma_bps=1.0, hl_max_h=1000.0)
    _, close, qv = xa.load_matrix(db, descs, END_MS, p.form_h)
    res: xa.AuthorResult = xa.author(close, qv, descs, p, (END_MS - p.form_h * HOUR_MS, END_MS))
    text: str = xa.render_table(res, p, len(descs))
    again: str = xa.render_table(xa.author(close, qv, descs, p, res.window), p, len(descs))
    assert text == again, "same window ⇒ same bytes ⇒ same table hash"
    body: list[str] = [ln for ln in text.splitlines() if ln and not ln.startswith("#")]
    assert len(body) == 12
    f: list[str] = body[0].split("\t")
    assert len(f) == 5 and f[0].startswith("binance-usdm:") and f[3] == "1"
    int(f[2])
    int(f[4])
    assert xa.table_descriptors(text) == descs or set(xa.table_descriptors(text)) == set(descs)


def test_seed_rows_cut_complete_hours_in_the_engine_shape(synthetic) -> None:
    db, series, _ = synthetic
    rows: list[tuple[str, int, int]] = xa.seed_rows(
        db, ["binance-usdm:aaausdt", "binance-usdm:holeyusdt"], END_MS, 100)
    a: list[tuple[str, int, int]] = [r for r in rows if r[0] == "binance-usdm:aaausdt"]
    h: list[tuple[str, int, int]] = [r for r in rows if r[0] == "binance-usdm:holeyusdt"]
    assert len(a) == 100 and len(h) == 98
    assert a[-1][1] == END_MS - HOUR_MS, "ends at the last COMPLETE hour"
    assert a[-1][2] == round(series["binance-usdm:aaausdt"][-1] * 1e6)
    assert all(r[2] > 1 for r in rows)
    text: str = xa.render_seed(rows)
    assert text.startswith("# xsd-seed.tsv")
    line: str = text.splitlines()[1]
    d, ts, c = line.split("\t")
    assert d == "binance-usdm:aaausdt" and int(ts) % HOUR_MS == 0 and int(c) > 1


def test_rotation_clock(tmp_path: pathlib.Path) -> None:
    t: pathlib.Path = tmp_path / "xsd-table.tsv"
    assert xa.rotation_due(t, 1_000.0)
    t.write_text("# x\n")
    now: float = t.stat().st_mtime
    assert not xa.rotation_due(t, now + 29 * 86_400)
    assert xa.rotation_due(t, now + 30 * 86_400)
    assert xa.table_age_days(t, now + 86_400) == pytest.approx(1.0)


def test_lanes_end_to_end(
    synthetic, tmp_path: pathlib.Path, capsys: pytest.CaptureFixture[str],
) -> None:
    _, _, dbp = synthetic
    universe: pathlib.Path = tmp_path / "xsd-universe.tsv"
    universe.write_text(
        "binance-usdm:aaausdt\nbinance-usdm:bbbusdt\nbinance-usdm:cccusdt\nbinance-usdm:dddusdt\n"
        "binance-usdm:cheapusdt\nbinance-usdm:thinusdt\nbinance-usdm:holeyusdt\n")
    knobs: list[str] = ["--end-ms", str(END_MS), "--form-h", "720", "--min-sigma-bps", "1",
                        "--hl-max-h", "1000"]
    table: pathlib.Path = tmp_path / "xsd-table.tsv"
    seed: pathlib.Path = tmp_path / "xsd-seed.tsv"
    rc: int = xa.main(["author", "--db", str(dbp), "--universe", str(universe), "--out", str(table),
                       *knobs])
    assert rc == 0 and table.exists()
    err: str = capsys.readouterr().err
    assert "xsd-author: universe=7 alive=4 targets=4 rows=12" in err
    assert "xsd-author: price: binance-usdm:cheapusdt" in err
    assert "xsd-author: holes: binance-usdm:holeyusdt" in err
    rc = xa.main(["seed-out", "--db", str(dbp), "--table", str(table), "--out", str(seed),
                  "--end-ms", str(END_MS), "--hours", "50"])
    assert rc == 0
    body: list[str] = [ln for ln in seed.read_text().splitlines() if not ln.startswith("#")]
    assert len(body) == 4 * 50
    rc = xa.main(["status", "--table", str(table)])
    assert rc == 0
    out: str = capsys.readouterr().out
    assert "rows=12 descriptors=4" in out and "rotation_due=no" in out
    # A dry run prints and writes nothing; an empty universe is a usage error.
    rc = xa.main(["author", "--db", str(dbp), "--universe", str(universe),
                  "--out", str(tmp_path / "never.tsv"), "--dry-run", *knobs])
    assert rc == 0 and not (tmp_path / "never.tsv").exists()
    assert capsys.readouterr().out.startswith("# xsd-table.tsv")
    (tmp_path / "empty.tsv").write_text("# nothing\n")
    assert xa.main(["author", "--db", str(dbp), "--universe", str(tmp_path / "empty.tsv"),
                    "--out", str(table)]) == 2
    # A universe that yields no rows leaves the existing table untouched (exit 3).
    before: str = table.read_text()
    thin: pathlib.Path = tmp_path / "thin.tsv"
    thin.write_text("binance-usdm:thinusdt\nbinance-usdm:cheapusdt\n")
    assert xa.main(["author", "--db", str(dbp), "--universe", str(thin), "--out", str(table),
                    "--end-ms", str(END_MS), "--form-h", "720"]) == 3
    assert table.read_text() == before
