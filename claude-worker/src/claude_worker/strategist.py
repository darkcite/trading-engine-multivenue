# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
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
import math
import os
import pathlib
import time
import typing

import claude_worker.channel_map
import claude_worker.config
import claude_worker.features
import claude_worker.frames
import claude_worker.iv_digest
import claude_worker.llm
import claude_worker.pmlr
import claude_worker.pnl_report
import claude_worker.regime
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
# v2 (2026-08-30): the static block now teaches the VM2 v2 grammar and
# the $50k-tier caps/gates; every v1-era cached response is stale by
# construction.
# v3 (2026-09-05, RG3): the static block teaches the grammar v2.1 regime
# keys (`regimes` / `regime_off` / `rel`), the gate law and regime
# VARIANTS of one signal, and asks for a label on every row.
# v4 (2026-09-05, RG5): the output contract gains the OPTIONAL regime
# VERDICT (`"regime": {"fast": "<decl>|measured", …}`) the worker turns
# into a declaration; the digest gains the REGIME section.
# v5 (2026-09-05, RG8 — operator ruling "enforce regime labelling
# everywhere"): `regimes` is no longer advisory — a proposal with ONE
# unlabelled row is malformed (`parse_proposal` refuses it, the cycle
# archives it as `malformed_output`), and a label must EARN itself: the
# gate compares the table against `--regime off` (net_on >= net_off).
STRATEGIST_PROMPT_VERSION: str = "strategist-v5"

# ---- events-ledger kinds (§7.5; written by daemon.py on the serve
# thread — the events table is serve-loop-only under the §7.6 law) ------

EVENT_STRATEGIST_CALL: str = "strategist_call"
EVENT_BUDGET_SKIP: str = "strategist_budget_skip"
EVENT_CAPTURE_SKIP: str = "strategist_capture_skip"
EVENT_CANDIDATE_REJECTED: str = "strategist_candidate_rejected"
EVENT_CALL_FAILED: str = "strategist_call_failed"
EVENT_PROMOTION: str = "promotion"
# §8.4 rollback-lane kinds (8h-H5; written by daemon.py, serve thread).
EVENT_ROLLBACK_TRIGGERED: str = "rollback_triggered"
EVENT_ROLLBACK_NO_PRIOR: str = "rollback_no_prior"
EVENT_MONITOR_SKIP: str = "monitor_skip_insufficient_data"
# RG5 (plan §5.4): the serve `_REGIME` phase + the strategist's verdict.
EVENT_REGIME_MEASURED: str = "regime_measured"
EVENT_REGIME_VERDICT: str = "regime_verdict"

# ---- §4.1 + VM2-V4 grammar mirrors (published constants; the
# authoritative enforcement is `ingress_ai::validate_ruleset` in the
# harness) --------------------------------------------------------------

MAX_RULESET_ROWS: int = 256
NAME_LEN_MAX: int = 64
EDGE_BPS_MAX: int = 10_000
HORIZON_MS_MIN: int = 10
HORIZON_MS_MAX: int = 86_400_000
LEVEL_MAX: float = 1.0
MAX_THESIS_CHARS: int = 4_000

# Risk caps — the $50k research tier (operator ruling 2026-08-29),
# mirrored from `ingress_ai::RULE_{ROW,SYM,TABLE}_MAX_RISK_1E6`. Only
# the per-ROW cap is enforceable here: the per-symbol and per-table
# sums need resolved symbol identities, so their Σ walk stays rule 7 in
# the Rust validator. The other two are published so the static prompt
# can state them and the model can size a whole table within them.
ROW_MAX_RISK_USD: float = 10_000.0
SYM_MAX_RISK_USD: float = 20_000.0
TABLE_MAX_RISK_USD: float = 100_000.0

FAMILIES: tuple[str, ...] = ("crypto", "politics", "sports", "macro", "other")
SIDES: tuple[str, ...] = ("bid", "ask", "both")

# ---- v2 grammar (VM2 V4; core_types::FeatId / CombineOp) --------------

DESCRIPTOR_LEN_MAX: int = 128  # ingress_ai::DESCRIPTOR_CAP
ROLL_WINDOW_MIN: int = 1
ROLL_WINDOW_MAX_MIN: int = 4_320  # 3 days
GROUP_MAX: int = 254  # 0xFF is GROUP_NONE
HOLD_S_MAX: int = 0xFFFF_FFFF  # u32

FEATURES: tuple[str, ...] = (
    "mid",
    "bid",
    "ask",
    "roll_mean",
    "roll_ema",
    "roll_min",
    "roll_max",
    "roll_std",
    "apr24",
    "apr72",
    "mark_px",
    "mark_iv",
    "depth_imb",
    "depth_spread_bps",
    "depth_notional",
    "clock_to_funding",
    "clock_utc_sod",
)
# Rule 3 window law: exactly these five REQUIRE a window; every other
# feature FORBIDS one.
ROLLING_FEATURES: frozenset[str] = frozenset(
    {"roll_mean", "roll_ema", "roll_min", "roll_max", "roll_std"}
)
# JSON tokens only. `lhs_only` is deliberately NOT a token: it is what
# the engine infers for a row carrying no `ref` (the combine law —
# required WITH `ref`, forbidden WITHOUT).
COMBINES: tuple[str, ...] = ("diff", "diff_bps", "ratio")
CMPS: tuple[str, ...] = ("ge", "le")

# ---- grammar v2.1 regime keys (RG3; core_types::regime text grammar) ----
# The STRUCTURAL mirror only: profile prefix, dimension names, value
# vocabulary per dimension (+ `*`, `!v`, `v1|v2`, the `unknown` mark on
# market dimensions). Duplicate `(profile, dim)`, empty sets and the
# stored-tail law are rule 11 in `ingress_ai::validate_ruleset`.
REGIME_OFFS: tuple[str, ...] = ("soft", "hard")
REGIME_PROFILES_PREFIX: tuple[str, ...] = ("fast", "slow")
REGIME_TERM_MAX: int = 16  # 2 profiles × (7 dims + rel) — more is a duplicate
REGIME_TERM_LEN_MAX: int = 64  # ingress_ai::REGIME_TERM_CAP
_REGIME_DIM_VALUES: dict[str, tuple[str, ...]] = {
    **claude_worker.frames.REGIME_VALUES,
    "rel": ("lagging", "inline", "leading"),
}

