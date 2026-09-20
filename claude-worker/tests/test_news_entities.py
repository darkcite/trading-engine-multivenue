# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §9.2 — deterministic entity resolution for the story key.

The properties defended here, in order of how much they would cost if they
broke:

* a market we do not trade can never enter a key — the caller's vocabulary is
  the only source of tickers, so a headline about a coin we have no market in
  contributes nothing;
* a ticker NEVER matches in lower case — the rule `filter.Vocabulary`
  measured on 2026-09-19, because half this vocabulary is ordinary English
  ("link", "near", "sand", "gas", "move", "etc", "hype", "dot", "arb");
* an alias must be CAPITALISED where it appears, which is what separates
  "an avalanche of liquidations" from "Avalanche halts withdrawals" without
  anyone guessing which names are rare enough as English;
* a title with no lower case at all gets no bare-ticker reading, because
  capitalising a word in a headline that capitalises everything says nothing;
* a longer name wins and consumes its span, so "Bitcoin Cash" is BCH and is
  NOT also BTC;
* the source's venue lands in the key even when the title never says it,
  which is the whole reason this module exists: "New listing: ZSUSDT
  Perpetual Contract" is a Bybit story and no model reading that title could
  know it;
* the escalation gate is NOT touched by any of this — `should_escalate`
  still reads the tagger's own answer.

