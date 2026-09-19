# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §10 — policy to frames, and the many ways an action does not send.

This is the module that can reach a venue, so most of what is asserted
here is a refusal. `fake_uds` sees the wire: a shadow test that only
checked a database row would pass while sending frames, so every
"nothing was sent" assertion is made against the socket.

The four that matter most:

* `shadow` computes the WHOLE frame and sends nothing — that is what
  makes a shadow row evidence the N4 gate can read, rather than a note;
* record-then-send, so a crash between the two leaves a row that says
  "we tried and do not know" instead of losing the frame;
* a slot `exec.toml` names LIVE is never disabled by this lane, whatever
  the policy says (LAW E-1);
* `venue_risk = critical` is a RED ALERT and never a halt (ruling Q5) —
  there is no halt kind to reach for.

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import typing

import claude_worker.frames
import claude_worker.news
import claude_worker.news.actions
import claude_worker.news.cascade
import claude_worker.news.detect
import claude_worker.news.store
import claude_worker.regime
import claude_worker.state
import claude_worker.uds

REPO_ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parents[2]
NOW: int = 1_789_826_400
MARKET_MAP: dict[str, int] = {"BTC-UP": 7, "ETH-UP": 8}


class _Client:
    """A UdsClient double that records what reached the wire."""

    def __init__(self, connected: bool = True, fail: bool = False) -> None:
        self.connected = connected
        self.fail = fail
        self.sent: list[dict[str, object]] = []
        self._seq = 100

    def send_cmd(self, **kwargs: object) -> int:
        if self.fail:
            raise claude_worker.uds.UdsError("no heartbeat on this connection")
        self.sent.append(kwargs)
        self._seq += 1
        return self._seq


def _paths(tmp_path: pathlib.Path) -> claude_worker.news.NewsPaths:
    return claude_worker.news.paths_from_env(
        {
            "CLAUDE_WORKER_NEWS_DIR": str(tmp_path / "worker" / "news"),
            "CLAUDE_WORKER_DB": str(tmp_path / "worker" / "state.db"),
            "CLAUDE_WORKER_MARKET_MAP": str(tmp_path / "market-map.json"),
            "CLAUDE_WORKER_REPLAY_DIR": str(tmp_path / "logs"),
            "CLAUDE_WORKER_MULTIVENUE_DIR": str(tmp_path),
        }
    )


def _policy(**over: object) -> claude_worker.news.actions.NewsPolicy:
    modes: dict[str, str] = {}
    for i in range(len(claude_worker.news.actions.POLICY_KINDS)):
        modes[claude_worker.news.actions.POLICY_KINDS[i]] = "shadow"
    fields: dict[str, object] = {"modes": modes, "valid": True}
    fields.update(over)
    return claude_worker.news.actions.NewsPolicy(**fields)  # type: ignore[arg-type]


def _emitter(
    tmp_path: pathlib.Path,
    policy: claude_worker.news.actions.NewsPolicy | None = None,
    client: _Client | None = None,
) -> tuple[
    claude_worker.news.actions.Emitter,
    claude_worker.news.store.Store,
    claude_worker.state.State,
]:
    store = claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")
    state = claude_worker.state.State(tmp_path / "worker" / "state.db")
    paths = _paths(tmp_path)
    emitter = claude_worker.news.actions.Emitter(
        state=state,
        store=store,
        policy=_policy() if policy is None else policy,
        market_map=MARKET_MAP,
        paths=paths,
        client=client,
        ctx=claude_worker.news.detect.Context(
            news_dir=paths.news_dir, xsd_table_path=tmp_path / "xsd-table.tsv"
        ),
    )
    return emitter, store, state


def _story(**over: object) -> dict[str, object]:
    row: dict[str, object] = {
        "story_id": "s1",
        "origins": 2,
        "venue_origin": 0,
        "max_impact": "high",
    }
    row.update(over)
    return row


def _bias(**over: object) -> claude_worker.news.cascade.Action:
    fields: dict[str, object] = {
        "kind": claude_worker.news.cascade.ACTION_SET_BIAS,
        "market": "BTC-UP",
        "dir": "down",
        "confidence": 0.8,
        "ttl_s": 900,
    }
    fields.update(over)
    return claude_worker.news.cascade.Action(**fields)  # type: ignore[arg-type]


# ---- §4.2 the policy ------------------------------------------------------


