# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §9.6 — resolutions against the tape, and the scorecard.

This file is the lane's own falsifier. Everything else in the package is
machinery for producing claims; this is the part that can say the claims
were worthless, and the tests below are mostly about making sure it CAN
say that:

* a tie is a MISS, because rounding a zero move into a win is how a null
  becomes a result;
* the base rate sits beside every hit rate, so "up was usually right in
  an uptrend" cannot masquerade as edge;
* `by_model` never pools the session brain with the automated one;
* a missing bar leaves the row PENDING rather than resolving it to zero
  — the 1 m lane is gap-filled hourly, so waiting is correct;
* intents are APPROXIMATE and say so in the file.

Prices come from a tmp `candles.db` built here with the real schema, so
these exercise the actual SQL.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import sqlite3
import typing

import claude_worker.frames
import claude_worker.news
import claude_worker.news.resolve
import claude_worker.news.store

NOW: int = 1_789_826_400
MINUTE: int = 60
DESCRIPTOR: str = "binance-usdm:btcusdt"
VENUE: int = claude_worker.frames.VENUE_BINANCE


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


def _paths(tmp_path: pathlib.Path) -> claude_worker.news.NewsPaths:
    return claude_worker.news.paths_from_env(
        {
            "CLAUDE_WORKER_NEWS_DIR": str(tmp_path / "worker" / "news"),
            "CLAUDE_WORKER_DB": str(tmp_path / "worker" / "state.db"),
            "CLAUDE_WORKER_CANDLES_DB": str(tmp_path / "worker" / "candles.db"),
            "CLAUDE_WORKER_REPLAY_DIR": str(tmp_path / "logs"),
            "CLAUDE_WORKER_MULTIVENUE_DIR": str(tmp_path),
        }
    )


def _candles(tmp_path: pathlib.Path, bars: list[tuple[int, float, float, float]]) -> None:
    """``(open_ts_s, low, high, close)`` written with the real schema."""
    path = tmp_path / "worker" / "candles.db"
    path.parent.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(str(path))
    conn.execute(
        """
        CREATE TABLE IF NOT EXISTS candles (
            venue INTEGER, descriptor TEXT, tf TEXT, open_ts INTEGER,
            o REAL, h REAL, l REAL, c REAL, v REAL, n INTEGER,
            source TEXT, fetched_ts INTEGER,
            PRIMARY KEY (venue, descriptor, tf, open_ts))
        """
    )
    for open_ts, low, high, close in bars:
        conn.execute(
            "INSERT OR REPLACE INTO candles VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            (VENUE, DESCRIPTOR, "1m", open_ts * 1000, close, high, low, close, 0.0, 1, "t", 0),
        )
    conn.commit()
    conn.close()


def _flat_then(move_bps: float, bars: int = 40) -> list[tuple[int, float, float, float]]:
    """A flat run before t0 and a single move by t1, so the direction and
    the RV ratio are both well defined."""
    out: list[tuple[int, float, float, float]] = []
    for i in range(bars):
        ts = NOW - (bars - i) * MINUTE
        out.append((ts, 100.0, 100.0, 100.0))
    out.append((NOW, 100.0, 100.0, 100.0))
    end = 100.0 * (1.0 + move_bps / 1e4)
    for i in range(1, 11):
        out.append((NOW + i * MINUTE, min(100.0, end), max(100.0, end), end))
    return out


def _open_label(
    store: claude_worker.news.store.Store, direction: str, **over: object
) -> None:
    subject_id = str(over.pop("subject_id", "s1"))
    fields: dict[str, object] = {
        "t0": NOW,
        "horizon_s": 600,
        "descriptor": DESCRIPTOR,
        "venue": VENUE,
        "direction": direction,
        "confidence": 0.8,
    }
    fields.update(over)
    claude_worker.news.resolve.open_resolution(
        store, claude_worker.news.resolve.SUBJECT_LABEL, subject_id, **fields  # type: ignore[arg-type]
    )


# ---- opening -------------------------------------------------------------


def test_horizon_clamped(tmp_path: pathlib.Path) -> None:
    """Under a minute there is no bar; over a day the claim is about
    something else."""
    with _store(tmp_path) as store:
        _open_label(store, "up", subject_id="tiny", horizon_s=1)
        _open_label(store, "up", subject_id="huge", horizon_s=10**9)
        rows = {str(r["subject_id"]): r for r in store.resolutions_due(NOW + 10**9)}
        assert rows["tiny"]["horizon_s"] == claude_worker.news.resolve.HORIZON_MIN_S
        assert rows["huge"]["horizon_s"] == claude_worker.news.resolve.HORIZON_MAX_S


