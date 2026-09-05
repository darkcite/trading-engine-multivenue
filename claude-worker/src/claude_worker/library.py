# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""library — the strategy LIBRARY (RG4, docs/regime-and-dashboard-plan.md §5.2).

The unit is a *member*: a named row set (1..n VM rows sharing a thesis;
regime-specific variants of one signal are separate rows of the SAME
member) or a coded-member reference (``icdp@<sha256>``). A *table* is a
composition of members (``compose.py``). Reuse happens at member
granularity — the table hash was never a usable identity.

- ``member_id`` = sha256 of the canonical rows (``strategist.artifact_bytes``
  over ``parse_row``-canonical rows) — content-addressed like an artifact;
  labels / thesis / status are metadata and never change the id.
- Member files live under ``~/multivenue/worker/library/<member_id>.json``
  (``$CLAUDE_WORKER_LIBRARY_DIR``; config-carrying callers use
  ``library_dir_for(db_path)`` = the worker dir + ``library`` so tests never
  touch the operator's directory); ``state.db`` carries the index
  (``library``), the per-window evidence (``library_evidence``) and the
  table ↔ members link (``compositions``).
- ``labels`` = the member's declared fit set: a list of term lists with
  ∃-semantics (the member fits a word when SOME label allows it); ``[]``
  = ANY. Derived from the rows' ``regimes`` keys at ``add`` (a member with
  an unlabelled row is ANY — that row can always enter); the operator
  overrides with ``label``.
- Status law (operator ruling 2026-09-05): ``import-catalog`` validates ONLY
  the table the engine is running (the latest committed registry row);
  every other artifact — committed on a pre-2 h-law, stale-blind v2 root
  or never committed — enters as ``candidate`` with its hash + thesis
  preserved, until ``evidence`` on ≤ 2 h seeded v3 windows re-validates it.
