# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""NEWS §7 — tier 0, the free filter.

Tier 0 decides what the cascade is allowed to spend on, so the two things
worth defending are its ORDER (the first stop wins, and the verdict recorded
is the reason) and its purity (the same inputs always give the same verdict —
a funnel you cannot reproduce is a funnel you cannot audit).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import claude_worker.news.filter
import claude_worker.news.sources

NOW: int = 1_758_290_000
WINDOW: int = 21_600
THRESHOLD: float = 0.6


def _item(**over: object) -> claude_worker.news.sources.Item:
    fields: dict[str, object] = {
        "source": "press",
        "guid": "g1",
        "ts": NOW,
        "title": "OKX will delist the FOO perpetual swap",
        "link": "https://press.example/1",
        "text": "The venue said withdrawals continue as normal.",
        "class_": "C",
        "weight": 1.0,
        "origin": "press.example",
        "venue": "",
    }
    fields.update(over)
    return claude_worker.news.sources.Item(**fields)  # type: ignore[arg-type]


def _vocab(*keywords: str) -> claude_worker.news.filter.Vocabulary:
    return claude_worker.news.filter.build_vocabulary([], [], keywords or ("delist",))


def _caps(remaining: int = 10, tier1: int = 100) -> claude_worker.news.filter.Caps:
    return claude_worker.news.filter.Caps(
        per_source_remaining={"press": remaining, "mill": remaining, "venue": remaining},
        tier1_remaining=tier1,
    )


def _verdict(
    item: claude_worker.news.sources.Item,
    vocab: claude_worker.news.filter.Vocabulary | None = None,
    recent: claude_worker.news.filter.RecentTitles | None = None,
    caps: claude_worker.news.filter.Caps | None = None,
) -> claude_worker.news.filter.Tier0Verdict:
    return claude_worker.news.filter.tier0(
        item,
        vocab=_vocab() if vocab is None else vocab,
        recent=claude_worker.news.filter.RecentTitles() if recent is None else recent,
        caps=_caps() if caps is None else caps,
        now_ts=NOW,
        near_dup_jaccard=THRESHOLD,
        near_dup_window_s=WINDOW,
    )


# ---- tokenizing and similarity ----


def test_tokens_lowercases_strips_punctuation_and_drops_short_and_stop_words() -> None:
    got = claude_worker.news.filter.tokens("OKX, the venue: will DELIST FOO-USD (again)!")
    assert got == frozenset({"okx", "venue", "delist", "foo", "usd", "again"})
    # "the" and "will" are stopwords; a two-letter word is too short.
    assert "the" not in got and "will" not in got
    assert claude_worker.news.filter.tokens("go to it") == frozenset()


def test_tokens_of_an_empty_or_punctuation_only_title_is_empty() -> None:
    assert claude_worker.news.filter.tokens("") == frozenset()
    assert claude_worker.news.filter.tokens("--- !!! ---") == frozenset()


def test_jaccard_is_zero_when_either_side_is_empty() -> None:
    both = frozenset({"okx", "delist"})
    assert claude_worker.news.filter.jaccard(both, both) == 1.0
    assert claude_worker.news.filter.jaccard(both, frozenset()) == 0.0
    assert claude_worker.news.filter.jaccard(frozenset(), frozenset()) == 0.0
    assert claude_worker.news.filter.jaccard(both, frozenset({"bybit"})) == 0.0


def test_jaccard_is_intersection_over_union() -> None:
    left = frozenset({"a", "b", "c"})
    right = frozenset({"b", "c", "d"})
    assert claude_worker.news.filter.jaccard(left, right) == 0.5


# ---- vocabulary ----


def test_base_asset_reads_every_descriptor_shape() -> None:
    assert claude_worker.news.filter.base_asset("binance-usdm:btcusdt") == "btc"
    assert claude_worker.news.filter.base_asset("okx:BTC-USDT-SWAP") == "btc"
    assert claude_worker.news.filter.base_asset("deribit:ETH-PERPETUAL") == "eth"
    assert claude_worker.news.filter.base_asset("binance:solusdc") == "sol"
    # A Polymarket token id names no asset.
    assert claude_worker.news.filter.base_asset("polymarket:71321045679252212594626385532") == ""
    assert claude_worker.news.filter.base_asset("") == ""


