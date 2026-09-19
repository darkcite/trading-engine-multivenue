# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §5.1 — scheduling, budgets, pacing and the origin refusal.

Every test runs over ``httpx.MockTransport`` (the ``fake_s3``/``test_cli``
precedent). No socket is ever opened and no clock is ever read: the fetcher
takes its clock injected, so the budget window and the GDELT pacer are
stepped by hand.

The redirect rules carry the weight here. ``follow_redirects=False`` plus a
one-hop same-origin allowance is what keeps a hijacked feed URL from
delivering another host's payload under a venue's name (survey §13).

Convention: full ``import x`` only. No ``from x import y``.
"""

import typing

import httpx

import claude_worker.features
import claude_worker.news.sources

NS: int = 1_000_000_000


class _Clock:
    """A hand-cranked monotonic clock."""

    def __init__(self) -> None:
        self.now: int = 0

    def __call__(self) -> int:
        return self.now


def _source(**over: object) -> claude_worker.news.sources.Source:
    fields: dict[str, object] = {
        "name": "a",
        "kind": "rss",
        "url": "https://a.example/feed",
        "origin": "a.example",
        "class_": "C",
        "poll_s": 60,
    }
    fields.update(over)
    return claude_worker.news.sources.Source(**typing.cast(dict[str, typing.Any], fields))


def _registry(*sources: claude_worker.news.sources.Source) -> claude_worker.news.sources.Registry:
    return claude_worker.news.sources.Registry(
        settings=claude_worker.news.sources.NewsSettings(user_agent="multivenue-news/test"),
        keywords=(),
        calendar=claude_worker.news.sources.Calendar(),
        sources=sources,
    )


def _client(handler: typing.Callable[[httpx.Request], httpx.Response]) -> httpx.Client:
    return httpx.Client(transport=httpx.MockTransport(handler))


def test_first_poll_is_due_at_zero_then_reschedules_within_the_jitter_band() -> None:
    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source), _client(lambda r: httpx.Response(200))
    )
    assert [s.name for s in fetcher.due(0)] == ["a"]

    fetcher.reschedule(source, 1_000)
    period = 60 * NS
    low = 1_000 + int(period * (1.0 - claude_worker.news.sources.JITTER_FRAC))
    high = 1_000 + int(period * (1.0 + claude_worker.news.sources.JITTER_FRAC))
    assert low <= fetcher.next_due_ns("a") <= high
    assert fetcher.due(1_000) == []


def test_disabled_sources_are_never_scheduled() -> None:
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(_source(), _source(name="b", enabled=0)),
        _client(lambda r: httpx.Response(200)),
    )
    assert [s.name for s in fetcher.due(0)] == ["a"]


def test_ok_carries_the_payload_and_the_declared_headers() -> None:
    seen: list[httpx.Request] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(request)
        return httpx.Response(200, text="<rss/>")

    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    result = fetcher.fetch(source, 0)
    assert result.status == claude_worker.news.sources.FETCH_STATUS_OK
    assert result.payload == "<rss/>"
    assert result.http_status == 200
    assert seen[0].headers["user-agent"] == "multivenue-news/test"
    assert "application/rss+xml" in seen[0].headers["accept"]


def test_a_non_200_is_an_http_status_and_carries_no_payload() -> None:
    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source), _client(lambda r: httpx.Response(503, text="down")), _Clock()
    )
    result = fetcher.fetch(source, 0)
    assert result.status == claude_worker.news.sources.FETCH_STATUS_HTTP
    assert result.http_status == 503
    assert result.payload is None


def test_a_transport_error_is_counted_not_raised() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("no route", request=request)

    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    result = fetcher.fetch(source, 0)
    assert result.status == claude_worker.news.sources.FETCH_STATUS_TRANSPORT
    assert result.payload is None


def test_an_off_origin_redirect_is_refused() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(302, headers={"location": "https://evil.example/feed"})

    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    result = fetcher.fetch(source, 0)
    assert result.status == claude_worker.news.sources.FETCH_STATUS_REFUSED_ORIGIN
    assert result.payload is None


def test_a_same_origin_redirect_takes_exactly_one_hop() -> None:
    seen: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        seen.append(str(request.url))
        if request.url.path == "/feed":
            return httpx.Response(301, headers={"location": "/feed/latest"})
        return httpx.Response(200, text="<rss/>")

    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    result = fetcher.fetch(source, 0)
    assert result.status == claude_worker.news.sources.FETCH_STATUS_OK
    assert result.payload == "<rss/>"
    assert seen == ["https://a.example/feed", "https://a.example/feed/latest"]


def test_a_second_redirect_after_the_hop_is_refused() -> None:
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(301, headers={"location": "/again"})

    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_REFUSED_ORIGIN


def test_a_redirect_without_a_location_is_refused() -> None:
    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source), _client(lambda r: httpx.Response(302)), _Clock()
    )
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_REFUSED_ORIGIN


def test_an_exhausted_host_budget_skips_the_request() -> None:
    calls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(str(request.url))
        return httpx.Response(200, text="ok")

    clock = _Clock()
    source = _source()
    budgets = {
        "a.example": claude_worker.features.RestBudget(max_calls=1, window_ns=NS, clock_ns=clock)
    }
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source), _client(handler), clock, budgets=budgets
    )
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_OK
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_BUDGET
    assert len(calls) == 1
    # The window rolls and the source is served again.
    clock.now = 2 * NS
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_OK


def test_a_rate_limited_source_is_paced_off_the_injected_clock() -> None:
    calls: list[str] = []

    def handler(request: httpx.Request) -> httpx.Response:
        calls.append(str(request.url))
        return httpx.Response(200, text="{}")

    source = _source(
        name="g", kind="gdelt", url="", origin="api.gdeltproject.org", query="btc", rate="1/5s"
    )
    fetcher = claude_worker.news.sources.Fetcher(_registry(source), _client(handler), _Clock())
    assert fetcher.fetch(source, 0).status == claude_worker.news.sources.FETCH_STATUS_OK
    assert fetcher.fetch(source, 1 * NS).status == claude_worker.news.sources.FETCH_STATUS_PACED
    assert fetcher.fetch(source, 5 * NS).status == claude_worker.news.sources.FETCH_STATUS_OK
    assert len(calls) == 2
    assert calls[0].startswith("https://api.gdeltproject.org/api/v2/doc/doc?query=")


def test_pacing_is_checked_before_the_budget_is_spent() -> None:
    clock = _Clock()
    source = _source(
        name="g", kind="gdelt", url="", origin="api.gdeltproject.org", query="btc", rate="1/5s"
    )
    budget = claude_worker.features.RestBudget(max_calls=10, window_ns=NS, clock_ns=clock)
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source),
        _client(lambda r: httpx.Response(200, text="{}")),
        clock,
        budgets={"api.gdeltproject.org": budget},
    )
    fetcher.fetch(source, 0)
    fetcher.fetch(source, 1 * NS)
    fetcher.fetch(source, 2 * NS)
    # Two paced attempts cost the host budget nothing.
    assert budget.skipped_total == 0


def test_every_fetch_reschedules_so_a_failing_source_backs_off() -> None:
    source = _source()
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(source), _client(lambda r: httpx.Response(500)), _Clock()
    )
    fetcher.fetch(source, 0)
    assert fetcher.next_due_ns("a") > 0
    assert fetcher.due(0) == []


def test_build_budgets_is_per_host_and_floors_at_the_minimum() -> None:
    sources = (
        _source(name="a", poll_s=60),
        _source(name="b", url="https://a.example/other", poll_s=300),
        _source(name="c", url="https://b.example/feed", origin="b.example", poll_s=3600),
    )
    budgets = claude_worker.news.sources.build_budgets(tuple(sources), _Clock())
    assert sorted(budgets) == ["a.example", "b.example"]
    # b.example: one source at 3600 s -> 2 * 1 * 3600 // 3600 = 2, floored to 60.
    rare = budgets["b.example"]
    for _ in range(claude_worker.news.sources.BUDGET_MIN_CALLS):
        assert rare.try_acquire()
    assert not rare.try_acquire()


def test_the_default_budget_table_is_built_from_the_enabled_sources_only() -> None:
    fetcher = claude_worker.news.sources.Fetcher(
        _registry(
            _source(),
            _source(name="z", url="https://z.example/f", origin="z.example", enabled=0),
        ),
        _client(lambda r: httpx.Response(200)),
        _Clock(),
    )
    assert sorted(fetcher.budgets) == ["a.example"]
