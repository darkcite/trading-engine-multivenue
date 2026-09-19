# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §8 — the class-A detectors, their proposals and the calendar.

Fixture law (§17): every transition is forced ONCE from a hand-edited
snapshot of a RECORDED payload. The pairs below start from the real
`tests/fixtures/news/instruments-*.json` and `status-*.json` bodies, parse
them through the real `sources.parse`, and edit the decoded body — so a
venue reshaping its wire moves these tests, which is the point. Nothing
here invents a payload shape.

`NOW` is 2026-09-19T14:00:00Z, the hour the fixtures were recorded, which
is what makes the Deribit fixture's own `BTC-20SEP26` (expiring 18 h later)
a real expiry case rather than a written-down one.

Three properties this file defends:

* a detector needs a baseline (`prev is None` emits nothing) and records a
  fact once, however often it re-runs;
* an open-ended outage opens ONE row and is closed by an update;
* every file output is append-only and skips what is already there.

No socket is opened: the one network reader (`/metrics`, for the red rule)
is injected, and a test asserts the default is never reached.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib
import tomllib
import typing

import claude_worker.news
import claude_worker.news.detect
import claude_worker.news.sources
import claude_worker.news.store

_FIXTURES: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "news"

#: 2026-09-19T14:00:00Z — the hour the class-A fixtures were recorded.
NOW: int = 1_789_826_400
#: The Deribit fixture's nearest dated future, 18 h after `NOW`.
BTC_20SEP26_EXPIRY: int = 1_789_891_200
#: The synthetic OKX/Bybit maintenance window, and an instant inside it.
WINDOW_BEGIN: int = 1_789_768_800
WINDOW_END: int = 1_789_772_400
WINDOW_NOW: int = 1_789_770_000
HOUR_S: int = 3_600


def _source(name: str, kind: str, venue: str) -> claude_worker.news.sources.Source:
    return claude_worker.news.sources.Source(
        name=name,
        kind=kind,
        url=f"https://{venue}.example/x",
        origin=f"{venue}.example",
        class_="A",
        venue=venue,
    )


def _recorded(
    source: claude_worker.news.sources.Source,
) -> claude_worker.news.sources.Snapshot:
    """The source's own kind, parsed from its RECORDED fixture."""
    path = _FIXTURES / (source.kind + claude_worker.news.sources.fixture_suffix(source.kind))
    parsed = claude_worker.news.sources.parse(source, path.read_text(encoding="utf-8"), NOW)
    assert parsed.snapshot is not None, f"{source.kind} fixture no longer parses"
    return parsed.snapshot


def _edit(
    snapshot: claude_worker.news.sources.Snapshot,
    mutate: typing.Callable[[dict[str, object]], None],
) -> claude_worker.news.sources.Snapshot:
    """A hand-edited snapshot, re-canonicalised exactly as `sources._snapshot`
    does — same separators, same sorted keys, same digest."""
    body = json.loads(snapshot.body)
    mutate(body)
    text = json.dumps(body, separators=(",", ":"), sort_keys=True)
    return snapshot._replace(
        body=text,
        sha256=hashlib.sha256(text.encode("utf-8")).hexdigest(),
        count=len(body),
    )


def _only(body: dict[str, object], *idents: str) -> None:
    """Keep just these rows — a 400-instrument fixture makes a transition
    hard to read, and the rows kept are the recorded ones."""
    for key in list(body):
        if key not in idents:
            del body[key]


def _store(tmp_path: pathlib.Path) -> claude_worker.news.store.Store:
    return claude_worker.news.store.Store(tmp_path / "worker" / "news" / "news.db")


def _ctx(tmp_path: pathlib.Path, **over: object) -> claude_worker.news.detect.Context:
    fields: dict[str, object] = {
        "news_dir": tmp_path / "worker" / "news",
        "xsd_table_path": tmp_path / "xsd-table.tsv",
    }
    fields.update(over)
    return claude_worker.news.detect.Context(**fields)  # type: ignore[arg-type]


def _kinds(events: list[claude_worker.news.detect.Event]) -> list[str]:
    out: list[str] = []
    for i in range(len(events)):
        out.append(events[i].kind)
    return out


# ---- the snapshot contract ------------------------------------------------


def test_the_field_order_matches_the_parsers_that_write_it() -> None:
    """The one silent failure mode: `sources.py` builds each keyed row as a
    LIST, so an index in `detect.py` that drifts from that tuple reads the
    wrong field and fabricates transitions. Pin both sides together."""
    assert claude_worker.news.sources._BN_USDM_KEYS.index("status") == (
        claude_worker.news.detect._BN_STATUS
    )
    assert claude_worker.news.sources._BN_USDM_KEYS.index("contractType") == (
        claude_worker.news.detect._BN_CONTRACT
    )
    assert claude_worker.news.sources._BN_USDM_KEYS.index("onboardDate") == (
        claude_worker.news.detect._BN_ONBOARD
    )
    assert claude_worker.news.sources._BN_USDM_KEYS.index("deliveryDate") == (
        claude_worker.news.detect._BN_DELIVERY
    )
    assert claude_worker.news.sources._OKX_INST_KEYS.index("state") == (
        claude_worker.news.detect._OKX_STATE
    )
    assert claude_worker.news.sources._OKX_INST_KEYS.index("listTime") == (
        claude_worker.news.detect._OKX_LIST
    )
    assert claude_worker.news.sources._OKX_INST_KEYS.index("expTime") == (
        claude_worker.news.detect._OKX_EXP
    )
    assert claude_worker.news.sources._DERIBIT_INST_KEYS.index("is_active") == (
        claude_worker.news.detect._DERIBIT_ACTIVE
    )
    assert claude_worker.news.sources._DERIBIT_INST_KEYS.index("expiration_timestamp") == (
        claude_worker.news.detect._DERIBIT_EXPIRES
    )
    assert claude_worker.news.sources._COINBASE_KEYS.index("status") == (
        claude_worker.news.detect._CB_STATUS
    )


