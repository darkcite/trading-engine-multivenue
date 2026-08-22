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

import hashlib
import pathlib
import sqlite3
import time
import typing

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


class StateError(RuntimeError):
    """State-layer failure (corrupted/unavailable database). The verb
    layer maps this to §6 exit code 5. Subclasses RuntimeError so
    pre-item-12 callers' expectations hold."""


class State:
    """Open handle on the worker database. Boot-time construction; verbs
    and the serve loop hold exactly one for their lifetime."""

    def __init__(self, db_path: pathlib.Path) -> None:
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn: sqlite3.Connection = sqlite3.connect(str(db_path))
        mode = self._conn.execute("PRAGMA journal_mode = WAL").fetchone()[0]
        if str(mode).lower() != "wal":
            self._conn.close()
            raise StateError(f"SQLite WAL mode unavailable for {db_path}: got {mode!r}")
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
            raise StateError("ai_seq row missing — state database corrupted")
        return int(row[0]) - 1

    def peek_seq(self) -> int:
        """Next value ``next_seq`` would return (test/diagnostic surface)."""
        row = self._conn.execute("SELECT next_seq FROM ai_seq WHERE id = 1").fetchone()
        if row is None:
            raise StateError("ai_seq row missing — state database corrupted")
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

    # ---- prompt cache (§5.3; PLAN §10.2 — first consumers in item 10) ----

    def cache_get(self, model: str, prompt_version_hash: str, content_hash: str) -> str | None:
        """Cached response for the exact (model, prompt-version, content)
        triple, or None."""
        row = self._conn.execute(
            "SELECT response FROM prompt_cache"
            " WHERE model = ? AND prompt_version_hash = ? AND content_hash = ?",
            (model, prompt_version_hash, content_hash),
        ).fetchone()
        return None if row is None else str(row[0])

    def cache_put(
        self,
        model: str,
        prompt_version_hash: str,
        content_hash: str,
        response: str,
        ts: int | None = None,
    ) -> None:
        """Store one response (idempotent overwrite on replay)."""
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            self._conn.execute(
                "INSERT OR REPLACE INTO prompt_cache"
                " (model, prompt_version_hash, content_hash, response, created_ts)"
                " VALUES (?, ?, ?, ?, ?)",
                (model, prompt_version_hash, content_hash, response, stamp),
            )

    def cached_complete(
        self,
        model: str,
        prompt_version: str,
        prompt: str,
        complete_fn: typing.Callable[[str, str], str],
    ) -> tuple[str, bool]:
        """The single LLM-call gate: consult the prompt cache, invoke
        ``complete_fn(model, prompt)`` only on a miss, store the result.
        Returns ``(response, cache_hit)``. The version string is hashed
        separately from the content so a prompt-template bump invalidates
        without touching stored rows (§5.3 key design)."""
        version_hash = hashlib.sha256(prompt_version.encode()).hexdigest()
        content_hash = hashlib.sha256(prompt.encode()).hexdigest()
        cached = self.cache_get(model, version_hash, content_hash)
        if cached is not None:
            return cached, True
        response = complete_fn(model, prompt)
        self.cache_put(model, version_hash, content_hash, response)
        return response, False

    # ---- ruleset registry (§5.3; consumers arrive with item 12) ----

    def stage_ruleset(  # noqa: PLR0913 — registry row fields, deliberately
        self,
        full_hash: str,
        path: str,
        report_path: str,
        author_mode: str,
        ts: int | None = None,
        *,
        model: str | None = None,
        thesis: str | None = None,
    ) -> None:
        """Record a gate-passed ruleset as STAGED (§6 stage-ruleset).

        Caller (``backtest.stage_ruleset`` — the only path to a Stage
        frame) has already enforced the gate binding; this row is the
        worker-side registry entry. Re-staging the same hash refreshes
        ``staged_ts`` and clears any earlier ``committed_ts`` (the engine
        stub's staged/committed state machine mirrors this: a new Stage
        supersedes an old Commit). ``author_mode`` rides the §8.7
        attribution column ('session' from verbs, 'auto' from serve).

        8h §8.2 (additive): optional ``model``/``thesis`` fill the
        pre-provisioned attribution columns — the registry's answer to
        "who wrote the live table and why". ``None`` PRESERVES any
        existing value (COALESCE): a later restage through the frozen
        pair — e.g. the §8.3 rollback restaging a prior hash — never
        erases the original attribution. Every pre-8h call site is
        byte-unchanged.
        """
        if author_mode not in ("auto", "session"):
            raise StateError(f"author_mode must be 'auto' or 'session': {author_mode!r}")
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            self._conn.execute(
                "INSERT INTO rulesets"
                " (hash, path, report_path, gates_passed, author_mode, model, thesis,"
                "  staged_ts, committed_ts)"
                " VALUES (?, ?, ?, 1, ?, ?, ?, ?, NULL)"
                " ON CONFLICT(hash) DO UPDATE SET"
                " path = excluded.path, report_path = excluded.report_path,"
                " gates_passed = 1, author_mode = excluded.author_mode,"
                " model = COALESCE(excluded.model, rulesets.model),"
                " thesis = COALESCE(excluded.thesis, rulesets.thesis),"
                " staged_ts = excluded.staged_ts, committed_ts = NULL",
                (full_hash, path, report_path, author_mode, model, thesis, stamp),
            )

    def ruleset_attribution(self, full_hash: str) -> tuple[str | None, str | None] | None:
        """The §8.2 attribution pair ``(model, thesis)`` for one registry
        row, or None when the hash is unknown. Kept OFF [`ruleset_row`]
        — its 7-tuple shape is pinned by pre-8h tests."""
        row = self._conn.execute(
            "SELECT model, thesis FROM rulesets WHERE hash = ?",
            (full_hash,),
        ).fetchone()
        if row is None:
            return None
        return (
            None if row[0] is None else str(row[0]),
            None if row[1] is None else str(row[1]),
        )

    def ruleset_row(
        self, full_hash: str
    ) -> tuple[str, str, str | None, bool, str | None, int | None, int | None] | None:
        """One registry row as ``(hash, path, report_path, gates_passed,
        author_mode, staged_ts, committed_ts)``, or None."""
        row = self._conn.execute(
            "SELECT hash, path, report_path, gates_passed, author_mode,"
            " staged_ts, committed_ts FROM rulesets WHERE hash = ?",
            (full_hash,),
        ).fetchone()
        if row is None:
            return None
        return (
            str(row[0]),
            str(row[1]),
            None if row[2] is None else str(row[2]),
            bool(row[3]),
            None if row[4] is None else str(row[4]),
            None if row[5] is None else int(row[5]),
            None if row[6] is None else int(row[6]),
        )

    def mark_ruleset_committed(self, full_hash: str, ts: int | None = None) -> None:
        """Stamp ``committed_ts`` on an existing row (after the Commit
        frame went out — send-then-record, so a failed send never leaves
        a phantom commit in the registry)."""
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                "UPDATE rulesets SET committed_ts = ? WHERE hash = ?",
                (stamp, full_hash),
            )
        if cur.rowcount != 1:
            raise StateError(f"mark_ruleset_committed: no registry row for {full_hash}")

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
            raise StateError("events insert returned no rowid")
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
