# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Policy -> frames: the only path from a model's opinion to the wire (§10).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Everything upstream of here produces OPINIONS. This module is where an
opinion either becomes a 64-byte AI command frame or becomes a recorded
refusal, and the default for every venue-reaching kind is `shadow`: the
full frame is computed and stored, and nothing is sent. That is not
timidity, it is how a claim earns the right to go live — the N4 gate reads
the shadow record against the tape, and a channel that cannot beat its
base rate in shadow has no business on the wire.

Four things hold, in this order, for every candidate action:

1. **Resolve or refuse.** A market not in the map, a venue we do not
   speak, a sym not in the newest manifest, a mid too old to price
   against — each is a recorded `refused` row with the reason, never a
   guess. A guessed field is how a paper lane becomes a real loss.
2. **Corroboration.** A class-C story needs `min_origins` independent
   origins, unless the venue itself said it (ruling Q6: a 0.3-weight mill
   is never an origin, so three mills are still one claim).
3. **Mode.** `off` refuses, `shadow` records the frame it would have
   sent, `live` continues — per kind, re-read from the operator's file on
   every run, so pulling a channel back to `off` takes as long as saving.
4. **Record, THEN send.** The row is inserted with `seq = 0` before the
   frame goes out, and updated after. A crash between the two leaves a
   row that says "we tried and do not know", which is the honest state;
   the reverse order would lose the frame entirely.

Two laws are absolute here and are tested as such: `halt` does not exist
in this grammar (ruling Q5 — `venue_risk = critical` is a RED ALERT, not
a halt), and a slot that `exec.toml` names LIVE is never disabled by this
lane whatever the policy says (LAW E-1: a live slot never falls back to
paper).