def test_label_opens_resolution_with_descriptor(tmp_path: pathlib.Path) -> None:
    """The descriptor is copied from label time — a SymbolId is
    meaningless across boots and the descriptor is not."""
    with _store(tmp_path) as store:
        _open_label(store, "down")
        row = store.resolutions_due(NOW + 3600)[0]
        assert row["descriptor"] == DESCRIPTOR
        assert row["venue"] == VENUE
        assert row["state"] == claude_worker.news.resolve.STATE_PENDING
        assert row["t1"] == NOW + 600


# ---- resolving -----------------------------------------------------------


def test_direction_hit_and_signed_bps(tmp_path: pathlib.Path) -> None:
    _candles(tmp_path, _flat_then(50.0))
    with _store(tmp_path) as store:
        _open_label(store, "up", subject_id="right")
        _open_label(store, "down", subject_id="wrong")
        stats = claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        assert stats.resolved == 2
        rows = {
            str(r["subject_id"]): r
            for r in store.resolutions_resolved(0)
        }
        assert int(typing.cast(int, rows["right"]["hit"])) == 1
        assert int(typing.cast(int, rows["wrong"]["hit"])) == 0
        assert round(float(typing.cast(float, rows["right"]["fwd_bps"]))) == 50
        assert round(float(typing.cast(float, rows["right"]["signed_bps"]))) == 50
        assert round(float(typing.cast(float, rows["wrong"]["signed_bps"]))) == -50
        assert rows["right"]["price_source"] == claude_worker.news.resolve.PRICE_CANDLES


def test_a_tie_is_a_miss(tmp_path: pathlib.Path) -> None:
    """Rounding a zero move into a win is how a null becomes a result."""
    _candles(tmp_path, _flat_then(0.0))
    with _store(tmp_path) as store:
        _open_label(store, "up")
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        row = store.resolutions_resolved(0)[0]
        assert float(typing.cast(float, row["fwd_bps"])) == 0.0
        assert int(typing.cast(int, row["hit"])) == 0


def test_resolve_waits_for_missing_bar_then_unresolvable_after_48h(
    tmp_path: pathlib.Path,
) -> None:
    """A missing bar is WAITED for, not resolved to zero: the 1 m lane is
    gap-filled hourly, so a gap now is usually a bar later."""
    with _store(tmp_path) as store:
        _open_label(store, "up")
        paths = _paths(tmp_path)
        early = claude_worker.news.resolve.resolve_due(store, paths, NOW + 3600)
        assert (early.resolved, early.still_pending) == (0, 1)
        assert store.resolutions_due(NOW + 3600)[0]["state"] == "pending"
        late = claude_worker.news.resolve.resolve_due(
            store, paths, NOW + claude_worker.news.resolve.GIVE_UP_S + 3600
        )
        assert late.unresolvable == 1
        reasons = store.unresolvable_reasons()
        assert reasons and DESCRIPTOR in reasons[0][0]


def test_direction_none_excluded_from_stats(tmp_path: pathlib.Path) -> None:
    """A directionless label is a vol claim, not a coin flip — it is
    resolved and recorded, and excluded from the direction numbers."""
    _candles(tmp_path, _flat_then(80.0))
    with _store(tmp_path) as store:
        _open_label(store, "none", subject_id="quiet", vol_claimed=1)
        _open_label(store, "up", subject_id="loud")
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        card = claude_worker.news.resolve.build_scorecard(store, NOW + 3600)
        direction = typing.cast(
            dict[str, object],
            typing.cast(dict[str, object], card["windows"])["all"],
        )["direction"]
        assert typing.cast(dict[str, object], direction)["n"] == 1, "only the signed one"
        # ...but the base rate counts BOTH, because it is direction-agnostic.
        assert typing.cast(dict[str, object], direction)["base_rate"] == 1.0


def test_rv_ratio_needs_10_bars(tmp_path: pathlib.Path) -> None:
    _candles(tmp_path, [(NOW - MINUTE, 100.0, 100.0, 100.0), (NOW + 600, 101.0, 101.0, 101.0)])
    with _store(tmp_path) as store:
        _open_label(store, "up")
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        row = store.resolutions_resolved(0)[0]
        assert float(typing.cast(float, row["rv_ratio"])) == 0.0, "too few bars to be a ratio"


