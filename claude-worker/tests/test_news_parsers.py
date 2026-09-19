# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §5.2 — one parser per ``kind``, against recorded fixtures.

Fixture law (Appendix B): every kind has a fixture recorded by
``python -m claude_worker.news probe --source <name> --record``, reduced to
the KEYED fields only and never carrying a raw payload. A parser without a
fixture does not merge — [`test_every_kind_has_a_fixture`] is that law.

The other half of the law is the failure doctrine: ``parse`` NEVER raises.
A venue that reshapes its wire must make this lane QUIET, not broken, so
every kind is fed garbage, a truncated payload, and every OTHER kind's
fixture, and must answer with an empty ``Parsed`` and no exception.

``status-okx`` and ``status-bybit`` carry hand-written SYNTHETIC fixtures:
the live endpoints answer an empty maintenance list outside a real window,
so the populated mapping cannot be recorded until one happens. The
``SYNTHETIC`` marker file names them and [`test_synthetic_fixtures_are_declared`]
keeps that list honest.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib

import pytest

import claude_worker.news.sources

_FIXTURES: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "news"
_SYNTHETIC_MARKER: pathlib.Path = _FIXTURES / "SYNTHETIC"
#: Kinds whose fixture could not be RECORDED (Appendix B), each hand-written
#: in the endpoint's own documented shape and replaced at the first real
#: observation: ``status-okx``/``status-bybit`` answer an empty maintenance
#: list outside a real window, ``arxiv-api`` answered 406 to every User-Agent
#: tried from this network, and ``gdelt`` alternated a 429 rate-limit notice
#: with a bare ``{}`` across 14 spaced attempts (all measured 2026-09-19).
_EXPECTED_SYNTHETIC: frozenset[str] = frozenset(
    ("status-okx", "status-bybit", "arxiv-api", "gdelt")
)

#: What a healthy parse of each kind produces (spec §5.2).
_SHAPE: dict[str, str] = {
    "rss": "items",
    "google-news": "items",
    "hnrss": "items",
    "reddit-rss": "items",
    "arxiv-api": "items",
    "gdelt": "items",
    "json-okx-ann": "items",
    "json-deribit-ann": "items",
    "json-bybit-ann": "items",
    "json-binance-cms": "items",
    "json-4chan-catalog": "items",
    "instruments-bn-usdm": "snapshot",
    "instruments-okx": "snapshot",
    "instruments-deribit": "snapshot",
    "instruments-coinbase": "snapshot",
    "status-okx": "snapshot",
    "status-deribit": "snapshot",
    "status-bybit": "snapshot",
    "status-kraken": "snapshot",
    "ping-binance": "snapshot",
    "calendar-fed": "snapshot",
    "series-bn-ls": "series",
    "series-bn-top-ls": "series",
    "series-bn-taker": "series",
    "series-bn-oi": "series",
    "series-dvol": "series",
    "series-cftc-cot": "series",
    "series-fng": "series",
    "series-defillama-stables": "series",
    "series-mempool-fees": "series",
    "series-blockchain-stats": "series",
    "series-kalshi": "series",
    "series-manifold": "series",
}

#: The ``series.key`` each class-D kind writes.
_SERIES_KEY: dict[str, str] = {
    "series-bn-ls": "longShortRatio",
    "series-bn-top-ls": "longShortRatio",
    "series-bn-taker": "buySellRatio",
    "series-bn-oi": "sumOpenInterestValue",
    "series-dvol": "dvol",
    "series-cftc-cot": "noncomm_net",
    "series-fng": "fng",
    "series-defillama-stables": "pegged_usd",
    "series-mempool-fees": "fastestFee",
    "series-blockchain-stats": "hash_rate",
    "series-kalshi": "market_count",
    "series-manifold": "market_count",
}

#: Payloads no kind may ever accept.
_GARBAGE: tuple[str, ...] = ("<<< not a payload >>>", "", "null", "3", '{"unexpected": 1}')

NOW: int = 1_758_290_000


def _class_for(kind: str) -> str:
    if kind.startswith(("instruments-", "status-", "ping-", "calendar-")):
        return "A"
    if kind.startswith("json-") and kind.endswith(("-ann", "-cms")):
        return "B"
    if kind.startswith("series-"):
        return "D"
    return "C"