When the engine is not connected, live actions are DROPPED and counted.
TTL'd intelligence is not queued for an engine that may return in an
hour — by then the claim has expired and sending it would be a lie about
when it was made.
"""

import dataclasses
import json
import pathlib
import tomllib
import typing

import claude_worker.features
import claude_worker.frames
import claude_worker.news
import claude_worker.news.cascade
import claude_worker.news.detect
import claude_worker.news.store
import claude_worker.pmlr
import claude_worker.regime
import claude_worker.state

# ---------------------------------------------------------------- constants

MODE_OFF: str = "off"
MODE_SHADOW: str = "shadow"
MODE_LIVE: str = "live"
MODES: tuple[str, ...] = (MODE_OFF, MODE_SHADOW, MODE_LIVE)

#: `actions.mode` also records the two outcomes that are not modes.
RECORD_REFUSED: str = "refused"
RECORD_PENDING: str = "pending"

#: Action kinds the policy switches on (spec §4.2 `[mode]`).
KIND_SET_BIAS: str = claude_worker.news.cascade.ACTION_SET_BIAS
KIND_DECLARE_VOL_HIGH: str = claude_worker.news.cascade.ACTION_DECLARE_VOL_HIGH
KIND_ORDER_INTENT: str = claude_worker.news.cascade.ACTION_ORDER_INTENT
KIND_DISABLE_SLOT: str = claude_worker.news.cascade.ACTION_DISABLE_SLOT
KIND_PROPOSE: str = "propose"
KIND_ALERT: str = claude_worker.news.cascade.ACTION_ALERT
POLICY_KINDS: tuple[str, ...] = (
    KIND_SET_BIAS,
    KIND_DECLARE_VOL_HIGH,
    KIND_ORDER_INTENT,
    KIND_DISABLE_SLOT,
    KIND_PROPOSE,
    KIND_ALERT,
)

#: Refusal reasons, closed so the dashboard can group them.
REFUSED_MODE_OFF: str = "mode_off"
REFUSED_BUDGET: str = "budget"
REFUSED_NO_MARKET: str = "no_market"
REFUSED_NO_FRESH_MID: str = "no_fresh_mid"
REFUSED_LOW_CONFIDENCE: str = "low_confidence"
REFUSED_NOT_CORROBORATED: str = "not_corroborated"
REFUSED_COOLDOWN: str = "cooldown"
REFUSED_OPERATOR_DECLARED: str = "operator_declared"
REFUSED_SLOT_NOT_ALLOWED: str = "slot_not_allowed"
REFUSED_SLOT_LIVE: str = "slot_live"
REFUSED_DISCONNECTED: str = "disconnected"
REFUSED_SEND_FAILED: str = "send_failed"

SECONDS_PER_HOUR: int = 3_600
MIN_DECLARE_TTL_S: int = 60
PX_QTY_SCALE: float = 1e6
BPS: float = 1e4
#: `qty_usd -> qty_1e6` given a 1e6-scaled mid: usd * 1e12 / mid_1e6.
QTY_USD_TO_1E6: float = 1e12


@dataclasses.dataclass(frozen=True, slots=True)
class Limits:
    min_origins: int = 2
    min_confidence_bias: float = 0.7
    bias_scale_1e6: int = 20_000
    ttl_cap_s: int = 14_400
    regime_cooldown_s: int = 3_600
    max_live_actions_per_hour: int = 4
    story_max_assessments: int = 2


@dataclasses.dataclass(frozen=True, slots=True)
class IntentLimits:
    qty_usd_cap: float = 1_000.0
    px_offset_bps_max: int = 50
    ttl_max_s: int = 900
    per_market_per_hour: int = 2
    mid_max_age_s: int = 120


@dataclasses.dataclass(frozen=True, slots=True)
class NewsPolicy:
    """`news-policy.toml`, validated. NOT `commander.Policy` — that one is
    the bias floor and TTL shape the legacy path already uses, and this one
    wraps it rather than replacing it.

    The default instance is the SAFE one: every venue-reaching mode `off`.
    An absent file and an invalid file both produce it, which is the whole
    point — a typo silences the lane instead of half-configuring it.
    """

    modes: dict[str, str] = dataclasses.field(default_factory=dict)
    limits: Limits = Limits()
    intent: IntentLimits = IntentLimits()
    ceilings: dict[str, int] = dataclasses.field(default_factory=dict)
    paper_allow: tuple[int, ...] = ()
    halt_allow: int = 0
    valid: bool = True

    def mode(self, kind: str) -> str:
        """The mode for an action kind; `off` for anything unnamed."""
        return self.modes.get(kind, MODE_OFF)


def _safe_policy(valid: bool = False) -> NewsPolicy:
    modes: dict[str, str] = {}
    for i in range(len(POLICY_KINDS)):
        modes[POLICY_KINDS[i]] = MODE_OFF
    return NewsPolicy(
        modes=modes,
        ceilings=dict(claude_worker.news.cascade.DEFAULT_CEILINGS),
        valid=valid,
    )


_MODE_KEYS: frozenset[str] = frozenset(POLICY_KINDS)
_LIMIT_KEYS: frozenset[str] = frozenset(
    (
        "min_origins",
        "min_confidence_bias",
        "bias_scale_1e6",
        "ttl_cap_s",
        "regime_cooldown_s",
        "max_live_actions_per_hour",
        "story_max_assessments",
    )
)
_INTENT_KEYS: frozenset[str] = frozenset(
    ("qty_usd_cap", "px_offset_bps_max", "ttl_max_s", "per_market_per_hour", "mid_max_age_s")
)
_BUDGET_KEYS: frozenset[str] = frozenset(
    ("tier1_calls_per_day", "tier2_calls_per_day", "tier3_calls_per_day")
)
_SLOT_KEYS: frozenset[str] = frozenset(("paper_allow",))
_HALT_KEYS: frozenset[str] = frozenset(("allow",))
_TOP_KEYS: frozenset[str] = frozenset(("mode", "limits", "budget", "intent", "slots", "halt"))

_TIER_OF_KEY: dict[str, str] = {
    "tier1_calls_per_day": claude_worker.news.cascade.TIER1,
    "tier2_calls_per_day": claude_worker.news.cascade.TIER2,
    "tier3_calls_per_day": claude_worker.news.cascade.TIER3,
}


def _reject_unknown(where: str, table: typing.Mapping[str, object], known: frozenset[str]) -> None:
    extra = sorted(set(table) - known)
    if extra:
        raise ValueError(f"{where}: unknown key(s) {extra}")


def load_policy(path: pathlib.Path) -> NewsPolicy:
    """Parse and validate the operator's policy.

    NEVER raises. An absent file, unreadable TOML, an unknown key or an
    out-of-vocabulary mode all return the SAFE policy — every mode `off` —
    with ``valid = False`` so the caller counts ``policy_invalid`` and the
    dashboard says so. Half a policy is more dangerous than none.
    """
    try:
        raw = path.read_bytes()
    except OSError:
        return _safe_policy(valid=True)
    try:
        doc = tomllib.loads(raw.decode("utf-8"))
        return _policy_from(doc)
    except (ValueError, TypeError, UnicodeDecodeError, tomllib.TOMLDecodeError):
        return _safe_policy(valid=False)


def _policy_from(doc: typing.Mapping[str, object]) -> NewsPolicy:
    _reject_unknown("news-policy.toml", doc, _TOP_KEYS)
    modes = _modes_from(typing.cast(dict[str, object], doc.get("mode", {})))
    limits_table = typing.cast(dict[str, object], doc.get("limits", {}))
    _reject_unknown("[limits]", limits_table, _LIMIT_KEYS)
    intent_table = typing.cast(dict[str, object], doc.get("intent", {}))
    _reject_unknown("[intent]", intent_table, _INTENT_KEYS)
    budget_table = typing.cast(dict[str, object], doc.get("budget", {}))
    _reject_unknown("[budget]", budget_table, _BUDGET_KEYS)
    slots_table = typing.cast(dict[str, object], doc.get("slots", {}))
    _reject_unknown("[slots]", slots_table, _SLOT_KEYS)
    halt_table = typing.cast(dict[str, object], doc.get("halt", {}))
    _reject_unknown("[halt]", halt_table, _HALT_KEYS)
    base = Limits()
    intent_base = IntentLimits()
    return NewsPolicy(
        modes=modes,
        limits=Limits(
            min_origins=int(typing.cast(int, limits_table.get("min_origins", base.min_origins))),
            min_confidence_bias=float(
                typing.cast(
                    float,
                    limits_table.get("min_confidence_bias", base.min_confidence_bias),
                )
            ),
            bias_scale_1e6=int(
                typing.cast(int, limits_table.get("bias_scale_1e6", base.bias_scale_1e6))
            ),
            ttl_cap_s=int(typing.cast(int, limits_table.get("ttl_cap_s", base.ttl_cap_s))),
            regime_cooldown_s=int(
                typing.cast(int, limits_table.get("regime_cooldown_s", base.regime_cooldown_s))
            ),
            max_live_actions_per_hour=int(
                typing.cast(
                    int,
                    limits_table.get("max_live_actions_per_hour", base.max_live_actions_per_hour),
                )
            ),
            story_max_assessments=int(
                typing.cast(
                    int, limits_table.get("story_max_assessments", base.story_max_assessments)
                )
            ),
        ),
        intent=IntentLimits(
            qty_usd_cap=float(
                typing.cast(float, intent_table.get("qty_usd_cap", intent_base.qty_usd_cap))
            ),
            px_offset_bps_max=int(
                typing.cast(
                    int, intent_table.get("px_offset_bps_max", intent_base.px_offset_bps_max)
                )
            ),
            ttl_max_s=int(typing.cast(int, intent_table.get("ttl_max_s", intent_base.ttl_max_s))),
            per_market_per_hour=int(
                typing.cast(
                    int, intent_table.get("per_market_per_hour", intent_base.per_market_per_hour)
                )
            ),
            mid_max_age_s=int(
                typing.cast(int, intent_table.get("mid_max_age_s", intent_base.mid_max_age_s))
            ),
        ),
        ceilings=_ceilings_from(budget_table),
        paper_allow=_slots_from(slots_table),
        halt_allow=int(typing.cast(int, halt_table.get("allow", 0))),
        valid=True,
    )


def _modes_from(table: typing.Mapping[str, object]) -> dict[str, str]:
    _reject_unknown("[mode]", table, _MODE_KEYS)
    out: dict[str, str] = {}
    for i in range(len(POLICY_KINDS)):
        kind = POLICY_KINDS[i]
        value = table.get(kind, MODE_OFF)
        if value not in MODES:
            raise ValueError(f"[mode] {kind}: unknown mode {value!r}")
        out[kind] = str(value)
    return out


def _ceilings_from(table: typing.Mapping[str, object]) -> dict[str, int]:
    out = dict(claude_worker.news.cascade.DEFAULT_CEILINGS)
    for key, tier in _TIER_OF_KEY.items():
        if key in table:
            out[tier] = int(typing.cast(int, table[key]))
    return out


def _slots_from(table: typing.Mapping[str, object]) -> tuple[int, ...]:
    raw = typing.cast(list[object], table.get("paper_allow", []))
    out: list[int] = []
    for i in range(len(raw)):
        slot = raw[i]
        if isinstance(slot, bool) or not isinstance(slot, int):
            raise ValueError(f"[slots] paper_allow: not an integer: {slot!r}")
        out.append(int(slot))
    return tuple(out)


# ------------------------------------------------------------ §10.3 the word


def compose_vol_high(effective: int | None) -> int:
    """A declared regime word that says `vol:high` and NOTHING ELSE new.

    This is the subtle one. A declared word REPLACES the whole effective
    word while it is fresh, and `frames.regime_word` treats an omitted
    dimension as EMPTY = "any" — so a bare `vol:high` declaration would
    silently UNCONSTRAIN trend, shape, funding and level for every row
    gated on them. The declaration is therefore built FROM the engine's
    own effective word, with only the vol byte rewritten.

    Unknown marks on the other dimensions are KEPT, never cleared: EMPTY
    means "any" and would loosen gating, which is the opposite of what
    declaring high volatility is for. An unreachable engine falls back to
    the UNKNOWN word, which is fail-closed for the same reason.
    """
    base = claude_worker.regime.UNKNOWN_WORD if effective is None else effective
    vol_shift = 8 * claude_worker.frames.REGIME_DIMS["vol"]
    base &= ~(0xFF << vol_shift)
    base |= 1 << (vol_shift + claude_worker.frames.REGIME_VALUES["vol"].index("high"))
    # SOURCE byte EMPTY — the engine stamps DECLARED itself.
    base &= ~(0xFF << (8 * claude_worker.frames.REGIME_DIMS["source"]))
    # Reserved byte 7 empty.
    base &= ~(0xFF << 56)
    if not claude_worker.frames.regime_word_is_wire_declared(base):
        raise ValueError(f"composed word is not wire-declared: {base:#018x}")
    return base


# ----------------------------------------------------------------- emitting


@dataclasses.dataclass(slots=True)
class EmitStats:
    """What one drain did. `shadow` and `refused` are the two numbers that
    matter before Stage 3 — they are the whole record."""

    recorded: int = 0
    shadow: int = 0
    live: int = 0
    refused: int = 0
    dropped: int = 0
    alerts: int = 0
    proposals: int = 0


class Frame(typing.NamedTuple):
    """The ten fields of an AI command frame, resolved but not yet sent.

    Computed identically in `shadow` and `live` — that is what makes a
    shadow row evidence rather than a note. The engine validates all of
    it again on arrival (`core_types::AiCmd::validate_shape`).
    """

    kind: int
    sym: int = claude_worker.frames.SYMBOL_ID_NONE
    px: int = 0
    qty: int = 0
    ttl_ns: int = 0
    venue: int = claude_worker.frames.VENUE_AI
    strategy_id: int = claude_worker.frames.STRATEGY_SLOT_NONE
    side: int = claude_worker.frames.SIDE_NONE
    param_id: int = 0
    flags: int = 0


class Outcome(typing.NamedTuple):
    """One candidate action's fate: the mode it was recorded under, the
    reason if refused, and the row id so a resolution can cite it."""

    mode: str
    reason: str = ""
    row_id: int = 0
    seq: int = 0


def _clamp_ttl_s(ttl_s: float, cap_s: int) -> int:
    return max(1, min(int(ttl_s), cap_s))


class Emitter:
    """Turns validated claims into recorded actions, and — only under an
    explicit `live` mode — into frames on the wire.

    The client is optional and may be disconnected. Both cases are normal:
    the lane path has no client at all (everything it records is shadow by
    construction), and `serve` may be between reconnects.
    """

    def __init__(  # noqa: PLR0913 — composition root: every collaborator injected
        self,
        *,
        state: claude_worker.state.State,
        store: claude_worker.news.store.Store,
        policy: NewsPolicy,
        market_map: typing.Mapping[str, int],
        paths: claude_worker.news.NewsPaths,
        client: object | None = None,
        ctx: claude_worker.news.detect.Context | None = None,
    ) -> None:
        self._state = state
        self._store = store
        self._policy = policy
        self._market_map = market_map
        self._paths = paths
        self._client = client
        self._ctx = ctx
        self.stats: EmitStats = EmitStats()
        self.alerts: list[claude_worker.news.detect.Alert] = []

    # ---- the common law (§10.1) ----------------------------------------

    def _connected(self) -> bool:
        return bool(getattr(self._client, "connected", False))

    def _hourly_live(self, kind: str, now_ts: int) -> int:
        rows = self._store.actions_since(now_ts - SECONDS_PER_HOUR)
        count = 0
        for i in range(len(rows)):
            if str(rows[i]["kind"]) == kind and str(rows[i]["mode"]) == MODE_LIVE:
                count += 1
        return count

    def _record(  # noqa: PLR0913 — one row of the actions table, field for field
        self,
        kind: str,
        mode: str,
        *,
        frame: Frame | None = None,
        story_id: str = "",
        event_id: int = 0,
        reason: str = "",
        detail: str = "",
        seq: int = 0,
        now_ts: int = 0,
    ) -> int:
        row: dict[str, object] = {
            "ts": now_ts,
            "story_id": story_id,
            "event_id": event_id,
            "kind": kind,
            "mode": mode,
            "refused_reason": reason,
            "detail": detail,
            "seq": seq,
        }
        if frame is not None:
            row.update(
                {
                    "frame_kind": frame.kind,
                    "sym": frame.sym,
                    "px": frame.px,
                    "qty": frame.qty,
                    "ttl_ns": frame.ttl_ns,
                    "venue": frame.venue,
                    "strategy_id": frame.strategy_id,
                    "side": frame.side,
                    "param_id": frame.param_id,
                    "flags": frame.flags,
                }
            )
        self.stats.recorded += 1
        if mode == MODE_LIVE:
            self.stats.live += 1
        elif mode == MODE_SHADOW:
            self.stats.shadow += 1
        else:
            self.stats.refused += 1
        return self._store.record_action(row)

    def gate(self, kind: str, now_ts: int) -> tuple[str, str]:
        """Mode, hourly budget and connectedness, in that order.

        Returns ``(effective_mode, reason)``. A live kind over its hourly
        budget is DOWNGRADED to shadow rather than dropped — the claim is
        still evidence, it just does not reach the wire.
        """
        mode = self._policy.mode(kind)
        if mode == MODE_OFF:
            return RECORD_REFUSED, REFUSED_MODE_OFF
        if mode != MODE_LIVE:
            return MODE_SHADOW, ""
        if self._hourly_live(kind, now_ts) >= self._policy.limits.max_live_actions_per_hour:
            return MODE_SHADOW, REFUSED_BUDGET
        if not self._connected():
            return RECORD_REFUSED, REFUSED_DISCONNECTED
        return MODE_LIVE, ""

    def corroborated(self, story: typing.Mapping[str, object] | None) -> bool:
        """Ruling Q6's corroboration rule. A venue speaking about itself is
        one origin and enough; prose needs `min_origins` DISTINCT
        full-weight origins, because the mills republish each other."""
        if story is None:
            return True
        if int(typing.cast(int, story["venue_origin"])):
            return True
        return int(typing.cast(int, story["origins"])) >= self._policy.limits.min_origins

    def _send(self, frame: Frame, now_ts: int) -> tuple[int, str]:
        """Record-then-send's second half. Returns ``(seq, error)``."""
        client = self._client
        if client is None:
            return 0, REFUSED_DISCONNECTED
        try:
            seq = int(
                client.send_cmd(  # type: ignore[attr-defined]
                    sym=frame.sym,
                    px=frame.px,
                    qty=frame.qty,
                    ttl_ns=frame.ttl_ns,
                    kind=frame.kind,
                    venue=frame.venue,
                    strategy_id=frame.strategy_id,
                    side=frame.side,
                    param_id=frame.param_id,
                    flags=frame.flags,
                )
            )
        except Exception:  # any UdsError shape; the row must still be honest
            return 0, REFUSED_SEND_FAILED
        self._state.record_frame_sent(seq, frame.kind, now_ts * 1_000_000_000)
        self._state.record_event(
            "news_action",
            json.dumps({"kind": frame.kind, "seq": seq, "sym": frame.sym}, sort_keys=True),
        )
        return seq, ""

    # ---- §10.2 set_bias -------------------------------------------------

    def bias_frame(
        self, market: str, direction: str, confidence: float, ttl_s: float
    ) -> Frame | None:
        """The SetBias frame, computed the same way the commander computes
        it — so a shadow row and a live send differ only in whether the
        bytes left the process."""
        sym = self._market_map.get(market)
        if sym is None:
            return None
        sign = 1 if direction == "up" else -1
        return Frame(
            kind=claude_worker.frames.KIND_SET_BIAS,
            sym=sym,
            px=sign * round(self._policy.limits.bias_scale_1e6 * confidence),
            ttl_ns=_clamp_ttl_s(ttl_s, self._policy.limits.ttl_cap_s) * 1_000_000_000,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
            side=claude_worker.frames.SIDE_NONE,
            flags=claude_worker.frames.FLAG_EXPIRE_ON_SILENCE,
        )

    def emit_bias(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
    ) -> Outcome:
        story_id = "" if story is None else str(story["story_id"])
        if action.confidence < self._policy.limits.min_confidence_bias:
            return self._refuse(KIND_SET_BIAS, REFUSED_LOW_CONFIDENCE, story_id, now_ts)
        if not self.corroborated(story):
            return self._refuse(KIND_SET_BIAS, REFUSED_NOT_CORROBORATED, story_id, now_ts)
        frame = self.bias_frame(action.market, action.dir, action.confidence, action.ttl_s)
        if frame is None:
            return self._refuse(KIND_SET_BIAS, REFUSED_NO_MARKET, story_id, now_ts)
        return self._dispatch(KIND_SET_BIAS, frame, story_id, now_ts, detail=action.market)

    # ---- §10.3 declare_vol_high ----------------------------------------

    def emit_declare(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
        effective: int | None = None,
    ) -> Outcome:
        story_id = "" if story is None else str(story["story_id"])
        if not self.corroborated(story):
            return self._refuse(KIND_DECLARE_VOL_HIGH, REFUSED_NOT_CORROBORATED, story_id, now_ts)
        if self._operator_declared(now_ts):
            return self._refuse(
                KIND_DECLARE_VOL_HIGH, REFUSED_OPERATOR_DECLARED, story_id, now_ts
            )
        if self._in_cooldown(KIND_DECLARE_VOL_HIGH, now_ts):
            return self._refuse(KIND_DECLARE_VOL_HIGH, REFUSED_COOLDOWN, story_id, now_ts)
        word = compose_vol_high(effective)
        ttl_s = max(MIN_DECLARE_TTL_S, _clamp_ttl_s(action.ttl_s, self._policy.limits.ttl_cap_s))
        profile = 0 if action.profile in ("fast", "both") else 1
        frame = Frame(
            kind=claude_worker.frames.KIND_SET_REGIME,
            px=word,
            qty=0 if effective is None else effective,
            ttl_ns=ttl_s * 1_000_000_000,
            venue=claude_worker.frames.VENUE_AI,
            param_id=profile,
            flags=1,
        )
        profile_name = claude_worker.regime.PROFILE_NAMES[profile]

        def send(sent: Frame, sent_ts: int) -> tuple[int, str]:
            return self._declare(sent, sent_ts, profile_name, ttl_s, story_id, effective)

        return self._dispatch(
            KIND_DECLARE_VOL_HIGH, frame, story_id, now_ts, detail=action.profile, send_fn=send
        )

    def _declare(  # noqa: PLR0913, PLR0917 — the declaration's own send path
        self,
        frame: Frame,
        now_ts: int,
        profile: str,
        ttl_s: int,
        story_id: str,
        effective: int | None,
    ) -> tuple[int, str]:
        """The live send for a declaration goes through `regime.declare_words`,
        NOT a raw frame: that call persists `declared.json`, which is what
        makes the post-boot repush re-send this declaration with its
        remaining TTL, exactly as an operator's own declaration survives a
        restart. A raw SetRegime would vanish at the next boot.
        """
        client = self._client
        if client is None:
            return 0, REFUSED_DISCONNECTED
        measured = None if effective is None else {profile: effective}
        try:
            seqs = claude_worker.regime.declare_words(
                client,
                self._paths.regime_dir,
                {profile: frame.px},
                now_ts * 1_000,
                ttl_s,
                f"news:{story_id}",
                measured,
            )
        except Exception:  # any UdsError shape; the row must still be honest
            return 0, REFUSED_SEND_FAILED
        return (int(seqs[0]) if seqs else 0), ""

    def _operator_declared(self, now_ts: int) -> bool:
        """An operator declaration is never overwritten by this lane. A
        fresh entry whose source is not ours wins, full stop."""
        try:
            declared = claude_worker.regime.load_declared(self._paths.regime_dir)
        except (OSError, ValueError):
            return False
        for name in declared:
            entry = declared[name]
            source = str(entry.get("source", ""))
            fresh = claude_worker.regime.declared_is_fresh(entry, now_ts * 1_000)
            if fresh and not source.startswith("news:"):
                return True
        return False

    def _in_cooldown(self, kind: str, now_ts: int) -> bool:
        rows = self._store.actions_since(now_ts - self._policy.limits.regime_cooldown_s)
        for i in range(len(rows)):
            row = rows[i]
            if str(row["kind"]) == kind and str(row["mode"]) in (MODE_LIVE, MODE_SHADOW):
                return True
        return False

    # ---- shared tail ----------------------------------------------------

    def _refuse(self, kind: str, reason: str, story_id: str, now_ts: int) -> Outcome:
        row_id = self._record(kind, RECORD_REFUSED, story_id=story_id, reason=reason, now_ts=now_ts)
        return Outcome(mode=RECORD_REFUSED, reason=reason, row_id=row_id)

    def _dispatch(  # noqa: PLR0913, PLR0917 — the shared tail; the sender varies by kind
        self,
        kind: str,
        frame: Frame,
        story_id: str,
        now_ts: int,
        detail: str = "",
        send_fn: typing.Callable[[Frame, int], tuple[int, str]] | None = None,
    ) -> Outcome:
        """Mode, then record, THEN send — and update the row with the seq.

        A crash between the insert and the send leaves a row that says "we
        tried and do not know", which is the honest state. The reverse
        order would lose the frame entirely.
        """
        mode, reason = self.gate(kind, now_ts)
        if mode == RECORD_REFUSED:
            if reason == REFUSED_DISCONNECTED:
                self.stats.dropped += 1
            row_id = self._record(
                kind, RECORD_REFUSED, frame=frame, story_id=story_id, reason=reason,
                detail=detail, now_ts=now_ts,
            )
            return Outcome(mode=RECORD_REFUSED, reason=reason, row_id=row_id)
        row_id = self._record(
            kind, mode, frame=frame, story_id=story_id, reason=reason, detail=detail,
            now_ts=now_ts,
        )
        if mode != MODE_LIVE:
            return Outcome(mode=mode, reason=reason, row_id=row_id)
        sender = self._send if send_fn is None else send_fn
        seq, error = sender(frame, now_ts)
        self._store.update_action(row_id, seq=seq, detail=error or detail)
        return Outcome(mode=MODE_LIVE, reason=error, row_id=row_id, seq=seq)

    # ---- §10.4 order_intent (Q11) ---------------------------------------

    def emit_intent(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
        now_ns: int = 0,
    ) -> Outcome:
        story_id = "" if story is None else str(story["story_id"])
        sym = self._market_map.get(action.market)
        if sym is None:
            return self._refuse(KIND_ORDER_INTENT, REFUSED_NO_MARKET, story_id, now_ts)
        if not self.corroborated(story):
            return self._refuse(KIND_ORDER_INTENT, REFUSED_NOT_CORROBORATED, story_id, now_ts)
        quote = last_tick(self._paths.replay_dir, sym)
        limits = self._policy.intent
        if quote is None:
            return self._refuse(KIND_ORDER_INTENT, REFUSED_NO_FRESH_MID, story_id, now_ts)
        age_s = (now_ns - quote.ts_ns) / 1e9 if now_ns else 0.0
        if age_s > limits.mid_max_age_s:
            return self._refuse(KIND_ORDER_INTENT, REFUSED_NO_FRESH_MID, story_id, now_ts)
        frame = self.intent_frame(action, sym, quote)
        if frame is None:
            return self._refuse(KIND_ORDER_INTENT, REFUSED_NO_FRESH_MID, story_id, now_ts)
        return self._dispatch(KIND_ORDER_INTENT, frame, story_id, now_ts, detail=action.market)

    def intent_frame(
        self, action: claude_worker.news.cascade.Action, sym: int, quote: "Quote"
    ) -> Frame | None:
        """A post-only intent: a BID below the mid, an ASK above it.

        `ai-exec.honor_intent` submits these as POST_ONLY, so an offset on
        the wrong side of the mid would be an instant taker — the sign is
        not cosmetic.
        """
        limits = self._policy.intent
        mid_1e6 = quote.mid_1e6
        if mid_1e6 <= 0:
            return None
        offset = max(-limits.px_offset_bps_max, min(limits.px_offset_bps_max, action.px_offset_bps))
        side_sign = -1 if action.side == "bid" else 1
        px_1e6 = round(mid_1e6 * (1.0 + side_sign * offset / BPS))
        qty_usd = min(action.qty_usd, limits.qty_usd_cap)
        qty_1e6 = max(1, round(qty_usd * QTY_USD_TO_1E6 / mid_1e6))
        if px_1e6 <= 0:
            return None
        return Frame(
            kind=claude_worker.frames.KIND_ORDER_INTENT,
            sym=sym,
            px=px_1e6,
            qty=qty_1e6,
            ttl_ns=_clamp_ttl_s(action.ttl_s, limits.ttl_max_s) * 1_000_000_000,
            venue=quote.venue,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_AI_EXEC,
            side=(
                claude_worker.frames.SIDE_BID
                if action.side == "bid"
                else claude_worker.frames.SIDE_ASK
            ),
        )

    # ---- §10.5 disable_paper_slot (mode off in v1, Q4) ------------------

    def emit_disable(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
    ) -> Outcome:
        """v1 alerts, it does not disable — and even when the mode is not
        `off`, a slot `exec.toml` names LIVE is refused outright (LAW E-1:
        a live slot never falls back to paper)."""
        story_id = "" if story is None else str(story["story_id"])
        if action.slot not in self._policy.paper_allow:
            return self._refuse(KIND_DISABLE_SLOT, REFUSED_SLOT_NOT_ALLOWED, story_id, now_ts)
        if live_slots(self._paths.multivenue_dir / "exec.toml") & (1 << action.slot):
            return self._refuse(KIND_DISABLE_SLOT, REFUSED_SLOT_LIVE, story_id, now_ts)
        frame = Frame(
            kind=claude_worker.frames.KIND_DISABLE_STRATEGY,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=action.slot,
        )
        outcome = self._dispatch(
            KIND_DISABLE_SLOT, frame, story_id, now_ts, detail=f"slot {action.slot}"
        )
        # The v1 deliverable: the alert, whatever the mode did.
        self.raise_alert(
            "red",
            f"maintenance on {action.venue}; slot {action.slot} is enabled",
            now_ts,
        )
        return outcome

    # ---- §10.6 propose_*, alert -----------------------------------------

    def raise_alert(self, severity: str, text: str, now_ts: int) -> None:
        """Queue an alert. `red` reaches the ALERT file through
        `detect.write_alert` — ONE writer for that file, never two."""
        self.alerts.append(
            claude_worker.news.detect.Alert(
                kind=f"news_{severity}", text=text[:claude_worker.news.cascade.ALERT_TEXT_MAX],
                at_ts=now_ts,
            )
        )
        self.stats.alerts += 1
        self._state.record_event("news_alert", json.dumps({"severity": severity, "text": text}))

    def flush_alerts(self, now_ts: int) -> int:
        """Write the newest RED alert, if any (spec §4.4). Shares
        `detect`'s writer, so the 6 h expiry stays one rule in one place."""
        if self._ctx is None:
            return 0
        red: list[claude_worker.news.detect.Alert] = []
        for i in range(len(self.alerts)):
            if self.alerts[i].kind == "news_red":
                red.append(self.alerts[i])
        return claude_worker.news.detect.write_alert(
            self._ctx.file(claude_worker.news.ALERT_FILE), red, now_ts
        )

    # ---- the composition ------------------------------------------------

    def emit_label(
        self,
        label: object,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
    ) -> Outcome:
        """A tier-2 label as a bias. A directionless label is a vol or
        liquidity signal for the analyst, never a bias — refused here for
        the same reason `commander.emit` refuses it."""
        direction = str(getattr(label, "direction", "none"))
        story_id = "" if story is None else str(story["story_id"])
        if direction == "none":
            return self._refuse(KIND_SET_BIAS, "no_direction", story_id, now_ts)
        descriptor = ""
        sym = int(getattr(label, "sym", 0))
        for name in self._market_map:
            if self._market_map[name] == sym:
                descriptor = name
                break
        action = claude_worker.news.cascade.Action(
            kind=KIND_SET_BIAS,
            market=descriptor,
            dir=direction,
            confidence=float(getattr(label, "confidence", 0.0)),
            ttl_s=int(float(getattr(label, "half_life_s", 0.0))),
        )
        return self.emit_bias(action, story, now_ts)

    def emit_assessment(
        self,
        story: typing.Mapping[str, object] | None,
        assessment: claude_worker.news.cascade.Assessment,
        now_ts: int,
        *,
        now_ns: int = 0,
        effective: int | None = None,
    ) -> list[Outcome]:
        """Every action an assessment proposes, each through its own gate.

        Ruling Q5 first, before any action is read: a `critical` venue
        risk is a RED ALERT and never a halt, whatever the actions list
        says — the kind does not exist.
        """
        out: list[Outcome] = []
        if assessment.channels.venue_risk.severity == "critical":
            self.raise_alert(
                "red",
                f"venue_risk critical on {assessment.channels.venue_risk.venue or 'unknown'}:"
                f" {assessment.thesis}",
                now_ts,
            )
        actions = assessment.actions
        for i in range(len(actions)):
            out.append(self._emit_one(actions[i], story, now_ts, now_ns, effective))
        return out

    def _emit_one(  # one dispatch, five things it may need
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
        now_ns: int,
        effective: int | None,
    ) -> Outcome:
        kind = action.kind
        if kind == claude_worker.news.cascade.ACTION_DECLARE_VOL_HIGH:
            return self.emit_declare(action, story, now_ts, effective)
        if kind == claude_worker.news.cascade.ACTION_ORDER_INTENT:
            return self.emit_intent(action, story, now_ts, now_ns)
        simple = _SIMPLE_EMITTERS.get(kind)
        if simple is None:
            # `none` is a real answer — "nothing should change" — and the
            # grammar has no other kind, so this is not a fallthrough.
            return Outcome(mode=RECORD_REFUSED, reason="none")
        return simple(self, action, story, now_ts)

    def emit_alert(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
    ) -> Outcome:
        story_id = "" if story is None else str(story["story_id"])
        mode, reason = self.gate(KIND_ALERT, now_ts)
        if mode == RECORD_REFUSED:
            return self._refuse(KIND_ALERT, reason, story_id, now_ts)
        self.raise_alert(action.severity, action.text, now_ts)
        row_id = self._record(
            KIND_ALERT, mode, story_id=story_id, detail=f"{action.severity}: {action.text}",
            now_ts=now_ts,
        )
        return Outcome(mode=mode, row_id=row_id)

    def emit_propose(
        self,
        action: claude_worker.news.cascade.Action,
        story: typing.Mapping[str, object] | None,
        now_ts: int,
    ) -> Outcome:
        """A proposal is a FILE the operator applies by hand (ruling Q10),
        so it touches no venue and its mode ships `live`."""
        story_id = "" if story is None else str(story["story_id"])
        mode, reason = self.gate(KIND_PROPOSE, now_ts)
        if mode == RECORD_REFUSED:
            return self._refuse(KIND_PROPOSE, reason, story_id, now_ts)
        written = 0
        if self._ctx is not None and mode == MODE_LIVE:
            written = self._write_proposal(action, now_ts)
        self.stats.proposals += written
        row_id = self._record(
            KIND_PROPOSE, mode, story_id=story_id,
            detail=f"{action.kind} {action.descriptor} written={written}", now_ts=now_ts,
        )
        return Outcome(mode=mode, row_id=row_id)

    def _write_proposal(
        self, action: claude_worker.news.cascade.Action, now_ts: int
    ) -> int:
        ctx = self._ctx
        if ctx is None:
            return 0
        event = claude_worker.news.detect.proposal_from_descriptor(
            action.descriptor, at_ts=action.before_ts or now_ts
        )
        if event is None:
            return 0
        if action.kind == claude_worker.news.cascade.ACTION_PROPOSE_UNIVERSE:
            return claude_worker.news.detect.write_universe_proposals(ctx, [(0, event)], now_ts)
        dropped, alerts = claude_worker.news.detect.write_xsd_proposals(
            ctx, [(0, event._replace(kind=claude_worker.news.detect.EVENT_DELISTING))], now_ts
        )
        self.alerts.extend(alerts)
        return dropped