def test_the_window_field_order_matches_the_status_parsers() -> None:
    """The same pin for the two window venues, read behaviourally: the
    recorded payload's `begin`/`end` must land on the indices the detector
    reads them from."""
    for kind, venue in (("status-okx", "okx"), ("status-bybit", "bybit")):
        snapshot = _recorded(_source(f"{venue}-status", kind, venue))
        body = json.loads(snapshot.body)
        row = body[sorted(body)[0]]
        assert int(row[claude_worker.news.detect._WINDOW_BEGIN]) == WINDOW_BEGIN * 1_000
        assert int(row[claude_worker.news.detect._WINDOW_END]) == WINDOW_END * 1_000


# ---- §8.1 the instrument-set diff ----------------------------------------


def test_first_poll_emits_nothing() -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    cur = _recorded(source)
    assert json.loads(cur.body), "the fixture must carry instruments"
    assert claude_worker.news.detect.diff_instruments(source, None, cur, NOW) == []


def test_absent_to_pending_listing_pending() -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP"))
    listing = NOW + 2 * HOUR_S

    def add(body: dict[str, object]) -> None:
        _only(body, "BTC-USD-SWAP")
        body["NEW-USDT-SWAP"] = ["SWAP", "preopen", str(listing * 1_000), "", "", ""]

    events = claude_worker.news.detect.diff_instruments(source, prev, _edit(recorded, add), NOW)
    assert len(events) == 1
    event = events[0]
    assert event.kind == claude_worker.news.detect.EVENT_LISTING_PENDING
    assert event.instrument == "NEW-USDT-SWAP"
    assert event.descriptor == "okx:NEW-USDT-SWAP"
    assert event.venue == "okx"
    # The listing's own time, not the poll's: that is what makes the event
    # worth having before it happens.
    assert event.at_ts == listing
    assert (event.section, event.value) == ("instruments", "NEW-USDT-SWAP")


def test_pending_to_trading_listing_live() -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)

    def pending(body: dict[str, object]) -> None:
        _only(body)
        body["NEW-USDT-SWAP"] = ["SWAP", "preopen", str((NOW + HOUR_S) * 1_000), "", "", ""]

    def live(body: dict[str, object]) -> None:
        _only(body)
        body["NEW-USDT-SWAP"] = ["SWAP", "live", str((NOW - HOUR_S) * 1_000), "", "", ""]

    events = claude_worker.news.detect.diff_instruments(
        source, _edit(recorded, pending), _edit(recorded, live), NOW
    )
    assert _kinds(events) == [claude_worker.news.detect.EVENT_LISTING_LIVE]
    assert events[0].at_ts == NOW


def test_absent_to_trading_is_live_not_pending() -> None:
    source = _source("bn-usdm-instruments", "instruments-bn-usdm", "binance")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "ETHUSDT"))
    events = claude_worker.news.detect.diff_instruments(
        source, prev, _edit(recorded, lambda body: _only(body, "ETHUSDT", "BTCUSDT")), NOW
    )
    assert _kinds(events) == [claude_worker.news.detect.EVENT_LISTING_LIVE]
    assert events[0].descriptor == "binance-usdm:btcusdt"
    assert (events[0].section, events[0].value) == ("usdm", "btcusdt")


def test_trading_to_settling_delisting() -> None:
    source = _source("bn-usdm-instruments", "instruments-bn-usdm", "binance")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "BTCUSDT"))

    def settling(body: dict[str, object]) -> None:
        _only(body, "BTCUSDT")
        row = typing.cast(list[object], body["BTCUSDT"])
        row[claude_worker.news.detect._BN_STATUS] = "SETTLING"

    events = claude_worker.news.detect.diff_instruments(
        source, prev, _edit(recorded, settling), NOW
    )
    assert _kinds(events) == [claude_worker.news.detect.EVENT_DELISTING]
    # BTCUSDT is a PERPETUAL, whose deliveryDate is the year-2100 sentinel:
    # the delisting is dated NOW, never 2100.
    assert events[0].at_ts == NOW


def test_a_scheduled_delivery_dates_the_delisting() -> None:
    source = _source("bn-usdm-instruments", "instruments-bn-usdm", "binance")
    recorded = _recorded(source)
    delivery = NOW + 5 * 86_400

    def dated(status: str) -> claude_worker.news.sources.Snapshot:
        def mutate(body: dict[str, object]) -> None:
            _only(body)
            body["BTCUSDT_261225"] = [status, "CURRENT_QUARTER", 0, delivery * 1_000]

        return _edit(recorded, mutate)

    events = claude_worker.news.detect.diff_instruments(
        source, dated("TRADING"), dated("DELIVERING"), NOW
    )
    assert events[0].kind == claude_worker.news.detect.EVENT_DELISTING
    assert events[0].at_ts == delivery
    assert events[0].section == "usdm_dated"


def test_an_instrument_that_vanishes_is_a_delisting() -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP", "ETH-USD-SWAP"))
    cur = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP"))
    events = claude_worker.news.detect.diff_instruments(source, prev, cur, NOW)
    assert _kinds(events) == [claude_worker.news.detect.EVENT_DELISTING]
    assert events[0].instrument == "ETH-USD-SWAP"
    assert events[0].at_ts == NOW


def test_an_unreadable_current_body_emits_nothing() -> None:
    """A venue that reshapes its wire makes this QUIET, not busy: an empty
    decode must never read as "every instrument vanished"."""
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    broken = recorded._replace(body="not json")
    assert claude_worker.news.detect.diff_instruments(source, recorded, broken, NOW) == []


def test_coinbase_has_no_pending_state_and_no_universe_section() -> None:
    """The live products payload carries hundreds of `delisted` rows, so an
    appearing non-trading row is a dead row, never a pending listing — and
    coinbase has no ingress, so nothing it lists is a universe candidate."""
    source = _source("coinbase-products", "instruments-coinbase", "coinbase")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "HYPER-USD"))
    cur = _edit(recorded, lambda body: _only(body, "HYPER-USD", "TRAC-USDT"))
    assert claude_worker.news.detect.diff_instruments(source, prev, cur, NOW) == []

    def offline(body: dict[str, object]) -> None:
        _only(body, "HYPER-USD")
        body["HYPER-USD"] = ["delisted", True]

    events = claude_worker.news.detect.diff_instruments(
        source, _edit(recorded, lambda body: _only(body, "HYPER-USD")),
        _edit(recorded, offline), NOW
    )
    assert _kinds(events) == [claude_worker.news.detect.EVENT_DELISTING]
    assert events[0].section == ""
    assert events[0].descriptor == ""


