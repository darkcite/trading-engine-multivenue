"""Fable-5 strategist internals (design §7) + the §8.1 install step.

serve-ONLY collaborator: the 7-verb surface is frozen — this module is
composed by ``daemon.py``'s research cycle and never appears on the CLI.
In semi-manual mode the operator's session IS the strategist
(``docs/prompts/ai-session.md`` §4) — same artifact grammar, same gates.

Division of labor (§7.6 threading law): everything here is either pure
(prompt build, §7.3 strict parse, digest capping, budget arithmetic) or
file-writing (candidate/rejected archives, the §8.1 atomic install) or
the ONE background-thread entry point [`call_with_cache`] — which opens
its OWN SQLite handle and touches the ``prompt_cache`` table only, via
the pinned ``state.cached_complete`` seam. Events-ledger reads/writes
(``strategist_call`` and friends) happen on the serve-loop thread inside
``daemon.py``; UDS frames never originate here.

Inputs (§7.1, all files, no sockets): feature files (replay-derived +
§6.1 REST) under ``CLAUDE_WORKER_FEATURES_DIR/<run>/``, the news NDJSON
digest under ``.../news/``, the market map, and the static grammar/caps
contract below. The ACTIVE ruleset's walk-forward report joins the
digest in 8h-H5 (§8.4) — the parameter seam exists now.

Strict-parse doctrine (§7.3, labeling.py discipline): the model's output
is validated STRUCTURALLY here — exact key sets, types (bool-rejecting),
enums, and the published domain bounds mirrored below. The semantic rule
families (universe membership, duplicate rows, cap Σ walks) remain the
harness-side ``ingress_ai::validate_ruleset`` — no second deep parser
exists to drift (§3.5 doctrine).

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import hashlib
import json
import os
import pathlib
import time
import typing

import claude_worker.config
import claude_worker.llm
import claude_worker.state

# ---- §7.5 env keys (read at the seam — the H3 interpretation-#2
# precedent: the frozen 202 construct ServeConfig directly, so the config
# dataclass field tuple is itself a frozen surface) ----------------------

STRATEGIST_INTERVAL_ENV: str = "CLAUDE_WORKER_STRATEGIST_INTERVAL_S"
STRATEGIST_INTERVAL_S_DEFAULT: int = 21_600  # 6 h cycle (H-D4)
STRATEGIST_DAILY_CAP_ENV: str = "CLAUDE_WORKER_STRATEGIST_DAILY_CAP"
STRATEGIST_DAILY_CAP_DEFAULT: int = 12

# ≤ 2 Fable-5 calls per cycle (H-D4): one proposal + one revision.
MAX_CALLS_PER_CYCLE: int = 2

# Dynamic-digest character cap (§7.2; the feeds.py TEXT_CAP precedent —
# a char cap, applied at build time so the user block is bounded no
# matter how much capture/news exists).
STRATEGIST_INPUT_CAP: int = 24_000
_TRUNCATION_MARKER: str = "\n...[digest truncated at cap]"

# Prompt-template version: feeds the prompt_cache key (state.py §5.3).
# Bump on ANY change to the static block or user-prompt scaffolding so
# stale cached responses cannot leak across versions (labeling.py rule).
STRATEGIST_PROMPT_VERSION: str = "strategist-v1"

# ---- events-ledger kinds (§7.5; written by daemon.py on the serve
# thread — the events table is serve-loop-only under the §7.6 law) ------

EVENT_STRATEGIST_CALL: str = "strategist_call"
EVENT_BUDGET_SKIP: str = "strategist_budget_skip"
EVENT_CAPTURE_SKIP: str = "strategist_capture_skip"
EVENT_CANDIDATE_REJECTED: str = "strategist_candidate_rejected"
EVENT_CALL_FAILED: str = "strategist_call_failed"
EVENT_PROMOTION: str = "promotion"

# ---- §4.1 grammar mirrors (published constants; the authoritative
# enforcement is `ingress_ai::validate_ruleset` in the harness) ---------

MAX_RULESET_ROWS: int = 256
NAME_LEN_MAX: int = 64
EDGE_BPS_MAX: int = 10_000
HORIZON_MS_MIN: int = 10
HORIZON_MS_MAX: int = 86_400_000
LEVEL_MAX: float = 1.0
ROW_MAX_RISK_USD: float = 100.0  # risk-policy single-order cap, tighten-only
MAX_THESIS_CHARS: int = 4_000

FAMILIES: tuple[str, ...] = ("crypto", "politics", "sports", "macro", "other")
SIDES: tuple[str, ...] = ("bid", "ask", "both")

_ROW_KEYS: frozenset[str] = frozenset(
    {"name", "family", "trigger", "sym", "side", "edge_bps", "horizon_ms", "max_risk_usd"}
)
_ROW_KEY_ORDER: tuple[str, ...] = (
    "name",
    "family",
    "trigger",
    "sym",
    "side",
    "edge_bps",
    "horizon_ms",
    "max_risk_usd",
)
_SYMBOL_ID_MAX: int = 0xFFFF_FFFE  # < SYMBOL_ID_NONE

_REJECTED_SUFFIX: str = ".rejected.json"


def interval_s(env: collections.abc.Mapping[str, str] | None = None) -> int:
    """Cycle interval from ``CLAUDE_WORKER_STRATEGIST_INTERVAL_S``
    (default 21600). Strict: malformed or < 1 is a usage error — a
    silently-tiny interval would burn the Anthropic budget."""
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    raw = source.get(STRATEGIST_INTERVAL_ENV, "")
    if not raw:
        return STRATEGIST_INTERVAL_S_DEFAULT
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError(f"{STRATEGIST_INTERVAL_ENV} must be an integer: {raw!r}") from exc
    if value < 1:
        raise ValueError(f"{STRATEGIST_INTERVAL_ENV} must be >= 1: {value}")
    return value


def daily_cap(env: collections.abc.Mapping[str, str] | None = None) -> int:
    """Hard daily Fable-5 call ceiling from
    ``CLAUDE_WORKER_STRATEGIST_DAILY_CAP`` (default 12). ``0`` is a
    legal kill switch (every cycle budget-skips); negative/malformed is
    a usage error."""
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    raw = source.get(STRATEGIST_DAILY_CAP_ENV, "")
    if not raw:
        return STRATEGIST_DAILY_CAP_DEFAULT
    try:
        value = int(raw)
    except ValueError as exc:
        raise ValueError(f"{STRATEGIST_DAILY_CAP_ENV} must be an integer: {raw!r}") from exc
    if value < 0:
        raise ValueError(f"{STRATEGIST_DAILY_CAP_ENV} must be >= 0: {value}")
    return value


def candidates_dir(db_path: pathlib.Path) -> pathlib.Path:
    """The §7.3 candidates directory: the worker dir (``db_path``'s
    parent) + ``candidates`` — `~/multivenue/worker/candidates/` under
    the default ``CLAUDE_WORKER_DB`` (§14 pinned path), and test-local
    under a tmp db. No new env key."""
    return db_path.parent / "candidates"


# ---- §7.2 prompt architecture ------------------------------------------

# STATIC system block: grammar + validator rules + caps + output contract
# + one worked example. Marked `cache_control: ephemeral` so every call
# after the first in a cache window pays ~10% for this bulk. Content
# changes REQUIRE a STRATEGIST_PROMPT_VERSION bump.
_STATIC_SYSTEM_TEXT: str = (
    "You are the offline strategist for a latency-arbitrage trading engine"
    " (Polymarket CLOB + reference venues). You author Tier-1 ruleset"
    " artifacts only; you never touch execution. Your candidate is"
    " validated by a strict byte-scanner and then backtested over real"
    " capture; hard gates decide promotion. Paper trading only.\n"
    "\n"
    "RULESET GRAMMAR (exact; unknown or duplicate keys are rejected):\n"
    'A ruleset is {"rows": [ROW, ...]} with 1..256 rows. Each ROW has'
    " EXACTLY these keys:\n"
    '  "name":        ASCII string, 1..64 chars, unique per table\n'
    '  "family":      one of "crypto"|"politics"|"sports"|"macro"|"other"\n'
    '  "trigger":     {"type": "cross_deviation", "ref": <SymbolId>}\n'
    '                 or {"type": "level_breach", "level": <0..1>}\n'
    '                 (no other trigger keys; level_breach must NOT carry "ref")\n'
    '  "sym":         integer SymbolId — the ACTION leg (order-emitting)\n'
    '  "side":        "bid"|"ask"|"both"\n'
    '  "edge_bps":    integer 0..10000 (no fractional part)\n'
    '  "horizon_ms":  integer 10..86400000 (cooldown/decision horizon)\n'
    '  "max_risk_usd": number > 0, <= 100.0 (per-row notional cap)\n'
    "\n"
    "VALIDATOR RULES (mirror of the engine's eight families):\n"
    "- Every sym and every cross_deviation ref must exist in the observed"
    " capture universe given in the user message; ref != sym.\n"
    "- Risk caps are TIGHTEN-ONLY mirrors of risk policy: <= $100 per row,"
    " sum per symbol <= $250, whole-table sum <= $1000. Never propose more.\n"
    "- Exact-duplicate rows (same sym, trigger, side, ref/level) are rejected.\n"
    "\n"
    "BACKTEST GATES the candidate must pass to be promoted (strict-cross"
    " maker fills, fees and latency charged, 70/30 IS/OOS split, OOS"
    " scored): OOS net P&L > 0; >= 50 OOS trades; >= 2 OOS trading days;"
    " OOS max drawdown <= $200; observed bounds within the caps above.\n"
    "\n"
    "OUTPUT CONTRACT (strict): respond with EXACTLY one JSON object and"
    ' nothing else — no prose, no code fences:\n'
    '{"thesis": "<why these rows, <= 4000 chars>", "rows": [ROW, ...]}\n'
    "Malformed output is archived and discarded; the cycle ends.\n"
    "\n"
    "WORKED EXAMPLE (shape only — author against the CURRENT digest):\n"
    '{"thesis": "BTC prints lead the PM btc-daily market; fade lagged'
    ' quotes on breach.", "rows": [{"name": "btc-pm-lag", "family":'
    ' "crypto", "trigger": {"type": "cross_deviation", "ref": 7}, "sym":'
    ' 42, "side": "bid", "edge_bps": 80, "horizon_ms": 1500,'
    ' "max_risk_usd": 50.0}]}\n'
)


def system_blocks() -> list[dict[str, object]]:
    """The §7.2 STATIC system block list, cache_control-marked."""
    return [
        {
            "type": "text",
            "text": _STATIC_SYSTEM_TEXT,
            "cache_control": {"type": "ephemeral"},
        }
    ]


def _append_capped(parts: list[str], used: int, text: str, cap: int) -> int:
    """Append ``text`` while the digest stays under ``cap`` chars;
    returns the new used-count. On overflow appends the truncation
    marker once and pins ``used`` at cap."""
    if used >= cap:
        return used
    room = cap - used
    if len(text) <= room:
        parts.append(text)
        return used + len(text)
    parts.append(text[:room])
    parts.append(_TRUNCATION_MARKER)
    return cap


def _news_tail(news_dir: pathlib.Path, budget: int) -> str:
    """Newest-first NDJSON lines from ``items-*.ndjson`` up to
    ``budget`` chars (file names embed ``time_ns`` — lexical sort is
    chronological)."""
    if not news_dir.is_dir():
        return ""
    lines: list[str] = []
    used = 0
    for path in sorted(news_dir.glob("items-*.ndjson"), reverse=True):
        try:
            text = path.read_text()
        except OSError:
            continue
        for line in reversed(text.splitlines()):
            if not line:
                continue
            if used + len(line) + 1 > budget:
                return "\n".join(reversed(lines))
            lines.append(line)
            used += len(line) + 1
    return "\n".join(reversed(lines))


def build_digest(
    features_dir: pathlib.Path,
    run_name: str | None,
    markets: dict[str, int],
    universe: collections.abc.Iterable[int] | None = None,
    performance: str | None = None,
    cap: int = STRATEGIST_INPUT_CAP,
) -> str:
    """The DYNAMIC user-block digest (§7.2): market map, capture-derived
    universe, feature files of the named run (replay-derived + §6.1
    REST, same directory), news NDJSON tail, and — from H5 — the ACTIVE
    ruleset's walk-forward ``performance`` text. Deterministic for
    identical file contents (the SQLite dedupe key rides on this)."""
    parts: list[str] = []
    used = 0
    map_lines = "\n".join(f"  {name} -> sym {markets[name]}" for name in sorted(markets))
    used = _append_capped(parts, used, f"MARKET MAP (name -> SymbolId):\n{map_lines or '  (empty)'}\n", cap)
    if universe is not None:
        syms = ", ".join(str(s) for s in sorted(set(universe)))
        used = _append_capped(parts, used, f"\nOBSERVED CAPTURE UNIVERSE (legal sym/ref values): {syms or '(none)'}\n", cap)
    if run_name is not None:
        run_dir = features_dir / run_name
        used = _append_capped(parts, used, f"\nFEATURES ({run_name}):\n", cap)
        if run_dir.is_dir():
            for path in sorted(run_dir.glob("*.json")):
                try:
                    body = path.read_text().strip()
                except OSError:
                    continue
                used = _append_capped(parts, used, f"{path.name}: {body}\n", cap)
    if performance is not None:
        used = _append_capped(parts, used, f"\nACTIVE RULESET WALK-FORWARD:\n{performance}\n", cap)
    news = _news_tail(features_dir / "news", max(0, cap - used))
    if news:
        used = _append_capped(parts, used, f"\nNEWS (NDJSON, oldest->newest):\n{news}\n", cap)
    return "".join(parts)


def build_user_prompt(digest: str) -> str:
    """Call-#1 (proposal) user block: instruction scaffold + digest."""
    return (
        "Author ONE ruleset candidate from the research digest below,"
        " per the system contract. Choose sym/ref values ONLY from the"
        " observed capture universe.\n\n" + digest
    )


