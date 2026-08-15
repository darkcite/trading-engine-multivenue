"""Durable worker state — SQLite in WAL mode (design §5.3).

One database (``CLAUDE_WORKER_DB``) shared by every mode. Item-9 scope:

- **seq allocator** (``ai_seq``): one strictly-increasing namespace across
  operator-verb processes AND a later ``serve`` run, surviving reconnects
  and restarts. Allocation is a single transactional
  ``UPDATE … RETURNING`` — concurrent processes can never double-allocate.
- **dedupe** (``seen_items``): ``(feed, guid)`` insert-or-ignore.
- **event log** (``events``): append-only. Frame sends are recorded here
  with their wall-clock send timestamp — per the §3 capture amendment this
  is the ONLY structured record of worker send times (the engine's PMLR
  capture carries the rewritten engine-monotonic stamp).

The full §5.3 schema (including ``prompt_cache`` and ``rulesets``) is
created up front so items 10-12 add consumers, not migrations.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3
import time

_SCHEMA: tuple[str, ...] = (
    """
    CREATE TABLE IF NOT EXISTS seen_items (
        feed     TEXT NOT NULL,
        guid     TEXT NOT NULL,
        first_ts INTEGER NOT NULL,
        PRIMARY KEY (feed, guid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS prompt_cache (
        model               TEXT NOT NULL,
        prompt_version_hash TEXT NOT NULL,
        content_hash        TEXT NOT NULL,
        response            TEXT NOT NULL,
        created_ts          INTEGER NOT NULL,
        PRIMARY KEY (model, prompt_version_hash, content_hash)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS ai_seq (
        id       INTEGER PRIMARY KEY CHECK (id = 1),
        next_seq INTEGER NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS rulesets (
        hash         TEXT PRIMARY KEY,
        path         TEXT NOT NULL,
        report_path  TEXT,
        gates_passed INTEGER NOT NULL DEFAULT 0,
        author_mode  TEXT CHECK (author_mode IN ('auto', 'session')),
        model        TEXT,
        thesis       TEXT,
        staged_ts    INTEGER,
        committed_ts INTEGER
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS events (
        id     INTEGER PRIMARY KEY AUTOINCREMENT,
        ts     INTEGER NOT NULL,
        kind   TEXT NOT NULL,
        detail TEXT NOT NULL
    )
    """,
)

EVENT_FRAME_SENT: str = "frame_sent"


class State:
    """Open handle on the worker database. Boot-time construction; verbs
    and the serve loop hold exactly one for their lifetime."""

    def __init__(self, db_path: pathlib.Path) -> None:
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn: sqlite3.Connection = sqlite3.connect(str(db_path))
        mode = self._conn.execute("PRAGMA journal_mode = WAL").fetchone()[0]
        if str(mode).lower() != "wal":
            self._conn.close()
            raise RuntimeError(f"SQLite WAL mode unavailable for {db_path}: got {mode!r}")
        with self._conn:
            for i in range(len(_SCHEMA)):
                self._conn.execute(_SCHEMA[i])
            # Seed the seq allocator exactly once; next_seq is the next
            # value to hand out.
            self._conn.execute("INSERT OR IGNORE INTO ai_seq (id, next_seq) VALUES (1, 1)")

    def close(self) -> None:
        """Flush and close. Idempotent."""
        self._conn.close()

    # ---- seq allocator (§5.3) ----

    def next_seq(self) -> int:
        """Allocate the next frame sequence number.

        Transactional ``UPDATE … RETURNING`` — safe across concurrent verb
        processes; the namespace survives reconnects and restarts, so the
        engine's per-connection ``SeqPolicy`` primes on whatever value
        arrives first and never sees a regression from a well-behaved
        worker.
        """
        with self._conn:
            row = self._conn.execute(
                "UPDATE ai_seq SET next_seq = next_seq + 1 WHERE id = 1 RETURNING next_seq"
            ).fetchone()
        if row is None:
            raise RuntimeError("ai_seq row missing — state database corrupted")
        return int(row[0]) - 1

    def peek_seq(self) -> int:
        """Next value ``next_seq`` would return (test/diagnostic surface)."""
        row = self._conn.execute("SELECT next_seq FROM ai_seq WHERE id = 1").fetchone()
        if row is None:
            raise RuntimeError("ai_seq row missing — state database corrupted")
        return int(row[0])

    # ---- dedupe (§5.3) ----

    def mark_seen(self, feed: str, guid: str, first_ts: int) -> bool:
        """Record ``(feed, guid)``; True when this is the first sighting."""
        with self._conn:
            cur = self._conn.execute(
                "INSERT OR IGNORE INTO seen_items (feed, guid, first_ts) VALUES (?, ?, ?)",
                (feed, guid, first_ts),
            )
        return cur.rowcount == 1

    # ---- event log (§5.3 + §3 capture amendment) ----

    def record_event(self, kind: str, detail: str, ts_ns: int | None = None) -> int:
        """Append one event; returns its id. ``ts_ns`` defaults to now
        (wall clock, ns — the worker's own clock domain)."""
        stamp = time.time_ns() if ts_ns is None else ts_ns
        with self._conn:
            cur = self._conn.execute(
                "INSERT INTO events (ts, kind, detail) VALUES (?, ?, ?)",
                (stamp, kind, detail),
            )
        rowid = cur.lastrowid
        if rowid is None:
            raise RuntimeError("events insert returned no rowid")
        return int(rowid)

    def record_frame_sent(self, seq: int, kind: int, send_ts_ns: int) -> int:
        """The structured send-time record (§3 capture amendment): one row
        per frame handed to the kernel, stamped with the wall-clock send
        time. Returns the event id."""
        return self.record_event(EVENT_FRAME_SENT, f"seq={seq} kind={kind}", ts_ns=send_ts_ns)

    def events(self, kind: str | None = None) -> list[tuple[int, int, str, str]]:
        """Events as ``(id, ts, kind, detail)`` rows, oldest first,
        optionally filtered by kind. Test/audit surface."""
        if kind is None:
            cur = self._conn.execute("SELECT id, ts, kind, detail FROM events ORDER BY id")
        else:
            cur = self._conn.execute(
                "SELECT id, ts, kind, detail FROM events WHERE kind = ? ORDER BY id",
                (kind,),
            )
        out: list[tuple[int, int, str, str]] = []
        for row in cur.fetchall():
            out.append((int(row[0]), int(row[1]), str(row[2]), str(row[3])))
        return out
