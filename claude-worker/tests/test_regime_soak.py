# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""RG7 — the ≤ 2 h-law soak judge + the seed-hole fix
(docs/regime-and-dashboard-plan.md §7.1).

The engine's ``/state`` regime block becomes a history sample (pid,
cumulative flips, minutes judged, effective words); ``judge_window``
counts flips per profile x market dimension from the engine's counters
(same pid) or the worker mirror, applies the bound / coverage / gating
laws; ``run_soak`` pools the windows and never waits; ``refresh_tail``
gap-fills the artifact's own 1 m candles before ``seed-out`` so the
seed reaches the boot minute. No live engine, no network (fake Http).

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import json
import pathlib

import claude_worker.frames
import claude_worker.regime
import tests.regime_fixture
import tests.test_candles

NOW_MS = tests.regime_fixture.NOW_MS
MS_5M = 300_000
WINDOW_MS = 2 * 3_600_000
DEAD_URL = "http://127.0.0.1:9/state"

_word = claude_worker.frames.regime_word


def _state(pid: int, flips_fast: list[int], flips_slow: list[int], **over: object) -> dict:
    """A minimal ``/state`` document with the regime block the sample reads."""
    doc: dict = {
        "v": 1,
        "seq": 7,
        "boot": {"pid": pid},
        "regime": {
            "configured": 1,
            "minutes_judged": 100,
            "profiles": [
                {
                    "name": "fast",
                    "flips": [*flips_fast, 0, 0],
                    "effective": {"hex": "0001028080010102"},
                },
                {
                    "name": "slow",
                    "flips": [*flips_slow, 0, 0],
                    "effective": {"hex": "0001028080010102"},
                },
            ],
        },
        "vm": {"rows_active": 2, "regime_hard_exits": 0},
    }
    for k, v in over.items():
        section, _, key = k.partition("__")
        doc[section][key] = v
    return doc


def _entry(ts_ms: int, engine: dict | None, fast: int, slow: int) -> dict:
    e: dict = {
        "ts_ms": ts_ms,
        "minute": ts_ms // 60_000,
        "age_min": 0,
        "rows": 1,
        "fast": claude_worker.regime.word_hex(fast),
        "slow": claude_worker.regime.word_hex(slow),
    }
    if engine is not None:
        e["engine"] = engine
    return e


def _samples(  # noqa: PLR0913, PLR0917 — one knob per sample shape, deliberately
    start_ms: int,
    n: int,
    pid: int = 100,
    fast_flip_at: int | None = None,
    fast_flip_dim: int = 2,
    flips_per_sample: int = 0,
    mirror_words: list[int] | None = None,
) -> list[dict]:
    """``n`` 5-min samples from ``start_ms``: engine flips grow by
    ``flips_per_sample`` on ``fast_flip_dim`` (one extra flip at
    ``fast_flip_at``); the mirror words cycle through ``mirror_words``."""
    out: list[dict] = []
    fast_flips = [0] * 6
    for i in range(n):
        ts = start_ms + i * MS_5M
        if i == fast_flip_at:
            fast_flips[fast_flip_dim] += 1
        fast_flips[fast_flip_dim] += flips_per_sample
        sample = claude_worker.regime.engine_regime_sample(
            _state(pid, list(fast_flips[:4]), [0, 0, 0, 0])
        )
        assert sample is not None
        words = mirror_words or [_word(trend="bull", shape="trend", vol="low", source="measured")]
        w = words[i % len(words)]
        out.append(_entry(ts, sample, w, w))
    return out