def test_expiry_within_72h_once(tmp_path: pathlib.Path) -> None:
    """The Deribit fixture's own nearest dated future expires 18 h after
    `NOW`. The rule is about the CLOCK, not about a change, so it fires on
    an UNCHANGED pair — and the dedupe key records it exactly once."""
    source = _source("deribit-instruments-btc-future", "instruments-deribit", "deribit")
    recorded = _recorded(source)
    pair = _edit(recorded, lambda body: _only(body, "BTC-PERPETUAL", "BTC-20SEP26"))
    events = claude_worker.news.detect.diff_instruments(source, pair, pair, NOW)
    assert _kinds(events) == [claude_worker.news.detect.EVENT_EXPIRY]
    assert events[0].instrument == "BTC-20SEP26"
    assert events[0].at_ts == BTC_20SEP26_EXPIRY
    assert events[0].descriptor == "deribit:BTC-20SEP26"
    # BTC-PERPETUAL's year-3000 sentinel is not an expiry.
    with _store(tmp_path) as store:
        first, _ = claude_worker.news.detect.apply_events(store, events, NOW)
        second, _ = claude_worker.news.detect.apply_events(store, events, NOW + 300)
        assert len(first) == 1
        assert second == []
        assert len(store.events_since(0)) == 1


def test_a_new_option_strike_is_an_event_but_never_a_proposal(
    tmp_path: pathlib.Path,
) -> None:
    """OBSERVED LIVE 2026-09-19 18:07Z: Deribit listed BTC-23SEP26-84500-C
    and -P, the detector caught both, and both were proposed for
    `[deribit] instruments`. That is wrong twice over — an option reaches
    the engine through `options_underlyings` and a DISCOVERED chain, never
    by naming a strike, and Deribit adds strikes continuously as spot
    moves, so the operator's hand-applied file would grow every hour.

    The EVENT stays: the venue really did list something, the events tail
    should show it, and the expiry detector needs the row."""
    source = _source("deribit-instruments-btc-option", "instruments-deribit", "deribit")
    recorded = _recorded(
        _source("deribit-instruments-btc-future", "instruments-deribit", "deribit")
    )
    strike = "BTC-23SEP26-84500-C"

    def prev(body: dict[str, object]) -> None:
        _only(body, "BTC-PERPETUAL")

    def cur(body: dict[str, object]) -> None:
        _only(body, "BTC-PERPETUAL")
        body[strike] = ["option", "reversed", True, (NOW - HOUR_S) * 1_000, 0]

    events = claude_worker.news.detect.diff_instruments(
        source, _edit(recorded, prev), _edit(recorded, cur), NOW
    )
    assert _kinds(events) == [claude_worker.news.detect.EVENT_LISTING_LIVE]
    assert events[0].instrument == strike
    assert events[0].descriptor == f"deribit:{strike}", "the descriptor survives"
    assert events[0].section == "" and events[0].value == "", "but not as a candidate"

    store = _store(tmp_path)
    ctx = _ctx(tmp_path)
    try:
        stored, _ = claude_worker.news.detect.apply_events(store, events, NOW)
        assert len(stored) == 1, "the event is recorded"
        assert claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW) == 0
    finally:
        store.close()
    assert not ctx.file(claude_worker.news.UNIVERSE_PROPOSALS_FILE).exists()
    # A future on the same venue still proposes normally.
    assert claude_worker.news.detect.is_option_name("BTC-23SEP26-84500-C") is True
    assert claude_worker.news.detect.is_option_name("BTC-20SEP26") is False
    assert claude_worker.news.detect.is_option_name("BTC-PERPETUAL") is False
    assert claude_worker.news.detect.proposal_from_descriptor(f"deribit:{strike}") is None
    assert claude_worker.news.detect.proposal_from_descriptor("deribit:BTC-20SEP26") is not None


def test_an_expiry_beyond_the_horizon_is_not_announced() -> None:
    source = _source("deribit-instruments-btc-future", "instruments-deribit", "deribit")
    recorded = _recorded(source)
    pair = _edit(recorded, lambda body: _only(body, "BTC-PERPETUAL"))
    assert claude_worker.news.detect.diff_instruments(source, pair, pair, NOW) == []


# ---- the snapshot store rule (§8.1) --------------------------------------


def test_an_unchanged_snapshot_is_not_stored_again(tmp_path: pathlib.Path) -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    cur = _recorded(source)
    with _store(tmp_path) as store:
        first = claude_worker.news.detect.observe(store, source, cur, NOW)
        second = claude_worker.news.detect.observe(store, source, cur, NOW + 300)
        assert first.snapshot_stored is True
        assert second.snapshot_stored is False
        rows = store._rows("SELECT * FROM snapshots", ())
        assert len(rows) == 1
        # ... and the first poll still emitted nothing.
        assert first.events == 0 and second.events == 0


def test_a_changed_snapshot_is_stored_and_diffed(tmp_path: pathlib.Path) -> None:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    first = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP"))
    second = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP", "ETH-USD-SWAP"))
    with _store(tmp_path) as store:
        claude_worker.news.detect.observe(store, source, first, NOW)
        outcome = claude_worker.news.detect.observe(store, source, second, NOW + 300)
        assert outcome.snapshot_stored is True
        assert outcome.events == 1
        event = store.events_since(0)[0]
        assert event["kind"] == claude_worker.news.detect.EVENT_LISTING_LIVE
        assert event["instrument"] == "ETH-USD-SWAP"


# ---- §8.2 venue status ----------------------------------------------------