def test_base_asset_never_strips_a_quote_down_to_nothing() -> None:
    # "usdt" alone must not become "" by eating its own suffix.
    assert claude_worker.news.filter.base_asset("binance:usdt") == "usdt"


def test_the_vocabulary_is_derived_from_markets_descriptors_venues_and_keywords() -> None:
    vocab = claude_worker.news.filter.build_vocabulary(
        ["binance:btcusdt"], ["okx:ETH-USDT-SWAP"], ["hard fork"]
    )
    assert {"binance", "btcusdt", "eth", "okx", "hard fork"} <= vocab.entries
    # Every venue name is always present, traded or not.
    assert "polymarket" in vocab.entries


def test_a_derived_name_only_counts_in_ticker_form() -> None:
    """Measured 2026-09-19: matching derived names case-insensitively passed
    88 % of a live cycle's items, because the real universe contributes LINK,
    NEAR, SAND and PEOPLE, and the market map contributes Polymarket question
    titles. A ticker is written in caps; ordinary prose is not."""
    vocab = claude_worker.news.filter.build_vocabulary(
        ["will-the-fed-cut-rates"],
        ["binance-usdm:linkusdt", "binance-usdm:nearusdt", "binance-usdm:sandusdt"],
        ["delist"],
    )
    assert vocab.hits("the link to move people near the sand") == 0
    assert vocab.hits("LINK and NEAR halted") == 2
    # The operator's own keywords and the venue names stay case-insensitive.
    assert vocab.hits("OKX will DELIST it") == 2
    assert vocab.hits("okx will delist it") == 2


def test_stopwords_and_token_ids_never_enter_the_vocabulary() -> None:
    vocab = claude_worker.news.filter.build_vocabulary(
        ["will-the-fed-cut-rates", "polymarket:" + "9" * 77], [], []
    )
    assert "the" not in vocab.entries
    assert "will" not in vocab.entries
    assert "9" * 77 not in vocab.entries
    assert "rates" in vocab.entries  # a real name, kept — but ticker-form only
    assert vocab.hits("the rates will move") == 0
    assert vocab.hits("RATES moved") == 1


def test_hits_counts_distinct_entries_case_insensitively() -> None:
    vocab = claude_worker.news.filter.build_vocabulary([], [], ["delist", "etf", "hard fork"])
    assert vocab.hits("DELIST delist Delist") == 1
    assert vocab.hits("An ETF and a hard fork and a delist") == 3
    assert vocab.hits("nothing here") == 0


def test_word_boundaries_keep_listing_out_of_delisting_and_eth_out_of_ethics() -> None:
    vocab = claude_worker.news.filter.build_vocabulary([], ["okx:ETH-USDT-SWAP"], ["listing"])
    assert vocab.hits("a delisting today") == 0
    assert vocab.hits("a listing today") == 1
    assert vocab.hits("research ETHICS board") == 0
    assert vocab.hits("ETH is up") == 1


def test_an_empty_vocabulary_hits_nothing_and_does_not_explode() -> None:
    vocab = claude_worker.news.filter.Vocabulary(frozenset(), None, None)
    assert vocab.hits("anything at all") == 0
    assert claude_worker.news.filter.build_vocabulary([], [], []).hits("okx") == 1


# ---- the recent ring ----


def test_the_recent_ring_is_bounded_and_windowed() -> None:
    recent = claude_worker.news.filter.RecentTitles(max_entries=3)
    for i in range(5):
        recent.add(NOW, f"s|{i}", frozenset({f"t{i}"}))
    assert len(recent) == 3

    ring = claude_worker.news.filter.RecentTitles()
    ring.add(NOW - WINDOW - 1, "s|old", claude_worker.news.filter.tokens("OKX will delist FOO"))
    tokens = claude_worker.news.filter.tokens("OKX will delist FOO")
    # Outside the window it is not a duplicate; inside it is.
    assert ring.survivor(tokens, THRESHOLD, NOW - WINDOW) is None
    ring.add(NOW, "s|new", tokens, "story-7")
    assert ring.survivor(tokens, THRESHOLD, NOW - WINDOW) == ("s|new", "story-7")