def test_engine_regime_sample_reads_the_state_block() -> None:
    doc = _state(4242, [1, 2, 3, 4], [5, 6, 7, 8], vm__regime_hard_exits=3)
    sample = claude_worker.regime.engine_regime_sample(doc)
    assert sample == {
        "pid": 4242,
        "seq": 7,
        "configured": 1,
        "minutes_judged": 100,
        "flips": {"fast": [1, 2, 3, 4, 0, 0], "slow": [5, 6, 7, 8, 0, 0]},
        "effective": {"fast": "0001028080010102", "slow": "0001028080010102"},
        "vm_rows": 2,
        "hard_exits": 3,
    }
    # A document without the regime block is not a sample.
    assert claude_worker.regime.engine_regime_sample({"v": 1, "boot": {"pid": 1}}) is None
    assert claude_worker.regime.engine_regime_sample({"v": 1, "regime": {}, "boot": {}}) is None
    # The state URL sits beside the metrics URL.
    assert claude_worker.regime.state_url({"CLAUDE_WORKER_METRICS_URL": "http://h:1/metrics"}) == (
        "http://h:1/state"
    )
    assert claude_worker.regime.engine_state(DEAD_URL) is None


def test_cycle_records_the_engine_sample_when_state_answers(tmp_path, monkeypatch) -> None:
    art = tests.regime_fixture.artifact(tmp_path)
    db = tests.regime_fixture.candles_db(tmp_path)
    directory = tmp_path / "regime"
    lines: list[str] = []
    # Dead engine: the line says so, no `engine` key.
    m = claude_worker.regime.run_cycle(
        art, db, directory, NOW_MS, lines.append, refresh_daily=False, engine_url=DEAD_URL
    )
    assert m is not None and "engine=unreachable" in lines[-1]
    tail = claude_worker.regime.history_tail(directory, NOW_MS)
    assert len(tail) == 1 and "engine" not in tail[0]
    # Live engine (mocked at the fetch): the sample rides the history line.
    monkeypatch.setattr(
        claude_worker.regime,
        "engine_state",
        lambda url, timeout_s=2.0: _state(75758, [0, 1, 0, 0], [0, 0, 0, 0]),
    )
    claude_worker.regime.run_cycle(
        art, db, directory, NOW_MS + MS_5M, lines.append, refresh_daily=False
    )
    assert "engine pid=75758 judged=100" in lines[-1]
    tail = claude_worker.regime.history_tail(directory, NOW_MS + MS_5M)
    assert tail[1]["engine"]["flips"]["fast"] == [0, 1, 0, 0, 0, 0]


def test_judge_window_engine_counters_pass_fail_mirror_short_ungated(tmp_path) -> None:
    start = NOW_MS
    w = claude_worker.regime.SoakWindow("run-1", start, start + WINDOW_MS)
    # 24 samples, one flip on vol inside the window (and a baseline sample
    # before the window with the same pid) ⇒ PASS from the engine.
    before = _samples(start - MS_5M, 1)
    inside = _samples(start, 24, fast_flip_at=5)
    v = claude_worker.regime.judge_window(before + inside, w, None)
    assert v.verdict == "PASS" and v.source == "engine" and v.samples == 24
    assert v.flips["fast"] == [0, 0, 1, 0, 0, 0] and v.worst == ("fast", "vol", 1)
    assert v.pnl_regime is False  # no reports dir given
    # Three flips on one dim ⇒ FAIL (bound is 2 per window).
    inside = _samples(start, 24, fast_flip_at=3, flips_per_sample=0)
    inside[10]["engine"]["flips"]["fast"][2] += 2
    for e in inside[11:]:
        e["engine"]["flips"]["fast"][2] += 2
    v = claude_worker.regime.judge_window(inside, w, None)
    assert v.verdict == "FAIL" and v.worst == ("fast", "vol", 3)
    # A restart inside the window ⇒ the mirror judges (5-min word changes).
    a = _samples(start, 12, pid=1)
    b = _samples(start + 12 * MS_5M, 12, pid=2)
    v = claude_worker.regime.judge_window(a + b, w, None)
    assert v.source == "mirror" and v.verdict == "PASS" and v.flips["fast"] == [0] * 6
    # Mirror flips: alternating words flip the mirror every sample ⇒ FAIL.
    words = [
        _word(trend="bull", shape="trend", vol="low", source="measured"),
        _word(trend="bear", shape="trend", vol="low", source="measured"),
    ]
    a = _samples(start, 12, pid=1, mirror_words=words)
    b = _samples(start + 12 * MS_5M, 12, pid=2, mirror_words=words)
    v = claude_worker.regime.judge_window(a + b, w, None)
    assert v.source == "mirror" and v.verdict == "FAIL" and v.worst[1] == "trend"
    # Too few samples ⇒ short (not counted).
    v = claude_worker.regime.judge_window(_samples(start, 5), w, None)
    assert v.verdict == "short"
    # Gating not live at one sample (no table) ⇒ ungated.
    inside = _samples(start, 24)
    inside[7]["engine"]["vm_rows"] = 0
    v = claude_worker.regime.judge_window(inside, w, None)
    assert v.verdict == "ungated"
    # No samples at all ⇒ short with source none.
    v = claude_worker.regime.judge_window([], w, None)
    assert v.verdict == "short" and v.source == "none"