def test_status_okx_window_events() -> None:
    source = _source("okx-status", "status-okx", "okx")
    cur = _recorded(source)
    before = claude_worker.news.detect.detect_status(source, None, cur, WINDOW_BEGIN - HOUR_S)
    assert _kinds(before) == [claude_worker.news.detect.EVENT_MAINTENANCE_PLANNED]
    assert before[0].at_ts == WINDOW_BEGIN
    assert before[0].until_ts == WINDOW_END
    assert "Perpetual swap system upgrade" in before[0].detail
    inside = claude_worker.news.detect.detect_status(source, None, cur, WINDOW_NOW)
    assert _kinds(inside) == [
        claude_worker.news.detect.EVENT_MAINTENANCE_PLANNED,
        claude_worker.news.detect.EVENT_MAINTENANCE_LIVE,
    ]
    # Both are keyed on `begin`, so one window is one row however often the
    # status page is polled (spec §8.2).
    assert inside[1].at_ts == WINDOW_BEGIN


def test_status_bybit_ignores_a_completed_window() -> None:
    source = _source("bybit-status", "status-bybit", "bybit")
    cur = _recorded(source)
    assert _kinds(claude_worker.news.detect.detect_status(source, None, cur, WINDOW_NOW)) == [
        claude_worker.news.detect.EVENT_MAINTENANCE_PLANNED,
        claude_worker.news.detect.EVENT_MAINTENANCE_LIVE,
    ]

    def completed(body: dict[str, object]) -> None:
        for key in list(body):
            row = typing.cast(list[object], body[key])
            row[claude_worker.news.detect._WINDOW_STATE] = "completed"

    done = _edit(cur, completed)
    assert claude_worker.news.detect.detect_status(source, None, done, WINDOW_NOW) == []


def test_a_window_without_both_edges_is_not_an_event() -> None:
    source = _source("okx-status", "status-okx", "okx")

    def blank(body: dict[str, object]) -> None:
        for key in list(body):
            row = typing.cast(list[object], body[key])
            row[claude_worker.news.detect._WINDOW_END] = ""

    assert claude_worker.news.detect.detect_status(
        source, None, _edit(_recorded(source), blank), WINDOW_NOW
    ) == []


def test_status_deribit_locked_partial() -> None:
    source = _source("deribit-status", "status-deribit", "deribit")
    recorded = _recorded(source)
    clear = claude_worker.news.detect.detect_status(source, None, recorded, NOW)
    assert len(clear) == 1 and clear[0].close is True

    def locked(body: dict[str, object]) -> None:
        body["locked"] = "partial"
        body["locked_indices"] = ["btc_usd"]
        body["locked_currencies"] = ["BTC"]

    events = claude_worker.news.detect.detect_status(source, None, _edit(recorded, locked), NOW)
    assert _kinds(events) == [claude_worker.news.detect.EVENT_MAINTENANCE_LIVE]
    assert events[0].close is False
    # No `begin` to key on, so the row is open-ended and closed by update.
    assert events[0].until_ts == 0
    assert events[0].at_ts == NOW
    assert "partial" in events[0].detail and "btc_usd" in events[0].detail


def test_status_kraken_not_online() -> None:
    source = _source("kraken-status", "status-kraken", "kraken")
    recorded = _recorded(source)
    assert claude_worker.news.detect.detect_status(source, None, recorded, NOW)[0].close is True
    down = _edit(recorded, lambda body: body.update({"status": "maintenance"}))
    events = claude_worker.news.detect.detect_status(source, None, down, NOW)
    assert _kinds(events) == [claude_worker.news.detect.EVENT_MAINTENANCE_LIVE]
    assert events[0].detail == "status=maintenance"


def test_an_unreadable_status_body_is_not_an_outage() -> None:
    source = _source("deribit-status", "status-deribit", "deribit")
    broken = _recorded(source)._replace(body="{{{")
    events = claude_worker.news.detect.detect_status(source, None, broken, NOW)
    assert len(events) == 1 and events[0].close is True


def test_maintenance_live_closes(tmp_path: pathlib.Path) -> None:
    """One outage, one row: the open is skipped while a row is already
    open, and the venue's all-clear closes it by UPDATE."""
    source = _source("deribit-status", "status-deribit", "deribit")
    recorded = _recorded(source)
    locked = _edit(recorded, lambda body: body.update({"locked": "true"}))
    with _store(tmp_path) as store:
        claude_worker.news.detect.observe(store, source, locked, NOW)
        again = claude_worker.news.detect.observe(store, source, locked, NOW + 60)
        assert again.events == 0, "a continuing outage must not open a second row"
        assert len(store.events_since(0)) == 1
        cleared = claude_worker.news.detect.observe(store, source, recorded, NOW + 120)
        assert cleared.closed == 1
        rows = store.events_since(0)
        assert len(rows) == 1
        assert int(typing.cast(int, rows[0]["until_ts"])) == NOW + 120
        # An all-clear with nothing open is a no-op, not an error.
        assert claude_worker.news.detect.observe(store, source, recorded, NOW + 180).closed == 0


# ---- §4.4 the proposals files (ruling Q10) --------------------------------


def _listings(
    tmp_path: pathlib.Path, *idents: str
) -> tuple[claude_worker.news.store.Store, list[tuple[int, claude_worker.news.detect.Event]]]:
    """A store carrying one `listing_live` per ident, through the real
    detector and the real event writer."""
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    prev = _edit(recorded, lambda body: _only(body, "BTC-USD-SWAP"))

    def cur(body: dict[str, object]) -> None:
        _only(body, "BTC-USD-SWAP")
        for i in range(len(idents)):
            body[idents[i]] = ["SWAP", "live", str((NOW - HOUR_S) * 1_000), "", "", ""]

    store = _store(tmp_path)
    events = claude_worker.news.detect.diff_instruments(source, prev, _edit(recorded, cur), NOW)
    stored, _ = claude_worker.news.detect.apply_events(store, events, NOW)
    return store, stored