- ``evidence`` = the frozen harness binary through the ADDITIVE
  ``backtest.run_harness_extra`` path: one run per (member, ≤ 2 h seeded
  window) with the carved ``0/100`` split (the whole window scored, the
  §8.3 monitor's precedent), the operator's fee tier and ``--emit-detail``;
  the row records the zero-fee AND tier nets, fills, ticks, the dominant
  fast word and whether the window was stale-JUDGED (v3) or blind (v2).

The ``rulesets`` table and the frozen ``stage-ruleset`` / ``commit-ruleset``
verbs are untouched — the library sits BEFORE them in the pipeline.
Module lane, not a Typer verb (the 8-verb surface stays as is):
``python -m claude_worker.library <lane>``.

Offline tool — allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import hashlib
import json
import os
import pathlib
import sys
import time
import tomllib
import typing

import claude_worker.backtest
import claude_worker.frames
import claude_worker.pnl_report
import claude_worker.regime
import claude_worker.state
import claude_worker.strategist

LIBRARY_DIR_ENV: str = "CLAUDE_WORKER_LIBRARY_DIR"
DEFAULT_LIBRARY_DIR: str = "~/multivenue/worker/library"
DB_ENV: str = "CLAUDE_WORKER_DB"
DEFAULT_DB: str = "~/multivenue/worker/state.db"
RULESET_DIR_ENV: str = "AI_RULESET_DIR"
DEFAULT_RULESET_DIR: str = "~/multivenue/artifacts/rulesets"
ICDP_PATH_ENV: str = "CLAUDE_WORKER_ICDP_TOML"
DEFAULT_ICDP_PATH: str = "~/multivenue/icdp.toml"
CANDIDATES_DIRNAME: str = "candidates"

KIND_VM_ROWS: str = "vm-rows"
KIND_CODED: str = "coded"
STATUS_CANDIDATE: str = "candidate"
STATUS_VALIDATED: str = "validated"
STATUS_RETIRED: str = "retired"
STATUSES: tuple[str, ...] = claude_worker.state.MEMBER_STATUSES
REGIME_OFF_VALUES: tuple[str, ...] = ("soft", "hard")
MEMBER_FILE_VERSION: int = 1
#: The evidence split — the carved all-OOS form (the harness scores the
#: whole ≤ 2 h window; the 70/30 split is the frozen GATE's shape).
EVIDENCE_SPLIT: str = "0/100"
#: Report/stderr prefix of every lane.
_TELL: str = "library"
_ID_PREFIX_MIN: int = 8
_HASH128_HEX: int = 32


class LibraryError(Exception):
    """A malformed member (rows the structural parser refuses, a file whose
    rows no longer hash to its id, an unknown member key)."""


class Member(typing.NamedTuple):
    """One library member as the file carries it."""

    member_id: str
    name: str
    kind: str
    rows: list[dict[str, object]]
    labels: list[list[str]]
    regime_off: str | None
    thesis: str | None
    origin: dict[str, object]
    status: str


# ---- paths ----


def library_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    """``~/multivenue/worker/library`` (``$CLAUDE_WORKER_LIBRARY_DIR``)."""
    source = os.environ if env is None else env
    return pathlib.Path(source.get(LIBRARY_DIR_ENV, "") or DEFAULT_LIBRARY_DIR).expanduser()


def library_dir_for(db_path: pathlib.Path) -> pathlib.Path:
    """The library beside a worker ``state.db`` (the ``regime_dir_for``
    precedent — no new env key for config-carrying callers)."""
    return db_path.parent / "library"


def db_path(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    source = os.environ if env is None else env
    return pathlib.Path(source.get(DB_ENV, "") or DEFAULT_DB).expanduser()


def ruleset_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    source = os.environ if env is None else env
    return pathlib.Path(source.get(RULESET_DIR_ENV, "") or DEFAULT_RULESET_DIR).expanduser()


def candidates_dir_for(db: pathlib.Path) -> pathlib.Path:
    return db.parent / CANDIDATES_DIRNAME


def icdp_path(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    source = os.environ if env is None else env
    return pathlib.Path(source.get(ICDP_PATH_ENV, "") or DEFAULT_ICDP_PATH).expanduser()


def member_file(directory: pathlib.Path, member_id: str) -> pathlib.Path:
    return directory / f"{member_id}.json"


# ---- identity + labels ----


def canonical_rows(rows: typing.Iterable[object]) -> list[dict[str, object]]:
    """Every row through the structural parser (canonical key order,
    validated types); a malformed row is a [`LibraryError`] naming its
    index — the library never stores what the engine would refuse."""
    out: list[dict[str, object]] = []
    for i, raw in enumerate(rows):
        row = claude_worker.strategist.parse_row(raw)
        if row is None:
            raise LibraryError(f"row {i} is malformed (structural parse refused it)")
        out.append(row)
    if not out:
        raise LibraryError("a member needs at least one row")
    return out


def member_id_of(rows: list[dict[str, object]]) -> str:
    """sha256 over the canonical artifact bytes of the rows — the SAME
    math as an artifact's hash, so a single-member composition's table
    hash equals its member id."""
    return hashlib.sha256(claude_worker.strategist.artifact_bytes(rows)).hexdigest()


def hash128_hex(full_hash: str) -> str:
    return full_hash[:_HASH128_HEX]


def word_terms(terms: typing.Iterable[str]) -> list[str]:
    """The word-level terms of a label — ``rel:`` terms are per-symbol
    (the engine judges them per instrument) and are not part of the
    word-fit law; they stay on the rows."""
    out: list[str] = []
    for t in terms:
        parts = t.split(":")
        dim = parts[1] if len(parts) == 3 else parts[0]  # noqa: PLR2004 — [profile:]dim:values
        if dim != "rel":
            out.append(t)
    return out


def row_terms(row: typing.Mapping[str, object]) -> list[str]:
    """A row's region as one term list: its ``regimes`` plus the ``rel``
    sugar rewritten to a ``rel:`` term (the validator's own rewrite)."""
    regimes = row.get("regimes")
    terms: list[str] = [str(t) for t in regimes] if isinstance(regimes, list) else []
    rel = row.get("rel")
    if isinstance(rel, str) and rel:
        prefix = ""
        rest = rel
        for p in claude_worker.regime.PROFILE_NAMES:
            if rel.startswith(p + ":"):
                prefix, rest = p + ":", rel[len(p) + 1 :]
                break
        terms.append(f"{prefix}rel:{rest}")
    return terms


def labels_from_rows(rows: typing.Iterable[typing.Mapping[str, object]]) -> list[list[str]]:
    """The fit set a row set declares: one label per distinct labelled
    row region; ANY (``[]``) as soon as one row is unlabelled — that row
    can always enter, so the member fits every word."""
    labels: list[list[str]] = []
    for row in rows:
        terms = row_terms(row)
        if not terms:
            return []
        if terms not in labels:
            labels.append(terms)
    return labels


def validate_labels(labels: typing.Iterable[typing.Iterable[str]]) -> list[list[str]]:
    """Every term must parse (the word terms through the RG5 mirror, the
    ``rel:`` terms through the strategist's structural check)."""
    out: list[list[str]] = []
    for label in labels:
        terms = [str(t) for t in label]
        if not terms:
            raise LibraryError("an empty label allows nothing — use ANY (no labels) instead")
        for t in terms:
            if not claude_worker.strategist.regime_term_ok(t):
                raise LibraryError(f"label term {t!r} does not parse")
        try:
            claude_worker.regime.label_masks(word_terms(terms))
        except ValueError as exc:
            raise LibraryError(str(exc)) from exc
        out.append(terms)
    return out


def require_labels(labels: list[list[str]], what: str) -> None:
    """RG8 (operator ruling 2026-09-05 — labels are enforced everywhere):
    an ANY member may exist as a CANDIDATE (legacy artifacts import, the
    operator may study them) but never becomes ``validated`` and never
    enters a composition — the gate is a no-op for a row without a
    label. Raises [`LibraryError`] naming ``what``."""
    if not labels:
        raise LibraryError(
            f"{what}: the member carries no regime label (ANY) — RG8 requires a label"
            " on every row before it is validated or composed (`library label <member>"
            ' --regimes "..."`, then evidence it on the pool)'
        )


def label_fits(labels: list[list[str]], words: dict[str, int]) -> bool:
    """∃-semantics: ANY fits everything; else some label allows BOTH
    profiles' words (a constrained profile fails closed on UNKNOWN, the
    engine's law)."""
    if not labels:
        return True
    for label in labels:
        terms = word_terms(label)
        if not terms:
            return True  # a rel-only label constrains nothing at word level
        if claude_worker.regime.regime_allows(terms, words):
            return True
    return False


# ---- member files ----


def write_member(directory: pathlib.Path, member: Member) -> pathlib.Path:
    directory.mkdir(parents=True, exist_ok=True)
    path = member_file(directory, member.member_id)
    payload = {
        "version": MEMBER_FILE_VERSION,
        "member_id": member.member_id,
        "name": member.name,
        "kind": member.kind,
        "rows": member.rows,
        "labels": member.labels,
        "regime_off": member.regime_off,
        "thesis": member.thesis,
        "origin": member.origin,
        "status": member.status,
    }
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(json.dumps(payload, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(tmp, path)
    return path


def read_member(path: pathlib.Path) -> Member:
    """Load a member file; the rows are re-canonicalized and re-hashed —
    a file whose rows no longer match its id is refused (tamper/drift)."""
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as exc:
        raise LibraryError(f"{path}: unreadable member file: {exc}") from exc
    if not isinstance(obj, dict):
        raise LibraryError(f"{path}: member file is not an object")
    kind = str(obj.get("kind", KIND_VM_ROWS))
    rows_raw = obj.get("rows")
    rows: list[dict[str, object]] = []
    if kind == KIND_VM_ROWS:
        if not isinstance(rows_raw, list):
            raise LibraryError(f"{path}: rows missing")
        rows = canonical_rows(rows_raw)
        if member_id_of(rows) != str(obj.get("member_id")):
            raise LibraryError(f"{path}: rows do not hash to member_id (file drifted)")
    labels_raw = obj.get("labels")
    labels = validate_labels(labels_raw) if isinstance(labels_raw, list) else []
    origin = obj.get("origin")
    return Member(
        member_id=str(obj.get("member_id")),
        name=str(obj.get("name", "")),
        kind=kind,
        rows=rows,
        labels=labels,
        regime_off=None if obj.get("regime_off") is None else str(obj.get("regime_off")),
        thesis=None if obj.get("thesis") is None else str(obj.get("thesis")),
        origin=dict(origin) if isinstance(origin, dict) else {},
        status=str(obj.get("status", STATUS_CANDIDATE)),
    )


def load_member(directory: pathlib.Path, row: claude_worker.state.LibraryMember) -> Member:
    """The member behind an index row — the file for rows, the index for
    the mutable metadata (status / labels / thesis are the index's)."""
    if row.kind == KIND_CODED:
        return Member(
            member_id=row.member_id,
            name=row.name,
            kind=row.kind,
            rows=[],
            labels=row.labels,
            regime_off=row.regime_off,
            thesis=row.thesis,
            origin=row.origin,
            status=row.status,
        )
    file_member = read_member(pathlib.Path(row.path) if row.path else member_file(directory, row.member_id))
    return file_member._replace(
        name=row.name,
        labels=row.labels,
        regime_off=row.regime_off,
        thesis=row.thesis,
        status=row.status,
    )


# ---- add / status / labels ----


def add_member(  # noqa: PLR0913 — one parameter per member field, deliberately
    state: claude_worker.state.State,
    directory: pathlib.Path,
    rows: typing.Iterable[object],
    name: str,
    *,
    labels: list[list[str]] | None = None,
    regime_off: str | None = None,
    thesis: str | None = None,
    origin: dict[str, object] | None = None,
    status: str = STATUS_CANDIDATE,
    ts: int | None = None,
) -> tuple[Member, bool]:
    """Canonicalize, hash, write the file, insert the index row. Returns
    ``(member, inserted)`` — an existing id is left untouched (the file is
    rewritten only when absent), the caller sees ``inserted == False``."""
    if not name or not name.isascii():
        raise LibraryError("member name must be non-empty ASCII")
    if regime_off is not None and regime_off not in REGIME_OFF_VALUES:
        raise LibraryError(f"regime_off must be one of {REGIME_OFF_VALUES}")
    if status not in STATUSES:
        raise LibraryError(f"status must be one of {STATUSES}")
    canon = canonical_rows(rows)
    member_id = member_id_of(canon)
    fit = labels_from_rows(canon) if labels is None else validate_labels(labels)
    if status == STATUS_VALIDATED:
        require_labels(fit, f"add {name!r} as validated")
    member = Member(
        member_id=member_id,
        name=name,
        kind=KIND_VM_ROWS,
        rows=canon,
        labels=fit,
        regime_off=regime_off,
        thesis=thesis,
        origin={} if origin is None else dict(origin),
        status=status,
    )
    path = member_file(directory, member_id)
    if not path.is_file():
        write_member(directory, member)
    inserted = state.library_insert(
        member_id,
        name,
        KIND_VM_ROWS,
        str(path),
        status,
        fit,
        member.origin,
        regime_off=regime_off,
        thesis=thesis,
        ts=ts,
    )
    return member, inserted


def add_coded_member(  # noqa: PLR0913 — one parameter per member field, deliberately
    state: claude_worker.state.State,
    member_id: str,
    name: str,
    *,
    labels: list[list[str]] | None = None,
    regime_off: str | None = None,
    thesis: str | None = None,
    origin: dict[str, object] | None = None,
    status: str = STATUS_VALIDATED,
    ts: int | None = None,
) -> bool:
    """A coded-member reference (``icdp@<sha256>``): catalogued for the
    AI's "what exists" view; never part of a composed table (the engine's
    strategy mask enables it)."""
    fit = [] if labels is None else validate_labels(labels)
    return state.library_insert(
        member_id,
        name,
        KIND_CODED,
        "",
        status,
        fit,
        {} if origin is None else dict(origin),
        regime_off=regime_off,
        thesis=thesis,
        ts=ts,
    )


def set_labels(
    state: claude_worker.state.State,
    directory: pathlib.Path,
    member: claude_worker.state.LibraryMember,
    labels: list[list[str]],
    regime_off: str | None,
) -> None:
    fit = validate_labels(labels)
    if regime_off is not None and regime_off not in REGIME_OFF_VALUES:
        raise LibraryError(f"regime_off must be one of {REGIME_OFF_VALUES}")
    state.library_set_labels(member.member_id, fit, regime_off)
    _sync_file(state, directory, member.member_id)


def set_status(
    state: claude_worker.state.State,
    directory: pathlib.Path,
    member: claude_worker.state.LibraryMember,
    status: str,
) -> None:
    if status not in STATUSES:
        raise LibraryError(f"status must be one of {STATUSES}")
    if status == STATUS_VALIDATED and member.kind == KIND_VM_ROWS:
        require_labels(member.labels, f"validate {member.name!r}")
    state.library_set_status(member.member_id, status)
    _sync_file(state, directory, member.member_id)


def _sync_file(state: claude_worker.state.State, directory: pathlib.Path, member_id: str) -> None:
    """Mirror the index's mutable metadata into the member file (the
    file is the AI-readable copy; the index is the truth)."""
    row = state.library_member(member_id)
    if row is None or row.kind != KIND_VM_ROWS:
        return
    write_member(directory, load_member(directory, row))


# ---- import-catalog ----


class ImportStats(typing.NamedTuple):
    """What ``import-catalog`` did (every hash + thesis preserved)."""

    registry_rows: int
    candidates: int
    coded: int
    inserted: int
    skipped_missing: int
    skipped_malformed: int
    validated: list[str]


def _name_for(rows: list[dict[str, object]]) -> str:
    names = [str(r.get("name", "")) for r in rows]
    if len(names) == 1:
        return names[0]
    prefix = os.path.commonprefix(names).rstrip("-_")
    return f"{prefix or names[0]}+{len(names) - 1}"


def _rows_of_file(path: pathlib.Path) -> list[object] | None:
    try:
        obj = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    rows = obj.get("rows") if isinstance(obj, dict) else None
    return rows if isinstance(rows, list) else None


def _file_hash(path: pathlib.Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def active_hash(state: claude_worker.state.State) -> str | None:
    """The table the engine is running: the latest committed registry row
    (the daily recommit re-commits exactly this one)."""
    committed = state.committed_rulesets()
    return committed[0][0] if committed else None


def active_canonical_hash(state: claude_worker.state.State, artifacts_dir: pathlib.Path) -> str | None:
    """The active table's hash over its CANONICAL rows — what a
    composition of exactly those rows would hash to. A hand-written
    artifact (the smoke-era `fde6f733…`) carries the same rows in another
    key order, so its file hash differs from the canonical one; the
    composer treats a canonical match as "already active" rather than
    flipping the table to byte-different, semantically identical rows.
    None when the active artifact cannot be read."""
    committed = state.committed_rulesets()
    if not committed:
        return None
    full_hash, path_str = committed[0][0], committed[0][1]
    path = pathlib.Path(path_str).expanduser()
    installed = artifacts_dir / f"{hash128_hex(full_hash)}.json"
    source = path if path.is_file() else installed if installed.is_file() else None
    if source is None:
        return None
    rows = _rows_of_file(source)
    if rows is None:
        return None
    try:
        return member_id_of(canonical_rows(rows))
    except LibraryError:
        return None


def coded_labels_from_regime_toml(regime_path: pathlib.Path, member: str) -> tuple[list[list[str]], str | None]:
    """``[labels.<member>]`` of ``regime.toml`` (``off`` + ``term1..term4``,
    plan §4.2) → ``(labels, off)``; an absent table = ANY / None."""
    if not regime_path.is_file():
        return [], None
    try:
        obj = tomllib.loads(regime_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return [], None
    coded = obj.get("labels")
    entry = coded.get(member) if isinstance(coded, dict) else None
    if not isinstance(entry, dict):
        return [], None
    labels: list[list[str]] = []
    for key in ("term1", "term2", "term3", "term4"):
        terms = entry.get(key)
        if isinstance(terms, list) and terms:
            labels.append([str(t) for t in terms])
    off = entry.get("off")
    return labels, (str(off) if isinstance(off, str) else None)


def import_catalog(  # noqa: PLR0912, PLR0913, PLR0915 — one linear pass over three sources
    state: claude_worker.state.State,
    directory: pathlib.Path,
    artifacts_dir: pathlib.Path,
    candidates_dir: pathlib.Path,
    *,
    icdp_toml: pathlib.Path | None = None,
    regime_toml: pathlib.Path | None = None,
    dry_run: bool = False,
    report: typing.Callable[[str], None] = print,
    ts: int | None = None,
) -> ImportStats:
    """One-time (idempotent) catalog import: every ``rulesets`` registry
    row and every raw candidate becomes a member — ``validated`` ONLY for
    the active table, ``candidate`` otherwise; the thesis and the source
    hash ride ``origin``. Nothing is lost, nothing is re-judged."""
    active = active_hash(state)
    inserted = 0
    missing = 0
    malformed = 0
    validated: list[str] = []
    seen_hashes: set[str] = set()

    registry = state.rulesets_all()
    for r in registry:
        full_hash = r.hash
        seen_hashes.add(full_hash)
        path = pathlib.Path(r.path).expanduser()
        installed = artifacts_dir / f"{hash128_hex(full_hash)}.json"
        source = path if path.is_file() else installed if installed.is_file() else None
        if source is None:
            missing += 1
            report(f"{_TELL} import: {hash128_hex(full_hash)} — no artifact file (registry path gone, not installed) — skipped")
            continue
        rows = _rows_of_file(source)
        if rows is None:
            malformed += 1
            report(f"{_TELL} import: {source} — not a rows artifact — skipped")
            continue
        status = STATUS_VALIDATED if full_hash == active else STATUS_CANDIDATE
        origin: dict[str, object] = {
            "source": "rulesets",
            "source_hash128": hash128_hex(full_hash),
            "source_hash": full_hash,
            "author_mode": r.author_mode,
            "model": r.model,
            "session_ts": r.staged_ts,
            "committed_ts": r.committed_ts,
        }
        try:
            canon = canonical_rows(rows)
        except LibraryError as exc:
            malformed += 1
            report(f"{_TELL} import: {source}: {exc} — skipped")
            continue
        name = _name_for(canon)
        if status == STATUS_VALIDATED and not labels_from_rows(canon):
            # RG8: the live table is the only auto-validated import, and
            # only when it carries labels — an unlabelled one is a
            # candidate the operator must label + evidence first.
            status = STATUS_CANDIDATE
            report(f"{_TELL} import: {hash128_hex(full_hash)} is the ACTIVE table but unlabelled — candidate (RG8)")
        if dry_run:
            report(f"{_TELL} import (dry-run): {status:9} {member_id_of(canon)[:12]} {name} <- {hash128_hex(full_hash)}")
            continue
        member, new = add_member(
            state,
            directory,
            canon,
            name,
            thesis=r.thesis,
            origin=origin,
            status=status,
            ts=ts,
        )
        if new:
            inserted += 1
            if status == STATUS_VALIDATED:
                validated.append(member.member_id)
        report(f"{_TELL} import: {'added' if new else 'kept '} {status:9} {member.member_id[:12]} {name} <- {hash128_hex(full_hash)}")

    n_candidates = 0
    if candidates_dir.is_dir():
        for path in sorted(candidates_dir.glob("*.json")):
            if path.name.endswith(claude_worker.backtest.REPORT_SUFFIX):
                continue
            n_candidates += 1
            full_hash = _file_hash(path)
            if full_hash in seen_hashes:
                continue
            rows = _rows_of_file(path)
            if rows is None:
                malformed += 1
                report(f"{_TELL} import: {path.name} — not a rows artifact — skipped")
                continue
            try:
                canon = canonical_rows(rows)
            except LibraryError as exc:
                malformed += 1
                report(f"{_TELL} import: {path.name}: {exc} — skipped")
                continue
            origin = {
                "source": "candidates",
                "source_hash128": hash128_hex(full_hash),
                "source_hash": full_hash,
                "file": path.name,
            }
            name = _name_for(canon)
            if dry_run:
                report(f"{_TELL} import (dry-run): candidate {member_id_of(canon)[:12]} {name} <- {path.name}")
                continue
            member, new = add_member(state, directory, canon, name, origin=origin, status=STATUS_CANDIDATE, ts=ts)
            if new:
                inserted += 1
            report(f"{_TELL} import: {'added' if new else 'kept '} candidate {member.member_id[:12]} {name} <- {path.name}")

    coded = 0
    if icdp_toml is not None and icdp_toml.is_file():
        coded = 1
        sha = _file_hash(icdp_toml)
        member_id = f"icdp@{sha}"
        labels, off = coded_labels_from_regime_toml(regime_toml, "icdp") if regime_toml is not None else ([], None)
        if dry_run:
            report(f"{_TELL} import (dry-run): coded {member_id[:17]} icdp labels={describe_labels(labels)}")
        elif add_coded_member(
            state,
            member_id,
            "icdp",
            labels=labels,
            regime_off=off,
            origin={"source": "coded", "artifact": str(icdp_toml), "sha256": sha},
            thesis="slot-6 intrabar candle-direction member (ICDP); enabled by the engine mask, never composed",
            ts=ts,
        ):
            inserted += 1
            report(f"{_TELL} import: added validated {member_id[:17]} icdp (coded)")
    return ImportStats(
        registry_rows=len(registry),
        candidates=n_candidates,
        coded=coded,
        inserted=inserted,
        skipped_missing=missing,
        skipped_malformed=malformed,
        validated=validated,
    )


# ---- evidence (one member x one <= 2 h seeded window) ----


def member_artifact(work_dir: pathlib.Path, member: Member) -> pathlib.Path:
    """The member's rows as a canonical artifact file the harness reads
    (hash == member_id by construction)."""
    work_dir.mkdir(parents=True, exist_ok=True)
    path = work_dir / f"{member.member_id[:_HASH128_HEX]}.json"
    data = claude_worker.strategist.artifact_bytes(member.rows)
    if not path.is_file() or path.read_bytes() != data:
        path.write_bytes(data)
    return path


def fee_flags(fees_path: pathlib.Path | None) -> list[str]:
    """The operator's tier as harness flags; ``None``/absent ⇒ zero fees
    (the evidence row then says so through ``net_usd_tier == net_usd_0``)."""
    if fees_path is None or not fees_path.is_file():
        return []
    return claude_worker.pnl_report.load_fee_flags(fees_path)


def _lane_stale_blind(detail: dict[str, object]) -> bool:
    stale = detail.get("stale")
    runs = stale.get("runs") if isinstance(stale, dict) else None
    if not isinstance(runs, list):
        return True
    for run in runs:
        lanes = run.get("lanes") if isinstance(run, dict) else None
        if not isinstance(lanes, dict):
            continue
        for lane in lanes.values():
            if isinstance(lane, dict) and lane.get("stale_blind") is True and int(lane.get("ticks", 0)) > 0:
                return True
    return False


def _dominant_word(detail: dict[str, object]) -> str:
    regime = detail.get("regime")
    profiles = regime.get("profiles") if isinstance(regime, dict) else None
    if not isinstance(profiles, list):
        return ""
    for profile in profiles:
        if isinstance(profile, dict) and profile.get("profile") == "fast":
            minutes = profile.get("minutes")
            if isinstance(minutes, list) and minutes and isinstance(minutes[0], dict):
                return str(minutes[0].get("bits", ""))
    return ""


def evidence_from_detail(
    member_id: str,
    window_dir: pathlib.Path,
    result: claude_worker.backtest.HarnessDetail,
    ts: int,
) -> claude_worker.state.EvidenceRow:
    """The evidence row of one additive-path run: nets from the report
    (tier) and the fee ladder's zero-bps rung, counts and the dominant
    fast word from the detail sidecar."""
    detail = result.detail
    window = detail.get("window")
    fills = detail.get("fills")
    oos = detail.get("oos")
    n_ticks = int(window.get("merged_records", 0)) if isinstance(window, dict) else 0
    n_fills = int(fills.get("total", 0)) if isinstance(fills, dict) else result.harness.oos_trades
    ladder = oos.get("fee_ladder_net_usd") if isinstance(oos, dict) else None
    net_0 = (
        float(ladder[0])
        if isinstance(ladder, list) and ladder and isinstance(ladder[0], (int, float))
        else result.harness.oos_net_pnl_usd
    )
    version = detail.get("detail_version")
    return claude_worker.state.EvidenceRow(
        member_id=member_id,
        window_id=window_dir.name,
        root=str(window_dir),
        n_ticks=n_ticks,
        n_fills=n_fills,
        net_usd_0=net_0,
        net_usd_tier=result.harness.oos_net_pnl_usd,
        max_dd_usd=result.harness.oos_max_drawdown_usd,
        regime_word_mode=_dominant_word(detail),
        judged=not _lane_stale_blind(detail),
        detail_version=int(version) if isinstance(version, int) else 0,
        ts=ts,
    )


def run_evidence(  # noqa: PLR0913 — one parameter per harness knob, deliberately
    state: claude_worker.state.State,
    member: Member,
    window_dir: pathlib.Path,
    work_dir: pathlib.Path,
    *,
    fees: list[str],
    run_fn: typing.Callable[[list[str]], str] | None = None,
    ts: int | None = None,
) -> claude_worker.state.EvidenceRow:
    """One (member, window) evidence run through the additive harness
    path (``0/100`` split, tier fees, ``--emit-detail``), recorded in
    ``library_evidence``. A window root longer than 2 h is refused by the
    harness's own law on the cut; here the window is a run dir the
    caller already bounded."""
    if member.kind != KIND_VM_ROWS or not member.rows:
        raise LibraryError(f"{member.name}: coded members carry no rows to run")
    artifact = member_artifact(work_dir, member)
    detail_path = work_dir / f"{member.member_id[:_HASH128_HEX]}.{window_dir.name}{claude_worker.backtest.DETAIL_SUFFIX}"
    result = claude_worker.backtest.run_harness_extra(
        artifact,
        window_dir,
        split=EVIDENCE_SPLIT,
        extra_flags=fees,
        detail_path=detail_path,
        run_fn=run_fn,
    )
    stamp = int(time.time()) if ts is None else ts
    row = evidence_from_detail(member.member_id, window_dir, result, stamp)
    state.evidence_upsert(
        row.member_id,
        row.window_id,
        row.root,
        n_ticks=row.n_ticks,
        n_fills=row.n_fills,
        net_usd_0=row.net_usd_0,
        net_usd_tier=row.net_usd_tier,
        max_dd_usd=row.max_dd_usd,
        regime_word_mode=row.regime_word_mode,
        judged=row.judged,
        detail_version=row.detail_version,
        ts=stamp,
    )
    return row


class EvidenceSummary(typing.NamedTuple):
    """A member's evidence folded for sorting/reporting."""

    windows: int
    judged: int
    net_usd_tier: float
    net_usd_0: float
    positive_tier: int


def evidence_summary(rows: list[claude_worker.state.EvidenceRow]) -> EvidenceSummary:
    return EvidenceSummary(
        windows=len(rows),
        judged=sum(1 for r in rows if r.judged),
        net_usd_tier=sum(r.net_usd_tier for r in rows),
        net_usd_0=sum(r.net_usd_0 for r in rows),
        positive_tier=sum(1 for r in rows if r.net_usd_tier > 0),
    )


# ---- regime query (`list --regime`) ----


def query_words(spec: str, now_ms: int, *, directory: pathlib.Path | None = None, url: str | None = None) -> tuple[dict[str, int], str]:
    """``current`` = the engine → fresh declaration → UNKNOWN chain (RG5
    ``current_words``); else ``"<fast-decl>[;<slow-decl>]"`` in the
    declaration grammar (``trend:bull,vol:low``) — unnamed market
    dimensions are UNKNOWN (a label constraining them fails closed, the
    engine's law), SOURCE = measured."""
    if spec == "current":
        return claude_worker.regime.current_words(
            claude_worker.regime.regime_dir() if directory is None else directory, now_ms, url
        )
    parts = spec.split(";")
    if len(parts) > len(claude_worker.regime.PROFILE_NAMES):
        raise LibraryError("regime query: at most fast;slow")
    words: dict[str, int] = {}
    for i, name in enumerate(claude_worker.regime.PROFILE_NAMES):
        decl = parts[i] if i < len(parts) else parts[-1]
        try:
            dims = claude_worker.regime.parse_declaration(decl)
        except ValueError as exc:
            raise LibraryError(f"regime query: {exc}") from exc
        word = claude_worker.regime.declaration_word(dims)
        words[name] = claude_worker.regime.word_with_source(
            claude_worker.regime.merge_declared(word, claude_worker.regime.UNKNOWN_WORD),
            claude_worker.regime.SOURCE_MEASURED,
        )
    return words, "query"


# ---- rendering ----


def describe_labels(labels: list[list[str]]) -> str:
    return "ANY" if not labels else " | ".join("[" + ",".join(t) + "]" for t in labels)


def member_line(
    row: claude_worker.state.LibraryMember,
    evidence: list[claude_worker.state.EvidenceRow],
    fits: bool | None = None,
) -> str:
    s = evidence_summary(evidence)
    fit = "" if fits is None else ("fit " if fits else "--- ")
    thesis = "" if not row.thesis else f' "{row.thesis[:60]}"'
    return (
        f"{fit}{row.status:9} {row.member_id[:12]} {row.kind:7} {row.name}"
        f" labels={describe_labels(row.labels)} off={row.regime_off or 'soft'}"
        f" evidence={s.windows}w/{s.judged}j tier={s.net_usd_tier:+.2f} zero={s.net_usd_0:+.2f}{thesis}"
    )


def member_json(row: claude_worker.state.LibraryMember, evidence: list[claude_worker.state.EvidenceRow]) -> dict[str, object]:
    s = evidence_summary(evidence)
    return {
        "member_id": row.member_id,
        "name": row.name,
        "kind": row.kind,
        "status": row.status,
        "labels": row.labels,
        "regime_off": row.regime_off,
        "thesis": row.thesis,
        "origin": row.origin,
        "evidence": {
            "windows": s.windows,
            "judged": s.judged,
            "net_usd_tier": s.net_usd_tier,
            "net_usd_0": s.net_usd_0,
            "positive_tier": s.positive_tier,
        },
        "updated_ts": row.updated_ts,
    }


# ---- CLI ----


def _split_terms(spec: str) -> list[str]:
    return [t.strip() for t in spec.split(",") if t.strip()]


def _resolve(state: claude_worker.state.State, key: str) -> claude_worker.state.LibraryMember:
    row = state.library_find(key)
    if row is None:
        raise LibraryError(f"no member {key!r} (id, ≥{_ID_PREFIX_MIN}-char id prefix or exact name)")
    return row


def _add_common(p: argparse.ArgumentParser) -> None:
    p.add_argument("--db", default=None, help=f"state.db (default ${DB_ENV} or {DEFAULT_DB})")
    p.add_argument("--dir", default=None, help=f"library dir (default ${LIBRARY_DIR_ENV} or {DEFAULT_LIBRARY_DIR})")


def main(argv: list[str] | None = None) -> int:  # noqa: PLR0915 — one dispatcher per lane
    parser = argparse.ArgumentParser(prog="claude_worker.library")
    sub = parser.add_subparsers(dest="lane", required=True)

    add = sub.add_parser("add", help="add a member from a rows artifact")
    _add_common(add)
    add.add_argument("--from", dest="source", required=True, help="ruleset JSON ({\"rows\": [...]})")
    add.add_argument("--name", required=True)
    add.add_argument("--thesis", default=None)
    add.add_argument("--regimes", action="append", default=None, help='one label per flag: "t1,t2" (∃ across flags); absent = derived from the rows')
    add.add_argument("--regime-off", default=None, choices=REGIME_OFF_VALUES)
    add.add_argument("--status", default=STATUS_CANDIDATE, choices=STATUSES)
    add.add_argument("--split-by-name-prefix", action="store_true", help="one member per row-name prefix (before the last '-')")

    imp = sub.add_parser("import-catalog", help="every registry row + candidate becomes a member (active table validated)")
    _add_common(imp)
    imp.add_argument("--artifacts", default=None, help=f"installed artifacts dir (default ${RULESET_DIR_ENV} or {DEFAULT_RULESET_DIR})")
    imp.add_argument("--candidates", default=None, help="candidates dir (default <worker dir>/candidates)")
    imp.add_argument("--icdp", default=None, help=f"icdp.toml for the coded member (default ${ICDP_PATH_ENV} or {DEFAULT_ICDP_PATH})")
    imp.add_argument("--regime", default=claude_worker.regime.REGIME_PATH_DEFAULT, help="regime.toml ([labels.icdp])")
    imp.add_argument("--dry-run", action="store_true")

    lst = sub.add_parser("list", help="members (+ evidence); --regime filters by fit")
    _add_common(lst)
    lst.add_argument("--regime", default=None, help='current | "<fast-decl>[;<slow-decl>]" (e.g. "trend:bull,vol:low")')
    lst.add_argument("--status", default=None, choices=STATUSES)
    lst.add_argument("--all", action="store_true", help="with --regime: show non-fitting members too")
    lst.add_argument("--json", action="store_true")
    lst.add_argument("--now-ms", type=int, default=None, help="tests only")

    lab = sub.add_parser("label", help="set a member's labels (∃ across --regimes flags) or --any")
    _add_common(lab)
    lab.add_argument("member")
    lab.add_argument("--regimes", action="append", default=None)
    lab.add_argument("--any", action="store_true")
    lab.add_argument("--regime-off", default=None, choices=REGIME_OFF_VALUES)

    for lane, target in (("retire", STATUS_RETIRED), ("validate", STATUS_VALIDATED), ("candidate", STATUS_CANDIDATE)):
        p = sub.add_parser(lane, help=f"set status {target}")
        _add_common(p)
        p.add_argument("member")
        p.set_defaults(target_status=target)

    ev = sub.add_parser("evidence", help="run a member on ≤ 2 h seeded windows and record the rows")
    _add_common(ev)
    ev.add_argument("member")
    ev.add_argument("--window", action="append", required=True, help="a cut window run dir (repeatable)")
    ev.add_argument("--fees", default=claude_worker.pnl_report.FEES_PATH_DEFAULT, help="fees.toml (or 'none' for zero fees)")
    ev.add_argument("--work-dir", default=None, help="artifact/detail scratch (default <library dir>/work)")
    ev.add_argument("--json", action="store_true")

    args = parser.parse_args(argv)
    db = pathlib.Path(args.db).expanduser() if args.db else db_path()
    directory = pathlib.Path(args.dir).expanduser() if args.dir else library_dir()
    state = claude_worker.state.State(db)
    try:
        return _dispatch(args, state, db, directory)
    except LibraryError as exc:
        print(f"{_TELL} {args.lane}: {exc}", file=sys.stderr)
        return 2
    finally:
        state.close()


def _dispatch(  # noqa: PLR0911, PLR0912, PLR0915 — one branch per lane
    args: argparse.Namespace,
    state: claude_worker.state.State,
    db: pathlib.Path,
    directory: pathlib.Path,
) -> int:
    if args.lane == "add":
        source = pathlib.Path(args.source).expanduser()
        rows = _rows_of_file(source)
        if rows is None:
            raise LibraryError(f"{source}: not a rows artifact")
        labels = None if args.regimes is None else [_split_terms(s) for s in args.regimes]
        origin: dict[str, object] = {"source": "add", "file": str(source), "source_hash": _file_hash(source)}
        groups: list[tuple[str, list[object]]] = [(args.name, rows)]
        if args.split_by_name_prefix:
            by_prefix: dict[str, list[object]] = {}
            for raw in rows:
                name = str(raw.get("name", "")) if isinstance(raw, dict) else ""
                prefix = name.rsplit("-", 1)[0] if "-" in name else name
                by_prefix.setdefault(prefix, []).append(raw)
            groups = [(f"{args.name}-{p}" if len(by_prefix) > 1 else args.name, g) for p, g in by_prefix.items()]
        for name, group in groups:
            member, new = add_member(
                state, directory, group, name,
                labels=labels, regime_off=args.regime_off, thesis=args.thesis, origin=origin, status=args.status,
            )
            print(
                f"{_TELL} add: {'added' if new else 'exists'} {member.status} {member.member_id} {member.name}"
                f" rows={len(member.rows)} labels={describe_labels(member.labels)}"
            )
        return 0

    if args.lane == "import-catalog":
        stats = import_catalog(
            state,
            directory,
            pathlib.Path(args.artifacts).expanduser() if args.artifacts else ruleset_dir(),
            pathlib.Path(args.candidates).expanduser() if args.candidates else candidates_dir_for(db),
            icdp_toml=pathlib.Path(args.icdp).expanduser() if args.icdp else icdp_path(),
            regime_toml=pathlib.Path(args.regime).expanduser(),
            dry_run=args.dry_run,
        )
        print(
            f"{_TELL} import{' (dry-run)' if args.dry_run else ''}: registry={stats.registry_rows}"
            f" candidates={stats.candidates} coded={stats.coded} inserted={stats.inserted}"
            f" skipped_missing={stats.skipped_missing} skipped_malformed={stats.skipped_malformed}"
            f" validated={[v[:12] for v in stats.validated]}"
        )
        return 0

    if args.lane == "list":
        now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
        words: dict[str, int] | None = None
        source = ""
        if args.regime is not None:
            words, source = query_words(args.regime, now_ms)
        rows = state.library_members(args.status)
        out: list[dict[str, object]] = []
        if words is not None and not args.json:
            shown = " ".join(
                f"{n}=[{claude_worker.regime.describe(words[n])}]" for n in claude_worker.regime.PROFILE_NAMES
            )
            print(f"{_TELL} list: regime ({source}) {shown}")
        for row in rows:
            fits = None if words is None else label_fits(row.labels, words)
            if fits is False and not args.all and not args.json:
                continue
            evidence = state.evidence_for(row.member_id)
            if args.json:
                item = member_json(row, evidence)
                if fits is not None:
                    item["fits"] = fits
                out.append(item)
            else:
                print(member_line(row, evidence, fits))
        if args.json:
            print(json.dumps({"members": out, "regime": None if words is None else {"source": source, **{n: f"{words[n]:016x}" for n in claude_worker.regime.PROFILE_NAMES}}}, sort_keys=True))
        elif not rows:
            print(f"{_TELL} list: empty — run import-catalog or add")
        return 0

    if args.lane == "label":
        row = _resolve(state, args.member)
        if args.any == (args.regimes is not None):
            raise LibraryError("give exactly one of --regimes (repeatable) / --any")
        labels = [] if args.any else [_split_terms(s) for s in args.regimes]
        set_labels(state, directory, row, labels, args.regime_off if args.regime_off is not None else row.regime_off)
        print(f"{_TELL} label: {row.member_id[:12]} {row.name} labels={describe_labels(labels)}")
        return 0

    if args.lane in ("retire", "validate", "candidate"):
        row = _resolve(state, args.member)
        set_status(state, directory, row, args.target_status)
        print(f"{_TELL} {args.lane}: {row.member_id[:12]} {row.name} -> {args.target_status}")
        return 0

    if args.lane == "evidence":
        row = _resolve(state, args.member)
        member = load_member(directory, row)
        fees = [] if args.fees == "none" else fee_flags(pathlib.Path(args.fees).expanduser())
        work = pathlib.Path(args.work_dir).expanduser() if args.work_dir else directory / "work"
        results: list[dict[str, object]] = []
        for w in args.window:
            window_dir = pathlib.Path(w).expanduser()
            if not window_dir.is_dir():
                raise LibraryError(f"window {window_dir} is not a directory")
            ev = run_evidence(state, member, window_dir, work, fees=fees)
            results.append(ev._asdict())
            if not args.json:
                print(
                    f"{_TELL} evidence: {member.name} {ev.window_id} fills={ev.n_fills} ticks={ev.n_ticks}"
                    f" tier={ev.net_usd_tier:+.2f} zero={ev.net_usd_0:+.2f} dd={ev.max_dd_usd:.2f}"
                    f" word={ev.regime_word_mode or '-'} judged={ev.judged}"
                )
        if args.json:
            print(json.dumps({"member_id": member.member_id, "evidence": results}, sort_keys=True))
        return 0
    return 2


if __name__ == "__main__":
    sys.exit(main())
