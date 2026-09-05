# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
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
created up front so items 10-12 add consumers, not migrations. RG4
(docs/regime-and-dashboard-plan.md §5.2) adds the strategy-library
tables ``library`` / ``library_evidence`` / ``compositions`` the same
way — additive ``CREATE TABLE IF NOT EXISTS``, no migration of any
earlier table.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
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
    # RG4 (docs/regime-and-dashboard-plan.md §5.2) — the strategy LIBRARY:
    # a member = a named row set (or a coded-member reference), keyed by
    # the sha256 of its canonical rows; labels/thesis/status are metadata
    # and never change the id. Additive tables — every pre-RG4 table and
    # tuple shape above is untouched.
    """
    CREATE TABLE IF NOT EXISTS library (
        member_id   TEXT PRIMARY KEY,
        name        TEXT NOT NULL,
        kind        TEXT NOT NULL CHECK (kind IN ('vm-rows', 'coded')),
        path        TEXT NOT NULL,
        status      TEXT NOT NULL CHECK (status IN ('candidate', 'validated', 'retired')),
        labels_json TEXT NOT NULL,
        regime_off  TEXT,
        thesis      TEXT,
        origin_json TEXT NOT NULL,
        created_ts  INTEGER NOT NULL,
        updated_ts  INTEGER NOT NULL
    )
    """,
    # One row per (member, ≤ 2 h window) the member has been run on —
    # judged (v3, stale-aware) or not; `regime_word_mode` = the window's
    # dominant fast word (hex), `net_usd_0` the zero-fee net, `net_usd_tier`
    # the operator-tier net.
    """
    CREATE TABLE IF NOT EXISTS library_evidence (
        member_id        TEXT NOT NULL,
        window_id        TEXT NOT NULL,
        root             TEXT NOT NULL,
        n_ticks          INTEGER NOT NULL,
        n_fills          INTEGER NOT NULL,
        net_usd_0        REAL NOT NULL,
        net_usd_tier     REAL NOT NULL,
        max_dd_usd       REAL NOT NULL,
        regime_word_mode TEXT NOT NULL,
        judged           INTEGER NOT NULL,
        detail_version   INTEGER NOT NULL,
        ts               INTEGER NOT NULL,
        PRIMARY KEY (member_id, window_id)
    )
    """,
    # A composed table ↔ its members: the link the `rulesets` registry
    # never had. `words_json` = the effective words the composition was
    # built for (both profiles, hex), `gate_json` = the verdict summary.
    """
    CREATE TABLE IF NOT EXISTS compositions (
        table_hash      TEXT PRIMARY KEY,
        hash128         TEXT NOT NULL,
        member_ids_json TEXT NOT NULL,
        words_json      TEXT NOT NULL,
        path            TEXT NOT NULL,
        gate_json       TEXT,
        composed_ts     INTEGER NOT NULL,
        staged_ts       INTEGER,
        committed_ts    INTEGER
    )
    """,
)

EVENT_FRAME_SENT: str = "frame_sent"

#: RG4 library member status vocabulary (plan §5.2).
MEMBER_STATUSES: tuple[str, ...] = ("candidate", "validated", "retired")
MEMBER_KINDS: tuple[str, ...] = ("vm-rows", "coded")
_LIBRARY_SELECT: str = (
    "SELECT member_id, name, kind, path, status, labels_json, regime_off, thesis,"
    " origin_json, created_ts, updated_ts FROM library"
)
_COMPOSITION_SELECT: str = (
    "SELECT table_hash, hash128, member_ids_json, words_json, path, gate_json,"
    " composed_ts, staged_ts, committed_ts FROM compositions"
)


class RegistryRow(typing.NamedTuple):
    """One `rulesets` registry row (the RG4 reader's shape)."""

    hash: str
    path: str
    report_path: str | None
    gates_passed: bool
    author_mode: str | None
    model: str | None
    thesis: str | None
    staged_ts: int | None
    committed_ts: int | None


class LibraryMember(typing.NamedTuple):
    """One `library` row, JSON columns decoded."""

    member_id: str
    name: str
    kind: str
    path: str
    status: str
    labels: list[list[str]]
    regime_off: str | None
    thesis: str | None
    origin: dict[str, object]
    created_ts: int
    updated_ts: int


class EvidenceRow(typing.NamedTuple):
    """One `library_evidence` row."""

    member_id: str
    window_id: str
    root: str
    n_ticks: int
    n_fills: int
    net_usd_0: float
    net_usd_tier: float
    max_dd_usd: float
    regime_word_mode: str
    judged: bool
    detail_version: int
    ts: int