# ---- row key sets (mirror of ingress_ai V1_ONLY / V2_ONLY / V2_REQUIRED)

_ROW_KEYS_SHARED: frozenset[str] = frozenset(
    {"name", "family", "side", "horizon_ms", "max_risk_usd"}
)
_ROW_KEYS_V1_ONLY: frozenset[str] = frozenset({"trigger", "sym", "edge_bps"})
_ROW_KEYS_V2_ONLY: frozenset[str] = frozenset(
    {
        "instrument",
        "ref",
        "feature",
        "ref_feature",
        "combine",
        "window_min",
        "ref_window_min",
        "cmp",
        "abs",
        "enter",
        "exit",
        "confirm_feature",
        "confirm_window_min",
        "confirm",
        "confirm_cmp",
        "confirm_abs",
        "confirm_pair",
        "group",
        "min_hold_s",
        "max_hold_s",
        "regimes",
        "regime_off",
        "rel",
    }
)
# v1 requires its ENTIRE key set (8g shape, byte-exact); v2 requires a
# small core and treats the rest as optional.
_ROW_KEYS_V1: frozenset[str] = _ROW_KEYS_SHARED | _ROW_KEYS_V1_ONLY
_ROW_KEYS_V2: frozenset[str] = _ROW_KEYS_SHARED | _ROW_KEYS_V2_ONLY
_ROW_REQUIRED_V2: frozenset[str] = frozenset(
    {"name", "instrument", "feature", "enter", "horizon_ms", "max_risk_usd"}
)
# Back-compat alias: the v1 arm's exact key set.
_ROW_KEYS: frozenset[str] = _ROW_KEYS_V1

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
# Deterministic v2 emission order — `artifact_bytes` hashes the exact
# bytes, so key order is part of the artifact identity.
_ROW_KEY_ORDER_V2: tuple[str, ...] = (
    "name",
    "family",
    "instrument",
    "ref",
    "feature",
    "ref_feature",
    "combine",
    "window_min",
    "ref_window_min",
    "side",
    "cmp",
    "abs",
    "enter",
    "exit",
    "confirm_feature",
    "confirm_window_min",
    "confirm",
    "confirm_cmp",
    "confirm_abs",
    "confirm_pair",
    "group",
    "min_hold_s",
    "max_hold_s",
    "regimes",
    "regime_off",
    "rel",
    "horizon_ms",
    "max_risk_usd",
)
_SYMBOL_ID_MAX: int = 0xFFFF_FFFE  # < SYMBOL_ID_NONE


def regime_term_ok(term: str) -> bool:
    """Structural check of one `regimes` term — `[fast:|slow:]<dim>:<values>`
    with `<values>` = `*` | `!<value>` | `<value>[|<value>…]` over the
    dimension's vocabulary (`unknown` allowed on market dimensions, the
    REL pseudo-dimension `rel` has no mark). Mirrors
    `core_types::regime::parse_label_term`'s vocabulary; the deep laws
    (duplicates, empty sets) stay in the Rust validator."""
    if not term or len(term) > REGIME_TERM_LEN_MAX or not term.isascii():
        return False
    parts = term.split(":")
    if len(parts) == 3:
        if parts[0] not in REGIME_PROFILES_PREFIX:
            return False
        dim, values = parts[1], parts[2]
    elif len(parts) == 2:
        dim, values = parts
    else:
        return False
    vocab = _REGIME_DIM_VALUES.get(dim)
    if vocab is None or not values:
        return False
    if values == "*":
        return True
    if values.startswith("!"):
        return values[1:] in vocab
    for v in values.split("|"):
        if v in vocab:
            continue
        if v == "unknown" and dim not in ("source", "rel"):
            continue
        return False
    return True