def build_revision_prompt(digest: str, prior_rows_json: str, gate_summary: str, report_text: str) -> str:
    """Call-#2 (revision) user block: the §7.4 gate summary + worker
    report ride along with the prior candidate."""
    return (
        "Your previous candidate FAILED the backtest gates. Revise it —"
        " same output contract, sym/ref only from the observed universe.\n"
        f"\nPREVIOUS ROWS:\n{prior_rows_json}\n"
        f"\nGATES: {gate_summary}\n"
        f"\nBACKTEST REPORT:\n{report_text}\n\n" + digest
    )


# ---- §7.3 output contract: strict structural parse ---------------------


class Proposal(typing.NamedTuple):
    """A parsed, structurally-valid strategist proposal."""

    thesis: str
    rows: list[dict[str, object]]  # canonical key order, validated types


def _int_field(value: object, lo: int, hi: int) -> int | None:
    """Bool-rejecting strict integer (validator rule-3 mirror: a
    fractional literal is NOT an integer field)."""
    if isinstance(value, bool) or not isinstance(value, int):
        return None
    if not lo <= value <= hi:
        return None
    return value


def _number_field(value: object, lo: float, hi: float) -> float | None:
    """Bool-rejecting numeric in [lo, hi]."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    out = float(value)
    if not lo <= out <= hi:
        return None
    return out


def _parse_trigger(value: object, sym: int) -> dict[str, object] | None:
    if not isinstance(value, dict):
        return None
    obj = typing.cast(dict[str, object], value)
    kind = obj.get("type")
    if kind == "cross_deviation":
        if set(obj) != {"type", "ref"}:
            return None
        ref = _int_field(obj.get("ref"), 0, _SYMBOL_ID_MAX)
        if ref is None or ref == sym:  # rule-6 mirror: ref != sym
            return None
        return {"type": "cross_deviation", "ref": ref}
    if kind == "level_breach":
        if set(obj) != {"type", "level"}:  # rule-6 mirror: no ref here
            return None
        level = _number_field(obj.get("level"), 0.0, LEVEL_MAX)
        if level is None:
            return None
        return {"type": "level_breach", "level": level}
    return None


def _parse_row(value: object) -> dict[str, object] | None:
    if not isinstance(value, dict):
        return None
    obj = typing.cast(dict[str, object], value)
    if set(obj) != _ROW_KEYS:
        return None
    name = obj.get("name")
    if not isinstance(name, str) or not 1 <= len(name) <= NAME_LEN_MAX or not name.isascii():
        return None
    family = obj.get("family")
    if family not in FAMILIES:
        return None
    side = obj.get("side")
    if side not in SIDES:
        return None
    sym = _int_field(obj.get("sym"), 0, _SYMBOL_ID_MAX)
    if sym is None:
        return None
    trigger = _parse_trigger(obj.get("trigger"), sym)
    if trigger is None:
        return None
    edge_bps = _int_field(obj.get("edge_bps"), 0, EDGE_BPS_MAX)
    if edge_bps is None:
        return None
    horizon_ms = _int_field(obj.get("horizon_ms"), HORIZON_MS_MIN, HORIZON_MS_MAX)
    if horizon_ms is None:
        return None
    max_risk = _number_field(obj.get("max_risk_usd"), 0.0, ROW_MAX_RISK_USD)
    if max_risk is None or max_risk <= 0.0:
        return None
    ordered: dict[str, object] = {}
    values: dict[str, object] = {
        "name": name,
        "family": family,
        "trigger": trigger,
        "sym": sym,
        "side": side,
        "edge_bps": edge_bps,
        "horizon_ms": horizon_ms,
        "max_risk_usd": max_risk,
    }
    for key in _ROW_KEY_ORDER:
        ordered[key] = values[key]
    return ordered


def parse_proposal(raw: str) -> Proposal | None:
    """STRICT §7.3 parse: exactly ``{"thesis": str, "rows": [...]}`` with
    rows in the §4.1 grammar (structural bounds mirrored above). ``None``
    on ANY deviation — the caller archives + counts, never crashes.
    Oversized (> 256 rows) is malformed."""
    try:
        obj = json.loads(raw)
    except (ValueError, TypeError):
        return None
    if not isinstance(obj, dict):
        return None
    top = typing.cast(dict[str, object], obj)
    if set(top) != {"thesis", "rows"}:
        return None
    thesis = top.get("thesis")
    if not isinstance(thesis, str) or not 1 <= len(thesis) <= MAX_THESIS_CHARS:
        return None
    rows_raw = top.get("rows")
    if not isinstance(rows_raw, list):
        return None
    entries = typing.cast(list[object], rows_raw)
    if not 1 <= len(entries) <= MAX_RULESET_ROWS:
        return None
    rows: list[dict[str, object]] = []
    for entry in entries:
        row = _parse_row(entry)
        if row is None:
            return None
        rows.append(row)
    return Proposal(thesis=thesis, rows=rows)


# ---- candidate files (§7.3) + the §8.1 install --------------------------


def artifact_bytes(rows: list[dict[str, object]]) -> bytes:
    """Canonical ``{"rows": [...]}`` artifact bytes (compact separators,
    fixed key order from [`parse_proposal`]) — deterministic, so the
    file hash is a pure function of the accepted rows. The thesis stays
    OUT of the artifact: the engine's validator is unknown-key-strict;
    attribution rides the registry column (§8.2)."""
    return json.dumps({"rows": rows}, separators=(",", ":")).encode()


def _atomic_write(path: pathlib.Path, data: bytes) -> None:
    """Same-dir temp + ``os.replace`` (§6.2 style): atomic-or-absent."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_bytes(data)
    os.replace(tmp, path)


