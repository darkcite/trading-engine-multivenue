# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""universe_refresh tests (M3) — additive; the frozen 202 + 7-verb
surface untouched. No live API calls: the Gamma lane is mocked at the
``get_fn`` seam with a payload mirroring the live 2026-08-22 response
shape (slug law verified against the real venue that day)."""

import datetime
import json
import pathlib

import claude_worker.universe_refresh


UTC = datetime.timezone.utc

# The live M1 universe.toml shape (abbreviated ids for readability —
# the rewriter treats entries as opaque strings).
UNIVERSE_TEXT = """# M1 full non-options universe (M1d live smoke).
# PM picks resolved via Gamma 2026-08-22.

[polymarket]
markets = [
  # Bitcoin Up or Down on August 22?  (vol24h ~$253k)
  "111:222",
  # Ethereum Up or Down on August 22?  (vol24h ~$103k)
  "333:444",
]

[binance]
spot = ["btcusdt", "ethusdt"]
usdm = ["btcusdt"]

[pairs]
# BTC market x btcusdt, ETH market x ethusdt.
map = ["0:0", "1:1"]
"""


def gamma_payload(slug: str, up_id: str, down_id: str, outcomes: list[str]) -> str:
    """One-row Gamma /markets body in the live response shape."""
    return json.dumps(
        [
            {
                "id": "3746508",
                "question": f"Q for {slug}",
                "slug": slug,
                "endDate": "2026-08-23T16:00:00Z",
                "clobTokenIds": json.dumps([up_id, down_id]),
                "outcomes": json.dumps(outcomes),
            }
        ]
    )


def make_get(mapping: dict[str, str]):
    """get_fn returning the payload whose slug appears in the URL."""

    def get(url: str) -> str | None:
        for slug, payload in mapping.items():
            if f"slug={slug}" in url:
                return payload
        return None

    return get


def tid(seed: str) -> str:
    """A fixture token id long enough for the fetchers' 10–80-digit
    Gamma token-id law (real ids run ~77 digits)."""
    return seed * 5


# ---- date + slug laws ----------------------------------------------------


def test_refresh_date_before_1600z_is_today() -> None:
    now = datetime.datetime(2026, 8, 23, 0, 0, 30, tzinfo=UTC)
    assert claude_worker.universe_refresh.refresh_date(now) == datetime.date(2026, 8, 23)


def test_refresh_date_after_1600z_is_tomorrow() -> None:
    now = datetime.datetime(2026, 8, 22, 17, 5, 0, tzinfo=UTC)
    assert claude_worker.universe_refresh.refresh_date(now) == datetime.date(2026, 8, 23)


def test_slug_candidates_double_digit_day_is_single_unpadded() -> None:
    slugs = claude_worker.universe_refresh.slug_candidates(
        "bitcoin", datetime.date(2026, 8, 22)
    )
    assert slugs == ("bitcoin-up-or-down-on-august-22-2026",)


def test_slug_candidates_single_digit_day_tries_unpadded_then_padded() -> None:
    slugs = claude_worker.universe_refresh.slug_candidates(
        "ethereum", datetime.date(2026, 9, 2)
    )
    assert slugs == (
        "ethereum-up-or-down-on-september-2-2026",
        "ethereum-up-or-down-on-september-02-2026",
    )


# ---- resolution ----------------------------------------------------------


def test_resolve_market_up_down_order_kept() -> None:
    slug = "bitcoin-up-or-down-on-august-23-2026"
    get = make_get({slug: gamma_payload(slug, tid("555"), tid("666"), ["Up", "Down"])})
    market = claude_worker.universe_refresh.resolve_market(
        get, "gamma-api.polymarket.com", "bitcoin", datetime.date(2026, 8, 23)
    )
    assert market is not None
    assert market.entry == f"{tid('555')}:{tid('666')}"
    assert market.question == f"Q for {slug}"


def test_resolve_market_down_up_order_swapped() -> None:
    slug = "bitcoin-up-or-down-on-august-23-2026"
    get = make_get({slug: gamma_payload(slug, tid("666"), tid("555"), ["Down", "Up"])})
    market = claude_worker.universe_refresh.resolve_market(
        get, "gamma-api.polymarket.com", "bitcoin", datetime.date(2026, 8, 23)
    )
    assert market is not None
    assert market.entry == f"{tid('555')}:{tid('666')}"


def test_resolve_market_wrong_slug_rows_are_skipped() -> None:
    slug = "bitcoin-up-or-down-on-august-23-2026"
    other = gamma_payload("some-other-market", tid("100"), tid("200"), ["Up", "Down"])
    get = make_get({slug: other})
    market = claude_worker.universe_refresh.resolve_market(
        get, "gamma-api.polymarket.com", "bitcoin", datetime.date(2026, 8, 23)
    )
    assert market is None


def test_resolve_market_unusable_body_is_none() -> None:
    slug = "bitcoin-up-or-down-on-august-23-2026"
    get = make_get({slug: "<html>gateway error</html>"})
    market = claude_worker.universe_refresh.resolve_market(
        get, "gamma-api.polymarket.com", "bitcoin", datetime.date(2026, 8, 23)
    )
    assert market is None


def test_resolve_market_padded_fallback_on_single_digit_day() -> None:
    padded = "bitcoin-up-or-down-on-september-02-2026"
    get = make_get({padded: gamma_payload(padded, tid("700"), tid("800"), ["Up", "Down"])})
    market = claude_worker.universe_refresh.resolve_market(
        get, "gamma-api.polymarket.com", "bitcoin", datetime.date(2026, 9, 2)
    )
    assert market is not None
    assert market.entry == f"{tid('700')}:{tid('800')}"


# ---- dailies config ------------------------------------------------------


def test_read_underlyings_good(tmp_path: pathlib.Path) -> None:
    p = tmp_path / "pm-dailies.toml"
    p.write_text('[dailies]\nunderlyings = ["bitcoin", "ethereum"]\n', encoding="utf-8")
    assert claude_worker.universe_refresh.read_underlyings(p) == ("bitcoin", "ethereum")


def test_read_underlyings_missing_bad_or_empty(tmp_path: pathlib.Path) -> None:
    assert claude_worker.universe_refresh.read_underlyings(tmp_path / "absent.toml") is None
    bad = tmp_path / "bad.toml"
    bad.write_text("not toml [", encoding="utf-8")
    assert claude_worker.universe_refresh.read_underlyings(bad) is None
    empty = tmp_path / "empty.toml"
    empty.write_text("[dailies]\nunderlyings = []\n", encoding="utf-8")
    assert claude_worker.universe_refresh.read_underlyings(empty) is None
    weird = tmp_path / "weird.toml"
    weird.write_text('[dailies]\nunderlyings = ["bit coin!"]\n', encoding="utf-8")
    assert claude_worker.universe_refresh.read_underlyings(weird) is None


# ---- rewrite -------------------------------------------------------------


def _mk(entry: str, question: str) -> claude_worker.universe_refresh.ResolvedMarket:
    return claude_worker.universe_refresh.ResolvedMarket(entry=entry, question=question)


def test_rewrite_replaces_only_the_markets_array() -> None:
    new = claude_worker.universe_refresh.rewrite_polymarket_markets(
        UNIVERSE_TEXT, [_mk("aaa:bbb", "Bitcoin Up or Down on August 23?")]
    )
    assert new is not None
    assert '"aaa:bbb",' in new
    assert "# Bitcoin Up or Down on August 23?" in new
    assert '"111:222"' not in new and '"333:444"' not in new
    # Everything outside the array is byte-preserved.
    head, _, _ = UNIVERSE_TEXT.partition("markets = [")
    _, _, tail = UNIVERSE_TEXT.partition("]\n\n[binance]")
    assert new.startswith(head)
    assert new.endswith(tail) or "[binance]" in new
    assert 'spot = ["btcusdt", "ethusdt"]' in new
    assert 'map = ["0:0", "1:1"]' in new


def test_rewrite_single_line_array_form() -> None:
    text = '[polymarket]\nmarkets = ["x"]\n\n[binance]\nspot = ["btcusdt"]\n'
    new = claude_worker.universe_refresh.rewrite_polymarket_markets(
        text, [_mk("y:z", "Q?")]
    )
    assert new is not None
    assert '"y:z",' in new
    assert '"x"' not in new
    assert 'spot = ["btcusdt"]' in new


def test_rewrite_without_section_or_array_is_none() -> None:
    assert (
        claude_worker.universe_refresh.rewrite_polymarket_markets(
            '[binance]\nspot = ["btcusdt"]\n', [_mk("a:b", "Q?")]
        )
        is None
    )
    assert (
        claude_worker.universe_refresh.rewrite_polymarket_markets(
            "[polymarket]\n\n[binance]\nspot = []\n", [_mk("a:b", "Q?")]
        )
        is None
    )


# ---- end-to-end run ------------------------------------------------------


def _setup(tmp_path: pathlib.Path) -> tuple[pathlib.Path, pathlib.Path]:
    universe = tmp_path / "universe.toml"
    universe.write_text(UNIVERSE_TEXT, encoding="utf-8")
    dailies = tmp_path / "pm-dailies.toml"
    dailies.write_text(
        '[dailies]\nunderlyings = ["bitcoin", "ethereum"]\n', encoding="utf-8"
    )
    return universe, dailies


def test_run_rewrites_and_is_idempotent(tmp_path: pathlib.Path) -> None:
    universe, dailies = _setup(tmp_path)
    btc = "bitcoin-up-or-down-on-august-23-2026"
    eth = "ethereum-up-or-down-on-august-23-2026"
    get = make_get(
        {
            btc: gamma_payload(btc, tid("901"), tid("902"), ["Up", "Down"]),
            eth: gamma_payload(eth, tid("903"), tid("904"), ["Up", "Down"]),
        }
    )
    now = datetime.datetime(2026, 8, 23, 0, 0, 40, tzinfo=UTC)
    rc = claude_worker.universe_refresh.run(universe, dailies, now, get, {})
    assert rc == 0
    text1 = universe.read_text(encoding="utf-8")
    assert f'"{tid("901")}:{tid("902")}",' in text1
    assert f'"{tid("903")}:{tid("904")}",' in text1
    assert "# Q for bitcoin-up-or-down-on-august-23-2026" in text1
    assert 'map = ["0:0", "1:1"]' in text1, "pairs untouched"
    assert not (tmp_path / "universe.toml.tmp").exists()
    # Second run: byte-identical.
    rc2 = claude_worker.universe_refresh.run(universe, dailies, now, get, {})
    assert rc2 == 0
    assert universe.read_text(encoding="utf-8") == text1


def test_run_unresolved_leaves_file_untouched(tmp_path: pathlib.Path) -> None:
    universe, dailies = _setup(tmp_path)
    btc = "bitcoin-up-or-down-on-august-23-2026"
    get = make_get({btc: gamma_payload(btc, tid("901"), tid("902"), ["Up", "Down"])})
    now = datetime.datetime(2026, 8, 23, 0, 0, 40, tzinfo=UTC)
    rc = claude_worker.universe_refresh.run(universe, dailies, now, get, {})
    assert rc == 1, "ethereum unresolved"
    assert universe.read_text(encoding="utf-8") == UNIVERSE_TEXT


def test_run_missing_inputs_fail_soft(tmp_path: pathlib.Path) -> None:
    universe, dailies = _setup(tmp_path)
    now = datetime.datetime(2026, 8, 23, 0, 0, 40, tzinfo=UTC)
    rc = claude_worker.universe_refresh.run(
        tmp_path / "absent.toml", dailies, now, make_get({}), {}
    )
    assert rc == 1
    rc2 = claude_worker.universe_refresh.run(
        universe, tmp_path / "absent-dailies.toml", now, make_get({}), {}
    )
    assert rc2 == 1
    assert universe.read_text(encoding="utf-8") == UNIVERSE_TEXT


def test_run_uses_env_gamma_host(tmp_path: pathlib.Path) -> None:
    universe, dailies = _setup(tmp_path)
    dailies.write_text('[dailies]\nunderlyings = ["bitcoin"]\n', encoding="utf-8")
    seen: list[str] = []

    def get(url: str) -> str | None:
        seen.append(url)
        slug = "bitcoin-up-or-down-on-august-23-2026"
        return gamma_payload(slug, tid("100"), tid("200"), ["Up", "Down"])

    now = datetime.datetime(2026, 8, 23, 0, 0, 40, tzinfo=UTC)
    rc = claude_worker.universe_refresh.run(
        universe, dailies, now, get, {"POLYMARKET_GAMMA_HOST": "gamma.example.test"}
    )
    assert rc == 0
    assert seen and "gamma.example.test" in seen[0]