def _source_for(kind: str, **over: object) -> claude_worker.news.sources.Source:
    fields: dict[str, object] = {
        "name": kind,
        "kind": kind,
        "url": "https://probe.example/x",
        "origin": "probe.example",
        "class_": _class_for(kind),
        "query": "btc",
    }
    fields.update(over)
    return claude_worker.news.sources.Source(**fields)  # type: ignore[arg-type]


def _fixture_path(kind: str) -> pathlib.Path:
    return _FIXTURES / f"{kind}{claude_worker.news.sources.fixture_suffix(kind)}"


def _read(kind: str) -> str:
    return _fixture_path(kind).read_text(encoding="utf-8")


def _parse(kind: str, payload: str, **over: object) -> claude_worker.news.sources.Parsed:
    return claude_worker.news.sources.parse(_source_for(kind, **over), payload, NOW)


def test_the_shape_table_covers_every_kind() -> None:
    assert sorted(_SHAPE) == sorted(claude_worker.news.sources.KINDS)
    assert sorted(_SERIES_KEY) == sorted(k for k in _SHAPE if _SHAPE[k] == "series")


def test_every_kind_has_a_fixture() -> None:
    missing = [k for k in claude_worker.news.sources.KINDS if not _fixture_path(k).is_file()]
    assert not missing, f"kinds without a recorded fixture: {missing}"


def test_no_fixture_exceeds_the_recording_cap() -> None:
    for kind in claude_worker.news.sources.KINDS:
        size = _fixture_path(kind).stat().st_size
        assert size <= claude_worker.news.sources.FIXTURE_MAX_BYTES, f"{kind} is {size} B"


def test_synthetic_fixtures_are_declared() -> None:
    declared = {
        line.strip()
        for line in _SYNTHETIC_MARKER.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    }
    assert declared == set(_EXPECTED_SYNTHETIC)


def test_every_fixture_parses_to_its_declared_shape() -> None:
    for kind in claude_worker.news.sources.KINDS:
        parsed = _parse(kind, _read(kind))
        assert not parsed.is_empty(), f"{kind} parsed EMPTY from its own fixture"
        shape = _SHAPE[kind]
        if shape == "items":
            assert parsed.items, kind
            assert parsed.snapshot is None, kind
            assert not parsed.series, kind
        elif shape == "snapshot":
            assert parsed.snapshot is not None, kind
            assert not parsed.items and not parsed.series, kind
        else:
            assert len(parsed.series) == 1, kind
            assert not parsed.items and parsed.snapshot is None, kind


def test_every_item_carries_a_guid_and_the_sources_own_metadata() -> None:
    for kind in claude_worker.news.sources.KINDS:
        if _SHAPE[kind] != "items":
            continue
        parsed = _parse(kind, _read(kind), venue="okx", weight=0.7)
        for item in parsed.items:
            assert item.guid, f"{kind}: an item with no guid"
            assert item.source == kind
            assert item.class_ == _class_for(kind)
            assert item.weight == 0.7
            assert item.venue == "okx"
            assert item.ts >= 0
            assert len(item.text) <= claude_worker.news.sources.NewsSettings().text_cap


def test_status_kraken_snapshot_omits_the_polling_timestamp() -> None:
    """MEASURED 2026-09-19: Kraken answers its own `timestamp` on every
    poll, so a body carrying it has a fresh sha256 every 60 s and the §8.1
    "an unchanged snapshot is not stored again" rule could never fire for
    this source — it was the one class-A source of thirteen that stored a
    second snapshot row across two live cycles. WHEN the state was read is
    already `taken_ts`; the body is the venue's STATE."""
    payload = _read("status-kraken")
    assert "timestamp" in payload, "the recorded fixture still carries it"
    snapshot = _parse("status-kraken", payload).snapshot
    assert snapshot is not None
    assert json.loads(snapshot.body) == {"status": "online"}
    # Two polls a minute apart therefore hash the same.
    later = claude_worker.news.sources.parse(
        _source_for("status-kraken"), payload, NOW + 60
    ).snapshot
    assert later is not None
    assert later.sha256 == snapshot.sha256
    assert later.taken_ts != snapshot.taken_ts