def test_universe_proposal_written_once_and_skips_present_descriptor(
    tmp_path: pathlib.Path,
) -> None:
    store, stored = _listings(tmp_path, "NEW-USDT-SWAP", "HAVE-USDT-SWAP")
    ctx = _ctx(tmp_path, manifest=frozenset({"okx:HAVE-USDT-SWAP"}))
    try:
        assert len(stored) == 2
        assert claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW) == 1
        # Append-only and idempotent: a second cycle over the same events
        # must not grow the file.
        assert claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW + 60) == 0
    finally:
        store.close()
    path = ctx.file(claude_worker.news.UNIVERSE_PROPOSALS_FILE)
    text = path.read_text(encoding="utf-8")
    assert text.count("[[proposal]]") == 1
    assert claude_worker.news.detect.proposed_descriptors(text) == frozenset(
        {"okx:NEW-USDT-SWAP"}
    )
    doc = tomllib.loads(text)
    row = typing.cast(list[dict[str, object]], doc["proposal"])[0]
    assert row["venue"] == "okx"
    assert row["section"] == "instruments"
    assert row["value"] == "NEW-USDT-SWAP"
    assert row["applied"] == 0
    assert int(typing.cast(int, row["event_id"])) > 0


def test_a_hand_mangled_proposals_file_still_blocks_a_duplicate(
    tmp_path: pathlib.Path,
) -> None:
    """The file is the operator's to edit. If it stops being TOML, the
    duplicate check falls back to a line scan — appending the same stanza
    every 60 s is the one outcome that must not happen."""
    store, stored = _listings(tmp_path, "NEW-USDT-SWAP")
    ctx = _ctx(tmp_path)
    try:
        claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW)
        path = ctx.file(claude_worker.news.UNIVERSE_PROPOSALS_FILE)
        path.write_text(path.read_text(encoding="utf-8") + "\nnot = toml = at all\n")
        assert claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW + 60) == 0
    finally:
        store.close()


def _delistings(
    tmp_path: pathlib.Path,
) -> tuple[claude_worker.news.store.Store, list[tuple[int, claude_worker.news.detect.Event]]]:
    source = _source("okx-instruments-swap", "instruments-okx", "okx")
    recorded = _recorded(source)
    prev = _edit(
        recorded, lambda body: _only(body, "BTC-USD-SWAP", "ETH-USD-SWAP", "SOL-USD-SWAP")
    )
    # SOL stays: an EMPTY body is how an unreadable payload decodes, and the
    # detector reads that as "no information", never as a venue-wide wipe.
    cur = _edit(recorded, lambda body: _only(body, "SOL-USD-SWAP"))
    store = _store(tmp_path)
    events = claude_worker.news.detect.diff_instruments(source, prev, cur, NOW)
    stored, _ = claude_worker.news.detect.apply_events(store, events, NOW)
    return store, stored


def test_xsd_drop_proposal_only_for_table_members(tmp_path: pathlib.Path) -> None:
    table = tmp_path / "xsd-table.tsv"
    table.write_text(
        "# target\tpartner\tbeta_1e9\trank\tt_adf_1e6\n"
        "okx:ETH-USD-SWAP\tbinance-usdm:ethusdt\t1000000000\t1\t-4200000\n",
        encoding="utf-8",
    )
    store, stored = _delistings(tmp_path)
    ctx = _ctx(tmp_path)
    try:
        assert len(stored) == 2, "both instruments vanished"
        rows, alerts = claude_worker.news.detect.write_xsd_proposals(ctx, stored, NOW)
        assert rows == 1, "BTC-USD-SWAP is not in the table"
        assert len(alerts) == 1
        assert alerts[0].kind == claude_worker.news.detect.ALERT_XSD_DROP
        assert "okx:ETH-USD-SWAP" in alerts[0].text and "delists" in alerts[0].text
        # Idempotent, like every other file this lane appends to.
        assert claude_worker.news.detect.write_xsd_proposals(ctx, stored, NOW + 60) == (0, [])
    finally:
        store.close()
    text = ctx.file(claude_worker.news.XSD_PROPOSALS_FILE).read_text(encoding="utf-8")
    lines = text.strip().splitlines()
    assert lines[0].startswith("# ts\taction\ttarget")
    fields = lines[1].split("\t")
    assert fields[1] == "drop"
    assert fields[2] == "okx:ETH-USD-SWAP"
    assert int(fields[3]) == NOW


def test_no_xsd_table_means_no_proposal(tmp_path: pathlib.Path) -> None:
    store, stored = _delistings(tmp_path)
    try:
        assert claude_worker.news.detect.write_xsd_proposals(_ctx(tmp_path), stored, NOW) == (
            0,
            [],
        )
    finally:
        store.close()
    assert not (tmp_path / "worker" / "news" / claude_worker.news.XSD_PROPOSALS_FILE).exists()


# ---- §8.2 the red rule ----------------------------------------------------


def _outage(store: claude_worker.news.store.Store, venue: str, at_ts: int) -> None:
    store.insert_event(
        kind=claude_worker.news.detect.EVENT_MAINTENANCE_LIVE,
        venue=venue,
        at_ts=at_ts,
        source=f"{venue}-status",
        detail="locked=true",
        created_ts=at_ts,
    )