def test_policy_example_loads() -> None:
    """The tracked example is the contract between the operator's file and
    this module; a stanza that drifts out of the grammar fails here rather
    than at 03:00 on a launchd slot."""
    policy = claude_worker.news.actions.load_policy(REPO_ROOT / "news-policy.toml.example")
    assert policy.valid is True
    assert policy.modes["set_bias"] == "shadow"
    assert policy.modes["declare_vol_high"] == "shadow"
    assert policy.modes["order_intent"] == "shadow"
    assert policy.modes["disable_paper_slot"] == "off"
    assert policy.modes["propose"] == "live"
    assert policy.modes["alert"] == "live"
    assert policy.limits.min_origins == 2
    assert policy.ceilings[claude_worker.news.cascade.TIER1] == 600
    assert policy.paper_allow == (1, 2, 4, 5, 6)
    assert policy.halt_allow == 0, "Q5: kept in the shape, never honoured"


def test_policy_unknown_key_all_off(tmp_path: pathlib.Path) -> None:
    """A typo silences the lane rather than half-configuring it."""
    path = tmp_path / "news-policy.toml"
    path.write_text('[mode]\nset_bias = "live"\nwat = "live"\n', encoding="utf-8")
    policy = claude_worker.news.actions.load_policy(path)
    assert set(policy.modes.values()) == {"off"}
    assert policy.valid is False
    # An unknown MODE is the same answer.
    path.write_text('[mode]\nset_bias = "yolo"\n', encoding="utf-8")
    assert set(claude_worker.news.actions.load_policy(path).modes.values()) == {"off"}
    # An absent file is a legitimate choice, not an error — but still off.
    absent = claude_worker.news.actions.load_policy(tmp_path / "nope.toml")
    assert set(absent.modes.values()) == {"off"} and absent.valid is True


# ---- §10.1 the common law -------------------------------------------------


def test_shadow_never_sends(tmp_path: pathlib.Path) -> None:
    """The whole frame is computed and stored; nothing reaches the wire."""
    client = _Client()
    emitter, store, state = _emitter(tmp_path, client=client)
    try:
        outcome = emitter.emit_bias(_bias(), _story(), NOW)
        assert outcome.mode == "shadow"
        assert client.sent == [], "shadow means the socket sees nothing"
        rows = store.actions_since(0)
        assert len(rows) == 1
        row = rows[0]
        assert row["mode"] == "shadow"
        assert row["seq"] == 0
        # ...and the frame is fully resolved, which is what makes the row
        # evidence rather than a note.
        assert row["frame_kind"] == claude_worker.frames.KIND_SET_BIAS
        assert row["sym"] == 7
        assert row["px"] == -16000
        assert row["ttl_ns"] == 900 * 1_000_000_000
        assert row["venue"] == claude_worker.frames.VENUE_AI
        assert row["flags"] == claude_worker.frames.FLAG_EXPIRE_ON_SILENCE
    finally:
        store.close()
        state.close()


def test_live_records_then_sends_seq_stored(tmp_path: pathlib.Path) -> None:
    client = _Client()
    emitter, store, state = _emitter(tmp_path, _policy(modes={"set_bias": "live"}), client)
    try:
        outcome = emitter.emit_bias(_bias(), _story(), NOW)
        assert outcome.mode == "live"
        assert len(client.sent) == 1
        assert client.sent[0]["kind"] == claude_worker.frames.KIND_SET_BIAS
        row = store.actions_since(0)[0]
        assert row["mode"] == "live"
        assert int(typing.cast(int, row["seq"])) == outcome.seq > 0
        # The worker ledger and audit-replay's chain must agree.
        events = state.events("news_action")
        assert len(events) == 1
    finally:
        store.close()
        state.close()


def test_live_send_failure_row_seq_zero(tmp_path: pathlib.Path) -> None:
    """A row that says "we tried and do not know" is the honest state; the
    reverse order would lose the frame entirely."""
    client = _Client(fail=True)
    emitter, store, state = _emitter(tmp_path, _policy(modes={"set_bias": "live"}), client)
    try:
        emitter.emit_bias(_bias(), _story(), NOW)
        row = store.actions_since(0)[0]
        assert row["mode"] == "live", "the attempt is recorded as live"
        assert row["seq"] == 0
        assert row["detail"] == claude_worker.news.actions.REFUSED_SEND_FAILED
    finally:
        store.close()
        state.close()