def _pnl_report(reports: pathlib.Path, day: str, with_rows: bool) -> None:
    strategies = (
        [{"strategy_id": 5, "label": "vm", "fills": 3, "net_usd": "1.0"}] if with_rows else []
    )
    obj = {
        "audit_pnl_version": 1,
        "day": day,
        "regime": {
            "modes": {},
            "profiles": [
                {
                    "profile": "fast",
                    "words": [{"word": "x", "bits": "00", "minutes": 5, "strategies": strategies}],
                },
                {
                    "profile": "slow",
                    "words": [{"word": "x", "bits": "00", "minutes": 5, "strategies": strategies}],
                },
            ],
        },
    }
    reports.mkdir(parents=True, exist_ok=True)
    (reports / f"pnl-{day}.json").write_text(json.dumps(obj), encoding="utf-8")


def test_run_soak_pools_windows_and_never_waits(tmp_path, capsys) -> None:
    directory = tmp_path / "regime"
    pool = tmp_path / "windows"
    reports = tmp_path / "reports"
    # Eight complete windows back to back, all sampled with a live engine.
    base = NOW_MS - 8 * WINDOW_MS
    entries: list[dict] = []
    for k in range(8):
        (pool / f"run-{(base + k * WINDOW_MS) * 1_000_000}").mkdir(parents=True)
        entries += _samples(base + k * WINDOW_MS, 24)
    for e in entries:
        claude_worker.regime.append_history(directory, e)
    day = datetime.datetime.fromtimestamp(base / 1000, tz=datetime.timezone.utc).strftime(
        "%Y-%m-%d"
    )
    _pnl_report(reports, day, with_rows=True)
    lines: list[str] = []
    windows = claude_worker.regime.soak_windows_from_pool(pool)
    assert len(windows) == 8 and windows[0].end_ms - windows[0].start_ms == WINDOW_MS
    out = claude_worker.regime.run_soak(directory, windows, reports, NOW_MS, lines.append)
    assert out["verdict"] == "PASS" and out["counted"] == 8 and out["failed"] == 0
    assert len(out["windows"]) == 8 and all(w["verdict"] == "PASS" for w in out["windows"])
    assert any(w["pnl_regime"] for w in out["windows"]), "the day report's per-regime rows count"
    assert lines[-1].startswith("soak verdict: PASS (windows 8, counted 8")
    path = pathlib.Path(str(out["path"]))
    assert path.is_file() and json.loads(path.read_text(encoding="utf-8"))["verdict"] == "PASS"
    # A pool short of windows is INSUFFICIENT — never a wait.
    (pool / f"run-{(base - WINDOW_MS) * 1_000_000}").mkdir()  # an unsampled (short) window
    windows = claude_worker.regime.soak_windows_from_pool(pool)
    out = claude_worker.regime.run_soak(
        directory, windows, reports, NOW_MS, lines.append, min_windows=9
    )
    assert out["verdict"] == "INSUFFICIENT" and out["counted"] == 8
    assert any(w["verdict"] == "short" for w in out["windows"])
    # Reports without fill-model rows ⇒ pnl_regime false; a missing report too.
    _pnl_report(reports, day, with_rows=False)
    out = claude_worker.regime.run_soak(directory, windows, reports, NOW_MS, lines.append)
    assert not any(w["pnl_regime"] for w in out["windows"])
    assert claude_worker.regime.pnl_regime_present(reports, "1999-01-01") is False
    # The lane: PASS ⇒ 0, anything else ⇒ 3.
    common = [
        "--dir",
        str(directory),
        "--now-ms",
        str(NOW_MS),
        "--pool",
        str(pool),
        "--reports-dir",
        str(reports),
    ]
    assert claude_worker.regime.main(["soak", *common]) == 0
    assert "soak verdict: PASS" in capsys.readouterr().out
    assert claude_worker.regime.main(["soak", "--min-windows", "9", *common]) == 3
    # Without --pool the windows come from the runs themselves (no cut
    # needed): an empty replay root has none ⇒ INSUFFICIENT.
    empty = tmp_path / "logs"
    empty.mkdir()
    assert claude_worker.regime.soak_windows_from_runs(empty, 0) == []
    code = claude_worker.regime.main(
        [
            "soak",
            "--dir",
            str(directory),
            "--now-ms",
            str(NOW_MS),
            "--replay-dir",
            str(empty),
            "--reports-dir",
            str(reports),
        ]
    )
    assert code == 3 and "INSUFFICIENT (windows 0" in capsys.readouterr().out


