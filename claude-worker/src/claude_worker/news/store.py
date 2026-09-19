# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""``news.db`` — the NEWS lane's own SQLite store (spec §4.3, §6).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Ruling Q1: a THIRD store, beside ``state.db`` (the control plane: the seq
allocator and the prompt cache) and ``candles.db``. The two are opened side
by side and never merged — this lane adds no table to ``state.db``, so a
news-lane schema change can never disturb the seq namespace the engine's
``SeqPolicy`` primes on.

Writers, by phase (spec §4.3): the aggregator writes
``sources``/``items``/``snapshots``/``series``/``events``; the cascade writes
``triage``/``stories``/``labels``/``assessments``/``budget``; ``actions.py``
writes ``actions``; ``resolve.py`` writes ``resolutions``. One handle per
lane run, single writer, WAL — the same discipline as ``State``.

The schema is additive ``CREATE TABLE IF NOT EXISTS`` only: a later phase
adds consumers, never a migration.
"""

import dataclasses
import datetime
import json
import pathlib
import sqlite3
import time
import typing

_SCHEMA: tuple[str, ...] = (
    """
    CREATE TABLE IF NOT EXISTS sources (
        name                  TEXT PRIMARY KEY,
        kind                  TEXT NOT NULL,
        class                 TEXT NOT NULL,
        origin                TEXT NOT NULL,
        enabled               INTEGER NOT NULL,
        last_ok_ts            INTEGER NOT NULL DEFAULT 0,
        last_err_ts           INTEGER NOT NULL DEFAULT 0,
        err_streak            INTEGER NOT NULL DEFAULT 0,
        polls_total           INTEGER NOT NULL DEFAULT 0,
        polls_ok              INTEGER NOT NULL DEFAULT 0,
        items_total           INTEGER NOT NULL DEFAULT 0,
        refused_origin_total  INTEGER NOT NULL DEFAULT 0,
        budget_skips_total    INTEGER NOT NULL DEFAULT 0,
        last_error            TEXT NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS items (
        source       TEXT NOT NULL,
        guid         TEXT NOT NULL,
        ts           INTEGER NOT NULL,
        fetched_ts   INTEGER NOT NULL,
        title        TEXT NOT NULL,
        link         TEXT NOT NULL,
        text         TEXT NOT NULL,
        origin       TEXT NOT NULL,
        class        TEXT NOT NULL,
        weight       REAL NOT NULL,
        venue        TEXT NOT NULL DEFAULT '',
        tier0        TEXT NOT NULL,
        dup_of       TEXT NOT NULL DEFAULT '',
        vocab_hits   INTEGER NOT NULL DEFAULT 0,
        triage_state TEXT NOT NULL DEFAULT 'new',
        story_id     TEXT NOT NULL DEFAULT '',
        hint         TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (source, guid)
    )
    """,
    "CREATE INDEX IF NOT EXISTS items_state ON items (triage_state, ts)",
    "CREATE INDEX IF NOT EXISTS items_ts ON items (ts)",
    """
    CREATE TABLE IF NOT EXISTS snapshots (
        source   TEXT NOT NULL,
        taken_ts INTEGER NOT NULL,
        sha256   TEXT NOT NULL,
        count    INTEGER NOT NULL,
        body     TEXT NOT NULL,
        PRIMARY KEY (source, taken_ts)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS series (
        source TEXT NOT NULL,
        key    TEXT NOT NULL,
        ts     INTEGER NOT NULL,
        value  REAL NOT NULL,
        PRIMARY KEY (source, key, ts)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS events (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        kind        TEXT NOT NULL,
        venue       TEXT NOT NULL,
        descriptor  TEXT NOT NULL DEFAULT '',
        instrument  TEXT NOT NULL DEFAULT '',
        at_ts       INTEGER NOT NULL,
        until_ts    INTEGER NOT NULL DEFAULT 0,
        source      TEXT NOT NULL,
        detail      TEXT NOT NULL,
        created_ts  INTEGER NOT NULL,
        dedupe_key  TEXT NOT NULL UNIQUE
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS triage (
        source         TEXT NOT NULL,
        guid           TEXT NOT NULL,
        model          TEXT NOT NULL,
        prompt_version TEXT NOT NULL,
        cache_hit      INTEGER NOT NULL,
        family         TEXT NOT NULL,
        impact         TEXT NOT NULL,
        reason         TEXT NOT NULL,
        event_type     TEXT NOT NULL,
        venues         TEXT NOT NULL,
        assets         TEXT NOT NULL,
        ts             INTEGER NOT NULL,
        PRIMARY KEY (source, guid)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS stories (
        story_id     TEXT PRIMARY KEY,
        family       TEXT NOT NULL,
        event_type   TEXT NOT NULL,
        venues       TEXT NOT NULL,
        assets       TEXT NOT NULL,
        first_ts     INTEGER NOT NULL,
        last_ts      INTEGER NOT NULL,
        item_count   INTEGER NOT NULL,
        origins      INTEGER NOT NULL,
        venue_origin INTEGER NOT NULL,
        max_impact   TEXT NOT NULL,
        state        TEXT NOT NULL,
        assessments  INTEGER NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS labels (
        story_id       TEXT PRIMARY KEY,
        model          TEXT NOT NULL,
        prompt_version TEXT NOT NULL,
        cache_hit      INTEGER NOT NULL,
        market         TEXT NOT NULL,
        sym            INTEGER NOT NULL,
        descriptor     TEXT NOT NULL,
        venue          INTEGER NOT NULL,
        direction      TEXT NOT NULL,
        confidence     REAL NOT NULL,
        half_life_s    REAL NOT NULL,
        vol            TEXT NOT NULL,
        liquidity      TEXT NOT NULL,
        ts             INTEGER NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS resolutions (
        subject_kind        TEXT NOT NULL,
        subject_id          TEXT NOT NULL,
        t0                  INTEGER NOT NULL,
        t1                  INTEGER NOT NULL,
        horizon_s           INTEGER NOT NULL,
        state               TEXT NOT NULL,
        descriptor          TEXT NOT NULL DEFAULT '',
        venue               INTEGER NOT NULL DEFAULT 0,
        px0_1e6             INTEGER NOT NULL DEFAULT 0,
        px1_1e6             INTEGER NOT NULL DEFAULT 0,
        fwd_bps             REAL NOT NULL DEFAULT 0,
        signed_bps          REAL NOT NULL DEFAULT 0,
        hit                 INTEGER NOT NULL DEFAULT 0,
        direction           TEXT NOT NULL DEFAULT '',
        confidence          REAL NOT NULL DEFAULT 0,
        vol_claimed         INTEGER NOT NULL DEFAULT 0,
        vol_reached_high    INTEGER NOT NULL DEFAULT 0,
        rv_ratio            REAL NOT NULL DEFAULT 0,
        filled              INTEGER NOT NULL DEFAULT 0,
        markout_bps         REAL NOT NULL DEFAULT 0,
        price_source        TEXT NOT NULL DEFAULT '',
        resolved_ts         INTEGER NOT NULL DEFAULT 0,
        detail              TEXT NOT NULL DEFAULT '',
        PRIMARY KEY (subject_kind, subject_id)
    )
    """,
    "CREATE INDEX IF NOT EXISTS resolutions_due ON resolutions (state, t1)",
    """
    CREATE TABLE IF NOT EXISTS assessments (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        story_id       TEXT NOT NULL,
        model          TEXT NOT NULL,
        prompt_version TEXT NOT NULL,
        cache_hit      INTEGER NOT NULL,
        body           TEXT NOT NULL,
        input_tokens   INTEGER NOT NULL,
        output_tokens  INTEGER NOT NULL,
        ts             INTEGER NOT NULL
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS actions (
        id             INTEGER PRIMARY KEY AUTOINCREMENT,
        ts             INTEGER NOT NULL,
        story_id       TEXT NOT NULL DEFAULT '',
        event_id       INTEGER NOT NULL DEFAULT 0,
        kind           TEXT NOT NULL,
        mode           TEXT NOT NULL,
        refused_reason TEXT NOT NULL DEFAULT '',
        frame_kind     INTEGER NOT NULL DEFAULT 0,
        sym            INTEGER NOT NULL DEFAULT 0,
        px             INTEGER NOT NULL DEFAULT 0,
        qty            INTEGER NOT NULL DEFAULT 0,
        ttl_ns         INTEGER NOT NULL DEFAULT 0,
        venue          INTEGER NOT NULL DEFAULT 0,
        strategy_id    INTEGER NOT NULL DEFAULT 0,
        side           INTEGER NOT NULL DEFAULT 0,
        param_id       INTEGER NOT NULL DEFAULT 0,
        flags          INTEGER NOT NULL DEFAULT 0,
        seq            INTEGER NOT NULL DEFAULT 0,
        detail         TEXT NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS budget (
        day           TEXT NOT NULL,
        tier          TEXT NOT NULL,
        calls         INTEGER NOT NULL,
        input_tokens  INTEGER NOT NULL,
        output_tokens INTEGER NOT NULL,
        skipped       INTEGER NOT NULL,
        PRIMARY KEY (day, tier)
    )
    """,
    """
    CREATE TABLE IF NOT EXISTS counters (
        name  TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    )
    """,
)

#: ``items.tier0`` vocabulary (spec §4.3). ``pass`` is the only value that
#: reaches a model.
TIER0_PASS: str = "pass"
TIER0_DROP_DUP: str = "drop_dup"
TIER0_DROP_VOCAB: str = "drop_vocab"
TIER0_DROP_WEIGHT: str = "drop_weight"
TIER0_DROP_CAP: str = "drop_cap"
TIER0_VALUES: tuple[str, ...] = (
    TIER0_PASS,
    TIER0_DROP_DUP,
    TIER0_DROP_VOCAB,
    TIER0_DROP_WEIGHT,
    TIER0_DROP_CAP,
)

#: ``items.triage_state`` vocabulary.
STATE_NEW: str = "new"
STATE_TRIAGED: str = "triaged"
STATE_ESCALATED: str = "escalated"
STATE_LABELED: str = "labeled"
STATE_ASSESSED: str = "assessed"
STATE_SKIPPED: str = "skipped"

#: Columns added after a store already existed in the wild. SQLite adds a
#: column with a constant DEFAULT as METADATA only — no table rewrite, no
#: row touched, O(1) whatever the row count — so this stays inside the
#: spirit of the §4.3 "additive only" rule while letting a live `news.db`
#: gain a field. Each entry is ``(table, column, DDL)`` and is applied only
#: when the column is absent; the DDL must never drop or rewrite anything.
#: Operator ruling 2026-09-20 (D25): the class-B `hint` is stored, so the
#: cascade can read a venue's own word for what it announced instead of
#: paying a model to guess at it.
_MIGRATIONS: tuple[tuple[str, str, str], ...] = (
    ("items", "hint", "ALTER TABLE items ADD COLUMN hint TEXT NOT NULL DEFAULT ''"),
)

#: ``stories.state`` vocabulary (spec §9.2).
STORY_OPEN: str = "open"
STORY_LABELED: str = "labeled"
STORY_ASSESSED: str = "assessed"
STORY_CLOSED: str = "closed"

#: ``prune`` drops only items that fed nothing (spec §6).
_PRUNABLE_STATES: tuple[str, ...] = (STATE_SKIPPED, STATE_TRIAGED)

SECONDS_PER_DAY: int = 86_400

#: Named counters (spec §4.3 "policy_invalid, registry_invalid, …").
COUNTER_POLICY_INVALID: str = "policy_invalid"
COUNTER_REGISTRY_INVALID: str = "registry_invalid"
COUNTER_PARSE_EMPTY: str = "parse_empty"


class StoreError(Exception):
    """``news.db`` could not be opened in the mode this lane requires."""


@dataclasses.dataclass(frozen=True, slots=True)
class PollOutcome:
    """One fetch's effect on the ``sources`` row.

    ``ok`` drives ``polls_ok``/``last_ok_ts`` and CLEARS the error streak;
    the two refusal counters are separate because the N1 gate reads them
    separately (``refused_origin_total = 0`` is a hard gate; budget skips
    are a tuning signal, not a failure).
    """

    ok: bool = False
    refused_origin: bool = False
    budget_skip: bool = False
    error: str = ""


def day_of(ts: int) -> str:
    """The UTC day key used by the ``budget`` table."""
    return datetime.datetime.fromtimestamp(ts, tz=datetime.UTC).strftime("%Y-%m-%d")


class Store:
    """One handle on ``news.db``. Single writer; every multi-row write is one
    transaction (``with self._conn``)."""

    def __init__(self, db_path: pathlib.Path) -> None:
        db_path.parent.mkdir(parents=True, exist_ok=True)
        self._conn: sqlite3.Connection = sqlite3.connect(str(db_path))
        mode = self._conn.execute("PRAGMA journal_mode = WAL").fetchone()[0]
        if str(mode).lower() != "wal":
            self._conn.close()
            raise StoreError(f"SQLite WAL mode unavailable for {db_path}: got {mode!r}")
        with self._conn:
            for i in range(len(_SCHEMA)):
                self._conn.execute(_SCHEMA[i])
        self._migrate()

    def _migrate(self) -> None:
        """Apply the additive column migrations this store has outgrown.

        Idempotent and cheap: a column already present is skipped, and the
        ones listed are metadata-only ``ADD COLUMN``s with constant
        defaults. A store that has never been opened by an older build
        does nothing here at all.
        """
        for i in range(len(_MIGRATIONS)):
            table, column, ddl = _MIGRATIONS[i]
            rows = self._conn.execute(f"PRAGMA table_info({table})").fetchall()
            names: set[str] = set()
            for j in range(len(rows)):
                names.add(str(rows[j][1]))
            if column in names:
                continue
            with self._conn:
                self._conn.execute(ddl)

    def close(self) -> None:
        """Flush and close. Idempotent."""
        self._conn.close()

    def __enter__(self) -> "Store":
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    # ---- sources -------------------------------------------------------

    def upsert_source(
        self, name: str, kind: str, class_: str, origin: str, enabled: int
    ) -> None:
        """Register (or re-register) a source. The counters are NEVER reset:
        a registry edit must not erase the poll history the N1 gate reads."""
        with self._conn:
            self._conn.execute(
                """
                INSERT INTO sources (name, kind, class, origin, enabled)
                VALUES (?, ?, ?, ?, ?)
                ON CONFLICT (name) DO UPDATE SET
                    kind = excluded.kind, class = excluded.class,
                    origin = excluded.origin, enabled = excluded.enabled
                """,
                (name, kind, class_, origin, int(enabled)),
            )

    def record_poll(self, name: str, ts: int, outcome: PollOutcome) -> None:
        """Fold one fetch attempt into the source's counters."""
        with self._conn:
            self._conn.execute(
                """
                UPDATE sources SET
                    polls_total = polls_total + 1,
                    polls_ok = polls_ok + ?,
                    last_ok_ts = CASE WHEN ? THEN ? ELSE last_ok_ts END,
                    last_err_ts = CASE WHEN ? THEN ? ELSE last_err_ts END,
                    err_streak = CASE WHEN ? THEN 0 ELSE err_streak + 1 END,
                    refused_origin_total = refused_origin_total + ?,
                    budget_skips_total = budget_skips_total + ?,
                    last_error = CASE WHEN ? THEN last_error ELSE ? END
                WHERE name = ?
                """,
                (
                    1 if outcome.ok else 0,
                    1 if outcome.ok else 0,
                    ts,
                    0 if outcome.ok else 1,
                    ts,
                    1 if outcome.ok else 0,
                    1 if outcome.refused_origin else 0,
                    1 if outcome.budget_skip else 0,
                    1 if outcome.ok else 0,
                    outcome.error,
                    name,
                ),
            )

    def add_items_total(self, name: str, count: int) -> None:
        with self._conn:
            self._conn.execute(
                "UPDATE sources SET items_total = items_total + ? WHERE name = ?", (count, name)
            )

    def source_rows(self) -> list[dict[str, object]]:
        return self._rows("SELECT * FROM sources ORDER BY name", ())

    # ---- items ---------------------------------------------------------

    def upsert_item(  # noqa: PLR0913 — one row of the items table, field for field
        self,
        *,
        source: str,
        guid: str,
        ts: int,
        fetched_ts: int,
        title: str,
        link: str,
        text: str,
        origin: str,
        class_: str,
        weight: float,
        venue: str = "",
        tier0: str = TIER0_PASS,
        vocab_hits: int = 0,
        hint: str = "",
    ) -> bool:
        """Insert a first sighting. Returns False when ``(source, guid)`` was
        already stored — the dedupe that keeps a re-polled feed from paying
        for the same item twice."""
        with self._conn:
            cursor = self._conn.execute(
                """
                INSERT OR IGNORE INTO items
                    (source, guid, ts, fetched_ts, title, link, text, origin, class,
                     weight, venue, tier0, vocab_hits, hint)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    source,
                    guid,
                    ts,
                    fetched_ts,
                    title,
                    link,
                    text,
                    origin,
                    class_,
                    weight,
                    venue,
                    tier0,
                    vocab_hits,
                    hint,
                ),
            )
        return cursor.rowcount > 0

    def mark_tier0(self, source: str, guid: str, tier0: str, dup_of: str = "") -> None:
        if tier0 not in TIER0_VALUES:
            raise ValueError(f"unknown tier0 verdict {tier0!r}")
        with self._conn:
            self._conn.execute(
                "UPDATE items SET tier0 = ?, dup_of = ? WHERE source = ? AND guid = ?",
                (tier0, dup_of, source, guid),
            )

    def set_triage_state(self, source: str, guid: str, state: str, story_id: str = "") -> None:
        with self._conn:
            self._conn.execute(
                """
                UPDATE items SET triage_state = ?,
                    story_id = CASE WHEN ? = '' THEN story_id ELSE ? END
                WHERE source = ? AND guid = ?
                """,
                (state, story_id, story_id, source, guid),
            )

    def item(self, source: str, guid: str) -> dict[str, object] | None:
        rows = self._rows("SELECT * FROM items WHERE source = ? AND guid = ?", (source, guid))
        return rows[0] if rows else None

    def items_since(self, since_ts: int, tier0: str = "") -> list[dict[str, object]]:
        if tier0:
            return self._rows(
                "SELECT * FROM items WHERE ts >= ? AND tier0 = ? ORDER BY ts",
                (since_ts, tier0),
            )
        return self._rows("SELECT * FROM items WHERE ts >= ? ORDER BY ts", (since_ts,))

    def items_pending_triage(self, limit: int) -> list[dict[str, object]]:
        """Tier-0 survivors no model has seen yet, OLDEST first.

        Oldest first on purpose: a backlog drains in the order the world
        happened, so a burst never starves the item that started it.
        """
        return self._rows(
            """
            SELECT * FROM items
            WHERE triage_state = ? AND tier0 = ?
            ORDER BY ts LIMIT ?
            """,
            (STATE_NEW, TIER0_PASS, limit),
        )

    def items_to_cluster(self, limit: int) -> list[dict[str, object]]:
        """Triaged items that reached a clusterable impact and have not
        been attached to a story yet, with their triage joined on.

        One query, and self-healing: an item whose pass ended before
        clustering (a spent budget, a crash) is picked up by the next one
        rather than stranded between two tables.
        """
        return self._rows(
            """
            SELECT i.source AS source, i.guid AS guid, i.ts AS ts, i.title AS title,
                   i.text AS text, i.origin AS origin, i.weight AS weight, i.venue AS venue,
                   i.hint AS hint,
                   t.family AS family, t.event_type AS event_type, t.impact AS impact,
                   t.venues AS venues, t.assets AS assets
            FROM items AS i JOIN triage AS t ON t.source = i.source AND t.guid = i.guid
            WHERE i.triage_state = ? AND i.story_id = ''
            ORDER BY i.ts LIMIT ?
            """,
            (STATE_ESCALATED, limit),
        )

    def story_items(self, story_id: str, limit: int = 0) -> list[dict[str, object]]:
        """Every item attached to a story, newest first."""
        if limit > 0:
            return self._rows(
                "SELECT * FROM items WHERE story_id = ? ORDER BY ts DESC LIMIT ?",
                (story_id, limit),
            )
        return self._rows(
            "SELECT * FROM items WHERE story_id = ? ORDER BY ts DESC", (story_id,)
        )

    def stories_open(self, since_ts: int) -> list[dict[str, object]]:
        """Stories still gathering items, oldest first. The clustering
        window bounds the scan, so this never grows with the table."""
        return self._rows(
            "SELECT * FROM stories WHERE state != ? AND last_ts >= ? ORDER BY first_ts",
            (STORY_CLOSED, since_ts),
        )

    def stories_unlabeled(self, since_ts: int) -> list[dict[str, object]]:
        """Open stories that have no label yet — tier 2's queue."""
        return self._rows(
            """
            SELECT s.* FROM stories AS s
            LEFT JOIN labels AS l ON l.story_id = s.story_id
            WHERE l.story_id IS NULL AND s.state = ? AND s.last_ts >= ?
            ORDER BY s.first_ts
            """,
            (STORY_OPEN, since_ts),
        )

    def close_stale_stories(self, before_ts: int) -> int:
        """A story with no item for a whole window is finished (spec §9.2).
        Returns how many closed."""
        with self._conn:
            cursor = self._conn.execute(
                "UPDATE stories SET state = ? WHERE state != ? AND last_ts < ?",
                (STORY_CLOSED, STORY_CLOSED, before_ts),
            )
        return max(0, cursor.rowcount)

    # ---- snapshots / series --------------------------------------------

    def insert_snapshot(
        self, source: str, taken_ts: int, sha256: str, count: int, body: str
    ) -> None:
        with self._conn:
            self._conn.execute(
                """
                INSERT OR REPLACE INTO snapshots (source, taken_ts, sha256, count, body)
                VALUES (?, ?, ?, ?, ?)
                """,
                (source, taken_ts, sha256, count, body),
            )

    def latest_snapshot(self, source: str) -> dict[str, object] | None:
        rows = self._rows(
            "SELECT * FROM snapshots WHERE source = ? ORDER BY taken_ts DESC LIMIT 1", (source,)
        )
        return rows[0] if rows else None

    def insert_series(self, source: str, key: str, ts: int, value: float) -> None:
        with self._conn:
            self._conn.execute(
                "INSERT OR REPLACE INTO series (source, key, ts, value) VALUES (?, ?, ?, ?)",
                (source, key, ts, value),
            )

    def series_tail(self, source: str, key: str, limit: int) -> list[tuple[int, float]]:
        cursor = self._conn.execute(
            "SELECT ts, value FROM series WHERE source = ? AND key = ? ORDER BY ts DESC LIMIT ?",
            (source, key, limit),
        )
        rows = cursor.fetchall()
        out: list[tuple[int, float]] = []
        for i in range(len(rows) - 1, -1, -1):
            out.append((int(rows[i][0]), float(rows[i][1])))
        return out

    # ---- events --------------------------------------------------------

    def insert_event(  # noqa: PLR0913 — one row of the events table, field for field
        self,
        *,
        kind: str,
        venue: str,
        at_ts: int,
        source: str,
        detail: str,
        created_ts: int,
        descriptor: str = "",
        instrument: str = "",
        until_ts: int = 0,
    ) -> int | None:
        """Insert one structural event. ``None`` means the dedupe key was
        already present — the same transition is never recorded twice, which
        is what makes the detectors safe to re-run over one snapshot pair."""
        dedupe_key = f"{kind}|{venue}|{instrument}|{at_ts}"
        with self._conn:
            cursor = self._conn.execute(
                """
                INSERT OR IGNORE INTO events
                    (kind, venue, descriptor, instrument, at_ts, until_ts, source, detail,
                     created_ts, dedupe_key)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                """,
                (
                    kind,
                    venue,
                    descriptor,
                    instrument,
                    at_ts,
                    until_ts,
                    source,
                    detail,
                    created_ts,
                    dedupe_key,
                ),
            )
        return int(typing.cast(int, cursor.lastrowid)) if cursor.rowcount > 0 else None

    def events_since(self, since_ts: int) -> list[dict[str, object]]:
        return self._rows("SELECT * FROM events WHERE at_ts >= ? ORDER BY at_ts", (since_ts,))

    def open_events(
        self, kind: str, venue: str, instrument: str = ""
    ) -> list[dict[str, object]]:
        """Rows of ``kind`` on ``venue`` that carry NO end yet.

        The detectors' one case the dedupe key cannot cover: an outage a
        venue publishes as a flag has no ``begin`` to key on, so it is
        opened once and closed by an update (spec §8.2, "never a second
        row"). This is both the guard against that second row and the
        lookup that finds the row to close.
        """
        return self._rows(
            """
            SELECT * FROM events
            WHERE kind = ? AND venue = ? AND instrument = ? AND until_ts = 0
            ORDER BY id
            """,
            (kind, venue, instrument),
        )

    def close_event(self, event_id: int, until_ts: int) -> bool:
        """End an open event. False when it was already closed — closing is
        idempotent, so a venue reporting clear twice writes once."""
        with self._conn:
            cursor = self._conn.execute(
                "UPDATE events SET until_ts = ? WHERE id = ? AND until_ts = 0",
                (until_ts, event_id),
            )
        return cursor.rowcount > 0

    def events_in_force(self, kind: str, now_ts: int) -> list[dict[str, object]]:
        """Rows of ``kind`` whose window covers ``now_ts``: begun, and
        either open-ended or not yet ended."""
        return self._rows(
            """
            SELECT * FROM events
            WHERE kind = ? AND at_ts <= ? AND (until_ts = 0 OR until_ts > ?)
            ORDER BY id
            """,
            (kind, now_ts, now_ts),
        )

    # ---- cascade -------------------------------------------------------

    def put_triage(self, row: typing.Mapping[str, object]) -> None:
        self._insert_mapping("triage", row, replace=True)

    def upsert_story(self, row: typing.Mapping[str, object]) -> None:
        self._insert_mapping("stories", row, replace=True)

    def story(self, story_id: str) -> dict[str, object] | None:
        rows = self._rows("SELECT * FROM stories WHERE story_id = ?", (story_id,))
        return rows[0] if rows else None

    def put_label(self, row: typing.Mapping[str, object]) -> None:
        self._insert_mapping("labels", row, replace=True)

    def label(self, story_id: str) -> dict[str, object] | None:
        rows = self._rows("SELECT * FROM labels WHERE story_id = ?", (story_id,))
        return rows[0] if rows else None

    def insert_assessment(self, row: typing.Mapping[str, object]) -> int:
        return self._insert_mapping("assessments", row, replace=False)

    def record_action(self, row: typing.Mapping[str, object]) -> int:
        return self._insert_mapping("actions", row, replace=False)

    def update_action(self, action_id: int, *, seq: int, detail: str = "") -> None:
        """Close the record-then-send loop: the row exists before the frame
        goes out, and this is what says whether it did."""
        with self._conn:
            self._conn.execute(
                "UPDATE actions SET seq = ?, detail = ? WHERE id = ?", (seq, detail, action_id)
            )

    def actions_since(self, since_ts: int) -> list[dict[str, object]]:
        return self._rows("SELECT * FROM actions WHERE ts >= ? ORDER BY id", (since_ts,))

    def put_resolution(self, row: typing.Mapping[str, object]) -> None:
        self._insert_mapping("resolutions", row, replace=True)

    def resolutions_due(self, now_ts: int, state: str = "pending") -> list[dict[str, object]]:
        return self._rows(
            "SELECT * FROM resolutions WHERE state = ? AND t1 <= ? ORDER BY t1",
            (state, now_ts),
        )

    def resolutions_resolved(self, since_ts: int) -> list[dict[str, object]]:
        """Resolved rows in a window, with the label's model joined on.

        The join is what keeps `by_model` honest: the `session` brain and
        the automated one are never pooled silently, and a resolution
        whose claim came from neither carries an empty model rather than
        being attributed to one.
        """
        return self._rows(
            """
            SELECT r.*, COALESCE(l.model, '') AS model
            FROM resolutions AS r
            LEFT JOIN labels AS l
              ON l.story_id = r.subject_id AND r.subject_kind = 'label'
            WHERE r.state = 'resolved' AND r.resolved_ts >= ?
            ORDER BY r.resolved_ts
            """,
            (since_ts,),
        )

    def resolution_states(self) -> dict[str, int]:
        cursor = self._conn.execute(
            "SELECT state, COUNT(*) FROM resolutions GROUP BY state"
        )
        rows = cursor.fetchall()
        out: dict[str, int] = {}
        for i in range(len(rows)):
            out[str(rows[i][0])] = int(rows[i][1])
        return out

    def unresolvable_reasons(self, limit: int = 3) -> list[tuple[str, int]]:
        cursor = self._conn.execute(
            """
            SELECT detail, COUNT(*) AS n FROM resolutions
            WHERE state = 'unresolvable' GROUP BY detail ORDER BY n DESC LIMIT ?
            """,
            (limit,),
        )
        rows = cursor.fetchall()
        out: list[tuple[str, int]] = []
        for i in range(len(rows)):
            out.append((str(rows[i][0]), int(rows[i][1])))
        return out

    def funnel_since(self, since_ts: int) -> dict[str, int]:
        """Tier-0 verdict counts in a window — the funnel, in one pass."""
        cursor = self._conn.execute(
            "SELECT tier0, COUNT(*) FROM items WHERE ts >= ? GROUP BY tier0", (since_ts,)
        )
        rows = cursor.fetchall()
        out: dict[str, int] = {}
        for i in range(len(rows)):
            out[str(rows[i][0])] = int(rows[i][1])
        return out

    def source_funnel_since(self, since_ts: int) -> list[dict[str, object]]:
        """Per-source item counts and pass rate in a window."""
        return self._rows(
            """
            SELECT source,
                   COUNT(*) AS items,
                   SUM(CASE WHEN tier0 = ? THEN 1 ELSE 0 END) AS passed
            FROM items WHERE ts >= ? GROUP BY source ORDER BY source
            """,
            (TIER0_PASS, since_ts),
        )

    def story_first_sources(self, since_ts: int) -> list[dict[str, object]]:
        """For each story, the source of its EARLIEST full-weight item, and
        whether the story later reached two independent origins.

        This is the number that says which sources are worth their poll:
        being first matters only if somebody else confirms it.
        """
        return self._rows(
            """
            SELECT s.story_id AS story_id, s.origins AS origins,
                   (SELECT i.source FROM items AS i
                    WHERE i.story_id = s.story_id AND i.weight >= 1.0
                    ORDER BY i.ts LIMIT 1) AS first_source
            FROM stories AS s WHERE s.first_ts >= ?
            """,
            (since_ts,),
        )

    # ---- budget / counters ---------------------------------------------

    def budget_add(  # noqa: PLR0913
        self,
        day: str,
        tier: str,
        *,
        calls: int = 0,
        input_tokens: int = 0,
        output_tokens: int = 0,
        skipped: int = 0,
    ) -> None:
        """Fold one tier's spend into today's row. Wide by construction: the
        budget table's four counters are the whole point of the call."""
        with self._conn:
            self._conn.execute(
                """
                INSERT INTO budget (day, tier, calls, input_tokens, output_tokens, skipped)
                VALUES (?, ?, ?, ?, ?, ?)
                ON CONFLICT (day, tier) DO UPDATE SET
                    calls = calls + excluded.calls,
                    input_tokens = input_tokens + excluded.input_tokens,
                    output_tokens = output_tokens + excluded.output_tokens,
                    skipped = skipped + excluded.skipped
                """,
                (day, tier, calls, input_tokens, output_tokens, skipped),
            )

    def budget_today(self, tier: str, now_ts: int | None = None) -> dict[str, int]:
        day = day_of(int(time.time()) if now_ts is None else now_ts)
        rows = self._rows("SELECT * FROM budget WHERE day = ? AND tier = ?", (day, tier))
        if not rows:
            return {"calls": 0, "input_tokens": 0, "output_tokens": 0, "skipped": 0}
        row = rows[0]
        return {
            "calls": int(typing.cast(int, row["calls"])),
            "input_tokens": int(typing.cast(int, row["input_tokens"])),
            "output_tokens": int(typing.cast(int, row["output_tokens"])),
            "skipped": int(typing.cast(int, row["skipped"])),
        }

    def counter_inc(self, name: str, delta: int = 1) -> None:
        with self._conn:
            self._conn.execute(
                """
                INSERT INTO counters (name, value) VALUES (?, ?)
                ON CONFLICT (name) DO UPDATE SET value = value + excluded.value
                """,
                (name, delta),
            )

    def counters(self) -> dict[str, int]:
        cursor = self._conn.execute("SELECT name, value FROM counters ORDER BY name")
        rows = cursor.fetchall()
        out: dict[str, int] = {}
        for i in range(len(rows)):
            out[str(rows[i][0])] = int(rows[i][1])
        return out

    # ---- retention -----------------------------------------------------

    def prune(self, now_ts: int, items_days: int, snapshots_days: int) -> dict[str, int]:
        """Spec §6: drop items that fed NOTHING (never a story member), and
        every snapshot past its window except the newest per source — the
        newest is the diff baseline and deleting it would fabricate a
        transition on the next cycle."""
        items_cutoff = now_ts - items_days * SECONDS_PER_DAY
        snapshots_cutoff = now_ts - snapshots_days * SECONDS_PER_DAY
        placeholders = ", ".join("?" for _ in _PRUNABLE_STATES)
        with self._conn:
            items_cursor = self._conn.execute(
                f"""
                DELETE FROM items
                WHERE ts < ? AND tier0 != ? AND story_id = ''
                  AND triage_state IN ({placeholders})
                """,
                (items_cutoff, "pass", *_PRUNABLE_STATES),
            )
            items_deleted = items_cursor.rowcount
            snapshots_cursor = self._conn.execute(
                """
                DELETE FROM snapshots
                WHERE taken_ts < ?
                  AND taken_ts < (
                      SELECT MAX(newer.taken_ts) FROM snapshots AS newer
                      WHERE newer.source = snapshots.source
                  )
                """,
                (snapshots_cutoff,),
            )
            snapshots_deleted = snapshots_cursor.rowcount
        return {"items": max(0, items_deleted), "snapshots": max(0, snapshots_deleted)}

    # ---- internals -----------------------------------------------------

    def _rows(self, sql: str, params: tuple[object, ...]) -> list[dict[str, object]]:
        cursor = self._conn.execute(sql, params)
        names: list[str] = []
        for i in range(len(cursor.description)):
            names.append(str(cursor.description[i][0]))
        raw = cursor.fetchall()
        out: list[dict[str, object]] = []
        for i in range(len(raw)):
            row: dict[str, object] = {}
            for j in range(len(names)):
                row[names[j]] = raw[i][j]
            out.append(row)
        return out

    def _insert_mapping(
        self, table: str, row: typing.Mapping[str, object], *, replace: bool
    ) -> int:
        keys = sorted(row)
        columns = ", ".join(keys)
        marks = ", ".join("?" for _ in keys)
        verb = "INSERT OR REPLACE" if replace else "INSERT"
        values: list[object] = []
        for i in range(len(keys)):
            value = row[keys[i]]
            values.append(json.dumps(value) if isinstance(value, (list, dict)) else value)
        with self._conn:
            cursor = self._conn.execute(
                f"{verb} INTO {table} ({columns}) VALUES ({marks})", tuple(values)
            )
        return int(typing.cast(int, cursor.lastrowid))