def test_disconnected_live_dropped_counted(tmp_path: pathlib.Path) -> None:
    """TTL'd intelligence is not queued for an engine that may return in an
    hour — by then the claim has expired."""
    client = _Client(connected=False)
    emitter, store, state = _emitter(tmp_path, _policy(modes={"set_bias": "live"}), client)
    try:
        outcome = emitter.emit_bias(_bias(), _story(), NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_DISCONNECTED
        assert emitter.stats.dropped == 1
        assert client.sent == []
    finally:
        store.close()
        state.close()


def test_corroboration_min_origins(tmp_path: pathlib.Path) -> None:
    emitter, store, state = _emitter(tmp_path)
    try:
        lonely = _story(origins=1, venue_origin=0)
        outcome = emitter.emit_bias(_bias(), lonely, NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_NOT_CORROBORATED
    finally:
        store.close()
        state.close()


def test_venue_origin_bypasses_min_origins(tmp_path: pathlib.Path) -> None:
    """A venue speaking about itself is one origin and enough."""
    emitter, store, state = _emitter(tmp_path)
    try:
        outcome = emitter.emit_bias(_bias(), _story(origins=1, venue_origin=1), NOW)
        assert outcome.mode == "shadow"
    finally:
        store.close()
        state.close()


def test_hourly_budget_downgrades_to_shadow(tmp_path: pathlib.Path) -> None:
    """Over budget is a DOWNGRADE, not a drop: the claim is still evidence,
    it just does not reach the wire."""
    client = _Client()
    policy = _policy(
        modes={"set_bias": "live"},
        limits=claude_worker.news.actions.Limits(max_live_actions_per_hour=2),
    )
    emitter, store, state = _emitter(tmp_path, policy, client)
    try:
        for _ in range(2):
            assert emitter.emit_bias(_bias(), _story(), NOW).mode == "live"
        third = emitter.emit_bias(_bias(), _story(), NOW)
        assert third.mode == "shadow"
        assert third.reason == claude_worker.news.actions.REFUSED_BUDGET
        assert len(client.sent) == 2
    finally:
        store.close()
        state.close()


def test_mode_off_refuses_before_anything_else(tmp_path: pathlib.Path) -> None:
    client = _Client()
    emitter, store, state = _emitter(tmp_path, _policy(modes={}), client)
    try:
        outcome = emitter.emit_bias(_bias(), _story(), NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_MODE_OFF
        assert client.sent == []
    finally:
        store.close()
        state.close()


def test_set_bias_frame_fields(tmp_path: pathlib.Path) -> None:
    emitter, store, state = _emitter(tmp_path)
    try:
        up = emitter.bias_frame("BTC-UP", "up", 0.9, 600)
        down = emitter.bias_frame("BTC-UP", "down", 0.9, 600)
        assert up is not None and down is not None
        assert up.px == 18000 and down.px == -18000, "the sign IS the direction"
        assert up.sym == 7
        assert up.strategy_id == claude_worker.frames.STRATEGY_SLOT_NONE
        assert up.side == claude_worker.frames.SIDE_NONE
        # The TTL is clamped to the policy cap, never the model's wish.
        capped = emitter.bias_frame("BTC-UP", "up", 0.9, 99_999)
        assert capped is not None
        assert capped.ttl_ns == emitter._policy.limits.ttl_cap_s * 1_000_000_000
        assert emitter.bias_frame("NOPE-UP", "up", 0.9, 600) is None
        # Below the confidence floor is a refusal, not a smaller bias.
        low = emitter.emit_bias(_bias(confidence=0.2), _story(), NOW)
        assert low.reason == claude_worker.news.actions.REFUSED_LOW_CONFIDENCE
    finally:
        store.close()
        state.close()


# ---- §10.3 the regime word ------------------------------------------------


def test_compose_vol_high_keeps_other_dims() -> None:
    """The subtle one. A declared word REPLACES the effective word while
    fresh, and an omitted dimension is EMPTY = "any" — so a bare vol:high
    declaration would UNCONSTRAIN trend and shape for every gated row.
    The declaration is built FROM the engine's own word."""
    effective = claude_worker.frames.regime_word(
        trend="bull", shape="chop", vol="normal", fund="pos", level="high", stretch="neutral"
    )
    composed = claude_worker.news.actions.compose_vol_high(effective)
    dims = claude_worker.frames.regime_word_dims(composed)
    assert dims["vol"] == "high"
    assert dims["trend"] == "bull", "the other dimensions survive untouched"
    assert dims["shape"] == "chop"
    assert dims["fund"] == "pos"
    assert dims["level"] == "high"
    assert claude_worker.frames.regime_word_is_wire_declared(composed)
    # The SOURCE byte is EMPTY — the engine stamps DECLARED itself.
    source_shift = 8 * claude_worker.frames.REGIME_DIMS["source"]
    assert (composed >> source_shift) & 0xFF == 0
    assert (composed >> 56) & 0xFF == 0, "reserved byte 7 empty"


def test_compose_vol_high_unknown_measured() -> None:
    """An unreachable engine falls back to the UNKNOWN word, which is
    fail-CLOSED: EMPTY would mean "any" and would loosen gating, which is
    the opposite of what declaring high volatility is for."""
    composed = claude_worker.news.actions.compose_vol_high(None)
    dims = claude_worker.frames.regime_word_dims(composed)
    assert dims["vol"] == "high"
    assert dims["trend"] == "unknown"
    assert dims["shape"] == "unknown"
    assert claude_worker.frames.regime_word_is_wire_declared(composed)


def _declare_action(**over: object) -> claude_worker.news.cascade.Action:
    fields: dict[str, object] = {
        "kind": claude_worker.news.cascade.ACTION_DECLARE_VOL_HIGH,
        "profile": "fast",
        "ttl_s": 3600,
    }
    fields.update(over)
    return claude_worker.news.cascade.Action(**fields)  # type: ignore[arg-type]


def test_declare_refused_when_operator_declared_fresh(tmp_path: pathlib.Path) -> None:
    """The operator's declaration is never overwritten by this lane."""
    emitter, store, state = _emitter(tmp_path)
    paths = _paths(tmp_path)
    paths.regime_dir.mkdir(parents=True, exist_ok=True)
    (paths.regime_dir / "declared.json").write_text(
        json.dumps(
            {
                "profiles": {
                    "fast": {
                        "word": "0x1",
                        "ts_ms": (NOW - 60) * 1000,
                        "ttl_s": 3600,
                        "source": "operator",
                    }
                }
            }
        ),
        encoding="utf-8",
    )
    try:
        outcome = emitter.emit_declare(_declare_action(), _story(), NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_OPERATOR_DECLARED
    finally:
        store.close()
        state.close()


def test_declare_cooldown(tmp_path: pathlib.Path) -> None:
    emitter, store, state = _emitter(tmp_path)
    try:
        assert emitter.emit_declare(_declare_action(), _story(), NOW).mode == "shadow"
        again = emitter.emit_declare(_declare_action(), _story(), NOW + 60)
        assert again.reason == claude_worker.news.actions.REFUSED_COOLDOWN
        # ...and after the cooldown it is allowed again.
        later = emitter.emit_declare(
            _declare_action(), _story(), NOW + emitter._policy.limits.regime_cooldown_s + 1
        )
        assert later.mode == "shadow"
    finally:
        store.close()
        state.close()


# ---- §10.4 order_intent ---------------------------------------------------


def _intent(**over: object) -> claude_worker.news.cascade.Action:
    fields: dict[str, object] = {
        "kind": claude_worker.news.cascade.ACTION_ORDER_INTENT,
        "market": "BTC-UP",
        "side": "bid",
        "px_offset_bps": 10,
        "qty_usd": 250.0,
        "ttl_s": 300,
    }
    fields.update(over)
    return claude_worker.news.cascade.Action(**fields)  # type: ignore[arg-type]


def test_order_intent_px_qty_scaling(tmp_path: pathlib.Path) -> None:
    """A BID sits BELOW the mid and an ASK above it: ai-exec submits these
    POST_ONLY, so an offset on the wrong side is an instant taker."""
    emitter, store, state = _emitter(tmp_path)
    quote = claude_worker.news.actions.Quote(
        ts_ns=NOW * 1_000_000_000, sym=7, venue=claude_worker.frames.VENUE_BINANCE,
        mid_1e6=100_000_000_000,
    )
    try:
        bid = emitter.intent_frame(_intent(), 7, quote)
        ask = emitter.intent_frame(_intent(side="ask"), 7, quote)
        assert bid is not None and ask is not None
        assert bid.px < quote.mid_1e6 < ask.px
        assert bid.px == round(quote.mid_1e6 * (1 - 10 / 1e4))
        assert bid.side == claude_worker.frames.SIDE_BID
        assert ask.side == claude_worker.frames.SIDE_ASK
        assert bid.strategy_id == claude_worker.frames.STRATEGY_SLOT_AI_EXEC
        assert bid.venue == claude_worker.frames.VENUE_BINANCE, "never VENUE_AI"
        # qty_usd -> qty_1e6 against a 100k mid: 250 USD is 0.0025 units.
        assert bid.qty == round(250.0 * 1e12 / quote.mid_1e6)
        assert bid.ttl_ns == 300 * 1_000_000_000
    finally:
        store.close()
        state.close()


def test_order_intent_caps(tmp_path: pathlib.Path) -> None:
    emitter, store, state = _emitter(tmp_path)
    quote = claude_worker.news.actions.Quote(
        ts_ns=NOW * 1_000_000_000, sym=7, venue=claude_worker.frames.VENUE_BINANCE,
        mid_1e6=100_000_000_000,
    )
    try:
        huge = emitter.intent_frame(
            _intent(qty_usd=1e9, px_offset_bps=9_000, ttl_s=99_999), 7, quote
        )
        assert huge is not None
        limits = emitter._policy.intent
        assert huge.qty == round(limits.qty_usd_cap * 1e12 / quote.mid_1e6)
        assert huge.px == round(quote.mid_1e6 * (1 - limits.px_offset_bps_max / 1e4))
        assert huge.ttl_ns == limits.ttl_max_s * 1_000_000_000
    finally:
        store.close()
        state.close()


def test_order_intent_refused_no_mid(tmp_path: pathlib.Path) -> None:
    """No capture, no order. An intent priced off a stale mid is a worse
    trade than no trade."""
    emitter, store, state = _emitter(tmp_path)
    try:
        outcome = emitter.emit_intent(_intent(), _story(), NOW, now_ns=NOW * 1_000_000_000)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_NO_FRESH_MID
        unknown = emitter.emit_intent(_intent(market="NOPE"), _story(), NOW)
        assert unknown.reason == claude_worker.news.actions.REFUSED_NO_MARKET
    finally:
        store.close()
        state.close()


# ---- §10.5 the maintenance path (mode off in v1, Q4) ---------------------


def _disable(**over: object) -> claude_worker.news.cascade.Action:
    fields: dict[str, object] = {
        "kind": claude_worker.news.cascade.ACTION_DISABLE_SLOT,
        "slot": 2,
        "venue": "binance",
        "until_ts": NOW + 3600,
    }
    fields.update(over)
    return claude_worker.news.cascade.Action(**fields)  # type: ignore[arg-type]


def test_disable_mode_off_only_alerts(tmp_path: pathlib.Path) -> None:
    """Ruling Q4: v1 alerts, it does not disable. The ALERT is the
    deliverable, and it is raised whatever the mode did."""
    client = _Client()
    policy = _policy(modes={"disable_paper_slot": "off"}, paper_allow=(1, 2, 4, 5, 6))
    emitter, store, state = _emitter(tmp_path, policy, client)
    try:
        outcome = emitter.emit_disable(_disable(), _story(), NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_MODE_OFF
        assert client.sent == []
        assert len(emitter.alerts) == 1
        assert emitter.alerts[0].kind == "news_red"
        assert "slot 2" in emitter.alerts[0].text
    finally:
        store.close()
        state.close()


def test_disable_refuses_live_slot_from_exec_toml(tmp_path: pathlib.Path) -> None:
    """LAW E-1: a live slot never falls back to paper. Even with the mode
    LIVE and the slot in paper_allow, exec.toml wins."""
    (tmp_path / "exec.toml").write_text(
        '[exec]\nenabled = true\n\n[exec.slot.3]\nmode = "live"\nname = "bin15"\n',
        encoding="utf-8",
    )
    client = _Client()
    policy = _policy(modes={"disable_paper_slot": "live"}, paper_allow=(1, 2, 3, 4, 5, 6))
    emitter, store, state = _emitter(tmp_path, policy, client)
    try:
        outcome = emitter.emit_disable(_disable(slot=3), _story(), NOW)
        assert outcome.mode == "refused"
        assert outcome.reason == claude_worker.news.actions.REFUSED_SLOT_LIVE
        assert client.sent == []
    finally:
        store.close()
        state.close()
    # An unreadable exec.toml fails CLOSED: every slot might be live.
    (tmp_path / "exec.toml").write_text("not = = toml", encoding="utf-8")
    assert claude_worker.news.actions.live_slots(tmp_path / "exec.toml") == 0xFF


def test_disable_refuses_slot_not_in_allow(tmp_path: pathlib.Path) -> None:
    policy = _policy(modes={"disable_paper_slot": "live"}, paper_allow=(1, 2))
    emitter, store, state = _emitter(tmp_path, policy, _Client())
    try:
        outcome = emitter.emit_disable(_disable(slot=7), _story(), NOW)
        assert outcome.reason == claude_worker.news.actions.REFUSED_SLOT_NOT_ALLOWED
    finally:
        store.close()
        state.close()


# ---- §10.6 alerts and proposals ------------------------------------------


def test_red_alert_writes_ALERT_file(tmp_path: pathlib.Path) -> None:
    """ONE writer for that file: `detect.write_alert`, so the §4.4 line
    format and the 6 h expiry stay one rule in one place (D13)."""
    emitter, store, state = _emitter(tmp_path, _policy(modes={"alert": "live"}), _Client())
    try:
        action = claude_worker.news.cascade.Action(
            kind=claude_worker.news.cascade.ACTION_ALERT,
            severity="red",
            text="OKX withdrawals halted",
        )
        outcome = emitter.emit_alert(action, _story(), NOW)
        assert outcome.mode == "live"
        assert emitter.flush_alerts(NOW) == 1
        line = (_paths(tmp_path).news_dir / claude_worker.news.ALERT_FILE).read_text(
            encoding="utf-8"
        )
        assert line.startswith(claude_worker.news.detect.iso(NOW))
        assert "news_red" in line and "OKX withdrawals halted" in line
        assert len(state.events("news_alert")) == 1
    finally:
        store.close()
        state.close()


def test_critical_venue_risk_is_red_never_halt(tmp_path: pathlib.Path) -> None:
    """Ruling Q5, and the reason there is no halt kind to reach for: a
    critical venue risk raises a RED ALERT before any action is even read."""
    client = _Client()
    emitter, store, state = _emitter(tmp_path, _policy(modes={"alert": "live"}), client)
    assessment = claude_worker.news.cascade.Assessment(
        thesis="The venue is not settling withdrawals.",
        mechanism="venue_risk",
        channels=claude_worker.news.cascade.Channels(
            direction=claude_worker.news.cascade.DirectionChannel("", "none", 0.0),
            vol=claude_worker.news.cascade.VolChannel("none", "none", 0.0, 0),
            venue_risk=claude_worker.news.cascade.VenueRiskChannel("okx", "critical"),
        ),
        affected_descriptors=(),
        actions=(claude_worker.news.cascade.Action(kind="none"),),
        half_life_s=3600,
        falsifier="Withdrawals resume.",
        evidence=(),
    )
    try:
        outcomes = emitter.emit_assessment(_story(), assessment, NOW)
        assert len(emitter.alerts) == 1
        assert emitter.alerts[0].kind == "news_red"
        assert "critical" in emitter.alerts[0].text
        assert client.sent == [], "a critical venue risk sends NOTHING"
        assert outcomes[0].reason == "none"
    finally:
        store.close()
        state.close()


def test_a_proposal_is_a_file_and_touches_no_venue(tmp_path: pathlib.Path) -> None:
    client = _Client()
    emitter, store, state = _emitter(tmp_path, _policy(modes={"propose": "live"}), client)
    try:
        action = claude_worker.news.cascade.Action(
            kind=claude_worker.news.cascade.ACTION_PROPOSE_UNIVERSE,
            descriptor="okx:NEW-USDT-SWAP",
        )
        outcome = emitter.emit_propose(action, _story(), NOW)
        assert outcome.mode == "live"
        assert client.sent == []
        text = (
            _paths(tmp_path).news_dir / claude_worker.news.UNIVERSE_PROPOSALS_FILE
        ).read_text(encoding="utf-8")
        assert 'value = "NEW-USDT-SWAP"' in text
        assert 'section = "instruments"' in text
        # A venue with no ingress cannot be proposed at all.
        none = claude_worker.news.detect.proposal_from_descriptor("coinbase:HYPER-USD")
        assert none is None
        dated = claude_worker.news.detect.proposal_from_descriptor("binance-usdm:btcusdt_261225")
        assert dated is not None and dated.section == "usdm_dated"
    finally:
        store.close()
        state.close()
