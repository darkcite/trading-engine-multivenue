# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""compose — regime-aware aggregation (RG4, docs/regime-and-dashboard-plan.md §5.3).

Input: the effective word per profile (the engine's ``/metrics`` gauges
through the RG5 chain, else the worker's own measured word), the library
(``validated`` members unless ``--include-candidates``), the caps.

1. **Select** — members that FIT the effective words (∃ label allowing
   both profiles' words), or fit any word of the *neighbourhood* (one
   market dimension of one profile changed — Hamming distance 1), or —
   with ``--fit-from-evidence`` — carry ≥ N judged evidence windows with a
   positive tier net under a neighbourhood word. A member is selected
   once with ALL of its rows: the engine's per-row masks decide which
   variant is open, so a regime move inside the neighbourhood needs no
   table flip (§2.4). Selected members are ordered by evidence (judged
   tier net, then window count, then name).
2. **Fit the caps** — the validator's conservative count (rule 7: every
   row counts, two-leg position rows charge both legs, group-blind), the
   256-row table, rule 5 (unique names) and the rule-8 identity law
   (two rows on one identity tuple whose regime regions INTERSECT are a
   duplicate — the engine would refuse the table, so the later member
   waits). Members are admitted in evidence order until a cap holds.
3. **Emit** — the composed artifact ``{"rows": [...]}`` in canonical
   bytes (``strategist.artifact_bytes``); same inputs ⇒ same bytes ⇒ same
   hash (idempotent). A single-member composition's hash IS the member id.
4. **Gate** — on the standing window POOL (``window_root.pool_ensure``:
   the newest K complete ≤ 2 h seeded windows, count-pruned): the frozen
   ``backtest.run_backtest`` on the pooled root (the report a stage binds
   on), the on/off delta (``--regime off`` on the same pool through the
   additive path — the label must not be worse than its absence), and
   leave-one-window-out (every pool-minus-one root keeps OOS net > 0 —
   no single window carries the edge). Per-member evidence rows are
   written for every pool window the member lacks (``0/100``, tier fees).
   Every harness run is charged to a WALL BUDGET of 2 hours (operator
   ruling 2026-09-05): a gate that would run longer FAILS, it never
   waits.
5. **Promote** (``--promote``) — only when the hash differs from the
   ACTIVE table and no ``FREEZE`` pin is set: install → the frozen
   ``stage_ruleset`` / ``commit_ruleset`` pair in-process (the
   ``daemon._try_promote`` shape), then the engine's ``table_epoch`` is
   watched for the flip. Re-compose triggers (a word leaving the
   neighbourhood, a library change, the daily refresh) are the cycle's
   (RG7); in semi-manual mode the session runs this lane.

Module lane, not a Typer verb: ``python -m claude_worker.compose``.
Offline tool — allocation is fine; never imported by the engine.
Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import json
import os
import pathlib
import sys
import time
import typing
import urllib.request

import claude_worker.backtest
import claude_worker.config
import claude_worker.frames
import claude_worker.library
import claude_worker.pnl_report
import claude_worker.regime
import claude_worker.state
import claude_worker.strategist
import claude_worker.uds
import claude_worker.window_root

COMPOSITIONS_DIRNAME: str = "compositions"
FREEZE_FILE: str = "FREEZE"
LOGS_DIR_ENV: str = "CLAUDE_WORKER_REPLAY_DIR"
DEFAULT_LOGS_DIR: str = "~/multivenue/logs"
#: Operator ruling 2026-09-05: no test / soak / protect time beyond 2 h.
WALL_BUDGET_S: float = 2 * 3600.0
#: Plan §5.3 step 4: the pooled gate needs at least this many windows.
MIN_WINDOWS: int = 4
MAX_ROWS: int = 256
CAP_LEG_USD: float = 10_000.0
CAP_SYMBOL_USD: float = 20_000.0
CAP_TABLE_USD: float = 100_000.0
EPOCH_POLL_S: float = 10.0
_TELL: str = "compose"
_HASH128_HEX: int = 32
#: Rule-8 identity tuple (ingress-ai `validate_ruleset`, RG3 amendment):
#: the row keys that make two rows "the same rule" — thresholds of
#: degree (horizon / holds / risk / name / exit / confirm threshold) are
#: NOT identity. `exit` presence (position flag) is folded in separately.
_IDENTITY_KEYS: tuple[str, ...] = (
    "instrument",
    "ref",
    "feature",
    "ref_feature",
    "confirm_feature",
    "window_min",
    "ref_window_min",
    "confirm_window_min",
    "combine",
    "cmp",
    "abs",
    "confirm_cmp",
    "confirm_abs",
    "confirm_pair",
    "group",
    "enter",
)
_REL_VALUES: tuple[str, ...] = ("lagging", "inline", "leading")


class ComposeError(Exception):
    """A composition that cannot proceed (no members, a bad regime
    query, a pool below the window floor, a promotion refused)."""