def _utc_stamp(now_s: float | None = None) -> str:
    stamp = time.time() if now_s is None else now_s
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(stamp))


class Candidate(typing.NamedTuple):
    """A written candidate artifact, ready for the backtest verb path."""

    path: pathlib.Path
    full_hash: str
    hash128_hex: str
    thesis: str


def write_candidate(
    dir_path: pathlib.Path,
    proposal: Proposal,
    now_s: float | None = None,
) -> Candidate:
    """Write the VALID candidate to
    ``<dir>/<utc-ts>-<hash128>.json`` (§7.3), atomically. The hash is
    sha256 over the exact file bytes — the same value the frozen
    ``backtest.ruleset_hashes`` recomputes."""
    data = artifact_bytes(proposal.rows)
    digest = hashlib.sha256(data).digest()  # == backtest.ruleset_hashes math
    full_hash = digest.hex()
    hash128 = digest[:16].hex()
    path = dir_path / f"{_utc_stamp(now_s)}-{hash128}.json"
    _atomic_write(path, data)
    return Candidate(path=path, full_hash=full_hash, hash128_hex=hash128, thesis=proposal.thesis)


def archive_rejected(
    dir_path: pathlib.Path,
    raw: str,
    now_s: float | None = None,
) -> pathlib.Path:
    """§7.3 malformed-output archive: the raw model text lands in the
    candidates dir under a ``.rejected`` marker name; the caller writes
    the state.db event. Atomic."""
    path = dir_path / f"{_utc_stamp(now_s)}{_REJECTED_SUFFIX}"
    _atomic_write(path, raw.encode())
    return path