# ------------------------------------------------- the two external reads


class Quote(typing.NamedTuple):
    """The freshest offline mid for a symbol, and the venue it came from."""

    ts_ns: int
    sym: int
    venue: int
    mid_1e6: int


#: How far back into a tick file to look for a symbol. A capture holds
#: every symbol interleaved, so the newest record for a quiet one can be
#: thousands of slots back — but not millions, and a bounded scan keeps a
#: 120 s lane bounded.
INTENT_TAIL_SLOTS: int = 8_192


def last_tick(replay_dir: pathlib.Path, sym: int) -> Quote | None:
    """The newest non-stale tick for ``sym`` in the newest run.

    The per-sym feature file carries a mid too, but it is only as fresh as
    the last 6-hourly `fetch` — never a price source for an order. This
    reads the capture itself. ``None`` when there is no run, no file, no
    record for the symbol, or only stale ones.
    """
    try:
        run = claude_worker.features.latest_run_dir(replay_dir)
    except OSError:
        return None
    if run is None:
        return None
    best: Quote | None = None
    for path in sorted(run.glob("*-ticks.pmlr")):
        found = _scan_tail(path, sym)
        if found is not None and (best is None or found.ts_ns > best.ts_ns):
            best = found
    return best


def _scan_tail(path: pathlib.Path, sym: int) -> Quote | None:
    try:
        with claude_worker.pmlr.Reader(path) as reader:
            total = len(reader)
            floor = max(0, total - INTENT_TAIL_SLOTS)
            for i in range(total - 1, floor - 1, -1):
                rec = reader.tick(i)
                if rec.sym != sym or rec.is_stale():
                    continue
                mid = rec.mid()
                if mid <= 0:
                    continue
                return Quote(ts_ns=rec.ts_ns, sym=sym, venue=rec.venue, mid_1e6=mid)
    except (OSError, ValueError, IndexError, AttributeError):
        return None
    return None