def regime_rel_ok(value: str) -> bool:
    """Structural check of the `rel` sugar — `[fast:|slow:]<values>` over
    the REL vocabulary (the validator rewrites it to a `rel:` term)."""
    if not value or not value.isascii():
        return False
    rest = value
    for prefix in REGIME_PROFILES_PREFIX:
        if value.startswith(prefix + ":"):
            rest = value[len(prefix) + 1 :]
            break
    if not rest or rest.startswith("rel:"):
        return False
    return regime_term_ok(f"rel:{rest}")

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
    "You are the offline strategist for a multi-venue trading engine"
    " (Polymarket CLOB + Binance + OKX + Deribit + Hyperliquid + Bybit)."
    " You author ruleset artifacts only; you never touch execution. Your"
    " candidate is validated by a strict byte-scanner, then backtested by"
    " the REAL engine VM replaying REAL capture; hard gates decide"
    " promotion. Paper trading only.\n"
    "\n"
    "A ruleset is {\"rows\": [ROW, ...]} with 1..256 rows. Unknown keys,"
    " duplicate keys and mixed row shapes are rejected. Each row is one"
    " statement of the same form:\n"
    "  signal = combine( feature(instrument, window_min),"
    " ref_feature(ref, ref_window_min) )\n"
    "\n"
    "SIGNAL DOMAIN (absolute law): every feature and combine output is an"
    " integer-valued quantity in x1e9 of its NATURAL unit — prices x1e9,"
    " APR/IV/imbalance fractions x1e9, basis points x1e9, notional USD"
    " x1e9, clock seconds x1e9. Thresholds live in that same domain, so"
    " 3 bps is 3.0 and a 20-point APR spread is 20.0. Write them as plain"
    " JSON numbers with at most 9 decimals.\n"
    "\n"
    "ROW KEYS (v2 grammar; * = required):\n"
    '  *"name":         ASCII string, 1..64 chars, unique per table\n'
    '   "family":       "crypto"|"politics"|"sports"|"macro"|"other"'
    ' (default "other")\n'
    '  *"instrument":   DESCRIPTOR STRING of the action leg — the leg that'
    " emits orders. Use ONLY descriptors listed in the digest's"
    " INSTRUMENTS section. Never a bare SymbolId: ordinals reshuffle"
    " every boot.\n"
    '   "ref":          descriptor string of the reference leg; must'
    ' differ from "instrument"\n'
    '  *"feature":      one of: mid, bid, ask, roll_mean, roll_ema,'
    " roll_min, roll_max, roll_std, apr24, apr72, mark_px, mark_iv,"
    " depth_imb, depth_spread_bps, depth_notional, clock_to_funding,"
    " clock_utc_sod\n"
    '   "ref_feature":  same vocabulary, for the ref leg (defaults to'
    ' "feature")\n'
    '   "combine":      "diff" (natural units — APR and IV spreads) |'
    ' "diff_bps" (relative price deviation) | "ratio".\n'
    '                   REQUIRED when "ref" is present, FORBIDDEN when it'
    " is absent (a ref-less row is single-leg).\n"
    '   "window_min" / "ref_window_min" / "confirm_window_min": integer'
    " 1..4320 minutes. REQUIRED for roll_mean/roll_ema/roll_min/roll_max/"
    "roll_std and FORBIDDEN for every other feature.\n"
    '   "cmp":          "ge" (default) | "le" — how signal is compared to'
    ' "enter"\n'
    '   "abs":          boolean (default false) — compare |signal|\n'
    '  *"enter":        entry threshold, signal domain\n'
    '   "exit":         exit threshold. PRESENT means this is a POSITION'
    " row; absent means a stateless refire row.\n"
    '   "confirm_feature" / "confirm" / "confirm_cmp" / "confirm_abs" /'
    ' "confirm_pair": an optional second condition that gates ENTRY only.'
    ' "confirm" requires "confirm_feature" and vice versa;'
    ' "confirm_pair": true computes the same combine over the confirm'
    ' feature on BOTH legs and requires "ref".\n'
    '   "side":         "bid"|"ask"|"both" — a direction FILTER, not a'
    " command\n"
    '   "group":        integer 0..254. Rows sharing a group hold AT MOST'
    " ONE position between them; the first qualifying row in table order"
    ' wins. Requires "exit".\n'
    '   "min_hold_s" / "max_hold_s": integer seconds. min_hold_s gates'
    " exits; max_hold_s is an unconditional age-out; max_hold_s must"
    ' exceed min_hold_s when both are set. Both require "exit".\n'
    '  *"horizon_ms":   integer 10..86400000 — refire cooldown, and'
    " re-entry cooldown after a position exits\n"
    '  *"max_risk_usd": number > 0, <= 10000.0 — notional cap PER LEG\n'
    '   "regimes":      REQUIRED on every row you propose — a proposal'
    " with ONE unlabelled row is REFUSED as malformed (RG8), and the gate"
    " refuses a table whose labels earn nothing against `--regime off`"
    " (net_on >= net_off is a gate): a non-empty list of label"
    ' terms "[fast:|slow:]<dim>:<values>" — the REGION of the market'
    " regime space in which this row may ENTER. Dimensions: trend"
    " (bear|neutral|bull), shape (chop|mixed|trend), vol"
    " (low|normal|high), fund (neg|pos), level (low|normal|high),"
    " stretch (ext_down|neutral|ext_up), source (measured|declared),"
    " rel (lagging|inline|leading — the row's instrument vs BTC)."
    ' <values> is "*" (any), "!v" (all but v) or "v1|v2" (exactly'
    ' these); the token "unknown" adds "trade even if this dimension'
    ' cannot be judged". Profiles: "fast:" = 1 h horizon (the default'
    ' when unprefixed), "slow:" = 4 h; a row is open only when BOTH'
    " profiles' words fit. An omitted dimension means any value.\n"
    '   "regime_off":   "soft" (default) | "hard" — what happens to an'
    " OPEN position when the row's region no longer holds: soft blocks"
    " new entries and lets the row's own exit law drain; hard blocks"
    " entries AND flattens the position at once.\n"
    '   "rel":          sugar for one more rel term, "[fast:|slow:]<values>"'
    ' over lagging|inline|leading (e.g. "lagging|inline").\n'
    "\n"
    "REGIME LAW: the regime is a GATE, never a signal — it decides whether"
    " a row may enter; it never sizes, prices or times an order, and exits"
    " are NEVER gated. A labelled row fails CLOSED while the regime is"
    " unknown (engine warm-up). Regime changes never flip the table: every"
    " row carries its own region, so author VARIANTS — the same signal"
    " with regime-specific exit / horizon / max_risk_usd / regime_off,"
    " each variant on a DISJOINT region (e.g. trend:bull vs trend:bear vs"
    " trend:neutral). Disjoint variants of one signal are legal; two rows"
    " on the same signal whose regions OVERLAP are rejected as duplicates."
    " Every backtest reports per-regime P&L and the on/off delta"
    " (--regime off replays every row unlabelled): a label must EARN its"
    " keep — prefer a region where the digest's per-regime P&L shows the"
    " edge, and say why in the thesis.\n"
    "\n"
    "DIRECTION LAW: a positive signal means ASK the instrument (sell the"
    " rich leg / short the higher-funding venue); negative means BID. A"
    " position row with a ref hedges both legs at equal notional. The"
    " single exit law is: signal x entry_sign <= exit — it covers both"
    " decay and sign flip.\n"
    "\n"
    "CHANNEL LAW: a feature only exists where its channel does."
    " apr24/apr72/clock_to_funding need a funding channel;"
    " mark_px/mark_iv need an options channel; depth_imb/"
    "depth_spread_bps/depth_notional need an L2 depth subscription. The"
    " digest's INSTRUMENTS section lists each descriptor's channels;"
    " naming a feature the instrument does not carry is refused. An empty"
    " feature window is ABSENT, never zero — the row simply holds.\n"
    "\n"
    "VALIDATOR RULES you must respect (enforced by the engine, not here):\n"
    "- Every descriptor must resolve against the LIVE boot universe.\n"
    "- Risk caps are TIGHTEN-ONLY: <= $10,000 per leg, sum per symbol"
    " <= $20,000, whole-table sum <= $100,000. Two-leg position rows"
    " charge their cap to BOTH legs, and the sum is group-blind — a wide"
    " table forces smaller legs. Never propose more.\n"
    "- Names must be unique; exact-duplicate rows are rejected (identity"
    " is instrument/ref/features/windows/combine/comparison/group/enter —"
    " horizon, holds, risk and name are NOT identity; two rows on one"
    " identity are duplicates only when their regime regions overlap).\n"
    '- Regime terms must parse; "regime_off" and "rel" need "regimes";'
    " a dimension named twice on one profile is rejected.\n"
    "- At most 8 distinct rolling windows per symbol, 256 across the"
    " table.\n"
    "\n"
    "BACKTEST GATES the candidate must pass to be promoted (strict-cross"
    " maker fills, fees and latency charged, 70/30 IS/OOS split, only OOS"
    " scored): OOS net P&L > 0; >= 50 OOS legs, and >= 10 round trips if"
    " the table has any position row; >= 1 OOS trading day; OOS max"
    " drawdown <= $7,500; observed notional bounds within the caps above"
    " (a breach anywhere in the window disqualifies). Gate failure is"
    " final for that candidate — there is no override.\n"
    "\n"
    "WARMUP: the backtest's warmup is TABLE-GLOBAL and equals the longest"
    " window any row references (apr24 counts as 24 h, apr72 as 72 h). A"
    " single apr72 row zeroes the whole backtest on a capture root"
    " younger than 72 h. Check the digest's capture span before reaching"
    " for long windows.\n"
    "\n"
    "OUTPUT CONTRACT (strict): respond with EXACTLY one JSON object and"
    " nothing else — no prose, no code fences:\n"
    '{"thesis": "<why these rows, <= 4000 chars>", "rows": [ROW, ...],'
    ' "regime": VERDICT}\n'
    "Malformed output is archived and discarded; the cycle ends.\n"
    "\n"
    'REGIME VERDICT: "regime" is OPTIONAL — your ruling on the CURRENT'
    " mode, which the worker DECLARES to the engine (a declaration"
    " overrides the measurement on the dimensions it names, for a"
    " bounded TTL, then the measurement resumes). VERDICT ="
    ' {"fast": "<decl>", "slow": "<decl>"} with either profile omitted;'
    ' <decl> is "measured" (confirm the worker-measured word in the'
    " digest's REGIME section) or a comma list of ONE value per named"
    ' dimension, e.g. "trend:bull,shape:trend" (dimensions: trend'
    " bull|bear|neutral, shape trend|chop|mixed, vol low|normal|high,"
    " fund pos|neg, level low|normal|high, stretch ext_up|ext_down|"
    "neutral; unknown marks a dimension unjudgeable). Rule ONLY when the"
    " digest's evidence (raw values, timeline, engine words) contradicts"
    ' the measurement or the measurement is unknown; otherwise "measured"'
    " or omit. Never invent a regime to fit the rows — the rows fit the"
    " regime.\n"
    "\n"
    "WORKED EXAMPLE A — stateless refire, cross-venue price deviation,"
    " labelled for calm markets on both horizons:\n"
    '{"name": "xv-btc-okx-bn", "family": "crypto", "instrument":'
    ' "okx:BTC-USDT", "ref": "binance:btcusdt", "feature": "mid",'
    ' "combine": "diff_bps", "abs": true, "enter": 3.0, "regimes":'
    ' ["vol:!high", "slow:shape:chop|mixed"], "horizon_ms":'
    " 60000, \"max_risk_usd\": 3000.0}\n"
    "\n"
    "WORKED EXAMPLE B — position pair on a funding spread, one per coin,"
    " with a 72 h confirm, hard-off when funding turns negative:\n"
    '{"name": "carry-btc", "family": "crypto", "instrument":'
    ' "binance-usdm:btcusdt", "ref": "bybit-linear:BTCUSDT", "feature":'
    ' "apr24", "combine": "diff", "abs": true, "enter": 20.0, "exit":'
    ' 0.1, "confirm_feature": "apr72", "confirm": 10.0, "confirm_abs":'
    ' true, "confirm_pair": true, "group": 0, "min_hold_s": 28800,'
    ' "max_hold_s": 864000, "regimes": ["fund:pos", "slow:level:normal|high"],'
    ' "regime_off": "hard", "horizon_ms": 3600000, "max_risk_usd":'
    " 1400.0}\n"
    "\n"
    "WORKED EXAMPLE C — two VARIANTS of one signal on disjoint regions"
    " (a bull-trend momentum row exits faster and sizes smaller in a"
    " bear trend; the neutral trend is left unlabelled on purpose = no row):\n"
    '{"name": "mom-eth-bull", "instrument": "okx:ETH-USDT-SWAP",'
    ' "feature": "mid", "ref": "binance-usdm:ethusdt", "ref_feature":'
    ' "roll_mean", "ref_window_min": 30, "combine": "diff_bps", "enter":'
    ' 25.0, "exit": 5.0, "side": "bid", "regimes": ["trend:bull",'
    ' "shape:trend"], "rel": "leading|inline", "horizon_ms": 300000,'
    ' "max_risk_usd": 2000.0}\n'
    '{"name": "mom-eth-bear", "instrument": "okx:ETH-USDT-SWAP",'
    ' "feature": "mid", "ref": "binance-usdm:ethusdt", "ref_feature":'
    ' "roll_mean", "ref_window_min": 30, "combine": "diff_bps", "enter":'
    ' 25.0, "exit": 12.0, "side": "bid", "regimes": ["trend:bear",'
    ' "shape:trend"], "regime_off": "hard", "horizon_ms": 300000,'
    ' "max_risk_usd": 800.0}\n'
    "\n"
    "LEGACY v1 SUGAR (accepted, but strictly weaker — prefer v2): a row of"
    ' EXACTLY {"name", "family", "trigger", "sym", "side", "edge_bps",'
    ' "horizon_ms", "max_risk_usd"} where "trigger" is {"type":'
    ' "cross_deviation", "ref": <SymbolId>} or {"type": "level_breach",'
    ' "level": <0..1>}. It uses bare SymbolIds, expresses only price'
    " deviation and level breach, and cannot express funding, options,"
    " depth, positions, groups, holds or confirms. Never mix v1 and v2"
    " keys in one row.\n"
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
    instruments: str | None = None,
    performance: str | None = None,
    positions: str | None = None,
    pnl: str | None = None,
    regime: str | None = None,
    cap: int = STRATEGIST_INPUT_CAP,
) -> str:
    """The DYNAMIC user-block digest (§7.2): market map, capture-derived
    universe, feature files of the named run (replay-derived + §6.1
    REST, same directory), news NDJSON tail, and — from H5 — the ACTIVE
    ruleset's walk-forward ``performance`` text. Deterministic for
    identical file contents (the SQLite dedupe key rides on this).

    VM2 (2026-08-30): ``instruments`` carries the §9.4 DESCRIPTOR
    vocabulary and each descriptor's channels — the v2 grammar names
    instruments by string, never by SymbolId, so without this section a
    keyed ``serve`` cannot author a v2 row at all. Produced by
    :func:`instruments_digest_text`; ``None`` omits the section (the
    pre-VM2 callers' dedupe keys are untouched).

    Ruling #7(a) (mvp-plan §8 item 7, 2026-08-23): ``positions`` and
    ``pnl`` carry the INVENTORY sections — the paper netting view and
    the latest per-strategy shadow-P&L. Callers produce them via
    :func:`positions_digest_text` / :func:`pnl_digest_text`, which
    render absent/empty sources as honest empty text (never errors);
    ``None`` omits the section entirely (pre-#7a callers unchanged —
    the dedupe key for their inputs is untouched).

    RG5 (plan §5.4): ``regime`` carries the REGIME section — the
    worker-measured words + raw values, the declaration in force, the
    engine's effective words, the 24 h timeline and the per-regime P&L
    of the latest nightly report (:func:`claude_worker.regime.regime_digest_text`
    / the serve phase's :class:`claude_worker.regime.ServeRegimeOutcome`
    digest); ``None`` omits it."""
    parts: list[str] = []
    used = 0
    map_lines = "\n".join(f"  {name} -> sym {markets[name]}" for name in sorted(markets))
    used = _append_capped(parts, used, f"MARKET MAP (name -> SymbolId):\n{map_lines or '  (empty)'}\n", cap)
    if universe is not None:
        syms = ", ".join(str(s) for s in sorted(set(universe)))
        used = _append_capped(parts, used, f"\nOBSERVED CAPTURE UNIVERSE (legal sym/ref values): {syms or '(none)'}\n", cap)
    if instruments is not None:
        used = _append_capped(
            parts,
            used,
            "\nINSTRUMENTS (legal v2 descriptor strings + the channels each"
            f" carries):\n{instruments}\n",
            cap,
        )
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
    if positions is not None:
        used = _append_capped(parts, used, f"\nPOSITIONS (paper netting, current run):\n{positions}\n", cap)
    if pnl is not None:
        used = _append_capped(
            parts, used, f"\nPER-STRATEGY SHADOW P&L (latest nightly report):\n{pnl}\n", cap
        )
    if regime is not None:
        used = _append_capped(
            parts,
            used,
            "\nREGIME (worker-measured words + raw values, declaration in force,"
            f" engine words, 24 h timeline, per-regime P&L):\n{regime}\n",
            cap,
        )
    news = _news_tail(features_dir / "news", max(0, cap - used))
    if news:
        used = _append_capped(parts, used, f"\nNEWS (NDJSON, oldest->newest):\n{news}\n", cap)
    return "".join(parts)