class BudgetExceeded(ComposeError):
    """The 2 h wall budget would be crossed by the next harness run."""


def compositions_dir_for(db_path: pathlib.Path) -> pathlib.Path:
    return db_path.parent / COMPOSITIONS_DIRNAME


def logs_dir(env: typing.Mapping[str, str] | None = None) -> pathlib.Path:
    source = os.environ if env is None else env
    return pathlib.Path(source.get(LOGS_DIR_ENV, "") or DEFAULT_LOGS_DIR).expanduser()


# ---- words + neighbourhood ----


def effective_words(  # noqa: PLR0913 — one parameter per source, deliberately
    directory: pathlib.Path,
    now_ms: int,
    *,
    url: str | None = None,
    regime_toml: pathlib.Path | None = None,
    candles_db: pathlib.Path | None = None,
    query: str | None = None,
) -> tuple[dict[str, int], str]:
    """``(words, source)``: an explicit ``query`` (``current`` or a
    declaration spec — ``library.query_words``), else the RG5 chain
    engine → fresh declaration, else the worker's own measurement from
    candles.db, else UNKNOWN (a constrained member then fails closed)."""
    if query is not None:
        return claude_worker.library.query_words(query, now_ms, directory=directory, url=url)
    words, source = claude_worker.regime.current_words(directory, now_ms, url)
    if source != "unknown":
        return words, source
    if regime_toml is not None and candles_db is not None and regime_toml.is_file() and candles_db.is_file():
        m = claude_worker.regime.measure(regime_toml, candles_db, now_ms)
        return claude_worker.regime.measured_words(m), "measured"
    return words, source


def neighbourhood(words: dict[str, int]) -> list[dict[str, int]]:
    """The word itself plus every word at Hamming distance 1 in dimension
    space: ONE market dimension of ONE profile set to another known
    value (an unknown/empty byte neighbours every known value)."""
    out: list[dict[str, int]] = [dict(words)]
    for name in claude_worker.regime.PROFILE_NAMES:
        base = words.get(name, claude_worker.regime.UNKNOWN_WORD)
        for dim, d in claude_worker.frames.REGIME_DIMS.items():
            if dim == "source":
                continue
            current = claude_worker.regime.word_dim(base, d)
            for v in range(len(claude_worker.frames.REGIME_VALUES[dim])):
                byte = 1 << v
                if byte == current:
                    continue
                w = (base & ~(0xFF << (8 * d))) | (byte << (8 * d))
                nb = dict(words)
                nb[name] = w
                out.append(nb)
    return out


def words_hex(words: dict[str, int]) -> dict[str, str]:
    return {n: f"{words.get(n, claude_worker.regime.UNKNOWN_WORD):016x}" for n in claude_worker.regime.PROFILE_NAMES}


# ---- rule-8 identity + regions (validator mirror, conservative) ----


def row_identity(row: typing.Mapping[str, object]) -> tuple[object, ...]:
    return (*(row.get(k) for k in _IDENTITY_KEYS), "exit" in row)


def _rel_nibbles(terms: typing.Iterable[str]) -> tuple[int, int]:
    """``rel:`` terms → (fast, slow) nibbles over lagging/inline/leading;
    0 = any (the wire's own convention)."""
    fast = 0
    slow = 0
    for t in terms:
        parts = t.split(":")
        profile = 0
        if len(parts) == 3:  # noqa: PLR2004 — profile:dim:values
            profile = 1 if parts[0] == "slow" else 0
            parts = parts[1:]
        if len(parts) != 2 or parts[0] != "rel":  # noqa: PLR2004 — dim:values
            continue
        values = parts[1]
        if values == "*":
            mask = 0
        elif values.startswith("!"):
            mask = 0b111 & ~(1 << _REL_VALUES.index(values[1:])) if values[1:] in _REL_VALUES else 0
        else:
            mask = 0
            for v in values.split("|"):
                if v in _REL_VALUES:
                    mask |= 1 << _REL_VALUES.index(v)
        if profile == 0:
            fast = mask
        else:
            slow = mask
    return fast, slow


class Region(typing.NamedTuple):
    """A row's regime region: per-profile label masks (0 = ANY) + REL nibbles."""

    fast: int
    slow: int
    rel_fast: int
    rel_slow: int


def row_region(row: typing.Mapping[str, object]) -> Region:
    terms = claude_worker.library.row_terms(row)
    masks = claude_worker.regime.label_masks(claude_worker.library.word_terms(terms))
    rel_fast, rel_slow = _rel_nibbles(terms)
    return Region(masks["fast"], masks["slow"], rel_fast, rel_slow)


def _labels_intersect(a: int, b: int) -> bool:
    """``RegimeLabel::intersects``: ANY meets everything; else every
    dimension byte must share a bit."""
    if a == 0 or b == 0:
        return True
    for d in range(claude_worker.regime.DIM_SOURCE + 1):
        if (claude_worker.regime.word_dim(a, d) & claude_worker.regime.word_dim(b, d)) == 0:
            return False
    return True