def test_the_ring_returns_the_best_match_not_the_first() -> None:
    ring = claude_worker.news.filter.RecentTitles()
    ring.add(NOW, "s|weak", claude_worker.news.filter.tokens("OKX delist something else here"))
    ring.add(NOW, "s|exact", claude_worker.news.filter.tokens("OKX will delist FOO swap"))
    match = ring.survivor(
        claude_worker.news.filter.tokens("OKX will delist FOO swap"), THRESHOLD, NOW - WINDOW
    )
    assert match is not None and match[0] == "s|exact"


# ---- caps ----


def test_caps_track_both_ceilings() -> None:
    caps = claude_worker.news.filter.Caps(per_source_remaining={"a": 1}, tier1_remaining=2)
    assert caps.available("a")
    caps.take("a")
    assert not caps.available("a")          # the per-source hour is spent
    assert not caps.available("unknown")    # a source with no allowance has none
    caps.per_source_remaining["a"] = 5
    caps.tier1_remaining = 0
    assert not caps.available("a")          # the daily tier-1 ceiling binds too


# ---- the order of the verdicts ----


def test_a_near_duplicate_drops_before_anything_else_is_considered() -> None:
    recent = claude_worker.news.filter.RecentTitles()
    recent.add(NOW, "press|g0", claude_worker.news.filter.tokens(_item().title))
    # Class B would otherwise pass unconditionally — the dup check is FIRST.
    verdict = _verdict(_item(class_="B", venue="okx"), recent=recent)
    assert verdict.kind == claude_worker.news.filter.TIER0_DROP_DUP
    assert verdict.dup_of == "press|g0"


def test_structure_and_venue_origin_prose_pass_without_a_keyword() -> None:
    empty = claude_worker.news.filter.build_vocabulary([], [], [])
    for item in (
        _item(class_="B", title="Scheduled system upgrade"),
        _item(class_="D", title="numbers"),
        _item(class_="C", venue="okx", title="An entirely unremarkable sentence"),
    ):
        verdict = _verdict(item, vocab=empty)
        assert verdict.passed, item.class_
        assert verdict.hits == claude_worker.news.filter.HITS_UNCHECKED


def test_prose_with_no_vocabulary_hit_is_dropped() -> None:
    verdict = _verdict(_item(title="A pleasant day in the park", text="Nothing at all."))
    assert verdict.kind == claude_worker.news.filter.TIER0_DROP_VOCAB
    assert verdict.hits == 0


def test_a_down_weighted_mill_needs_two_hits() -> None:
    vocab = claude_worker.news.filter.build_vocabulary([], [], ["delist", "withdrawals"])
    one_hit = _item(source="mill", weight=0.3, title="FOO delist rumour", text="Nothing else.")
    assert _verdict(one_hit, vocab=vocab).kind == claude_worker.news.filter.TIER0_DROP_WEIGHT
    two_hits = _item(
        source="mill", weight=0.3, title="FOO delist rumour", text="And withdrawals halted."
    )
    assert _verdict(two_hits, vocab=vocab).passed
    # A full-weight source is through on one.
    assert _verdict(_item(title="FOO delist rumour", text="Nothing else."), vocab=vocab).passed


def test_an_exhausted_cap_is_the_last_stop_and_keeps_its_hits() -> None:
    verdict = _verdict(_item(), caps=_caps(remaining=0))
    assert verdict.kind == claude_worker.news.filter.TIER0_DROP_CAP
    assert verdict.hits >= 1
    daily = _verdict(_item(), caps=_caps(remaining=10, tier1=0))
    assert daily.kind == claude_worker.news.filter.TIER0_DROP_CAP


def test_the_verdict_is_deterministic() -> None:
    vocab = _vocab("delist")
    first = _verdict(_item(), vocab=vocab)
    second = _verdict(_item(), vocab=vocab)
    assert first == second