def live_slots(exec_toml: pathlib.Path) -> int:
    """A bitmask of the slots `exec.toml` names LIVE.

    Read fresh every time and fail CLOSED: an unreadable or malformed file
    returns "every slot might be live", because the alternative — assuming
    paper because we could not parse the file — is how LAW E-1 gets broken
    by a typo.
    """
    try:
        doc = tomllib.loads(exec_toml.read_text(encoding="utf-8"))
    except OSError:
        return 0
    except (ValueError, TypeError, UnicodeDecodeError):
        return 0xFF
    slots = doc.get("exec", {})
    table = slots.get("slot", {}) if isinstance(slots, dict) else {}
    mask = 0
    if not isinstance(table, dict):
        return 0xFF
    for key in table:
        entry = table[key]
        if isinstance(entry, dict) and str(entry.get("mode", "")) == MODE_LIVE and key.isdigit():
            mask |= 1 << int(key)
    return mask


#: The kinds whose emitter needs only ``(action, story, now_ts)``. The two
#: that need more (a regime word, a clock for the mid's age) are branched
#: on directly above.
_SIMPLE_EMITTERS: dict[
    str,
    typing.Callable[
        [Emitter, claude_worker.news.cascade.Action, typing.Mapping[str, object] | None, int],
        Outcome,
    ],
] = {
    claude_worker.news.cascade.ACTION_SET_BIAS: Emitter.emit_bias,
    claude_worker.news.cascade.ACTION_DISABLE_SLOT: Emitter.emit_disable,
    claude_worker.news.cascade.ACTION_ALERT: Emitter.emit_alert,
    claude_worker.news.cascade.ACTION_PROPOSE_UNIVERSE: Emitter.emit_propose,
    claude_worker.news.cascade.ACTION_PROPOSE_XSD_DROP: Emitter.emit_propose,
}