# ---- ruling #7(a) inventory sections (2026-08-23; landed with the
# 2026-08-28 remediation plan) --------------------------------------------

POSITIONS_EMPTY_TEXT: str = "  (no positions view available)"
PNL_EMPTY_TEXT: str = "  (no shadow-P&L report on disk yet)"

# VM2: the v2 grammar's instrument vocabulary.
INSTRUMENTS_EMPTY_TEXT: str = "  (no instrument manifest in the latest run)"


def instruments_digest_text(replay_dir: pathlib.Path) -> str:
    """The INSTRUMENTS section: every descriptor the LATEST run
    allocated, with the channels it carries, one per line.

    Source of truth is that run's ``instrument-manifest.tsv`` (the same
    file the engine's ``DescriptorTable`` is built from), read through
    the shared :func:`claude_worker.iv_digest.read_manifest`; channels
    come from :func:`claude_worker.channel_map.caps_of_descriptor`, the
    pinned mirror of ``ingress_ai::caps_of_descriptor``. A missing run
    or missing manifest renders as honest empty text — never an error,
    and never a guess: proposing against a stale vocabulary would only
    earn a ``Descriptor`` refuse at stage time."""
    run_dir = claude_worker.features.latest_run_dir(replay_dir)
    if run_dir is None:
        return INSTRUMENTS_EMPTY_TEXT
    manifest = claude_worker.iv_digest.read_manifest(run_dir)
    if manifest is None or not manifest[0]:
        return INSTRUMENTS_EMPTY_TEXT
    lines: list[str] = []
    for descriptor in sorted(set(manifest[0].values())):
        caps = claude_worker.channel_map.caps_of_descriptor(descriptor)
        lines.append(f"  {descriptor} [{claude_worker.channel_map.channel_names(caps)}]")
    return "\n".join(lines)