def test_member_enabled_on_maintenance_red_rule(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        _outage(store, "deribit", NOW - HOUR_S)
        # slot 1 is vrp on Deribit; slot 3 is bin15 on Hyperliquid.
        alerts = claude_worker.news.detect.maintenance_alerts(store, (1 << 1) | (1 << 3), NOW)
        assert len(alerts) == 1
        assert alerts[0].kind == claude_worker.news.detect.ALERT_VENUE_MAINTENANCE
        assert "slot 1" in alerts[0].text and "deribit" in alerts[0].text
        assert claude_worker.news.detect.maintenance_alerts(store, 1 << 3, NOW) == []
        # An unreachable engine raises nothing: the premise could not be read.
        assert claude_worker.news.detect.maintenance_alerts(store, None, NOW) == []


def test_a_closed_window_is_no_longer_in_force(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        _outage(store, "binance", NOW - HOUR_S)
        assert claude_worker.news.detect.maintenance_alerts(store, 1 << 2, NOW)
        rows = store.open_events(
            claude_worker.news.detect.EVENT_MAINTENANCE_LIVE, "binance"
        )
        store.close_event(int(typing.cast(int, rows[0]["id"])), NOW - 60)
        assert claude_worker.news.detect.maintenance_alerts(store, 1 << 2, NOW) == []


def test_the_metrics_gauge_is_injected_and_never_reached_without_a_url() -> None:
    def explode(url: str, name: str) -> int | None:
        raise AssertionError(f"the engine must not be read: {url} {name}")

    assert claude_worker.news.detect.engine_mask(
        claude_worker.news.detect.Context(
            news_dir=pathlib.Path("/nonexistent"),
            xsd_table_path=pathlib.Path("/nonexistent"),
            metrics_url="",
            gauge=explode,
        )
    ) is None

    seen: list[str] = []

    def gauge(url: str, name: str) -> int | None:
        seen.append(name)
        return 0b1010

    mask = claude_worker.news.detect.engine_mask(
        claude_worker.news.detect.Context(
            news_dir=pathlib.Path("/nonexistent"),
            xsd_table_path=pathlib.Path("/nonexistent"),
            metrics_url="http://127.0.0.1:9191/metrics",
            gauge=gauge,
        )
    )
    assert mask == 0b1010
    assert seen == [claude_worker.news.detect.ENABLED_MASK_GAUGE]


# ---- §8.3 the calendar ----------------------------------------------------


_FED = _source("fed-calendar", "calendar-fed", "")


def _registry(
    *,
    bls: tuple[str, ...] = (),
    daily: tuple[claude_worker.news.sources.FixedDaily, ...] = (),
    sources: tuple[claude_worker.news.sources.Source, ...] = (),
) -> claude_worker.news.sources.Registry:
    return claude_worker.news.sources.Registry(
        settings=claude_worker.news.sources.NewsSettings(),
        keywords=(),
        calendar=claude_worker.news.sources.Calendar(bls_releases=bls, fixed_daily=daily),
        sources=sources,
    )


def _fed_snapshot() -> claude_worker.news.sources.Snapshot:
    path = _FIXTURES / "calendar-fed.json"
    parsed = claude_worker.news.sources.parse(_FED, path.read_text(encoding="utf-8"), NOW)
    assert parsed.snapshot is not None
    return parsed.snapshot


def test_calendar_next_7_days_sorted(tmp_path: pathlib.Path) -> None:
    """The RECORDED Fed calendar carries two events inside the week after
    `NOW` (2026-09-22 and 2026-09-23) and several outside it. Only the two
    appear, and the document is sorted."""
    with _store(tmp_path) as store:
        store.insert_snapshot(*_fed_snapshot())
        doc = claude_worker.news.detect.build_calendar(
            _registry(sources=(_FED,)), store, NOW
        )
    assert doc["v"] == claude_worker.news.SCHEMA_VERSION
    assert doc["generated_ts"] == NOW
    rows = typing.cast(list[dict[str, object]], doc["events"])
    assert len(rows) == 2
    stamps: list[int] = []
    for i in range(len(rows)):
        stamps.append(int(typing.cast(int, rows[i]["at_ts"])))
        assert rows[i]["kind"] == claude_worker.news.detect.CAL_FED_SPEECH
        assert rows[i]["source"] == "fed-calendar"
        assert NOW <= stamps[i] <= NOW + claude_worker.news.detect.CALENDAR_HORIZON_S
    assert stamps == sorted(stamps)


def test_calendar_classifies_fomc_and_takes_the_second_meeting_day(
    tmp_path: pathlib.Path,
) -> None:
    """A two-day FOMC meeting decides on its SECOND day, which is the
    instant anything trading around it cares about."""
    def add(body: dict[str, object]) -> None:
        body["2026-09-22-23T2:00 p.m.|FOMC"] = {
            "month": "2026-09",
            "days": "22-23",
            "time": "2:00 p.m.",
            "title": "FOMC Meeting",
            "type": "FOMC Meeting",
        }
        body["2026-09-24T2:00 p.m.|Minutes"] = {
            "month": "2026-09",
            "days": "24",
            "time": "2:00 p.m.",
            "title": "Minutes of the FOMC Meeting",
            "type": "Minutes",
        }

    with _store(tmp_path) as store:
        store.insert_snapshot(*_edit(_fed_snapshot(), add))
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(
                _registry(sources=(_FED,)), store, NOW
            )["events"],
        )
    by_kind: dict[str, int] = {}
    for i in range(len(rows)):
        kind = str(rows[i]["kind"])
        by_kind[kind] = int(typing.cast(int, rows[i]["at_ts"]))
    # 2026-09-23 14:00 ET = 2026-09-23T18:00:00Z (EDT), the meeting's
    # SECOND day — not its first.
    assert by_kind[claude_worker.news.detect.CAL_FOMC_STATEMENT] == 1_790_186_400
    assert claude_worker.news.detect.CAL_FOMC_MINUTES in by_kind


def test_calendar_drops_the_rows_section_8_3_does_not_name(tmp_path: pathlib.Path) -> None:
    """MEASURED 2026-09-19: the live document is 2585 events and only 645 of
    them are speeches or testimony — the rest are statistical releases
    (`Stat`, `events`), the Beige Book, board meetings and federal
    holidays. §8.3 names FOMC statement / minutes / press conference /
    speech, so the others are not entries on this calendar."""
    def add(body: dict[str, object]) -> None:
        for i, (kind, title) in enumerate(
            (
                ("Stat", "H.6 - Money Stock Measures"),
                ("events", "G.19 - Consumer Credit"),
                ("Beige", "Beige Book"),
                ("Other", "Holiday - Columbus Day"),
                ("Board", "Board Meeting"),
            )
        ):
            body[f"noise-{i}"] = {
                "month": "2026-09",
                "days": "22",
                "time": "2:00 p.m.",
                "title": title,
                "type": kind,
            }

    with _store(tmp_path) as store:
        store.insert_snapshot(*_edit(_fed_snapshot(), add))
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(
                _registry(sources=(_FED,)), store, NOW
            )["events"],
        )
    assert len(rows) == 2, "only the two recorded Speeches survive"
    assert claude_worker.news.detect.fed_kind({"type": "Stat", "title": "H.6"}) == ""
    assert claude_worker.news.detect.fed_kind({"type": "Testimony", "title": "x"}) == (
        claude_worker.news.detect.CAL_FED_SPEECH
    )
    # The live vocabulary, measured: `FOMC` types and the press conference.
    assert claude_worker.news.detect.fed_kind(
        {"type": "FOMC", "title": "FOMC Press Conference"}
    ) == claude_worker.news.detect.CAL_FOMC_STATEMENT
    assert claude_worker.news.detect.fed_kind({"type": "FOMC", "title": "FOMC Minutes"}) == (
        claude_worker.news.detect.CAL_FOMC_MINUTES
    )