Convention: full ``import x`` only. No ``from x import y``.
"""

import claude_worker.labeling
import claude_worker.news.entities
import claude_worker.news.filter

VOCAB: tuple[str, ...] = (
    "BTC", "BCH", "ETH", "ETC", "SOL", "XRP", "HYPE", "ONE", "SAND", "NEAR",
    "LSK", "ZEC", "DOGE", "OPUSDT", "LINK", "GAS", "MOVE", "DOT", "VET",
    "ARB", "ATOM", "GRT", "STX", "XLM", "1000PEPE", "1000FLOKI", "AVAX",
)
VENUES: tuple[str, ...] = claude_worker.labeling.VENUE_NAMES


def _resolve(
    title: str, venue: str = "", vocab: tuple[str, ...] = VOCAB
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    return claude_worker.news.entities.resolve(title, vocab, venue, VENUES)


def test_a_headline_name_resolves_onto_the_ticker_we_trade() -> None:
    venues, assets = _resolve("Bitcoin reclaims $80,000 as Solana rallies")
    assert assets == ("BTC", "SOL")
    assert venues == ()


def test_a_longer_name_wins_and_is_not_also_counted_as_the_shorter_one() -> None:
    _, assets = _resolve("Bitcoin Cash hard fork ships this week")
    assert assets == ("BCH",)

    # ...and the same for a name that CONTAINS another vocabulary name.
    _, assets = _resolve("Ethereum Classic reorgs again")
    assert assets == ("ETC",)


def test_a_ticker_never_matches_in_lower_case() -> None:
    """The rule `filter.Vocabulary` measured on 2026-09-19. Every line here
    is a headline a first cut of this module got wrong."""
    for prose in (
        "Regulators near a decision on one of the largest funds",
        "Bybit: link your account to claim the airdrop",
        "Binance adds support for spot, margin, futures, etc.",
        "Analysts say the hype around the ETF is fading",
        "OKX to vet every new listing before it goes live",
        "Deribit will move the settlement window",
        "Gas fees hit a new low",
        "The dot-com crash is the wrong analogy for crypto",
        "Traders chase the arb between spot and perps",
        "Every atom of liquidity has left the book",
    ):
        assert _resolve(prose)[1] == (), prose

    # The symbol, as financial prose writes one: the asset.
    _, assets = _resolve("OKX to delist ONE perpetual futures")
    assert assets == ("ONE",)


def test_an_alias_must_be_capitalised_to_count() -> None:
    """The rule that lets the table keep a name that is also a word.

    Every pair here is the same word twice: once as market prose, once as the
    thing itself. Getting this from capitalisation rather than from a list of
    names someone judged "rare enough" is the point — that judgement had
    already been wrong about six tickers.
    """
    for prose, proper, ticker in (
        ("The ripple effect across markets is only beginning",
         "Ripple settles with the SEC over token sales", "XRP"),
        ("An avalanche of liquidations hit perps overnight",
         "Avalanche halts withdrawals after a node fault", "AVAX"),
        ("Traders look to the cosmos for the next narrative",
         "Cosmos Hub passes the inflation cut", "ATOM"),
    ):
        assert _resolve(prose)[1] == (), prose
        assert _resolve(proper)[1] == (ticker,), proper


def test_an_alias_is_never_an_ordinary_english_word() -> None:
    """Each of these resolved to a wrong asset before the table was pruned."""
    for prose in (
        "The proposal vanished into the ether",
        "A stellar quarter for the exchange",
        "Regulators seek harmony across jurisdictions",
        "The graph shows funding flipping negative",
        "Stacks of unsold inventory weigh on miners",
        "Optimism around the ETF decision is fading",
    ):
        # Capitalised, and still nothing: these are the names capitalisation
        # cannot rescue, because a headline capitalises them anyway.
        assert _resolve(prose)[1] == (), prose


def test_a_title_with_no_lower_case_gets_no_bare_ticker_reading() -> None:
    """Capitalising a word in an ALL-CAPS headline says nothing, and venue
    feeds publish caps titles."""
    _, assets = _resolve("BYBIT ROLLS OUT ONE MORE UPGRADE FOR ALL PEOPLE")
    assert assets == ()

    # ...while the instrument name, the alias and the source venue all still
    # resolve, which is what a caps announcement actually carries.
    venues, assets = _resolve("OKX TO DELIST ONEUSDT PERPETUAL", venue="okx")
    assert venues == ("okx",) and assets == ("ONE",)


def test_an_instrument_name_resolves_to_its_asset() -> None:
    _, assets = _resolve("OKX to delist perpetual futures for ONEUSDT")
    assert assets == ("ONE",)

    # A symbol that is ITSELF spelled with a quote suffix is not stripped.
    _, assets = _resolve("Delayed Sends/Receives - OPUSDT")
    assert assets == ("OPUSDT",)


def test_a_market_we_do_not_trade_cannot_enter_a_key() -> None:
    # Dogecoin is in the alias table but NOT in this caller's vocabulary.
    _, assets = _resolve("Dogecoin surges on ETF filing", vocab=("BTC",))
    assert assets == ()


def test_an_untraded_long_name_still_shadows_the_short_one_inside_it() -> None:
    """The span belongs to the NAME, not to what it resolves to.

    A desk with no BCH market must get NOTHING from a Bitcoin Cash headline —
    never BTC. That is a wrong asset in a story key, not a missed one, and it
    would cluster a Bitcoin Cash fork into the Bitcoin story.
    """
    for title in (
        "Bitcoin Cash hard fork ships this week",
        "Ethereum Classic suffers a 51 percent reorg",
        "Ethereum Name Service launches .eth renewals",
    ):
        assert _resolve(title, vocab=("BTC", "ETH"))[1] == (), title


def test_the_quote_suffixes_stay_in_step_with_the_filters() -> None:
    """A venue that starts quoting in a new currency must not silently stop
    resolving here. `filter._QUOTES` is the list the rest of the lane
    derives its vocabulary with; this one is its upper-cased mirror."""
    mirrored = set(claude_worker.news.entities.QUOTE_SUFFIXES)
    for quote in claude_worker.news.filter._QUOTES:
        assert quote.upper() in mirrored, quote


def test_a_long_instrument_name_still_resolves() -> None:
    """At a 12-character ceiling this matched nothing at all."""
    _, assets = _resolve("Binance Futures will launch 1000FLOKIUSDT perpetuals")
    assert assets == ("1000FLOKI",)


def test_the_venue_comes_from_the_source_when_the_title_is_silent() -> None:
    venues, assets = _resolve(
        "New listing: ZSUSDT TradFi Perpetual Contract, with up to 25x leverage",
        venue="bybit",
    )
    assert venues == ("bybit",)
    assert assets == ()


def test_a_venue_named_in_the_title_is_found_too() -> None:
    venues, _ = _resolve("Binance halts withdrawals after outage")
    assert venues == ("binance",)

    # ...and an unlisted venue on the source folds into `other`, never into
    # one of ours.
    venues, _ = _resolve("Scheduled maintenance", venue="upbit")
    assert venues == ("other",)


def test_hyperliquid_is_both_a_venue_and_an_asset() -> None:
    venues, assets = _resolve("Hyperliquid open interest hits a record")
    assert venues == ("hyperliquid",)
    assert assets == ("HYPE",)


def test_the_result_is_sorted_so_a_key_hashes_the_same_every_time() -> None:
    venues, assets = _resolve("Solana and Bitcoin trade on OKX and Binance", venue="okx")
    assert list(venues) == sorted(venues)
    assert list(assets) == sorted(assets)
    assert venues == ("binance", "okx")
