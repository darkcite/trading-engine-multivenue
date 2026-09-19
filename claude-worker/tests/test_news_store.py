# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §4.3/§6 — ``news.db``.

Every test opens a tmp store: this lane's DB is a THIRD store and must never
be confused with ``state.db``, whose seq namespace is load-bearing for the
engine. The two properties worth defending are the idempotence of the
writers (a re-polled feed pays for nothing twice, a re-run detector records
no second event) and the conservatism of ``prune`` (an item that fed a story
and the newest snapshot per source are never dropped).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3

import pytest

import claude_worker.news.store

DAY_S: int = claude_worker.news.store.SECONDS_PER_DAY
NOW: int = 1_758_000_000


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


def _item(store: claude_worker.news.store.Store, **over: object) -> bool:
    fields: dict[str, object] = {
        "source": "okx-ann",
        "guid": "g1",
        "ts": NOW,
        "fetched_ts": NOW,
        "title": "OKX will delist FOO",
        "link": "https://www.okx.com/a/1",
        "text": "body",
        "origin": "www.okx.com",
        "class_": "B",
        "weight": 1.0,
        "venue": "okx",
    }
    fields.update(over)
    return store.upsert_item(**fields)  # type: ignore[arg-type]


def test_the_schema_is_created_in_wal_and_carries_every_table(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "worker" / "news" / "news.db"
    store = claude_worker.news.store.Store(path)
    assert path.is_file()
    conn = sqlite3.connect(str(path))
    mode = conn.execute("PRAGMA journal_mode").fetchone()[0]
    names = {row[0] for row in conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'")}
    conn.close()
    store.close()
    assert str(mode).lower() == "wal"
    assert {
        "sources",
        "items",
        "snapshots",
        "series",
        "events",
        "triage",
        "stories",
        "labels",
        "resolutions",
        "assessments",
        "actions",
        "budget",
        "counters",
    } <= names


def test_opening_twice_is_additive_and_keeps_the_rows(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.upsert_source("okx-ann", "json-okx-ann", "B", "www.okx.com", 1)
        _item(store)
    with _store(tmp_path) as store:
        assert store.item("okx-ann", "g1") is not None
        assert len(store.source_rows()) == 1


def test_a_re_registered_source_keeps_its_counters(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.upsert_source("s", "rss", "C", "a.example", 1)
        store.record_poll("s", NOW, claude_worker.news.store.PollOutcome(ok=True))
        store.upsert_source("s", "rss", "C", "a.example", 0)
        row = store.source_rows()[0]
        assert row["enabled"] == 0
        assert row["polls_total"] == 1
        assert row["polls_ok"] == 1


def test_poll_outcomes_fold_into_the_gate_counters(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.upsert_source("s", "rss", "C", "a.example", 1)
        store.record_poll("s", 10, claude_worker.news.store.PollOutcome(ok=True))
        store.record_poll("s", 20, claude_worker.news.store.PollOutcome(error="http 503"))
        store.record_poll("s", 30, claude_worker.news.store.PollOutcome(error="http 503"))
        row = store.source_rows()[0]
        assert row["polls_total"] == 3
        assert row["polls_ok"] == 1
        assert row["last_ok_ts"] == 10
        assert row["last_err_ts"] == 30
        assert row["err_streak"] == 2
        assert row["last_error"] == "http 503"

        # One success clears the streak but never the history.
        store.record_poll("s", 40, claude_worker.news.store.PollOutcome(ok=True))
        row = store.source_rows()[0]
        assert row["err_streak"] == 0
        assert row["last_err_ts"] == 30

        store.record_poll("s", 50, claude_worker.news.store.PollOutcome(refused_origin=True))
        store.record_poll("s", 60, claude_worker.news.store.PollOutcome(budget_skip=True))
        row = store.source_rows()[0]
        assert row["refused_origin_total"] == 1
        assert row["budget_skips_total"] == 1


def test_items_total_is_additive(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.upsert_source("s", "rss", "C", "a.example", 1)
        store.add_items_total("s", 3)
        store.add_items_total("s", 2)
        assert store.source_rows()[0]["items_total"] == 5


def test_a_second_sighting_of_one_item_is_not_stored_again(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        assert _item(store) is True
        assert _item(store, title="edited upstream") is False
        row = store.item("okx-ann", "g1")
        assert row is not None
        assert row["title"] == "OKX will delist FOO"
        assert row["tier0"] == claude_worker.news.store.TIER0_PASS
        assert row["triage_state"] == claude_worker.news.store.STATE_NEW
        assert row["story_id"] == ""


def test_tier0_verdicts_and_triage_state(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        _item(store)
        store.mark_tier0("okx-ann", "g1", claude_worker.news.store.TIER0_DROP_DUP, "okx-ann|g0")
        row = store.item("okx-ann", "g1")
        assert row is not None
        assert row["tier0"] == claude_worker.news.store.TIER0_DROP_DUP
        assert row["dup_of"] == "okx-ann|g0"

        store.set_triage_state("okx-ann", "g1", claude_worker.news.store.STATE_TRIAGED)
        row = store.item("okx-ann", "g1")
        assert row is not None and row["story_id"] == ""
        store.set_triage_state("okx-ann", "g1", claude_worker.news.store.STATE_LABELED, "story-7")
        row = store.item("okx-ann", "g1")
        assert row is not None and row["story_id"] == "story-7"
        # An empty story_id never CLEARS one already set.
        store.set_triage_state("okx-ann", "g1", claude_worker.news.store.STATE_ASSESSED)
        row = store.item("okx-ann", "g1")
        assert row is not None and row["story_id"] == "story-7"


def test_an_unknown_tier0_verdict_is_refused(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        _item(store)
        with pytest.raises(ValueError):
            store.mark_tier0("okx-ann", "g1", "maybe")


def test_items_since_filters_by_time_and_verdict(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        _item(store, guid="old", ts=NOW - 100)
        _item(store, guid="new", ts=NOW)
        store.mark_tier0("okx-ann", "old", claude_worker.news.store.TIER0_DROP_VOCAB)
        assert len(store.items_since(NOW - 200)) == 2
        assert [r["guid"] for r in store.items_since(NOW - 200, "pass")] == ["new"]


def test_snapshots_keep_their_history_and_the_latest_wins(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.insert_snapshot("okx-inst", 100, "aaa", 2, '{"A":1}')
        store.insert_snapshot("okx-inst", 200, "bbb", 3, '{"A":2}')
        latest = store.latest_snapshot("okx-inst")
        assert latest is not None
        assert latest["taken_ts"] == 200
        assert latest["sha256"] == "bbb"
        assert store.latest_snapshot("nobody") is None


def test_series_rows_are_keyed_and_returned_oldest_first(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.insert_series("fng", "fng", 100, 40.0)
        store.insert_series("fng", "fng", 200, 55.0)
        store.insert_series("fng", "fng", 200, 56.0)
        assert store.series_tail("fng", "fng", 10) == [(100, 40.0), (200, 56.0)]
        assert store.series_tail("fng", "fng", 1) == [(200, 56.0)]


def test_the_same_event_is_never_recorded_twice(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        first = store.insert_event(
            kind="delisting",
            venue="okx",
            at_ts=NOW,
            source="okx-inst",
            detail="FOO-USDT-SWAP left the set",
            created_ts=NOW,
            instrument="FOO-USDT-SWAP",
        )
        again = store.insert_event(
            kind="delisting",
            venue="okx",
            at_ts=NOW,
            source="okx-inst",
            detail="different words, same transition",
            created_ts=NOW + 60,
            instrument="FOO-USDT-SWAP",
        )
        assert isinstance(first, int)
        assert again is None
        assert len(store.events_since(NOW - 1)) == 1

        other = store.insert_event(
            kind="delisting",
            venue="okx",
            at_ts=NOW,
            source="okx-inst",
            detail="a different instrument",
            created_ts=NOW,
            instrument="BAR-USDT-SWAP",
        )
        assert isinstance(other, int)


def test_the_cascade_tables_round_trip(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.put_triage(
            {
                "source": "okx-ann",
                "guid": "g1",
                "model": "session",
                "prompt_version": "triage-v2",
                "cache_hit": 0,
                "family": "crypto",
                "impact": "high",
                "reason": "delisting",
                "event_type": "delisting",
                "venues": ["okx"],
                "assets": ["FOO"],
                "ts": NOW,
            }
        )
        store.upsert_story(
            {
                "story_id": "s1",
                "family": "crypto",
                "event_type": "delisting",
                "venues": '["okx"]',
                "assets": '["FOO"]',
                "first_ts": NOW,
                "last_ts": NOW,
                "item_count": 1,
                "origins": 1,
                "venue_origin": 1,
                "max_impact": "high",
                "state": "open",
            }
        )
        story = store.story("s1")
        assert story is not None and story["max_impact"] == "high"
        # A second upsert replaces in place, never duplicates.
        store.upsert_story(
            {
                "story_id": "s1",
                "family": "crypto",
                "event_type": "delisting",
                "venues": '["okx"]',
                "assets": '["FOO"]',
                "first_ts": NOW,
                "last_ts": NOW + 60,
                "item_count": 3,
                "origins": 2,
                "venue_origin": 1,
                "max_impact": "high",
                "state": "labeled",
            }
        )
        story = store.story("s1")
        assert story is not None and story["item_count"] == 3 and story["state"] == "labeled"

        store.put_label(
            {
                "story_id": "s1",
                "model": "session",
                "prompt_version": "label-v2",
                "cache_hit": 0,
                "market": "binance:btcusdt",
                "sym": 7,
                "descriptor": "binance:btcusdt",
                "venue": 1,
                "direction": "down",
                "confidence": 0.8,
                "half_life_s": 900.0,
                "vol": "high",
                "liquidity": "normal",
                "ts": NOW,
            }
        )
        label = store.label("s1")
        assert label is not None and label["direction"] == "down"

        assessment_id = store.insert_assessment(
            {
                "story_id": "s1",
                "model": "claude-opus-5",
                "prompt_version": "assess-v2",
                "cache_hit": 0,
                "body": '{"v":1}',
                "input_tokens": 100,
                "output_tokens": 20,
                "ts": NOW,
            }
        )
        assert assessment_id > 0

        action_id = store.record_action(
            {"ts": NOW, "story_id": "s1", "kind": "set_bias", "mode": "shadow", "sym": 7}
        )
        assert action_id > 0
        assert [r["id"] for r in store.actions_since(NOW - 1)] == [action_id]


def test_resolutions_come_due_by_t1(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        for subject, t1 in (("s1", NOW + 60), ("s2", NOW + 3600)):
            store.put_resolution(
                {
                    "subject_kind": "label",
                    "subject_id": subject,
                    "t0": NOW,
                    "t1": t1,
                    "horizon_s": t1 - NOW,
                    "state": "pending",
                }
            )
        assert [r["subject_id"] for r in store.resolutions_due(NOW + 100)] == ["s1"]
        assert len(store.resolutions_due(NOW + 7200)) == 2
        store.put_resolution(
            {
                "subject_kind": "label",
                "subject_id": "s1",
                "t0": NOW,
                "t1": NOW + 60,
                "horizon_s": 60,
                "state": "resolved",
                "hit": 1,
            }
        )
        assert [r["subject_id"] for r in store.resolutions_due(NOW + 7200)] == ["s2"]


def test_budget_accumulates_per_day_and_tier(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        day = claude_worker.news.store.day_of(NOW)
        store.budget_add(day, "tier1", calls=1, input_tokens=900, output_tokens=40)
        store.budget_add(day, "tier1", calls=1, input_tokens=950, output_tokens=45, skipped=2)
        store.budget_add(day, "tier2", calls=1)
        today = store.budget_today("tier1", NOW)
        assert today == {"calls": 2, "input_tokens": 1850, "output_tokens": 85, "skipped": 2}
        assert store.budget_today("tier3", NOW)["calls"] == 0
        # Another day is another row.
        assert store.budget_today("tier1", NOW + DAY_S)["calls"] == 0


def test_counters_accumulate(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.counter_inc(claude_worker.news.store.COUNTER_POLICY_INVALID)
        store.counter_inc(claude_worker.news.store.COUNTER_POLICY_INVALID)
        store.counter_inc(claude_worker.news.store.COUNTER_PARSE_EMPTY, 5)
        assert store.counters() == {
            claude_worker.news.store.COUNTER_PARSE_EMPTY: 5,
            claude_worker.news.store.COUNTER_POLICY_INVALID: 2,
        }


def test_prune_keeps_everything_that_fed_something(tmp_path: pathlib.Path) -> None:
    old = NOW - 30 * DAY_S
    with _store(tmp_path) as store:
        # Dropped: old, not a pass, triaged, no story.
        _item(store, guid="junk", ts=old)
        store.mark_tier0("okx-ann", "junk", claude_worker.news.store.TIER0_DROP_VOCAB)
        store.set_triage_state("okx-ann", "junk", claude_worker.news.store.STATE_TRIAGED)
        # Kept: it passed tier 0.
        _item(store, guid="passed", ts=old)
        # Kept: it fed a story.
        _item(store, guid="member", ts=old)
        store.mark_tier0("okx-ann", "member", claude_worker.news.store.TIER0_DROP_DUP)
        store.set_triage_state("okx-ann", "member", claude_worker.news.store.STATE_TRIAGED, "s1")
        # Kept: young.
        _item(store, guid="fresh", ts=NOW)
        store.mark_tier0("okx-ann", "fresh", claude_worker.news.store.TIER0_DROP_CAP)
        store.set_triage_state("okx-ann", "fresh", claude_worker.news.store.STATE_SKIPPED)

        store.insert_snapshot("okx-inst", old, "a", 1, "{}")
        store.insert_snapshot("okx-inst", old + 1, "b", 1, "{}")
        store.insert_snapshot("bn-inst", old, "c", 1, "{}")

        deleted = store.prune(NOW, items_days=14, snapshots_days=14)
        assert deleted["items"] == 1
        # Each source keeps its newest snapshot; only okx-inst's older one goes.
        assert deleted["snapshots"] == 1
        kept = {row["guid"] for row in store.items_since(0)}
        assert kept == {"passed", "member", "fresh"}
        assert store.latest_snapshot("okx-inst") is not None
        assert store.latest_snapshot("bn-inst") is not None


def test_prune_on_an_empty_store_is_a_no_op(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        assert store.prune(NOW, items_days=14, snapshots_days=30) == {"items": 0, "snapshots": 0}


def test_day_of_is_utc(tmp_path: pathlib.Path) -> None:
    del tmp_path
    assert claude_worker.news.store.day_of(0) == "1970-01-01"
    assert claude_worker.news.store.day_of(1_758_240_000) == "2025-09-19"