def gather_positions_payload(
    replay_dir: pathlib.Path,
    hip4_pairs: collections.abc.Sequence[tuple[int, int]] = (),
) -> dict[str, object] | None:
    """Assemble the §6 ``positions`` netting view for the LATEST run
    dir — the same read-only composition the ``positions`` verb makes
    (read_fills -> reconstruct -> tick marks -> views -> HIP-4 pairs ->
    total), deliberately mirrored here so the digest never shells out
    to the frozen CLI. Returns ``None`` when there is no run dir or the
    capture is unreadable (the caller renders that honestly)."""
    try:
        run_dir = claude_worker.features.latest_run_dir(replay_dir)
        if run_dir is None:
            return None
        fills, fills_torn = claude_worker.features.read_fills(run_dir)
        reconstructed = claude_worker.features.reconstruct_positions(fills)
        marks: dict[int, int] = {}
        for path in sorted(run_dir.glob("*-ticks.pmlr")):
            with claude_worker.pmlr.Reader(path) as reader:
                claude_worker.features.collect_marks(reader, into=marks)
        views = claude_worker.features.position_views(reconstructed, marks)
        pair_views = claude_worker.features.hip4_pair_views(views, list(hip4_pairs))
        total = claude_worker.features.total_exposure(views, pair_views)
    except (OSError, ValueError):
        return None
    scale = 1_000_000.0
    to_usd = claude_worker.features.to_usd
    positions_out: list[dict[str, object]] = []
    for sym in sorted(views):
        view = views[sym]
        positions_out.append(
            {
                "sym": view.sym,
                "net_qty": view.net_qty / scale,
                "avg_px": view.avg_px / scale,
                "mark_px": view.mark_px / scale,
                "realized_usd": to_usd(view.realized),
                "unrealized_usd": to_usd(view.unrealized),
                "exposure_usd": to_usd(view.exposure),
            }
        )
    pairs_out: list[dict[str, object]] = []
    for pv in pair_views:
        pairs_out.append(
            {
                "yes_sym": pv.yes_sym,
                "no_sym": pv.no_sym,
                "net_qty": pv.net_qty / scale,
                "exposure_usd": to_usd(pv.exposure),
            }
        )
    return {
        "run_dir": str(run_dir),
        "fills_torn": fills_torn,
        "positions": positions_out,
        "hip4_pairs": pairs_out,
        "total_exposure_usd": to_usd(total),
    }


