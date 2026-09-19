# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §4.1 — ``news.toml`` parsing and validation.

The registry is the lane's trust boundary: every later stage believes the
origin, the class and the weight this file declares, so the tests here are
mostly about what load_registry REFUSES. A source that slips through
mistyped is a source that polls the wrong host under the operator's name.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.news.sources

_OKX = (
    '{ name = "okx-ann", kind = "json-okx-ann", '
    'url = "https://www.okx.com/api/v5/support/announcements", '
    'origin = "www.okx.com", class = "B", venue = "okx" }'
)

_HEAD = """
[news]
poll_default_s = 120

[keywords]
event = ["listing", "delisting"]

[calendar]
bls_releases = ["2026-10-02T12:30:00Z"]
fixed_daily = [{ kind = "engine_restart", at = "00:10" }]

[registry]
sources = [
"""


def _doc(*entries: str) -> str:
    return _HEAD + ",\n".join(entries) + ",\n]\n"


def _load(tmp_path: pathlib.Path, body: str) -> claude_worker.news.sources.Registry:
    path = tmp_path / "news.toml"
    path.write_text(body, encoding="utf-8")
    return claude_worker.news.sources.load_registry(path)


def _refuses(tmp_path: pathlib.Path, body: str, needle: str) -> None:
    with pytest.raises(ValueError) as exc:
        _load(tmp_path, body)
    assert needle in str(exc.value)


def test_minimal_registry_loads_and_applies_every_default(tmp_path: pathlib.Path) -> None:
    registry = _load(tmp_path, _doc(_OKX))
    assert registry.settings.poll_default_s == 120
    # Untouched [news] keys keep the dataclass defaults.
    assert registry.settings.text_cap == claude_worker.news.sources.NewsSettings().text_cap
    assert registry.keywords == ("listing", "delisting")
    assert registry.calendar.bls_releases == ("2026-10-02T12:30:00Z",)
    assert registry.calendar.fixed_daily[0].kind == "engine_restart"
    assert registry.calendar.fixed_daily[0].at == "00:10"

    source = registry.sources[0]
    assert source.name == "okx-ann"
    assert source.venue == "okx"
    # poll_s falls back to [news].poll_default_s, not to Source's own default.
    assert source.poll_s == 120
    assert source.weight == 1.0
    assert source.items_per_hour == claude_worker.news.sources.ITEMS_PER_HOUR_DEFAULT
    assert source.enabled == 1
    assert source.rate == ""
    assert source.is_origin


def test_by_name_and_enabled_sources(tmp_path: pathlib.Path) -> None:
    off = _OKX.replace('name = "okx-ann"', 'name = "okx-off"').replace(" }", ", enabled = 0 }")
    registry = _load(tmp_path, _doc(_OKX, off))
    assert registry.by_name("okx-ann") is not None
    assert registry.by_name("nope") is None
    assert [s.name for s in registry.enabled_sources()] == ["okx-ann"]


def test_a_mill_weight_is_never_an_independent_origin(tmp_path: pathlib.Path) -> None:
    mill = (
        '{ name = "mill", kind = "rss", url = "https://mill.example/feed/", '
        'origin = "mill.example", class = "C", weight = 0.3 }'
    )
    registry = _load(tmp_path, _doc(mill))
    assert registry.sources[0].weight == 0.3
    assert not registry.sources[0].is_origin


def test_unknown_keys_are_refused_everywhere(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX.replace(" }", ", colour = 1 }")), "unknown key")
    _refuses(tmp_path, _doc(_OKX).replace("poll_default_s = 120", "poll_defualt_s = 120"), "[news]")
    _refuses(tmp_path, _doc(_OKX) + '\n[extra]\nk = 1\n', "unknown key")
    bad_daily = _doc(_OKX).replace(
        '{ kind = "engine_restart", at = "00:10" }', '{ kind = "x", when = "1" }'
    )
    _refuses(tmp_path, bad_daily, "fixed_daily")


def test_a_missing_required_key_is_refused(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX.replace('class = "B", ', "")), "missing required key")


def test_duplicate_names_are_refused(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX, _OKX), "duplicate source name")


def test_origin_must_be_the_urls_host(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX.replace('origin = "www.okx.com"', 'origin = "okx.com"')), "!=")


def test_unknown_kind_class_and_venue_are_refused(tmp_path: pathlib.Path) -> None:
    bad_kind = _OKX.replace('kind = "json-okx-ann"', 'kind = "json-okx"')
    _refuses(tmp_path, _doc(bad_kind), "unknown kind")
    _refuses(tmp_path, _doc(_OKX.replace('class = "B"', 'class = "E"')), "class must be one of")
    _refuses(tmp_path, _doc(_OKX.replace('venue = "okx"', 'venue = "okex"')), "unknown venue")


def test_name_shape_is_enforced(tmp_path: pathlib.Path) -> None:
    shouty = _OKX.replace('name = "okx-ann"', 'name = "OKX_Ann"')
    _refuses(tmp_path, _doc(shouty), "source name must be")
    long_name = "a" * (claude_worker.news.sources.NAME_MAX_LEN + 1)
    overlong = _OKX.replace('name = "okx-ann"', f'name = "{long_name}"')
    _refuses(tmp_path, _doc(overlong), "source name")