def test_is_unconditional_matches_the_class_rule() -> None:
    assert claude_worker.news.filter.is_unconditional(_item(class_="A"))
    assert claude_worker.news.filter.is_unconditional(_item(class_="B"))
    assert claude_worker.news.filter.is_unconditional(_item(class_="D"))
    assert claude_worker.news.filter.is_unconditional(_item(class_="C", venue="okx"))
    assert not claude_worker.news.filter.is_unconditional(_item(class_="C"))


def test_the_vocabulary_survives_missing_operator_files(tmp_path: pathlib.Path) -> None:
    vocab = claude_worker.news.filter.vocabulary_from(
        tmp_path / "no-market-map.json", tmp_path / "no-logs", ["delist"]
    )
    # Narrowed to the venues plus the keyword — never an exception.
    assert "delist" in vocab.entries
    assert "binance" in vocab.entries


def test_the_asset_list_is_tickers_only_and_drops_quote_duplicates() -> None:
    """`Vocabulary.assets` is the closed list tier 1 offers a model.

    Two things it must not be. It must not carry the venue words or the
    operator's event keywords — offering "delist" as an asset is a prompt
    that lies about its own grammar. And it must not carry a base asset
    twice: measured 2026-09-20 on the real universe, 122 of 281 entries
    were a base asset repeated with `USDT`, which is 43 % of a list that
    rides EVERY tier-1 prompt and an ambiguity for a model asked to name
    the asset while offered both `AAVE` and `AAVEUSDT`.
    """
    vocab = claude_worker.news.filter.build_vocabulary(
        market_names=(
            "binance-usdm:btcusdt",
            "okx:ETH-USDT-SWAP",
            "Bitcoin up or down on August 22?",
        ),
        descriptors=(
            "binance-usdm:btcusdt",
            "okx:ETH-USDT-SWAP",
            "binance-usdm:arusdt",
            "deribit:BTC-PERPETUAL",
        ),
        keywords=("delist", "insolvency"),
    )
    assert "BTC" in vocab.assets
    assert "BTCUSDT" not in vocab.assets, "the base asset is already there"
    assert "ETH" in vocab.assets
    # A quote-suffixed name whose stem is too short to be derived at all
    # (`ar`) stays: it is the only spelling the engine has for that market.
    assert "ARUSDT" in vocab.assets
    # THE one that matters. The market map carries whole Polymarket question
    # titles; offering them — or the words inside them — as ASSETS would let
    # `parse_triage_v2` accept `AUGUST` as an asset, after which it becomes
    # part of a story key and stories cluster on it.
    for noise in ("BITCOIN UP OR DOWN ON AUGUST 22?", "AUGUST", "DOWN", "BITCOIN"):
        assert noise not in vocab.assets, noise
    assert not [a for a in vocab.assets if " " in a or "?" in a]
    # ...but tier 0's vocabulary still CARRIES all of it, which is the point
    # of the split: `entries` is unchanged, so nothing about tier 0's measured
    # behaviour moves.
    assert "bitcoin up or down on august 22?" in vocab.entries
    # Worth recording what that entry is actually WORTH, since it is the
    # reason the noise was in the asset list at all: a derived entry matches in
    # ticker form only (the D8 two-pattern rule) AND the alternation is
    # `\b`-anchored on both sides, so an entry ending in `?` can never match
    # anything. A market-map question title therefore contributes nothing to
    # tier 0 either. That is a separate question from this one and is NOT
    # changed here — but it means dropping these from the asset list costs
    # the lane nothing at all.
    assert vocab.hits("Bitcoin up or down on August 22?") == 0
    assert vocab.hits("BITCOIN UP OR DOWN ON AUGUST 22?") == 0
    # The tokens that DO earn tier-0 hits are the tickers and the venue words.
    assert vocab.hits("BTC and ETH halted on Binance after a delist notice") >= 4
    # Venue words and the operator's keywords are vocabulary, not assets.
    for word in ("BINANCE", "OKX", "DERIBIT", "DELIST", "INSOLVENCY"):
        assert word not in vocab.assets, word
    # ...but they are still in `entries`, so tier 0 is untouched.
    assert "binance" in vocab.entries and "delist" in vocab.entries
    assert vocab.hits("LINK halted on Binance after a delist notice") >= 2
    # Sorted and unique, so the prompt text is deterministic and the cache
    # key for one item does not move between cycles.
    assert list(vocab.assets) == sorted(set(vocab.assets))