def positions_digest_text(payload: dict[str, object] | None) -> str:
    """Render the #7(a) POSITIONS section body. Absent/empty sources
    render honestly (never an error): ``None`` payload means no run /
    unreadable capture; a payload with no rows means flat."""
    if payload is None:
        return POSITIONS_EMPTY_TEXT
    lines: list[str] = []
    positions = payload.get("positions")
    if isinstance(positions, list):
        for pos in positions:
            if not isinstance(pos, dict):
                continue
            lines.append(
                "  sym {sym}  net {net_qty:.6f}  avg {avg_px:.6f}  mark {mark_px:.6f}"
                "  realized ${realized_usd:.2f}  unrealized ${unrealized_usd:.2f}"
                "  exposure ${exposure_usd:.2f}".format(**pos)
            )
    pairs = payload.get("hip4_pairs")
    if isinstance(pairs, list):
        for pv in pairs:
            if not isinstance(pv, dict):
                continue
            lines.append(
                "  pair yes {yes_sym} / no {no_sym}  net {net_qty:.6f}"
                "  exposure ${exposure_usd:.2f}".format(**pv)
            )
    if not lines:
        lines.append("  (flat — no open positions)")
    total = payload.get("total_exposure_usd")
    if isinstance(total, (int, float)):
        lines.append(f"  total exposure ${total:.2f}")
    if payload.get("fills_torn"):
        lines.append("  (fills file torn tail — last partial record ignored)")
    return "\n".join(lines)


def pnl_digest_text(reports_dir: pathlib.Path) -> str:
    """Render the #7(a) PER-STRATEGY SHADOW P&L section body from the
    newest ``pnl-<day>.json`` (the M4.3 nightly pair). Absent dir /
    report / malformed JSON render honestly, never raise."""
    path = claude_worker.pnl_report.latest_report(reports_dir)
    if path is None:
        return PNL_EMPTY_TEXT
    try:
        obj = json.loads(path.read_text())
    except (OSError, ValueError):
        return f"  (report unreadable: {path.name})"
    if not isinstance(obj, dict):
        return f"  (report malformed: {path.name})"
    lines = [f"  report {path.name}  runs={obj.get('runs')}"]
    paper = obj.get("paper")
    if isinstance(paper, dict):
        lines.append(
            "  paper: "
            + json.dumps(paper, sort_keys=True, separators=(",", ":"))
        )
    strategies = obj.get("strategies")
    if isinstance(strategies, list) and strategies:
        for row in strategies:
            if not isinstance(row, dict):
                continue
            lines.append(
                "  strategy: "
                + json.dumps(row, sort_keys=True, separators=(",", ":"))
            )
    else:
        lines.append("  (no per-strategy rows in the report)")
    return "\n".join(lines)


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
    """A parsed, structurally-valid strategist proposal. ``regime`` is the
    RG5 verdict (plan §5.4) — ``{"fast": "<decl>|measured", …}`` the
    worker turns into a declaration — or ``None`` when omitted."""

    thesis: str
    rows: list[dict[str, object]]  # canonical key order, validated types
    regime: dict[str, str] | None = None


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


def _bool_field(value: object) -> bool | None:
    """Strict boolean — ``1``/``0`` are integers, not booleans (the
    validator's ``scan_bool_field`` accepts only the JSON literals)."""
    if not isinstance(value, bool):
        return None
    return value