def test_numeric_bounds(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX.replace(" }", ", weight = 1.5 }")), "weight must be in")
    _refuses(tmp_path, _doc(_OKX.replace(" }", ", weight = -0.1 }")), "weight must be in")
    _refuses(tmp_path, _doc(_OKX.replace(" }", ", poll_s = 14 }")), "poll_s must be >=")
    _refuses(tmp_path, _doc(_OKX.replace(" }", ", items_per_hour = 0 }")), "items_per_hour")
    # The floor itself is legal.
    floor = claude_worker.news.sources.POLL_MIN_S
    ok = _load(tmp_path, _doc(_OKX.replace(" }", f", poll_s = {floor} }}")))
    assert ok.sources[0].poll_s == floor


def test_a_status_page_needs_the_operators_explicit_flag(tmp_path: pathlib.Path) -> None:
    status = (
        '{ name = "kraken-status-rss", kind = "rss", '
        'url = "https://status.kraken.com/history.rss", origin = "status.kraken.com", class = "C" }'
    )
    _refuses(tmp_path, _doc(status), "status page")
    registry = _load(tmp_path, _doc(status.replace(" }", ", allow_statuspage = 1 }")))
    assert registry.sources[0].allow_statuspage == 1

    hosted = (
        '{ name = "vendor", kind = "rss", url = "https://acme.statuspage.io/history.rss", '
        'origin = "acme.statuspage.io", class = "C" }'
    )
    _refuses(tmp_path, _doc(hosted), "status page")


def test_query_kinds_build_their_own_url(tmp_path: pathlib.Path) -> None:
    gnews = (
        '{ name = "gnews", kind = "google-news", url = "", query = "okx OR bybit", '
        'origin = "news.google.com", class = "C" }'
    )
    registry = _load(tmp_path, _doc(gnews))
    built = claude_worker.news.sources.request_url(registry.sources[0])
    assert built.startswith("https://news.google.com/rss/search?q=")
    assert "okx%20OR%20bybit" in built

    with_url = gnews.replace('url = ""', 'url = "https://news.google.com/x"')
    _refuses(tmp_path, _doc(with_url), "url must be ''")
    no_query = gnews.replace('query = "okx OR bybit"', 'query = ""')
    _refuses(tmp_path, _doc(no_query), "non-empty query")
    wrong_origin = gnews.replace('origin = "news.google.com"', 'origin = "google.com"')
    _refuses(tmp_path, _doc(wrong_origin), "origin must be")


def test_the_other_two_query_kinds_build_their_urls() -> None:
    hn = claude_worker.news.sources.Source(
        name="hn", kind="hnrss", url="", origin="hnrss.org", class_="C", query="crypto OR btc"
    )
    assert claude_worker.news.sources.request_url(hn) == (
        "https://hnrss.org/newest?q=crypto%20OR%20btc"
    )
    gdelt = claude_worker.news.sources.Source(
        name="g", kind="gdelt", url="", origin="api.gdeltproject.org", class_="C", query="(btc)"
    )
    built = claude_worker.news.sources.request_url(gdelt)
    assert built.startswith("https://api.gdeltproject.org/api/v2/doc/doc?query=")
    assert "maxrecords=50" in built and "timespan=1h" in built


def test_a_plain_source_keeps_its_own_url() -> None:
    source = claude_worker.news.sources.Source(
        name="s", kind="rss", url="https://a.example/feed", origin="a.example", class_="C"
    )
    assert claude_worker.news.sources.request_url(source) == "https://a.example/feed"


def test_rate_grammar() -> None:
    assert claude_worker.news.sources.rate_interval_ns("") == 0
    assert claude_worker.news.sources.rate_interval_ns("1/5s") == 5 * 1_000_000_000
    assert claude_worker.news.sources.rate_interval_ns("2/10s") == 5 * 1_000_000_000
    for bad in ("1/5", "5s", "1/0s", "0/5s", "one/five"):
        with pytest.raises(ValueError):
            claude_worker.news.sources.rate_interval_ns(bad)


def test_a_bad_rate_is_refused_by_the_registry(tmp_path: pathlib.Path) -> None:
    _refuses(tmp_path, _doc(_OKX.replace(" }", ', rate = "every 5s" }')), "rate must look like")


def test_an_absent_registry_raises_filenotfound(tmp_path: pathlib.Path) -> None:
    with pytest.raises(FileNotFoundError):
        claude_worker.news.sources.load_registry(tmp_path / "nope.toml")


def test_an_empty_registry_is_legal_and_empty(tmp_path: pathlib.Path) -> None:
    registry = _load(tmp_path, "[registry]\nsources = []\n")
    assert registry.sources == ()
    assert registry.keywords == ()
    assert registry.calendar.fixed_daily == ()
    defaults = claude_worker.news.sources.NewsSettings()
    assert registry.settings.poll_default_s == defaults.poll_default_s