# ---- the tier-2 menu (doc 03 finding 3, 2026-09-20) -----------------------


#: One of the 27 Polymarket CLOB token ids the live map carries as a NAME.
RAW_ID: str = "71423091995281421569049483578795165916750772181249002772963926241618543567909"


def test_a_raw_venue_id_is_not_a_market_name() -> None:
    assert claude_worker.news.filter.is_raw_market_id(RAW_ID)
    assert claude_worker.news.filter.is_raw_market_id(" 113671852401471742541 ")
    # A real name is never a long run of digits, and a short number is a
    # name a venue could plausibly use.
    assert not claude_worker.news.filter.is_raw_market_id("BTC-UP")
    assert not claude_worker.news.filter.is_raw_market_id("1000PEPE")
    assert not claude_worker.news.filter.is_raw_market_id("42")


def test_the_menu_offers_only_what_the_run_still_agrees_with() -> None:
    """The live shape, 2026-09-20: a perp whose name IS its descriptor, a
    token id, and four Polymarket names sharing two slots that now carry
    something else entirely — including a Fed-rates market on the same sym
    as the Bitcoin dailies. Naming one of those does not name a settled
    market, it names a DIFFERENT UNDERLYING, so none of them is offered and
    no "canonical" one is invented."""
    markets = {
        "binance-usdm:solusdt": 16_777_731,
        RAW_ID: 42,
        "Bitcoin Up or Down on August 22?": 42,
        "bitcoin-up-or-down-on-september-1-2026": 42,
        "Will the Fed decrease interest rates by 25 bps at the next meeting?": 42,
        "Ethereum Up or Down on September 1?": 3,
    }
    descriptors = {
        16_777_731: "binance-usdm:solusdt",
        42: "77000913001596421533092826130547686125108595735288460407310406855818448852926",
        3: "54917514915978021107540776117639596739424210253104427531373723820109308652540",
    }
    menu = claude_worker.news.filter.tradeable_markets(markets, descriptors)
    assert menu == {"binance-usdm:solusdt": 16_777_731}
    # A name returns to the menu the moment the run agrees with it again.
    agreed = dict(descriptors)
    agreed[3] = "Ethereum Up or Down on September 1?"
    assert claude_worker.news.filter.tradeable_markets(markets, agreed) == {
        "binance-usdm:solusdt": 16_777_731,
        "Ethereum Up or Down on September 1?": 3,
    }
    assert list(menu) == sorted(menu), "deterministic on any machine"


def test_a_raw_id_is_refused_even_when_the_run_agrees_with_it() -> None:
    """A token id that IS the live descriptor still buys nothing: no model
    can match 70 digits to a headline, and 27 of them cost 2165 tokens."""
    menu = claude_worker.news.filter.tradeable_markets({RAW_ID: 42}, {42: RAW_ID})
    assert menu == {}


def test_an_unreadable_manifest_narrows_the_menu_but_never_empties_it() -> None:
    """`None` means "no manifest", not "nothing agrees". Failing every name
    would make each tier-2 answer a null — a silent, total outage."""
    markets = {"binance-usdm:solusdt": 16_777_731, RAW_ID: 42,
               "Bitcoin Up or Down on August 22?": 42}
    menu = claude_worker.news.filter.tradeable_markets(markets, None)
    assert menu == {
        "binance-usdm:solusdt": 16_777_731,
        "Bitcoin Up or Down on August 22?": 42,
    }
    assert claude_worker.news.filter.tradeable_markets({}, None) == {}


def test_the_menu_from_a_missing_map_is_empty_not_an_error(
    tmp_path: pathlib.Path,
) -> None:
    menu = claude_worker.news.filter.tradeable_markets_from(
        tmp_path / "absent.json", tmp_path / "no-runs"
    )
    assert menu == {}
    assert claude_worker.news.filter.descriptors_from(tmp_path / "no-runs") is None