def _signal_field(value: object) -> float | None:
    """A threshold in the x1e9 signal domain: bool-rejecting, finite,
    any sign. The 9-decimal lexical law is the validator's
    ``scan_signal_field`` — a JSON literal has already lost its text
    form by the time it reaches us, so that check stays harness-side
    (the §3.5 no-second-deep-parser doctrine)."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    out = float(value)
    if not math.isfinite(out):
        return None
    return out


def _descriptor_field(value: object) -> str | None:
    """A §9.4 descriptor STRING: 1..128 printable-ASCII chars — the
    validator's ``scan_descriptor`` range 0x20..0x7E, which is exactly
    ASCII ∩ printable (DEL and every control byte fail both).

    Resolution against the live boot universe is commit-time and
    engine-side (a ``Descriptor`` refuse) — we only check the shape,
    because the legal set changes with every boot."""
    if not isinstance(value, str):
        return None
    if not 1 <= len(value) <= DESCRIPTOR_LEN_MAX:
        return None
    if not value.isascii() or not value.isprintable():
        return None
    return value


def _feature_field(value: object) -> str | None:
    """One of the 17 ``FeatId`` tokens."""
    if not isinstance(value, str) or value not in FEATURES:
        return None
    return value


def _window_ok(
    obj: dict[str, object],
    out: dict[str, object],
    key: str,
    feature: str | None,
) -> bool:
    """Rule 3 window law for one leg: ``key`` is present iff
    ``feature`` rolls, and its value is 1..4320. ``feature is None``
    means the leg itself is absent, so the window must be too."""
    present = key in obj
    rolls = feature in ROLLING_FEATURES if feature is not None else False
    if present != rolls:
        return False
    if present:
        window = _int_field(obj.get(key), ROLL_WINDOW_MIN, ROLL_WINDOW_MAX_MIN)
        if window is None:
            return False
        out[key] = window
    return True


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


def parse_row(value: object) -> dict[str, object] | None:
    """RG4 (plan §5.2): the structural row parser as a public seam for
    the strategy library — one artifact row in, the canonical-key-order
    row out (or ``None`` when malformed). Same arm dispatch and laws as
    [`parse_proposal`]'s per-row step; nothing cross-row."""
    return _parse_row(value)


def _parse_row(value: object) -> dict[str, object] | None:
    """Arm dispatch (validator rule 2): a row is v1 XOR v2. A row
    carrying keys from BOTH shapes is malformed, exactly as
    ``ingress_ai::parse_and_admit_row`` rejects it; a row carrying
    neither falls to the v1 arm, whose exact-key-set check refuses it."""
    if not isinstance(value, dict):
        return None
    obj = typing.cast(dict[str, object], value)
    keys = set(obj)
    is_v1 = bool(keys & _ROW_KEYS_V1_ONLY)
    is_v2 = bool(keys & _ROW_KEYS_V2_ONLY)
    if is_v1 and is_v2:
        return None
    return _parse_row_v2(obj) if is_v2 else _parse_row_v1(obj)