def regions_intersect(a: Region, b: Region) -> bool:
    rel_f = a.rel_fast == 0 or b.rel_fast == 0 or (a.rel_fast & b.rel_fast) != 0
    rel_s = a.rel_slow == 0 or b.rel_slow == 0 or (a.rel_slow & b.rel_slow) != 0
    return _labels_intersect(a.fast, b.fast) and _labels_intersect(a.slow, b.slow) and rel_f and rel_s


def rows_conflict(a: typing.Mapping[str, object], b: typing.Mapping[str, object]) -> bool:
    """Rule 8 (RG3 amendment): one identity tuple + intersecting regions."""
    return row_identity(a) == row_identity(b) and regions_intersect(row_region(a), row_region(b))


# ---- caps (rule 7 mirror, conservative) ----


class CapUsage(typing.NamedTuple):
    table_usd: float
    max_symbol_usd: float
    max_leg_usd: float
    rows: int


def cap_usage(rows: typing.Iterable[typing.Mapping[str, object]]) -> CapUsage:
    """Every row counts; a two-leg position row (``exit`` + ``ref``)
    charges its cap to BOTH legs; the table sum is group-blind."""
    table = 0.0
    per_symbol: dict[str, float] = {}
    max_leg = 0.0
    n = 0
    for row in rows:
        n += 1
        risk = float(row.get("max_risk_usd", 0.0) or 0.0)  # type: ignore[arg-type]
        legs = [str(row.get("instrument", row.get("sym", "")))]
        ref = row.get("ref")
        if "exit" in row and isinstance(ref, str) and ref:
            legs.append(ref)
        for leg in legs:
            per_symbol[leg] = per_symbol.get(leg, 0.0) + risk
            table += risk
        max_leg = max(max_leg, risk)
    return CapUsage(table, max(per_symbol.values(), default=0.0), max_leg, n)


def caps_ok(rows: list[dict[str, object]]) -> str | None:
    """None when the table fits every cap, else the reason."""
    u = cap_usage(rows)
    if u.rows > MAX_ROWS:
        return f"{u.rows} rows > {MAX_ROWS}"
    if u.max_leg_usd > CAP_LEG_USD:
        return f"leg ${u.max_leg_usd:,.0f} > ${CAP_LEG_USD:,.0f}"
    if u.max_symbol_usd > CAP_SYMBOL_USD:
        return f"symbol ${u.max_symbol_usd:,.0f} > ${CAP_SYMBOL_USD:,.0f}"
    if u.table_usd > CAP_TABLE_USD:
        return f"table ${u.table_usd:,.0f} > ${CAP_TABLE_USD:,.0f}"
    return None


# ---- selection ----


class Selected(typing.NamedTuple):
    member: claude_worker.library.Member
    fit: str
    evidence: claude_worker.library.EvidenceSummary


class Composition(typing.NamedTuple):
    """A composed table: the admitted members (in order), their rows,
    the canonical artifact and its hashes, plus what was left out and why."""

    words: dict[str, int]
    words_source: str
    members: list[Selected]
    skipped: list[tuple[str, str]]
    rows: list[dict[str, object]]
    data: bytes
    full_hash: str
    hash128: str


def _evidence_fit(
    rows: list[claude_worker.state.EvidenceRow], nb_words: set[str], min_windows: int
) -> bool:
    n = sum(1 for r in rows if r.judged and r.net_usd_tier > 0 and r.regime_word_mode in nb_words)
    return n >= min_windows


def _fast_hex(words: dict[str, int]) -> str:
    return f"{words.get('fast', claude_worker.regime.UNKNOWN_WORD):016x}"


def select_members(  # noqa: PLR0913 — one parameter per selection knob, deliberately
    state: claude_worker.state.State,
    directory: pathlib.Path,
    words: dict[str, int],
    *,
    include_candidates: bool = False,
    fit_from_evidence: bool = False,
    min_windows: int = MIN_WINDOWS,
    include_any: bool = False,
) -> tuple[list[Selected], list[tuple[str, str]]]:
    """Step 1 (+ the evidence ordering): every vm-rows member that fits
    the words, the neighbourhood, or (opt-in) its evidence — sorted by
    judged tier net desc, windows desc, name. Retired members never;
    candidates only on request; coded members are catalog-only; ANY
    (unlabelled) members only on ``include_any`` (RG8 — the gate is a
    no-op for them, so by default they never enter a composition)."""
    nb = neighbourhood(words)
    nb_fast = {_fast_hex(w) for w in nb}
    selected: list[Selected] = []
    skipped: list[tuple[str, str]] = []
    for row in state.library_members():
        if row.kind != claude_worker.library.KIND_VM_ROWS:
            continue
        if row.status == claude_worker.library.STATUS_RETIRED:
            continue
        if row.status == claude_worker.library.STATUS_CANDIDATE and not include_candidates:
            skipped.append((row.name, "candidate (use --include-candidates)"))
            continue
        try:
            member = claude_worker.library.load_member(directory, row)
        except claude_worker.library.LibraryError as exc:
            skipped.append((row.name, f"unloadable: {exc}"))
            continue
        evidence = state.evidence_for(row.member_id)
        summary = claude_worker.library.evidence_summary(evidence)
        if not member.labels:
            if not include_any:
                skipped.append((row.name, "unlabelled (ANY) — RG8 excludes it (use --include-any)"))
                continue
            fit = "any"
        elif claude_worker.library.label_fits(member.labels, words):
            fit = "word"
        elif any(claude_worker.library.label_fits(member.labels, w) for w in nb):
            fit = "neighbour"
        elif fit_from_evidence and _evidence_fit(evidence, nb_fast, min_windows):
            fit = "evidence"
        else:
            skipped.append((row.name, f"no fit for {claude_worker.regime.describe(words.get('fast', 0))} (labels {claude_worker.library.describe_labels(member.labels)})"))
            continue
        selected.append(Selected(member, fit, summary))
    selected.sort(key=lambda s: (-s.evidence.net_usd_tier, -s.evidence.windows, s.member.name))
    return selected, skipped