def install_candidate(
    ai_ruleset_dir: pathlib.Path,
    candidate_path: pathlib.Path,
    hash128_hex: str,
) -> pathlib.Path:
    """§8.1 promote step 1: atomic install of the gates-passed artifact
    to ``$AI_RULESET_DIR/<hash128-hex>.json`` — the path the ENGINE
    resolves a Stage frame against (ai-session §4 step 5, automated).
    Callers invoke this ONLY on gates PASS."""
    target = ai_ruleset_dir / f"{hash128_hex}.json"
    _atomic_write(target, candidate_path.read_bytes())
    return target


# ---- budget ledger arithmetic (§7.5; serve-thread callers) -------------

_NS_PER_DAY: int = 86_400_000_000_000


def utc_day_start_ns(now_ns: int) -> int:
    """Start of the current UTC calendar day, ns — the daily-ceiling
    window (§7.5; consistent with the harness's UTC trading_days)."""
    return (now_ns // _NS_PER_DAY) * _NS_PER_DAY


def calls_today(state: claude_worker.state.State, now_ns: int) -> int:
    """Fable-5 calls burned in the current UTC day — a query over the
    ``strategist_call`` ledger (§7.5: the ledger IS the counter)."""
    start = utc_day_start_ns(now_ns)
    count = 0
    for _id, ts, _kind, _detail in state.events(kind=EVENT_STRATEGIST_CALL):
        if start <= ts <= now_ns:
            count += 1
    return count


def call_detail(
    completion: claude_worker.llm.Completion,
    purpose: str,
) -> str:
    """The ``strategist_call`` event detail (§7.5): model, usage tokens,
    the Anthropic cache-read flag, and which cycle call this was."""
    return json.dumps(
        {
            "model": claude_worker.config.MODEL_STRATEGIST,
            "purpose": purpose,
            "input_tokens": completion.input_tokens,
            "output_tokens": completion.output_tokens,
            "cache_read_input_tokens": completion.cache_read_input_tokens,
            "cache_creation_input_tokens": completion.cache_creation_input_tokens,
            "cache_read": completion.cache_read_input_tokens > 0,
        },
        sort_keys=True,
        separators=(",", ":"),
    )


# ---- the background-thread call (§7.6) ---------------------------------

# The strategist completion seam daemon.py injects: (system blocks, user
# prompt) -> Completion. Bound to MODEL_STRATEGIST + STRATEGIST_MAX_TOKENS
# by the composition root; faked wholesale in tests (no live SDK).
CompleteFn = typing.Callable[[list[dict[str, object]], str], claude_worker.llm.Completion]


class CallResult(typing.NamedTuple):
    """What the background job hands back to the serve loop."""

    text: str
    sqlite_cache_hit: bool
    completion: claude_worker.llm.Completion | None  # None on a dedupe hit


def call_with_cache(
    db_path: pathlib.Path,
    user_prompt: str,
    complete_fn: CompleteFn,
) -> CallResult:
    """RUNS ON THE BACKGROUND THREAD (§7.6): opens its OWN State handle
    and touches the ``prompt_cache`` table only — through the pinned
    ``cached_complete`` seam, so an identical (version, prompt) replays
    the stored response at zero API cost. The serve loop records the
    ledger row (events table) after the future resolves; this function
    never touches ``events``, files, or the UDS."""
    holder: list[claude_worker.llm.Completion] = []

    def fn(_model: str, prompt: str) -> str:
        completion = complete_fn(system_blocks(), prompt)
        holder.append(completion)
        return completion.text

    own_state = claude_worker.state.State(db_path)
    try:
        text, hit = own_state.cached_complete(
            claude_worker.config.MODEL_STRATEGIST,
            STRATEGIST_PROMPT_VERSION,
            user_prompt,
            fn,
        )
    finally:
        own_state.close()
    return CallResult(
        text=text,
        sqlite_cache_hit=hit,
        completion=holder[0] if holder else None,
    )