def test_refresh_tail_gap_fills_the_artifacts_descriptors(tmp_path, capsys) -> None:
    art = tests.regime_fixture.artifact(tmp_path)
    db = tests.regime_fixture.candles_db(tmp_path, lag_min=60)  # the store lags the boot by 1 h
    universe = tmp_path / "universe.toml"
    members = ", ".join(f'"{m.split(":")[1]}"' for m in tests.regime_fixture.MEMBERS)
    universe.write_text(
        f'[binance]\nspot = ["btcusdt"]\nusdm = ["btcusdt", {members}, "unrelated"]\n',
        encoding="utf-8",
    )
    open_ts = (NOW_MS // 60_000) * 60_000
    bars = [
        tests.test_candles.mk_candle(open_ts - i * 60_000, base=100.0 + i)
        for i in range(70, -1, -1)
    ]
    venue = tests.test_candles.ForwardVenue(bars, page=100)
    http = tests.test_candles.http_forward(venue)
    lines: list[str] = []
    touched = claude_worker.regime.refresh_tail(
        art, db, universe, NOW_MS, {}, lines.append, http=http
    )
    # ref + 4 members = 5 usdm descriptors; `binance:btcusdt` (spot) and
    # `unrelated` are not the artifact's.
    assert touched == 5 and len(venue.calls) == 5
    assert all("1m pages=1" in line for line in lines)
    # The seed now reaches the boot minute (no hole).
    out = tmp_path / "seed.tsv"
    n_desc, n_rows = claude_worker.regime.seed_out(art, db, out, 120, NOW_MS)
    last = max(
        int(line.split("\t")[1])
        for line in out.read_text().splitlines()
        if not line.startswith("#")
    )
    assert last == NOW_MS // 60_000 - 1 and n_desc == 5 and n_rows > 0
    # No matching descriptor ⇒ 0 touched, honest line, seed still written.
    (tmp_path / "u2.toml").write_text('[binance]\nusdm = ["unrelated"]\n', encoding="utf-8")
    assert (
        claude_worker.regime.refresh_tail(
            art, db, tmp_path / "u2.toml", NOW_MS, {}, lines.append, http=http
        )
        == 0
    )
    assert "skipped" in lines[-1]
    code = claude_worker.regime.main(
        [
            "seed-out",
            "--regime",
            str(art),
            "--db",
            str(db),
            "--out",
            str(out),
            "--refresh-tail",
            "--universe",
            str(tmp_path / "u2.toml"),
            "--now-ms",
            str(NOW_MS),
        ]
    )
    assert code == 0
    captured = capsys.readouterr()
    assert (
        "tail refreshed for 0 descriptors" in captured.err
        and "rows for 5 descriptors" in captured.out
    )