def admit(selected: list[Selected]) -> tuple[list[Selected], list[dict[str, object]], list[tuple[str, str]]]:
    """Step 2: admit members in order while rule 5 (names), rule 8
    (identity ∩ region) and rule 7 (caps) hold for the growing table."""
    admitted: list[Selected] = []
    rows: list[dict[str, object]] = []
    names: set[str] = set()
    skipped: list[tuple[str, str]] = []
    for s in selected:
        reason: str | None = None
        for row in s.member.rows:
            name = str(row.get("name", ""))
            if name in names:
                reason = f"row name {name!r} already in the table (rule 5)"
                break
            for prior in rows:
                if rows_conflict(prior, row):
                    reason = f"row {name!r} duplicates {prior.get('name')!r} on an intersecting region (rule 8)"
                    break
            if reason is not None:
                break
        if reason is None:
            reason = caps_ok(rows + s.member.rows)
        if reason is not None:
            skipped.append((s.member.name, reason))
            continue
        admitted.append(s)
        rows.extend(s.member.rows)
        names.update(str(r.get("name", "")) for r in s.member.rows)
    return admitted, rows, skipped


def compose(  # noqa: PLR0913 — one parameter per selection knob, deliberately
    state: claude_worker.state.State,
    directory: pathlib.Path,
    words: dict[str, int],
    words_source: str,
    *,
    include_candidates: bool = False,
    fit_from_evidence: bool = False,
    min_windows: int = MIN_WINDOWS,
    include_any: bool = False,
) -> Composition:
    """Steps 1-3: select, admit, emit canonical bytes."""
    selected, skipped = select_members(
        state,
        directory,
        words,
        include_candidates=include_candidates,
        fit_from_evidence=fit_from_evidence,
        min_windows=min_windows,
        include_any=include_any,
    )
    admitted, rows, more = admit(selected)
    if not admitted:
        raise ComposeError("no member fits — " + "; ".join(f"{n}: {r}" for n, r in skipped + more) if skipped + more else "no member fits (empty library?)")
    data = claude_worker.strategist.artifact_bytes(rows)
    full_hash = claude_worker.library.member_id_of(rows)
    return Composition(
        words=dict(words),
        words_source=words_source,
        members=admitted,
        skipped=skipped + more,
        rows=rows,
        data=data,
        full_hash=full_hash,
        hash128=full_hash[:_HASH128_HEX],
    )


