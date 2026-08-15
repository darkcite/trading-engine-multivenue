"""state.py — WAL mode, durable seq allocator, dedupe, event log (§5.3).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3

import claude_worker.state


def test_wal_mode_active(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    mode = st._conn.execute("PRAGMA journal_mode").fetchone()[0]
    assert str(mode).lower() == "wal"
    st.close()


def test_seq_starts_at_one_and_increments(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    assert st.peek_seq() == 1
    assert st.next_seq() == 1
    assert st.next_seq() == 2
    assert st.next_seq() == 3
    assert st.peek_seq() == 4
    st.close()


def test_seq_survives_reopen(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "state.db"
    st = claude_worker.state.State(db)
    for _ in range(5):
        st.next_seq()
    st.close()
    st2 = claude_worker.state.State(db)
    assert st2.next_seq() == 6, "allocator is durable across restarts/reconnects"
    st2.close()


def test_seq_shared_across_concurrent_handles(tmp_path: pathlib.Path) -> None:
    # Two open handles = two verb processes (§5.3: one namespace across
    # modes). Interleaved allocations must never collide.
    db = tmp_path / "state.db"
    a = claude_worker.state.State(db)
    b = claude_worker.state.State(db)
    got = [a.next_seq(), b.next_seq(), a.next_seq(), b.next_seq()]
    assert got == sorted(got)
    assert len(set(got)) == 4
    a.close()
    b.close()


def test_mark_seen_dedupes_by_feed_and_guid(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    assert st.mark_seen("feed-a", "guid-1", 100) is True
    assert st.mark_seen("feed-a", "guid-1", 200) is False, "duplicate"
    assert st.mark_seen("feed-a", "guid-2", 300) is True
    assert st.mark_seen("feed-b", "guid-1", 400) is True, "keyed by (feed, guid)"
    # first_ts is the FIRST sighting's stamp.
    row = st._conn.execute(
        "SELECT first_ts FROM seen_items WHERE feed = ? AND guid = ?",
        ("feed-a", "guid-1"),
    ).fetchone()
    assert row[0] == 100
    st.close()


def test_event_log_appends_and_filters(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    e1 = st.record_event("boot", "hello", ts_ns=10)
    e2 = st.record_frame_sent(seq=41, kind=0, send_ts_ns=20)
    e3 = st.record_frame_sent(seq=42, kind=3, send_ts_ns=30)
    assert e1 < e2 < e3

    everything = st.events()
    assert [row[0] for row in everything] == [e1, e2, e3]

    sent = st.events(kind=claude_worker.state.EVENT_FRAME_SENT)
    assert len(sent) == 2
    assert sent[0][1] == 20 and sent[1][1] == 30, "send timestamps preserved"
    assert "seq=41 kind=0" == sent[0][3]
    assert "seq=42 kind=3" == sent[1][3]
    st.close()


def test_record_event_defaults_to_now(tmp_path: pathlib.Path) -> None:
    st = claude_worker.state.State(tmp_path / "state.db")
    st.record_event("boot", "x")
    ts = st.events()[0][1]
    assert ts > 0
    st.close()


def test_full_schema_created_up_front(tmp_path: pathlib.Path) -> None:
    # Items 10-12 must find their tables (§5.3): schema, not migrations.
    st = claude_worker.state.State(tmp_path / "state.db")
    names = set()
    for row in st._conn.execute("SELECT name FROM sqlite_master WHERE type = 'table'").fetchall():
        names.add(row[0])
    for table in ("seen_items", "prompt_cache", "ai_seq", "rulesets", "events"):
        assert table in names, f"missing table {table}"
    st.close()


def test_missing_seq_row_reseeded_on_open(tmp_path: pathlib.Path) -> None:
    db = tmp_path / "state.db"
    st = claude_worker.state.State(db)
    st.close()
    raw = sqlite3.connect(str(db))
    with raw:
        raw.execute("DELETE FROM ai_seq")
    raw.close()
    st2 = claude_worker.state.State(db)
    # Re-seeding on open restores the invariant rather than limping.
    assert st2.next_seq() == 1
    st2.close()