def test_every_snapshot_is_canonical_and_hashes_its_own_body() -> None:
    for kind in claude_worker.news.sources.KINDS:
        if _SHAPE[kind] != "snapshot":
            continue
        snapshot = _parse(kind, _read(kind)).snapshot
        assert snapshot is not None
        assert snapshot.source == kind
        assert snapshot.taken_ts == NOW
        body = json.loads(snapshot.body)
        assert isinstance(body, dict)
        assert snapshot.count == len(body)
        assert snapshot.body == json.dumps(body, separators=(",", ":"), sort_keys=True)
        assert snapshot.sha256 == hashlib.sha256(snapshot.body.encode("utf-8")).hexdigest()


def test_a_snapshot_hash_moves_only_when_the_keyed_state_moves() -> None:
    payload = _read("instruments-okx")
    first = _parse("instruments-okx", payload).snapshot
    again = _parse("instruments-okx", payload, name="instruments-okx").snapshot
    assert first is not None and again is not None
    assert first.sha256 == again.sha256

    doc = json.loads(payload)
    doc["data"][0]["state"] = "suspend"
    moved = _parse("instruments-okx", json.dumps(doc)).snapshot
    assert moved is not None
    assert moved.sha256 != first.sha256


def test_every_series_kind_emits_its_declared_key() -> None:
    for kind, key in _SERIES_KEY.items():
        series = _parse(kind, _read(kind)).series
        assert len(series) == 1, kind
        name, ts, value = series[0]
        assert name == key, kind
        assert ts > 0, kind
        assert isinstance(value, float), kind


def test_garbage_is_an_empty_parse_for_every_kind() -> None:
    for kind in claude_worker.news.sources.KINDS:
        for payload in _GARBAGE:
            assert _parse(kind, payload).is_empty(), f"{kind} accepted {payload!r}"