def write_composition(directory: pathlib.Path, composition: Composition) -> pathlib.Path:
    """The artifact at ``<compositions dir>/<hash128>.json`` — canonical
    bytes, so the frozen harness/stage path hashes exactly the emission."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / f"{composition.hash128}.json"
    if not path.is_file() or path.read_bytes() != composition.data:
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_bytes(composition.data)
        os.replace(tmp, path)
    return path


def thesis_of(composition: Composition) -> str:
    parts = [f"{s.member.name}({s.member.member_id[:12]},{s.fit})" for s in composition.members]
    fast = claude_worker.regime.describe(composition.words.get("fast", 0))
    slow = claude_worker.regime.describe(composition.words.get("slow", 0))
    return f"composed ({composition.words_source}) fast=[{fast}] slow=[{slow}] members: " + ", ".join(parts)


# ---- gate ----


class WallBudget:
    """The 2 h ceiling every gate run is charged to (fail, never wait)."""

    def __init__(self, budget_s: float = WALL_BUDGET_S, clock: typing.Callable[[], float] = time.monotonic) -> None:
        self._clock = clock
        self._start = clock()
        self.budget_s = budget_s

    def elapsed(self) -> float:
        return self._clock() - self._start

    def check(self, step: str) -> None:
        elapsed = self.elapsed()
        if elapsed >= self.budget_s:
            raise BudgetExceeded(f"{step}: wall budget {self.budget_s:.0f} s exhausted after {elapsed:.0f} s")


class GateVerdict(typing.NamedTuple):
    passed: bool
    reasons: list[str]
    pooled: claude_worker.backtest.GateResult | None
    pooled_net: float | None
    off_net: float | None
    lowo: list[tuple[str, float]]
    evidence_runs: int
    windows: list[str]
    wall_s: float
    report_path: pathlib.Path | None

    def as_dict(self) -> dict[str, object]:
        return {
            "passed": self.passed,
            "reasons": list(self.reasons),
            "pooled": None if self.pooled is None else self.pooled._asdict(),
            "pooled_net": self.pooled_net,
            "off_net": self.off_net,
            "lowo": [{"window": w, "net": n} for w, n in self.lowo],
            "evidence_runs": self.evidence_runs,
            "windows": list(self.windows),
            "wall_s": self.wall_s,
        }


def _regime_off_flags() -> list[str]:
    return ["--regime", "off"]


def _report_regime_block(report_path: pathlib.Path) -> dict[str, object] | None:
    """The RG8 ``regime`` block of a worker report (``None`` on a pre-RG8
    report or an unreadable file)."""
    try:
        obj = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    block = obj.get("regime") if isinstance(obj, dict) else None
    return block if isinstance(block, dict) and block.get("labelled") else None


def gate(  # noqa: PLR0913, PLR0917 — one parameter per gate input, deliberately
    state: claude_worker.state.State,
    composition: Composition,
    composition_path: pathlib.Path,
    pool_dir: pathlib.Path,
    windows: list[pathlib.Path],
    work_dir: pathlib.Path,
    *,
    fees: list[str],
    run_fn: typing.Callable[[list[str]], str] | None = None,
    report: typing.Callable[[str], None] = print,
    budget: WallBudget | None = None,
    lowo: bool = True,
    min_windows: int = MIN_WINDOWS,
    ts: int | None = None,
) -> GateVerdict:
    """Step 4: evidence per member per window (skipping rows that exist),
    the pooled frozen gate (the binding report), the on/off delta, LOWO.
    Never raises on a verdict — a budget overrun or a harness error is a
    FAIL with its reason."""
    wall = WallBudget() if budget is None else budget
    reasons: list[str] = []
    names = [w.name for w in windows]
    evidence_runs = 0
    pooled: claude_worker.backtest.GateResult | None = None
    pooled_net: float | None = None
    off_net: float | None = None
    lowo_nets: list[tuple[str, float]] = []
    report_path: pathlib.Path | None = None
    if len(windows) < min_windows:
        reasons.append(f"pool holds {len(windows)} window(s) < {min_windows}")
        return GateVerdict(False, reasons, None, None, None, [], 0, names, wall.elapsed(), None)
    try:
        # evidence rows (per member x window, cached by window id)
        for s in composition.members:
            have = {r.window_id for r in state.evidence_for(s.member.member_id)}
            for w in windows:
                if w.name in have:
                    continue
                wall.check(f"evidence {s.member.name} {w.name}")
                ev = claude_worker.library.run_evidence(state, s.member, w, work_dir, fees=fees, run_fn=run_fn, ts=ts)
                evidence_runs += 1
                report(
                    f"{_TELL} evidence: {s.member.name} {w.name} fills={ev.n_fills} tier={ev.net_usd_tier:+.2f}"
                    f" zero={ev.net_usd_0:+.2f} judged={ev.judged} word={ev.regime_word_mode or '-'}"
                )
        # the pooled frozen gate — the report a stage binds on
        wall.check("pooled gate")
        outcome = claude_worker.backtest.run_backtest(composition_path, pool_dir, run_fn=run_fn)
        pooled = outcome.gates
        pooled_net = outcome.harness.oos_net_pnl_usd
        report_path = outcome.report_path
        report(
            f"{_TELL} pooled: net={pooled_net:+.2f} legs={outcome.harness.oos_legs if outcome.harness.oos_legs >= 0 else outcome.harness.oos_trades}"
            f" rt={outcome.harness.oos_round_trips} days={outcome.harness.oos_trading_days}"
            f" dd={outcome.harness.oos_max_drawdown_usd:.2f} -> {'PASS' if pooled.all_passed else 'FAIL'}"
        )
        if not pooled.all_passed:
            reasons.append("pooled gates: " + ", ".join(k for k, v in pooled._asdict().items() if not v))
        # the on/off delta: the labels must not be worse than their absence.
        # RG8: for a LABELLED table the frozen run above already replayed
        # `--regime off` (the earned-label gate) and recorded it in the
        # report — reuse it (one harness run saved under the 2 h budget);
        # an unlabelled table (only with --include-any) still runs it here.
        wall.check("regime off")
        regime_block = _report_regime_block(report_path)
        if regime_block is not None and regime_block.get("net_off") is not None:
            off_net = float(typing.cast(float, regime_block["net_off"]))
        else:
            off = claude_worker.backtest.run_harness_extra(composition_path, pool_dir, extra_flags=_regime_off_flags(), run_fn=run_fn)
            off_net = off.harness.oos_net_pnl_usd
        report(f"{_TELL} regime off: net={off_net:+.2f} delta(on-off)={pooled_net - off_net:+.2f}")
        if pooled_net - off_net < 0:
            reasons.append(f"on/off delta negative ({pooled_net - off_net:+.2f})")
        # leave-one-window-out: every pool-minus-one keeps OOS net > 0
        if lowo:
            for k, held_out in enumerate(windows):
                wall.check(f"lowo {held_out.name}")
                root = claude_worker.window_root.symlink_root(work_dir / f"lowo-{k}", [w for w in windows if w is not held_out])
                res = claude_worker.backtest.run_harness_extra(composition_path, root, run_fn=run_fn)
                lowo_nets.append((held_out.name, res.harness.oos_net_pnl_usd))
                report(f"{_TELL} lowo -{held_out.name}: net={res.harness.oos_net_pnl_usd:+.2f}")
            bad = [w for w, n in lowo_nets if n <= 0]
            if bad:
                reasons.append("lowo: OOS net <= 0 without " + ", ".join(bad))
    except BudgetExceeded as exc:
        reasons.append(str(exc))
    except (claude_worker.backtest.BacktestError, claude_worker.library.LibraryError, OSError) as exc:
        reasons.append(f"harness: {exc}")
    passed = not reasons
    return GateVerdict(passed, reasons, pooled, pooled_net, off_net, lowo_nets, evidence_runs, names, wall.elapsed(), report_path)


# ---- promote ----


class PromoteResult(typing.NamedTuple):
    done: bool
    tell: str
    staged_seq: int | None
    committed_seq: int | None
    epoch_before: int | None
    epoch_after: int | None


def metrics_table_epoch(url: str, timeout_s: float = 2.0) -> int | None:
    try:
        with urllib.request.urlopen(url, timeout=timeout_s) as resp:  # loopback only
            text = resp.read().decode("utf-8", errors="replace")
    except (OSError, ValueError):
        return None
    for line in text.splitlines():
        if line.startswith("engine_vm_table_epoch "):
            try:
                return int(line.split()[1])
            except (IndexError, ValueError):
                return None
    return None


def promote(  # noqa: PLR0913 — one parameter per promotion input, deliberately
    state: claude_worker.state.State,
    cfg: claude_worker.config.BaseConfig,
    composition: Composition,
    composition_path: pathlib.Path,
    report_path: pathlib.Path,
    *,
    freeze_file: pathlib.Path,
    metrics_url: str | None = None,
    report: typing.Callable[[str], None] = print,
    wait_s: float = EPOCH_POLL_S,
    clock: typing.Callable[[], float] = time.monotonic,
    sleep: typing.Callable[[float], None] = time.sleep,
) -> PromoteResult:
    """Step 5: FREEZE pin -> refuse; hash == active -> no-op; else install
    → stage → commit through the frozen pair, then watch ``table_epoch``."""
    if freeze_file.is_file():
        return PromoteResult(False, f"promotion refused: {freeze_file} pin is set (soak)", None, None, None, None)
    active = claude_worker.library.active_hash(state)
    if active == composition.full_hash:
        return PromoteResult(False, f"already active: {composition.hash128}", None, None, None, None)
    if claude_worker.library.active_canonical_hash(state, cfg.ai_ruleset_dir) == composition.full_hash:
        return PromoteResult(
            False,
            f"already active: the live table {(active or '')[:_HASH128_HEX]} carries these very rows"
            f" (non-canonical bytes) — no flip for {composition.hash128}",
            None, None, None, None,
        )
    installed = claude_worker.strategist.install_candidate(cfg.ai_ruleset_dir, composition_path, composition.hash128)
    installed_report = claude_worker.backtest.report_path_for(installed)
    installed_report.write_bytes(report_path.read_bytes())
    url = claude_worker.regime.metrics_url() if metrics_url is None else metrics_url
    epoch_before = metrics_table_epoch(url)
    client = claude_worker.uds.UdsClient(cfg.ai_ingress_sock, cfg.ai_ingress_hmac_key, state)
    client.connect()
    try:
        client.send_heartbeat()
        staged_seq, full_hash = claude_worker.backtest.stage_ruleset(state, client, installed, installed_report, "session")
        state.stage_ruleset(full_hash, str(installed), str(installed_report), "session", thesis=thesis_of(composition))
        state.composition_mark(full_hash, "staged_ts")
        committed_seq = claude_worker.backtest.commit_ruleset(state, client, full_hash)
        state.composition_mark(full_hash, "committed_ts")
    finally:
        client.close()
    report(f"{_TELL} promote: installed {installed.name}, staged seq={staged_seq}, committed seq={committed_seq}")
    epoch_after = epoch_before
    if epoch_before is not None:
        deadline = clock() + wait_s
        while clock() < deadline:
            epoch_after = metrics_table_epoch(url)
            if epoch_after is not None and epoch_after > epoch_before:
                break
            sleep(0.5)
    if epoch_before is None:
        tell = "promoted (engine /metrics unreachable — verify vm_rows_active/table_epoch by hand)"
    elif epoch_after is not None and epoch_after > epoch_before:
        tell = f"promoted: table_epoch {epoch_before} -> {epoch_after}"
    else:
        tell = f"frames sent but table_epoch stayed {epoch_before} within {wait_s:.0f} s — check the engine log (validator refusal?)"
    return PromoteResult(True, tell, staged_seq, committed_seq, epoch_before, epoch_after)


# ---- rendering ----


def render(composition: Composition) -> str:
    u = cap_usage(composition.rows)
    lines = [
        f"{_TELL}: words ({composition.words_source}) fast=[{claude_worker.regime.describe(composition.words.get('fast', 0))}]"
        f" slow=[{claude_worker.regime.describe(composition.words.get('slow', 0))}]",
        f"{_TELL}: table {composition.hash128} rows={u.rows} table=${u.table_usd:,.0f} max_symbol=${u.max_symbol_usd:,.0f}"
        f" members={len(composition.members)}",
    ]
    for s in composition.members:
        lines.append(
            f"  + {s.member.name} {s.member.member_id[:12]} fit={s.fit} rows={len(s.member.rows)}"
            f" labels={claude_worker.library.describe_labels(s.member.labels)}"
            f" evidence={s.evidence.windows}w/{s.evidence.judged}j tier={s.evidence.net_usd_tier:+.2f}"
        )
    for name, reason in composition.skipped:
        lines.append(f"  - {name}: {reason}")
    return "\n".join(lines)


# ---- CLI ----


def main(argv: list[str] | None = None) -> int:  # noqa: PLR0911, PLR0912, PLR0915 — one dispatcher, one linear pipeline
    parser = argparse.ArgumentParser(prog="claude_worker.compose")
    parser.add_argument("--db", default=None, help=f"state.db (default ${claude_worker.library.DB_ENV})")
    parser.add_argument("--library-dir", default=None, help=f"library dir (default ${claude_worker.library.LIBRARY_DIR_ENV})")
    parser.add_argument("--regime-dir", default=None, help="worker regime state dir (declared.json)")
    parser.add_argument("--regime", default=None, help='words: current | "<fast-decl>[;<slow-decl>]" (default: engine -> declaration -> measured)')
    parser.add_argument("--regime-toml", default=claude_worker.regime.REGIME_PATH_DEFAULT)
    parser.add_argument("--candles-db", default=None, help="candles.db for the measured fallback")
    parser.add_argument("--include-candidates", action="store_true")
    parser.add_argument("--fit-from-evidence", action="store_true")
    parser.add_argument(
        "--include-any",
        action="store_true",
        help="admit unlabelled (ANY) members too — RG8 excludes them by default",
    )
    parser.add_argument("--min-windows", type=int, default=MIN_WINDOWS)
    parser.add_argument("--dry-run", action="store_true", help="select + emit only (no harness run)")
    parser.add_argument("--promote", action="store_true", help="on PASS: install -> stage -> commit (hash change only)")
    parser.add_argument("--no-lowo", action="store_true", help="skip leave-one-window-out (evidence + pooled + off only)")
    parser.add_argument("--pool", default=None, help="window pool dir (default <worker dir>/windows)")
    parser.add_argument("--pool-size", type=int, default=claude_worker.window_root.POOL_SIZE_DEFAULT)
    parser.add_argument("--no-refresh", action="store_true", help="use the pool as it is (no cut/prune)")
    parser.add_argument("--logs", default=None, help=f"capture runs root (default ${LOGS_DIR_ENV} or {DEFAULT_LOGS_DIR})")
    parser.add_argument("--fees", default=claude_worker.pnl_report.FEES_PATH_DEFAULT, help="fees.toml for evidence rows (or 'none')")
    parser.add_argument("--budget-s", type=float, default=WALL_BUDGET_S, help="wall budget for the gate (<= 7200)")
    parser.add_argument("--freeze", action="store_true", help="set the FREEZE pin (promotion refused until --unfreeze)")
    parser.add_argument("--unfreeze", action="store_true")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--now-ms", type=int, default=None, help="tests only")
    args = parser.parse_args(argv)

    db = pathlib.Path(args.db).expanduser() if args.db else claude_worker.library.db_path()
    lib_dir = pathlib.Path(args.library_dir).expanduser() if args.library_dir else claude_worker.library.library_dir()
    comp_dir = compositions_dir_for(db)
    freeze_file = comp_dir / FREEZE_FILE
    if args.freeze or args.unfreeze:
        comp_dir.mkdir(parents=True, exist_ok=True)
        if args.freeze:
            freeze_file.write_text(f"frozen {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}\n")
            print(f"{_TELL}: FREEZE pin set ({freeze_file})")
        else:
            freeze_file.unlink(missing_ok=True)
            print(f"{_TELL}: FREEZE pin cleared")
        return 0
    if args.budget_s > WALL_BUDGET_S or args.budget_s <= 0:
        print(f"{_TELL}: --budget-s must be in (0, {WALL_BUDGET_S:.0f}] (the 2 h law)", file=sys.stderr)
        return 2

    now_ms = args.now_ms if args.now_ms is not None else int(time.time() * 1000)
    regime_dir = pathlib.Path(args.regime_dir).expanduser() if args.regime_dir else claude_worker.regime.regime_dir()
    regime_toml = pathlib.Path(args.regime_toml).expanduser()
    candles_db = (
        pathlib.Path(args.candles_db).expanduser() if args.candles_db else claude_worker.regime.regime_inputs()[1]
    )
    state = claude_worker.state.State(db)
    try:
        try:
            words, source = effective_words(
                regime_dir, now_ms, regime_toml=regime_toml, candles_db=candles_db, query=args.regime
            )
            composition = compose(
                state,
                lib_dir,
                words,
                source,
                include_candidates=args.include_candidates,
                fit_from_evidence=args.fit_from_evidence,
                include_any=args.include_any,
                min_windows=args.min_windows,
            )
        except (ComposeError, claude_worker.library.LibraryError) as exc:
            print(f"{_TELL}: {exc}", file=sys.stderr)
            return 2
        path = write_composition(comp_dir, composition)
        state.composition_insert(
            composition.full_hash, composition.hash128, [s.member.member_id for s in composition.members],
            words_hex(composition.words), str(path), None,
        )
        out: dict[str, object] = {
            "hash": composition.full_hash,
            "hash128": composition.hash128,
            "path": str(path),
            "words": words_hex(composition.words),
            "words_source": composition.words_source,
            "members": [
                {"member_id": s.member.member_id, "name": s.member.name, "fit": s.fit, "rows": len(s.member.rows)}
                for s in composition.members
            ],
            "skipped": [{"name": n, "reason": r} for n, r in composition.skipped],
            "rows": len(composition.rows),
        }
        if not args.json:
            print(render(composition))
            print(f"{_TELL}: artifact {path}")
        if args.dry_run:
            if args.json:
                print(json.dumps(out, sort_keys=True))
            return 0

        pool_dir = pathlib.Path(args.pool).expanduser() if args.pool else claude_worker.window_root.pool_dir_for(db)
        seed = claude_worker.regime.regime_inputs()
        seed_pair = seed if seed[0].is_file() and seed[1].is_file() else None
        if args.no_refresh:
            windows = claude_worker.window_root.pool_windows(pool_dir)[-args.pool_size :]
        else:
            windows = claude_worker.window_root.pool_ensure(
                logs_dir() if args.logs is None else pathlib.Path(args.logs).expanduser(),
                pool_dir,
                args.pool_size,
                seed_pair,
                report=None if args.json else print,
            )
        if seed_pair is None and not args.json:
            print(f"{_TELL}: NO regime seed pair (regime.toml/candles.db) — windows warm live; labelled rows may fail closed")
        fees = [] if args.fees == "none" else claude_worker.library.fee_flags(pathlib.Path(args.fees).expanduser())
        verdict = gate(
            state,
            composition,
            path,
            pool_dir,
            windows,
            comp_dir / "work",
            fees=fees,
            report=(lambda _s: None) if args.json else print,
            budget=WallBudget(args.budget_s),
            lowo=not args.no_lowo,
            min_windows=args.min_windows,
        )
        state.composition_insert(
            composition.full_hash, composition.hash128, [s.member.member_id for s in composition.members],
            words_hex(composition.words), str(path), verdict.as_dict(),
        )
        out["gate"] = verdict.as_dict()
        if not args.json:
            print(f"{_TELL}: gate {'PASS' if verdict.passed else 'FAIL'} ({verdict.wall_s:.0f} s, {len(verdict.windows)} windows)"
                  + ("" if verdict.passed else " — " + "; ".join(verdict.reasons)))
        if not verdict.passed:
            if args.json:
                print(json.dumps(out, sort_keys=True))
            return 3
        if args.promote:
            if verdict.report_path is None:
                print(f"{_TELL}: no binding report — cannot promote", file=sys.stderr)
                return 2
            cfg = claude_worker.config.load_base_from_env()
            try:
                result = promote(state, cfg, composition, path, verdict.report_path, freeze_file=freeze_file,
                                 report=(lambda _s: None) if args.json else print)
            except (claude_worker.uds.UdsError, claude_worker.backtest.GateRefused) as exc:
                print(f"{_TELL}: promote failed: {exc}", file=sys.stderr)
                return 4
            out["promote"] = result._asdict()
            if not args.json:
                print(f"{_TELL}: {result.tell}")
        if args.json:
            print(json.dumps(out, sort_keys=True))
        return 0
    finally:
        state.close()


if __name__ == "__main__":
    sys.exit(main())