def _fed_row(days: str, month: str, title: str, kind: str) -> dict[str, object]:
    return {"month": month, "days": days, "time": "2:00 p.m.", "title": title, "type": kind}


def test_calendar_always_carries_the_next_fomc(tmp_path: pathlib.Path) -> None:
    """Operator ruling 2026-09-20. MEASURED 2026-09-19: the next FOMC
    minutes are 18 days out and the next meeting 39, both past the 7-day
    horizon — so the one scheduled event that moves vol would never appear
    on a calendar that showed only the week ahead. Each PINNED kind carries
    its next occurrence however far out it is; everything else still obeys
    the horizon."""
    def add(body: dict[str, object]) -> None:
        body["far-minutes"] = _fed_row("7", "2026-10", "FOMC Minutes", "FOMC")
        body["far-meeting"] = _fed_row("28", "2026-10", "FOMC Meeting", "FOMC")
        body["later-meeting"] = _fed_row("9", "2026-12", "FOMC Meeting", "FOMC")
        body["far-speech"] = _fed_row("20", "2026-10", "Speech - Governor", "Speeches")

    with _store(tmp_path) as store:
        store.insert_snapshot(*_edit(_fed_snapshot(), add))
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(
                _registry(sources=(_FED,)), store, NOW
            )["events"],
        )
    kinds: list[str] = []
    for i in range(len(rows)):
        kinds.append(str(rows[i]["kind"]))
    # The two recorded in-window speeches, plus ONE of each pinned kind.
    assert kinds.count(claude_worker.news.detect.CAL_FED_SPEECH) == 2, "the far speech is out"
    assert kinds.count(claude_worker.news.detect.CAL_FOMC_MINUTES) == 1
    assert kinds.count(claude_worker.news.detect.CAL_FOMC_STATEMENT) == 1, "the NEXT one only"
    by_kind: dict[str, int] = {}
    for i in range(len(rows)):
        by_kind.setdefault(str(rows[i]["kind"]), int(typing.cast(int, rows[i]["at_ts"])))
    # 2026-10-28 14:00 ET, not the December meeting.
    assert by_kind[claude_worker.news.detect.CAL_FOMC_STATEMENT] == 1_793_210_400
    assert by_kind[claude_worker.news.detect.CAL_FOMC_MINUTES] == 1_791_396_000
    stamps: list[int] = []
    for i in range(len(rows)):
        stamps.append(int(typing.cast(int, rows[i]["at_ts"])))
    assert stamps == sorted(stamps), "pinned rows are sorted in with the rest"


def test_an_fomc_inside_the_horizon_is_not_carried_twice(tmp_path: pathlib.Path) -> None:
    def add(body: dict[str, object]) -> None:
        body["near-meeting"] = _fed_row("23", "2026-09", "FOMC Meeting", "FOMC")
        body["far-meeting"] = _fed_row("28", "2026-10", "FOMC Meeting", "FOMC")

    with _store(tmp_path) as store:
        store.insert_snapshot(*_edit(_fed_snapshot(), add))
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(
                _registry(sources=(_FED,)), store, NOW
            )["events"],
        )
    statements: list[int] = []
    for i in range(len(rows)):
        if rows[i]["kind"] == claude_worker.news.detect.CAL_FOMC_STATEMENT:
            statements.append(int(typing.cast(int, rows[i]["at_ts"])))
    assert len(statements) == 1
    assert statements[0] < NOW + claude_worker.news.detect.CALENDAR_HORIZON_S


def test_calendar_bls_static_and_fixed_daily(tmp_path: pathlib.Path) -> None:
    registry = _registry(
        bls=(
            "2026-09-22T12:30:00Z CPI (August)",
            "2026-09-23T12:30:00Z",
            "2020-01-01T12:30:00Z",
            "not a date",
        ),
        daily=(
            claude_worker.news.sources.FixedDaily(kind="pm_daily_resolve", at="16:00"),
            claude_worker.news.sources.FixedDaily(kind="engine_restart", at="00:10"),
            claude_worker.news.sources.FixedDaily(kind="broken", at="25:00"),
        ),
    )
    with _store(tmp_path) as store:
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(registry, store, NOW)["events"],
        )
    kinds: list[str] = []
    for i in range(len(rows)):
        kinds.append(str(rows[i]["kind"]))
    assert kinds.count(claude_worker.news.detect.CAL_BLS_RELEASE) == 2, "past + junk dropped"
    details: list[str] = []
    for i in range(len(rows)):
        if rows[i]["kind"] == claude_worker.news.detect.CAL_BLS_RELEASE:
            details.append(str(rows[i]["detail"]))
    # The optional label becomes the detail; a bare stamp is its own.
    assert details == ["CPI (August)", "2026-09-23T12:30:00Z"]
    # Seven days of a daily event, from the next occurrence to the horizon.
    assert kinds.count("pm_daily_resolve") == 7
    assert kinds.count("engine_restart") == 7
    assert "broken" not in kinds


def test_calendar_carries_expiries_and_open_maintenance(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        store.insert_event(
            kind=claude_worker.news.detect.EVENT_EXPIRY,
            venue="deribit",
            at_ts=BTC_20SEP26_EXPIRY,
            source="deribit-instruments-btc-future",
            detail="active future",
            created_ts=NOW,
            instrument="BTC-20SEP26",
        )
        store.insert_event(
            kind=claude_worker.news.detect.EVENT_MAINTENANCE_PLANNED,
            venue="okx",
            at_ts=NOW + HOUR_S,
            until_ts=NOW + 2 * HOUR_S,
            source="okx-status",
            detail="Perpetual swap system upgrade",
            created_ts=NOW,
        )
        store.insert_event(
            kind=claude_worker.news.detect.EVENT_MAINTENANCE_PLANNED,
            venue="bybit",
            at_ts=NOW - 3 * HOUR_S,
            until_ts=NOW - 2 * HOUR_S,
            source="bybit-status",
            detail="over already",
            created_ts=NOW,
        )
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(_registry(), store, NOW)["events"],
        )
    kinds: list[str] = []
    for i in range(len(rows)):
        kinds.append(str(rows[i]["kind"]))
    assert kinds.count(claude_worker.news.detect.CAL_DERIBIT_EXPIRY) == 1
    assert kinds.count(claude_worker.news.detect.CAL_VENUE_MAINTENANCE) == 1, "closed one dropped"


