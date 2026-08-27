# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""commander (design §5.1): labeled events + operator policy -> AiCmd
frames, plus the serve-mode Heartbeat cadence.

Policy doctrine (v1, documented interpretation): a news label is
directional PRESSURE, not a fair level — the commander emits ``SetBias``
(the signed channel, §3) and never invents fair values or order intents;
those stay with ai-exec's own quoting and the semi-manual operator. The
bias magnitude scales linearly with model confidence
(``bias_scale_1e6 * confidence``), signed by direction.

TTL from half-life (§5.1): ``ttl_ns = clamp(half_life_s, ttl_min_s,
ttl_cap_s) * 1e9`` — the engine's TTL base is its own monotonic accept
stamp (decision 1), so the worker sends a pure duration. Labels below
``min_confidence`` are refused and counted, never sent.

Heartbeat cadence is the §13-decision-6 compile-time 5 s constant,
measured on the injected monotonic clock; the transport-level
heartbeat-precedes-payload rule stays enforced in ``uds.py`` (item 9) —
the daemon (item 11) drives ``maybe_heartbeat`` ahead of emissions each
iteration. Transport failures (``UdsError``) deliberately bubble to the
daemon, which owns reconnect policy; the commander holds no socket
state of its own (single-writer stays structural).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses

import claude_worker.frames
import claude_worker.labeling
import claude_worker.uds

# §13 decision 6: 5 s heartbeat, compile-time constant (tests inject
# synthetic clocks instead of tuning it).
HEARTBEAT_INTERVAL_NS: int = 5_000_000_000

_NS_PER_S: float = 1e9


@dataclasses.dataclass(frozen=True, slots=True)
class Policy:
    """Operator policy for auto-emission (worker config, not prompts)."""

    min_confidence: float = 0.7
    bias_scale_1e6: int = 20_000  # 2 cents of bias at confidence 1.0
    ttl_min_s: float = 1.0
    ttl_cap_s: float = 3_600.0
    expire_on_silence: bool = True

    def ttl_ns_for(self, half_life_s: float) -> int:
        """TTL from label half-life, clamped to [ttl_min_s, ttl_cap_s]."""
        clamped = min(max(half_life_s, self.ttl_min_s), self.ttl_cap_s)
        return int(clamped * _NS_PER_S)


class Commander:
    """Policy gate + frame emission over the item-9 client. One per
    serve loop; the semi-manual path never constructs one (verbs push
    explicit frames instead)."""

    def __init__(self, client: claude_worker.uds.UdsClient, policy: Policy) -> None:
        self._client = client
        self._policy = policy
        self._last_heartbeat_ns: int | None = None
        self.emitted_total: int = 0
        self.refused_low_confidence_total: int = 0

    def maybe_heartbeat(self, now_ns: int) -> bool:
        """Send a Heartbeat when the 5 s cadence is due (first call is
        always due). Returns whether one was sent; UdsError bubbles."""
        last = self._last_heartbeat_ns
        if last is not None and now_ns - last < HEARTBEAT_INTERVAL_NS:
            return False
        self._client.send_heartbeat()
        self._last_heartbeat_ns = now_ns
        return True

    def reset_cadence(self) -> None:
        """Forget heartbeat history (daemon calls this on reconnect: the
        new connection needs its heartbeat immediately)."""
        self._last_heartbeat_ns = None

    def emit(self, label: claude_worker.labeling.Label) -> int | None:
        """One label -> one SetBias frame, or a counted refusal when the
        label is below the policy's confidence floor. Returns the seq
        used, None when refused."""
        if label.confidence < self._policy.min_confidence:
            self.refused_low_confidence_total += 1
            return None
        sign = 1 if label.direction == "up" else -1
        bias_1e6 = sign * round(self._policy.bias_scale_1e6 * label.confidence)
        flags = claude_worker.frames.FLAG_EXPIRE_ON_SILENCE if self._policy.expire_on_silence else 0
        seq = self._client.send_cmd(
            sym=label.sym,
            px=bias_1e6,
            qty=0,
            ttl_ns=self._policy.ttl_ns_for(label.half_life_s),
            kind=claude_worker.frames.KIND_SET_BIAS,
            venue=claude_worker.frames.VENUE_AI,
            strategy_id=claude_worker.frames.STRATEGY_SLOT_NONE,
            side=claude_worker.frames.SIDE_NONE,
            param_id=0,
            flags=flags,
        )
        self.emitted_total += 1
        return seq