class CompositionRow(typing.NamedTuple):
    """One `compositions` row, JSON columns decoded."""

    table_hash: str
    hash128: str
    member_ids: list[str]
    words: dict[str, str]
    path: str
    gate: dict[str, object] | None
    composed_ts: int
    staged_ts: int | None
    committed_ts: int | None


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

    def committed_rulesets(self) -> list[tuple[str, str, str | None, int, int]]:
        """COMMITTED, gates-passed registry rows as ``(hash, path,
        report_path, staged_ts, committed_ts)``, most recently committed
        first (8h §8.3 — the monitor's active/prior source).

        Ordering ties (``committed_ts`` is second-resolution) break on
        ``staged_ts`` DESC then ``hash`` — deterministic; the daemon
        additionally disambiguates the ACTIVE hash via the events ledger
        (AUTOINCREMENT order), and production commits are cycle-spaced.
        Rows whose ``committed_ts`` was cleared by a supersede restage
        are correctly absent."""
        cur = self._conn.execute(
            "SELECT hash, path, report_path, staged_ts, committed_ts FROM rulesets"
            " WHERE committed_ts IS NOT NULL AND gates_passed = 1"
            " ORDER BY committed_ts DESC, staged_ts DESC, hash"
        )
        out: list[tuple[str, str, str | None, int, int]] = []
        for row in cur.fetchall():
            out.append(
                (
                    str(row[0]),
                    str(row[1]),
                    None if row[2] is None else str(row[2]),
                    int(row[3]),
                    int(row[4]),
                )
            )
        return out

    def rulesets_all(self) -> list[RegistryRow]:
        """Every registry row (staged or not), oldest staged first — the
        RG4 library import's source. Additive reader; the frozen 7-tuple
        of [`ruleset_row`] is untouched."""
        cur = self._conn.execute(
            "SELECT hash, path, report_path, gates_passed, author_mode, model, thesis,"
            " staged_ts, committed_ts FROM rulesets ORDER BY staged_ts, hash"
        )
        out: list[RegistryRow] = []
        for r in cur.fetchall():
            out.append(
                RegistryRow(
                    hash=str(r[0]),
                    path=str(r[1]),
                    report_path=None if r[2] is None else str(r[2]),
                    gates_passed=bool(r[3]),
                    author_mode=None if r[4] is None else str(r[4]),
                    model=None if r[5] is None else str(r[5]),
                    thesis=None if r[6] is None else str(r[6]),
                    staged_ts=None if r[7] is None else int(r[7]),
                    committed_ts=None if r[8] is None else int(r[8]),
                )
            )
        return out

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

    # ---- RG4: strategy library (plan §5.2) ----

    def library_insert(  # noqa: PLR0913, PLR0917 — one parameter per column, deliberately
        self,
        member_id: str,
        name: str,
        kind: str,
        path: str,
        status: str,
        labels: list[list[str]],
        origin: dict[str, object],
        *,
        regime_off: str | None = None,
        thesis: str | None = None,
        ts: int | None = None,
    ) -> bool:
        """Insert a member; returns False (row untouched) when the id
        already exists — an import re-run never downgrades a status the
        operator set, never rewrites labels. Use the setters for changes."""
        if kind not in MEMBER_KINDS:
            raise StateError(f"library kind must be one of {MEMBER_KINDS}: {kind!r}")
        if status not in MEMBER_STATUSES:
            raise StateError(f"library status must be one of {MEMBER_STATUSES}: {status!r}")
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                "INSERT OR IGNORE INTO library"
                " (member_id, name, kind, path, status, labels_json, regime_off, thesis,"
                "  origin_json, created_ts, updated_ts)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    member_id,
                    name,
                    kind,
                    path,
                    status,
                    json.dumps(labels, separators=(",", ":")),
                    regime_off,
                    thesis,
                    json.dumps(origin, sort_keys=True, separators=(",", ":")),
                    stamp,
                    stamp,
                ),
            )
        return cur.rowcount == 1

    def library_set_status(self, member_id: str, status: str, ts: int | None = None) -> None:
        if status not in MEMBER_STATUSES:
            raise StateError(f"library status must be one of {MEMBER_STATUSES}: {status!r}")
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                "UPDATE library SET status = ?, updated_ts = ? WHERE member_id = ?",
                (status, stamp, member_id),
            )
        if cur.rowcount != 1:
            raise StateError(f"library_set_status: no member {member_id}")

    def library_set_labels(
        self,
        member_id: str,
        labels: list[list[str]],
        regime_off: str | None,
        ts: int | None = None,
    ) -> None:
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                "UPDATE library SET labels_json = ?, regime_off = ?, updated_ts = ?"
                " WHERE member_id = ?",
                (json.dumps(labels, separators=(",", ":")), regime_off, stamp, member_id),
            )
        if cur.rowcount != 1:
            raise StateError(f"library_set_labels: no member {member_id}")

    def library_set_thesis(self, member_id: str, thesis: str, ts: int | None = None) -> None:
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                "UPDATE library SET thesis = ?, updated_ts = ? WHERE member_id = ?",
                (thesis, stamp, member_id),
            )
        if cur.rowcount != 1:
            raise StateError(f"library_set_thesis: no member {member_id}")

    @staticmethod
    def _member_from_row(row: tuple[typing.Any, ...]) -> LibraryMember:
        labels_raw = json.loads(str(row[5]))
        labels: list[list[str]] = []
        if isinstance(labels_raw, list):
            for term_list in labels_raw:
                if isinstance(term_list, list):
                    labels.append([str(t) for t in term_list])
        origin_raw = json.loads(str(row[8]))
        origin: dict[str, object] = origin_raw if isinstance(origin_raw, dict) else {}
        return LibraryMember(
            member_id=str(row[0]),
            name=str(row[1]),
            kind=str(row[2]),
            path=str(row[3]),
            status=str(row[4]),
            labels=labels,
            regime_off=None if row[6] is None else str(row[6]),
            thesis=None if row[7] is None else str(row[7]),
            origin=origin,
            created_ts=int(row[9]),
            updated_ts=int(row[10]),
        )

    def library_members(self, status: str | None = None) -> list[LibraryMember]:
        """Members ordered by name then id (deterministic); optionally one status."""
        if status is None:
            cur = self._conn.execute(_LIBRARY_SELECT + " ORDER BY name, member_id")
        else:
            cur = self._conn.execute(
                _LIBRARY_SELECT + " WHERE status = ? ORDER BY name, member_id",
                (status,),
            )
        return [self._member_from_row(tuple(r)) for r in cur.fetchall()]

    def library_member(self, member_id: str) -> LibraryMember | None:
        row = self._conn.execute(_LIBRARY_SELECT + " WHERE member_id = ?", (member_id,)).fetchone()
        return None if row is None else self._member_from_row(tuple(row))

    def library_find(self, key: str) -> LibraryMember | None:
        """Resolve a member by exact id, unique id prefix (≥ 8 hex chars) or
        exact name; None when nothing (or more than one prefix match)."""
        member = self.library_member(key)
        if member is not None:
            return member
        by_name = [m for m in self.library_members() if m.name == key]
        if len(by_name) == 1:
            return by_name[0]
        if len(key) >= 8:  # noqa: PLR2004 — the documented prefix floor
            by_prefix = [m for m in self.library_members() if m.member_id.startswith(key)]
            if len(by_prefix) == 1:
                return by_prefix[0]
        return None

    def evidence_upsert(  # noqa: PLR0913 — one parameter per column, deliberately
        self,
        member_id: str,
        window_id: str,
        root: str,
        *,
        n_ticks: int,
        n_fills: int,
        net_usd_0: float,
        net_usd_tier: float,
        max_dd_usd: float,
        regime_word_mode: str,
        judged: bool,
        detail_version: int,
        ts: int | None = None,
    ) -> None:
        """Record (replace) one member x window evidence row."""
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            self._conn.execute(
                "INSERT INTO library_evidence"
                " (member_id, window_id, root, n_ticks, n_fills, net_usd_0, net_usd_tier,"
                "  max_dd_usd, regime_word_mode, judged, detail_version, ts)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
                " ON CONFLICT(member_id, window_id) DO UPDATE SET"
                " root = excluded.root, n_ticks = excluded.n_ticks, n_fills = excluded.n_fills,"
                " net_usd_0 = excluded.net_usd_0, net_usd_tier = excluded.net_usd_tier,"
                " max_dd_usd = excluded.max_dd_usd, regime_word_mode = excluded.regime_word_mode,"
                " judged = excluded.judged, detail_version = excluded.detail_version,"
                " ts = excluded.ts",
                (
                    member_id,
                    window_id,
                    root,
                    n_ticks,
                    n_fills,
                    net_usd_0,
                    net_usd_tier,
                    max_dd_usd,
                    regime_word_mode,
                    1 if judged else 0,
                    detail_version,
                    stamp,
                ),
            )

    def evidence_for(self, member_id: str) -> list[EvidenceRow]:
        cur = self._conn.execute(
            "SELECT member_id, window_id, root, n_ticks, n_fills, net_usd_0, net_usd_tier,"
            " max_dd_usd, regime_word_mode, judged, detail_version, ts"
            " FROM library_evidence WHERE member_id = ? ORDER BY window_id",
            (member_id,),
        )
        out: list[EvidenceRow] = []
        for r in cur.fetchall():
            out.append(
                EvidenceRow(
                    member_id=str(r[0]),
                    window_id=str(r[1]),
                    root=str(r[2]),
                    n_ticks=int(r[3]),
                    n_fills=int(r[4]),
                    net_usd_0=float(r[5]),
                    net_usd_tier=float(r[6]),
                    max_dd_usd=float(r[7]),
                    regime_word_mode=str(r[8]),
                    judged=bool(r[9]),
                    detail_version=int(r[10]),
                    ts=int(r[11]),
                )
            )
        return out

    def composition_insert(  # noqa: PLR0913, PLR0917 — one parameter per column, deliberately
        self,
        table_hash: str,
        hash128: str,
        member_ids: list[str],
        words: dict[str, str],
        path: str,
        gate: dict[str, object] | None,
        ts: int | None = None,
    ) -> None:
        """Insert or refresh a composition row (a re-compose landing on
        the same hash refreshes `composed_ts`/`gate_json`, keeps the
        stage/commit stamps)."""
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            self._conn.execute(
                "INSERT INTO compositions"
                " (table_hash, hash128, member_ids_json, words_json, path, gate_json,"
                "  composed_ts, staged_ts, committed_ts)"
                " VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL)"
                " ON CONFLICT(table_hash) DO UPDATE SET"
                " member_ids_json = excluded.member_ids_json, words_json = excluded.words_json,"
                " path = excluded.path, gate_json = excluded.gate_json,"
                " composed_ts = excluded.composed_ts",
                (
                    table_hash,
                    hash128,
                    json.dumps(member_ids, separators=(",", ":")),
                    json.dumps(words, sort_keys=True, separators=(",", ":")),
                    path,
                    None if gate is None else json.dumps(gate, sort_keys=True, separators=(",", ":")),
                    stamp,
                ),
            )

    def composition_mark(self, table_hash: str, column: str, ts: int | None = None) -> None:
        """Stamp `staged_ts` or `committed_ts` on a composition row."""
        if column not in ("staged_ts", "committed_ts"):
            raise StateError(f"composition_mark: bad column {column!r}")
        stamp = int(time.time()) if ts is None else ts
        with self._conn:
            cur = self._conn.execute(
                f"UPDATE compositions SET {column} = ? WHERE table_hash = ?",  # column whitelisted above
                (stamp, table_hash),
            )
        if cur.rowcount != 1:
            raise StateError(f"composition_mark: no composition {table_hash}")

    @staticmethod
    def _composition_from_row(row: tuple[typing.Any, ...]) -> CompositionRow:
        ids_raw = json.loads(str(row[2]))
        words_raw = json.loads(str(row[3]))
        gate_raw = None if row[5] is None else json.loads(str(row[5]))
        return CompositionRow(
            table_hash=str(row[0]),
            hash128=str(row[1]),
            member_ids=[str(i) for i in ids_raw] if isinstance(ids_raw, list) else [],
            words={str(k): str(v) for k, v in words_raw.items()} if isinstance(words_raw, dict) else {},
            path=str(row[4]),
            gate=gate_raw if isinstance(gate_raw, dict) else None,
            composed_ts=int(row[6]),
            staged_ts=None if row[7] is None else int(row[7]),
            committed_ts=None if row[8] is None else int(row[8]),
        )

    def composition_row(self, table_hash: str) -> CompositionRow | None:
        row = self._conn.execute(
            _COMPOSITION_SELECT + " WHERE table_hash = ?", (table_hash,)
        ).fetchone()
        return None if row is None else self._composition_from_row(tuple(row))

    def compositions(self) -> list[CompositionRow]:
        """Every composition, most recently composed first."""
        cur = self._conn.execute(_COMPOSITION_SELECT + " ORDER BY composed_ts DESC, table_hash")
        return [self._composition_from_row(tuple(r)) for r in cur.fetchall()]

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