def test_calendar_collapses_same_instant_option_expiries(tmp_path: pathlib.Path) -> None:
    """MEASURED 2026-09-19: one poll of the live BTC option chain (972
    instruments) records 190 expiry events in THREE instants. Each is a
    real event row — the xsd path keys on the instrument — but the calendar
    carries one entry per instant, naming the count."""
    with _store(tmp_path) as store:
        for i in range(8):
            store.insert_event(
                kind=claude_worker.news.detect.EVENT_EXPIRY,
                venue="deribit",
                at_ts=BTC_20SEP26_EXPIRY,
                source="deribit-instruments-btc-option",
                detail="active option",
                created_ts=NOW,
                instrument=f"BTC-20SEP26-{100000 + i}-C",
            )
        store.insert_event(
            kind=claude_worker.news.detect.EVENT_EXPIRY,
            venue="deribit",
            at_ts=BTC_20SEP26_EXPIRY + 86_400,
            source="deribit-instruments-any-future",
            detail="active future",
            created_ts=NOW,
            instrument="BTC-21SEP26",
        )
        assert len(store.events_since(0)) == 9
        rows = typing.cast(
            list[dict[str, object]],
            claude_worker.news.detect.build_calendar(_registry(), store, NOW)["events"],
        )
    assert len(rows) == 2
    assert str(rows[0]["detail"]).startswith("8 instruments (BTC-20SEP26-100000-C")
    assert "+5 more" in str(rows[0]["detail"])
    assert rows[1]["detail"] == "BTC-21SEP26"


# ---- the ALERT file and the once-per-cycle tail ---------------------------


def test_the_alert_file_carries_the_newest_line_and_expires(tmp_path: pathlib.Path) -> None:
    path = tmp_path / claude_worker.news.ALERT_FILE
    alert = claude_worker.news.detect.Alert("xsd_drop", "xsd target okx:X delists", NOW)
    assert claude_worker.news.detect.write_alert(path, [alert], NOW) == 1
    line = path.read_text(encoding="utf-8").strip()
    assert line == f"{claude_worker.news.detect.iso(NOW)} xsd_drop xsd target okx:X delists"
    # Still inside the 6 h window: kept.
    claude_worker.news.detect.write_alert(
        path, [], NOW + claude_worker.news.detect.ALERT_TTL_S - 1
    )
    assert path.is_file()
    claude_worker.news.detect.write_alert(path, [], NOW + claude_worker.news.detect.ALERT_TTL_S)
    assert not path.is_file()


def test_a_foreign_alert_line_is_left_alone(tmp_path: pathlib.Path) -> None:
    path = tmp_path / claude_worker.news.ALERT_FILE
    path.write_text("something the operator wrote\n", encoding="utf-8")
    long_after = NOW + 10 * claude_worker.news.detect.ALERT_TTL_S
    claude_worker.news.detect.write_alert(path, [], long_after)
    assert path.is_file()


def test_finalize_writes_the_calendar_and_the_red_alert(tmp_path: pathlib.Path) -> None:
    ctx = _ctx(
        tmp_path,
        metrics_url="http://127.0.0.1:9191/metrics",
        gauge=lambda url, name: 1 << 1,
    )
    with _store(tmp_path) as store:
        store.insert_snapshot(*_fed_snapshot())
        _outage(store, "deribit", NOW - HOUR_S)
        out = claude_worker.news.detect.finalize(
            store, _registry(sources=(_FED,)), NOW, [], ctx
        )
    assert out.calendar_events == 2
    assert out.alerts == 1
    doc = json.loads(ctx.file(claude_worker.news.CALENDAR_FILE).read_text(encoding="utf-8"))
    assert doc["v"] == claude_worker.news.SCHEMA_VERSION
    assert len(doc["events"]) == 2
    assert "deribit is in maintenance" in ctx.file(claude_worker.news.ALERT_FILE).read_text(
        encoding="utf-8"
    )


def test_finalize_without_a_context_writes_nothing(tmp_path: pathlib.Path) -> None:
    with _store(tmp_path) as store:
        out = claude_worker.news.detect.finalize(store, _registry(), NOW, [])
    assert out.calendar_events == 0
    assert not (tmp_path / "worker" / "news" / claude_worker.news.CALENDAR_FILE).exists()


def test_an_unwritable_news_dir_does_not_break_a_cycle(tmp_path: pathlib.Path) -> None:
    """Every file output is best-effort: a cycle that recorded its events
    has done the work that matters."""
    blocked = tmp_path / "blocked"
    blocked.write_text("not a directory", encoding="utf-8")
    ctx = _ctx(tmp_path, news_dir=blocked)
    store, stored = _listings(tmp_path, "NEW-USDT-SWAP")
    try:
        assert claude_worker.news.detect.write_universe_proposals(ctx, stored, NOW) == 0
        out = claude_worker.news.detect.finalize(store, _registry(), NOW, [], ctx)
        assert out.calendar_events == 0
    finally:
        store.close()


def test_observe_ignores_a_source_without_a_detector(tmp_path: pathlib.Path) -> None:
    source = _source("binance-ping", "ping-binance", "binance")
    snapshot = _recorded(source)
    with _store(tmp_path) as store:
        out = claude_worker.news.detect.observe(store, source, snapshot, NOW, _ctx(tmp_path))
        assert out.snapshot_stored is True
        assert out.events == 0
        assert store.events_since(0) == []
    assert claude_worker.news.detect.detectors_for("ping-binance") is False
    assert claude_worker.news.detect.detectors_for("instruments-okx") is True