def _parse_row_v2(  # noqa: PLR0911, PLR0912, PLR0915
    obj: dict[str, object],
) -> dict[str, object] | None:
    """The VM2-V4 arm. STRUCTURAL only, per the §3.5 doctrine: key sets,
    types, enums, published bounds, and the shape laws that need no
    cross-row or universe state (combine law, rule 3 windows, confirm
    shape, rule 9 positions). Descriptor resolution (rule 6 / D-6),
    rule 5 name uniqueness, the rule 7 cap Σ walk, rule 8 identity and
    the rule 10 channel/bind-budget families stay in
    ``ingress_ai::validate_ruleset`` — no second deep parser.

    The return/branch-count lints are waived deliberately, for the same
    reason ``parse_and_admit_row`` carries ``#[allow(clippy::
    too_many_lines)]``: one row is one LINEAR rule sequence, and
    splitting it into helpers hides the order the rules must run in."""
    keys = set(obj)
    if not keys <= _ROW_KEYS_V2 or not _ROW_REQUIRED_V2 <= keys:
        return None

    out: dict[str, object] = {}

    name = obj.get("name")
    if not isinstance(name, str) or not 1 <= len(name) <= NAME_LEN_MAX or not name.isascii():
        return None
    out["name"] = name

    if "family" in keys:  # optional in v2 — the engine defaults to "other"
        family = obj.get("family")
        if family not in FAMILIES:
            return None
        out["family"] = family

    instrument = _descriptor_field(obj.get("instrument"))
    if instrument is None:
        return None
    out["instrument"] = instrument

    has_ref = "ref" in keys
    if has_ref:
        ref = _descriptor_field(obj.get("ref"))
        # Rule-6 mirror: identical descriptors resolve to one symbol.
        if ref is None or ref == instrument:
            return None
        out["ref"] = ref

    feature = _feature_field(obj.get("feature"))
    if feature is None:
        return None
    out["feature"] = feature

    # Combine law: required WITH `ref`, forbidden WITHOUT. `lhs_only` is
    # the engine's inference for a ref-less row, never a token.
    if has_ref != ("combine" in keys):
        return None
    if has_ref:
        combine = obj.get("combine")
        if combine not in COMBINES:
            return None
        out["combine"] = combine

    # ref-only keys are illegal on a single-leg row.
    if not has_ref and {"ref_feature", "ref_window_min"} & keys:
        return None
    ref_feature: str | None = None
    if "ref_feature" in keys:
        ref_feature = _feature_field(obj.get("ref_feature"))
        if ref_feature is None:
            return None
        out["ref_feature"] = ref_feature

    # Rule 3, per leg. The ref leg's effective feature falls back to
    # `feature` when `ref_feature` is absent (the validator's
    # `feat_b.unwrap_or(feat_a)`).
    if not _window_ok(obj, out, "window_min", feature):
        return None
    ref_effective = (ref_feature or feature) if has_ref else None
    if not _window_ok(obj, out, "ref_window_min", ref_effective):
        return None

    if "side" in keys:
        side = obj.get("side")
        if side not in SIDES:
            return None
        out["side"] = side

    if "cmp" in keys:
        cmp_token = obj.get("cmp")
        if cmp_token not in CMPS:
            return None
        out["cmp"] = cmp_token
    if "abs" in keys:
        abs_flag = _bool_field(obj.get("abs"))
        if abs_flag is None:
            return None
        out["abs"] = abs_flag

    enter = _signal_field(obj.get("enter"))
    if enter is None:
        return None
    out["enter"] = enter

    position = "exit" in keys
    if position:
        exit_level = _signal_field(obj.get("exit"))
        if exit_level is None:
            return None
        out["exit"] = exit_level

    # Confirm shape: the threshold and the feature imply each other, and
    # every other confirm key needs the feature.
    confirm_keys = {
        "confirm",
        "confirm_cmp",
        "confirm_abs",
        "confirm_pair",
        "confirm_window_min",
    }
    has_confirm_feature = "confirm_feature" in keys
    if bool(confirm_keys & keys) != has_confirm_feature:
        return None
    if has_confirm_feature and "confirm" not in keys:
        return None
    confirm_feature: str | None = None
    if has_confirm_feature:
        confirm_feature = _feature_field(obj.get("confirm_feature"))
        if confirm_feature is None:
            return None
        out["confirm_feature"] = confirm_feature
    if not _window_ok(obj, out, "confirm_window_min", confirm_feature):
        return None
    if has_confirm_feature:
        confirm = _signal_field(obj.get("confirm"))
        if confirm is None:
            return None
        out["confirm"] = confirm
        if "confirm_cmp" in keys:
            confirm_cmp = obj.get("confirm_cmp")
            if confirm_cmp not in CMPS:
                return None
            out["confirm_cmp"] = confirm_cmp
        if "confirm_abs" in keys:
            confirm_abs = _bool_field(obj.get("confirm_abs"))
            if confirm_abs is None:
                return None
            out["confirm_abs"] = confirm_abs
        if "confirm_pair" in keys:
            confirm_pair = _bool_field(obj.get("confirm_pair"))
            if confirm_pair is None:
                return None
            # The paired combine needs a second leg to compute over.
            if confirm_pair and not has_ref:
                return None
            out["confirm_pair"] = confirm_pair

    # Rule 9: groups and holds are position-only; a max age-out must
    # outlast the min hold.
    hold_keys = {"group", "min_hold_s", "max_hold_s"}
    if not position and hold_keys & keys:
        return None
    if "group" in keys:
        group = _int_field(obj.get("group"), 0, GROUP_MAX)
        if group is None:
            return None
        out["group"] = group
    min_hold = 0
    max_hold = 0
    if "min_hold_s" in keys:
        parsed = _int_field(obj.get("min_hold_s"), 0, HOLD_S_MAX)
        if parsed is None:
            return None
        min_hold = parsed
        out["min_hold_s"] = min_hold
    if "max_hold_s" in keys:
        parsed = _int_field(obj.get("max_hold_s"), 0, HOLD_S_MAX)
        if parsed is None:
            return None
        max_hold = parsed
        out["max_hold_s"] = max_hold
    if min_hold > 0 and max_hold > 0 and max_hold <= min_hold:
        return None

    # RG3 grammar v2.1: the regime keys (structural; rule 11 — duplicate
    # dimensions, empty sets, the stored-tail law — is the validator's).
    has_regimes = "regimes" in keys
    if not has_regimes and {"regime_off", "rel"} & keys:
        return None
    if has_regimes:
        regimes = obj.get("regimes")
        if not isinstance(regimes, list) or not 1 <= len(regimes) <= REGIME_TERM_MAX:
            return None
        terms: list[str] = []
        for term in typing.cast(list[object], regimes):
            if not isinstance(term, str) or not regime_term_ok(term):
                return None
            terms.append(term)
        out["regimes"] = terms
        if "regime_off" in keys:
            off = obj.get("regime_off")
            if off not in REGIME_OFFS:
                return None
            out["regime_off"] = off
        if "rel" in keys:
            rel = obj.get("rel")
            if not isinstance(rel, str) or not regime_rel_ok(rel):
                return None
            out["rel"] = rel

    horizon_ms = _int_field(obj.get("horizon_ms"), HORIZON_MS_MIN, HORIZON_MS_MAX)
    if horizon_ms is None:
        return None
    out["horizon_ms"] = horizon_ms

    max_risk = _number_field(obj.get("max_risk_usd"), 0.0, ROW_MAX_RISK_USD)
    if max_risk is None or max_risk <= 0.0:
        return None
    out["max_risk_usd"] = max_risk

    # Deterministic key order — `artifact_bytes` hashes the exact bytes.
    return {key: out[key] for key in _ROW_KEY_ORDER_V2 if key in out}


def _parse_row_v1(obj: dict[str, object]) -> dict[str, object] | None:
    """The 8g arm, byte-exact: the whole key set is required."""
    if set(obj) != _ROW_KEYS_V1:
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


def parse_proposal(raw: str, require_labels: bool = True) -> Proposal | None:
    """STRICT §7.3 parse: exactly ``{"thesis": str, "rows": [...]}`` plus
    the OPTIONAL RG5 ``"regime"`` verdict, with rows in the §4.1 grammar
    (structural bounds mirrored above). ``None`` on ANY deviation — the
    caller archives + counts, never crashes. Oversized (> 256 rows) is
    malformed; so is a verdict that is not a profile → declaration map.

    RG8 (operator ruling 2026-09-05): every proposed row must carry
    ``regimes`` — an unlabelled row makes the proposal malformed
    (``require_labels``; the library's ``parse_row`` seam stays
    structural so legacy artifacts still import as candidates)."""
    try:
        obj = json.loads(raw)
    except (ValueError, TypeError):
        return None
    if not isinstance(obj, dict):
        return None
    top = typing.cast(dict[str, object], obj)
    if set(top) - {"regime"} != {"thesis", "rows"}:
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
        if row is None or (require_labels and "regimes" not in row):
            return None
        rows.append(row)
    regime: dict[str, str] | None = None
    if "regime" in top:
        verdict = top["regime"]
        if not isinstance(verdict, dict):
            return None
        try:
            regime = claude_worker.regime.parse_verdict(typing.cast(dict[str, object], verdict))
        except ValueError:
            return None
    return Proposal(thesis=thesis, rows=rows, regime=regime)


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