def test_vol_reached_high_from_history_engine_word_preferred(tmp_path: pathlib.Path) -> None:
    """The engine's EFFECTIVE word is preferred over the worker's measured
    one, because that is what actually gated the strategies — and which
    one answered is recorded, so the number cannot quietly change meaning."""
    paths = _paths(tmp_path)
    paths.regime_dir.mkdir(parents=True, exist_ok=True)
    high = claude_worker.frames.regime_word(vol="high")
    normal = claude_worker.frames.regime_word(vol="normal")
    (paths.regime_dir / "history.ndjson").write_text(
        "\n".join(
            [
                json.dumps({"ts_ms": (NOW + 60) * 1000, "fast": f"{normal:x}"}),
                json.dumps(
                    {
                        "ts_ms": (NOW + 120) * 1000,
                        "fast": f"{normal:x}",
                        "engine": {"fast_effective": f"{high:x}"},
                    }
                ),
            ]
        )
        + "\n",
        encoding="utf-8",
    )
    reached, which = claude_worker.news.resolve.reached_vol_high(
        paths.regime_dir, NOW, NOW + 600
    )
    assert reached == 1
    assert which == "engine_effective"
    # Outside the window it does not count.
    assert claude_worker.news.resolve.reached_vol_high(paths.regime_dir, NOW + 300, NOW + 400) == (
        0,
        "none",
    )


def test_intent_fill_strict_cross_candle_proxy(tmp_path: pathlib.Path) -> None:
    """A BID fills only if a bar trades BELOW it. Conservative on purpose:
    the harness fill model is the authority and the N4 tool re-judges."""
    _candles(
        tmp_path,
        [
            (NOW, 99.0, 100.5, 100.0),
            (NOW + MINUTE, 98.0, 100.0, 99.5),
            (NOW + 600, 99.0, 100.0, 99.0),
        ],
    )
    paths = _paths(tmp_path)
    conn = sqlite3.connect(str(paths.candles_db_path))
    row: dict[str, object] = {
        "descriptor": DESCRIPTOR, "venue": VENUE, "t0": NOW, "t1": NOW + 600,
    }
    try:
        filled, markout = claude_worker.news.resolve.intent_fill(
            conn, row, 98_500_000, claude_worker.frames.SIDE_BID
        )
        assert filled == 1, "a bar traded through 98.5"
        assert markout > 0, "and the close came back above it"
        missed, _ = claude_worker.news.resolve.intent_fill(
            conn, row, 90_000_000, claude_worker.frames.SIDE_BID
        )
        assert missed == 0, "nothing traded down to 90"
        ask, _ = claude_worker.news.resolve.intent_fill(
            conn, row, 100_400_000, claude_worker.frames.SIDE_ASK
        )
        assert ask == 1, "a bar traded above 100.4"
    finally:
        conn.close()


def test_resolution_reads_a_WAL_candles_db(tmp_path: pathlib.Path) -> None:
    """MEASURED on the operator's live databases 2026-09-20, and the reason
    this test exists: `candles.db` runs in WAL, and a `?mode=ro` URI
    connection CANNOT open a WAL database that has un-checkpointed content
    and no live `-shm` — it fails with "unable to open database file",
    intermittently, depending on whether a writer happens to be holding it.

    The failure was silent in the worst direction: no bars, every
    resolution pending, the whole scorecard quietly `unresolvable` after
    48 h. It was invisible to every other test in this file because a tmp
    database built by `sqlite3.connect` uses a rollback journal, not WAL.
    So this one builds a real WAL database with un-checkpointed content,
    the way the operator's actually looks.
    """
    _candles(tmp_path, _flat_then(35.0))
    path = tmp_path / "worker" / "candles.db"
    writer = sqlite3.connect(str(path))
    assert writer.execute("PRAGMA journal_mode = WAL").fetchone()[0] == "wal"
    writer.execute(
        "INSERT OR REPLACE INTO candles VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
        (VENUE, DESCRIPTOR, "1m", (NOW + 2000) * 1000, 1.0, 1.0, 1.0, 1.0, 0.0, 1, "t", 0),
    )
    writer.commit()
    # The writer stays OPEN across the resolve — which is both what holds
    # un-checkpointed content in the WAL and what the hourly candles cycle
    # actually looks like from here.
    try:
        assert (path.parent / (path.name + "-wal")).exists(), "un-checkpointed WAL content"
        with _store(tmp_path) as store:
            _open_label(store, "up")
            stats = claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
            assert stats.resolved == 1, "a WAL candles.db must still resolve"
            row = store.resolutions_resolved(0)[0]
            assert int(typing.cast(int, row["hit"])) == 1
    finally:
        writer.close()

    # ...and the connection SQLite hands back refuses writes itself.
    conn = claude_worker.news.resolve._open_candles(path)
    assert conn is not None
    try:
        conn.execute("DELETE FROM candles WHERE 0")
        raise AssertionError("the read connection allowed a write")
    except sqlite3.OperationalError as error:
        assert "readonly" in str(error)
    finally:
        conn.close()