def test_a_truncated_fixture_is_an_empty_parse_for_every_kind() -> None:
    for kind in claude_worker.news.sources.KINDS:
        body = _read(kind)
        assert _parse(kind, body[: len(body) // 2]).is_empty(), kind


def test_no_parser_raises_on_another_kinds_fixture() -> None:
    payloads = {kind: _read(kind) for kind in claude_worker.news.sources.KINDS}
    for kind in claude_worker.news.sources.KINDS:
        for other, payload in payloads.items():
            if other == kind:
                continue
            # The only contract is "no exception" — a coincidental parse is fine.
            _parse(kind, payload)


def test_an_unknown_kind_is_an_empty_parse() -> None:
    source = claude_worker.news.sources.Source(
        name="x", kind="not-a-kind", url="https://a.example/x", origin="a.example", class_="C"
    )
    assert claude_worker.news.sources.parse(source, "{}", NOW).is_empty()


def test_every_fixture_is_its_own_reduction() -> None:
    """A recorded fixture is already reduced, so reducing it again is a
    fixed point. This is what proves the file carries no un-keyed field."""
    for kind in claude_worker.news.sources.KINDS:
        body = _read(kind)
        assert claude_worker.news.sources.reduce_payload(kind, body) == body, kind


def test_reduce_payload_refuses_an_unknown_kind() -> None:
    with pytest.raises(ValueError):
        claude_worker.news.sources.reduce_payload("not-a-kind", "{}")


def test_reduce_payload_refuses_a_payload_it_cannot_navigate() -> None:
    with pytest.raises(ValueError):
        claude_worker.news.sources.reduce_payload("json-okx-ann", '{"nope": []}')


def test_fixture_suffix_matches_the_recorded_files() -> None:
    assert claude_worker.news.sources.fixture_suffix("rss") == ".xml"
    assert claude_worker.news.sources.fixture_suffix("json-okx-ann") == ".json"


# ---- kind-specific behaviour the fixtures cannot pin on their own ----


def test_strip_text_removes_tags_entities_and_bare_urls() -> None:
    raw = "<p>OKX  will &amp; <b>delist</b>\nFOO</p> see https://x.example/a?b=1 now"
    assert claude_worker.news.sources.strip_text(raw, 200) == "OKX will & delist FOO see now"
    assert claude_worker.news.sources.strip_text("&lt;script&gt;alert(1)&lt;/script&gt;", 200) == (
        "<script>alert(1)</script>"
    )
    assert claude_worker.news.sources.strip_text("abcdef", 3) == "abc"
    assert claude_worker.news.sources.strip_text("", 10) == ""


def test_the_okx_annotation_type_becomes_a_hint() -> None:
    payload = json.dumps(
        {
            "data": [
                {
                    "details": [
                        {
                            "annType": "announcements-delistings",
                            "title": "Delisting of FOO",
                            "url": "https://www.okx.com/a/1",
                            "pTime": "1758290000000",
                        },
                        {
                            "annType": "announcements-new-listings",
                            "title": "Listing of BAR",
                            "url": "https://www.okx.com/a/2",
                            "pTime": "1758290100000",
                        },
                        {
                            "annType": "announcements-latest-announcements",
                            "title": "Something else",
                            "url": "https://www.okx.com/a/3",
                            "pTime": "1758290200000",
                        },
                    ]
                }
            ]
        }
    )
    items = _parse("json-okx-ann", payload).items
    assert [i.hint for i in items] == ["delisting", "listing", "other"]
    # pTime is milliseconds on the wire and seconds in the row.
    assert items[0].ts == 1_758_290_000
    assert items[0].guid == "https://www.okx.com/a/1"


def test_the_bybit_type_key_and_tags_both_feed_the_hint() -> None:
    payload = json.dumps(
        {
            "result": {
                "list": [
                    {
                        "title": "Delisting",
                        "description": "d",
                        "type": {"key": "delistings"},
                        "url": "https://x/1",
                        "publishTime": 1758290000000,
                    },
                    {
                        "title": "Maintenance",
                        "description": "m",
                        "type": {"key": "unknown_thing"},
                        "tags": ["maintenance_updates"],
                        "url": "https://x/2",
                        "publishTime": 1758290100000,
                    },
                    {
                        "title": "Plain",
                        "description": "p",
                        "url": "https://x/3",
                        "dateTimestamp": 1758290200000,
                    },
                ]
            }
        }
    )
    items = _parse("json-bybit-ann", payload).items
    assert [i.hint for i in items] == ["delisting", "maintenance", "other"]
    # publishTime wins; dateTimestamp is the fallback.
    assert items[2].ts == 1_758_290_200


def test_the_binance_cms_endpoint_is_read_in_both_known_shapes() -> None:
    article = {"title": "New listing", "code": "abc123", "releaseDate": 1758290000000}
    nested = json.dumps({"data": {"catalogs": [{"articles": [article]}]}})
    flat = json.dumps({"data": {"articles": [article]}})
    for payload in (nested, flat):
        items = _parse("json-binance-cms", payload).items
        assert len(items) == 1
        assert items[0].guid == "abc123"
        assert items[0].link == "https://www.binance.com/en/support/announcement/abc123"
        assert items[0].ts == 1_758_290_000


def test_4chan_keeps_only_threads_above_the_reply_floor() -> None:
    floor = claude_worker.news.sources.FOURCHAN_MIN_REPLIES
    payload = json.dumps(
        [
            {
                "page": 1,
                "threads": [
                    {"no": 1, "sub": "BTC to 200k", "com": "<b>soon</b>", "time": 1758290000,
                     "replies": floor},
                    {"no": 2, "com": "quiet thread", "time": 1758290100, "replies": floor - 1},
                    {"no": 3, "com": "x" * 200, "time": 1758290200, "replies": floor + 5},
                ],
            }
        ]
    )
    items = _parse("json-4chan-catalog", payload).items
    assert [i.guid for i in items] == ["1", "3"]
    assert items[0].title == "BTC to 200k"
    assert items[0].text == "soon"
    # No subject: the title is the stripped comment, capped.
    assert items[1].title == "x" * claude_worker.news.sources.FOURCHAN_TITLE_CAP


def test_a_4chan_catalog_below_the_floor_is_an_empty_parse() -> None:
    payload = json.dumps([{"threads": [{"no": 1, "com": "hi", "time": 1, "replies": 0}]}])
    assert _parse("json-4chan-catalog", payload).is_empty()


def test_a_gdelt_item_reports_the_publishers_domain_not_gdelts() -> None:
    payload = json.dumps(
        {
            "articles": [
                {
                    "url": "https://news.example/a",
                    "title": "OKX halts withdrawals",
                    "seendate": "20260919T120000Z",
                    "domain": "News.Example",
                }
            ]
        }
    )
    item = _parse("gdelt", payload, origin="api.gdeltproject.org").items[0]
    assert item.origin == "news.example"
    assert item.ts == 1_789_819_200


def test_a_gdelt_article_without_a_domain_falls_back_to_the_source_origin() -> None:
    payload = json.dumps(
        {"articles": [{"url": "https://n/a", "title": "t", "seendate": "not-a-stamp"}]}
    )
    item = _parse("gdelt", payload, origin="api.gdeltproject.org").items[0]
    assert item.origin == "api.gdeltproject.org"
    assert item.ts == 0


def test_google_news_keys_on_the_link_and_plain_rss_on_the_guid() -> None:
    body = (
        '<?xml version="1.0"?><rss version="2.0"><channel><item>'
        "<guid>tag:opaque-id</guid><link>https://pub.example/a</link>"
        "<title>T</title><description>D</description></item></channel></rss>"
    )
    assert _parse("google-news", body).items[0].guid == "https://pub.example/a"
    assert _parse("rss", body).items[0].guid == "tag:opaque-id"


def test_a_feed_with_no_usable_entry_is_an_empty_parse() -> None:
    assert _parse("rss", '<?xml version="1.0"?><rss><channel></channel></rss>').is_empty()


def test_the_binance_ping_is_liveness_and_only_accepts_an_empty_body() -> None:
    parsed = _parse("ping-binance", "{}")
    assert parsed.snapshot is not None
    assert parsed.snapshot.count == 1
    assert _parse("ping-binance", '{"code": -1121}').is_empty()


def test_an_all_clear_status_still_snapshots() -> None:
    """An empty maintenance list is the venue's NORMAL state; reading it as a
    parse failure would make "all clear" indistinguishable from a reshape."""
    okx = _parse("status-okx", '{"data": []}')
    assert okx.snapshot is not None
    assert okx.snapshot.count == 0
    assert not okx.is_empty()

    bybit = _parse("status-bybit", '{"result": {"list": []}}')
    assert bybit.snapshot is not None
    assert bybit.snapshot.count == 0


def test_a_locked_deribit_and_an_unlocked_one_hash_differently() -> None:
    unlocked = _parse("status-deribit", '{"result": {"locked": "false"}}').snapshot
    locked = _parse("status-deribit", '{"result": {"locked": "true"}}').snapshot
    assert unlocked is not None and locked is not None
    assert unlocked.sha256 != locked.sha256
    assert _parse("status-deribit", '{"result": {}}').is_empty()


def test_the_dvol_series_takes_the_last_rows_close() -> None:
    payload = json.dumps(
        {"result": {"data": [[1758200000000, 40, 45, 39, 41], [1758290000000, 41, 47, 40, 46]]}}
    )
    assert _parse("series-dvol", payload).series == [("dvol", 1_758_290_000, 46.0)]
    assert _parse("series-dvol", '{"result": {"data": [[1, 2]]}}').is_empty()


def test_the_cot_series_is_the_bitcoin_rows_net_non_commercial() -> None:
    payload = json.dumps(
        [
            {"market_and_exchange_names": "GOLD - COMMODITY EXCHANGE",
             "noncomm_positions_long_all": "1", "noncomm_positions_short_all": "2"},
            {"market_and_exchange_names": "BITCOIN - CHICAGO MERCANTILE EXCHANGE",
             "noncomm_positions_long_all": "1500", "noncomm_positions_short_all": "900"},
        ]
    )
    assert _parse("series-cftc-cot", payload).series == [("noncomm_net", NOW, 600.0)]
    assert _parse("series-cftc-cot", json.dumps([{"market_and_exchange_names": "GOLD"}])).is_empty()


def test_the_stablecoin_series_sums_every_pegged_asset() -> None:
    payload = json.dumps(
        {
            "peggedAssets": [
                {"name": "USDT", "circulating": {"peggedUSD": 100.5}},
                {"name": "USDC", "circulating": {"peggedUSD": 50.25}},
                {"name": "broken", "circulating": None},
            ]
        }
    )
    assert _parse("series-defillama-stables", payload).series == [("pegged_usd", NOW, 150.75)]


def test_a_binance_series_takes_the_last_row_and_its_own_key() -> None:
    payload = json.dumps(
        [
            {"longShortRatio": "1.10", "timestamp": 1758200000000},
            {"longShortRatio": "1.25", "timestamp": 1758290000000},
        ]
    )
    assert _parse("series-bn-ls", payload).series == [("longShortRatio", 1_758_290_000, 1.25)]
    assert _parse("series-bn-taker", payload).is_empty()


def test_a_boolean_never_passes_as_a_number() -> None:
    assert _parse("series-mempool-fees", '{"fastestFee": true}').is_empty()
    assert _parse("series-blockchain-stats", '{"hash_rate": false}').is_empty()