# ---- the scorecard --------------------------------------------------------


def test_scorecard_windows_wilson_base_rate(tmp_path: pathlib.Path) -> None:
    """The two numbers that stop a hit rate from lying: a Wilson lower
    bound, and the direction-agnostic base rate beside it."""
    _candles(tmp_path, _flat_then(40.0))
    with _store(tmp_path) as store:
        for i in range(6):
            _open_label(store, "up" if i < 4 else "down", subject_id=f"s{i}")
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        card = claude_worker.news.resolve.build_scorecard(store, NOW + 3600)
    assert card["v"] == claude_worker.news.SCHEMA_VERSION
    windows = typing.cast(dict[str, object], card["windows"])
    assert set(windows) == {"7d", "30d", "all"}
    direction = typing.cast(
        dict[str, object], typing.cast(dict[str, object], windows["all"])["direction"]
    )
    assert direction["n"] == 6
    assert direction["hits"] == 4, "the four ups were right, the two downs were not"
    assert direction["base_rate"] == 1.0, "every bar moved up, whatever we said"
    lo = float(typing.cast(float, direction["wilson_lo"]))
    hi = float(typing.cast(float, direction["wilson_hi"]))
    assert 0.0 < lo < 4 / 6 < hi <= 1.0
    # The gate's own reading: this is NOT edge, because lo < base_rate.
    assert lo < float(typing.cast(float, direction["base_rate"]))
    assert "wilson_lo > base_rate" in str(card["legend"])


def test_scorecard_by_model_not_pooled(tmp_path: pathlib.Path) -> None:
    """The session brain and the automated one are never pooled silently —
    that comparison is the whole point of running both."""
    _candles(tmp_path, _flat_then(30.0))
    with _store(tmp_path) as store:
        for name, model in (("auto", "claude-sonnet-5"), ("human", "session")):
            store.put_label(
                {
                    "story_id": name, "model": model, "prompt_version": "label-v2",
                    "cache_hit": 0, "market": DESCRIPTOR, "sym": 7, "descriptor": DESCRIPTOR,
                    "venue": VENUE, "direction": "up", "confidence": 0.8,
                    "half_life_s": 600.0, "vol": "none", "liquidity": "none", "ts": NOW,
                }
            )
            _open_label(store, "up", subject_id=name)
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        card = claude_worker.news.resolve.build_scorecard(store, NOW + 3600)
    direction = typing.cast(
        dict[str, object],
        typing.cast(dict[str, object], typing.cast(dict[str, object], card["windows"])["all"])[
            "direction"
        ],
    )
    by_model = typing.cast(dict[str, object], direction["by_model"])
    assert set(by_model) == {"claude-sonnet-5", "session"}
    assert typing.cast(dict[str, object], by_model["session"])["n"] == 1


def test_scorecard_reliability_buckets(tmp_path: pathlib.Path) -> None:
    """If the 0.9 bucket does not beat the 0.7 bucket, the confidence
    number is decoration and any policy floor on it is arbitrary."""
    _candles(tmp_path, _flat_then(25.0))
    with _store(tmp_path) as store:
        _open_label(store, "up", subject_id="sure", confidence=0.95)
        _open_label(store, "down", subject_id="unsure", confidence=0.55)
        claude_worker.news.resolve.resolve_due(store, _paths(tmp_path), NOW + 3600)
        card = claude_worker.news.resolve.build_scorecard(store, NOW + 3600)
    buckets = typing.cast(list[dict[str, object]], card["reliability"])
    assert len(buckets) == 4
    low = buckets[0]
    high = buckets[3]
    assert low["n"] == 1 and low["hit_rate"] == 0.0
    assert high["n"] == 1 and high["hit_rate"] == 1.0


def test_scorecard_json_written_atomic(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        card = claude_worker.news.resolve.build_scorecard(store, NOW)
    path = tmp_path / "worker" / "news" / claude_worker.news.SCORECARD_FILE
    assert claude_worker.news.resolve.write_scorecard(path, card) is True
    doc = json.loads(path.read_text(encoding="utf-8"))
    # An empty lane writes a HONEST zero everywhere, not an error.
    assert doc["generated_ts"] == NOW
    everything = typing.cast(dict[str, object], doc["windows"])["all"]
    assert typing.cast(dict[str, object], everything)["direction"]["n"] == 0
    assert typing.cast(dict[str, object], everything)["intents"]["approximate"] is True
    assert doc["pending"] == 0
    assert not (path.parent / (path.name + ".tmp")).exists()
