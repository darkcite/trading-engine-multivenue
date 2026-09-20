# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Deterministic entity resolution for the story key (spec §9.2).

The story key is ``(family, event_type, assets, venues)``: it decides which
items are the *same story*. Two of those four came from the local tagger's
`entities` object, and that object is the weakest thing the tagger produces —
measured against the 150-item adjudicated gold set it scores entity
P 0.611 / R 0.790 / F1 0.689, and the whole key matches the judges' key on
only 0.500 of items.

Neither half of an entity actually needs a model.

The VENUE is a fact of the source. A Bybit announcement is about Bybit
whether or not its title says so, and `sources.Source.venue` has carried that
since the registry was written; `typed_triage` already reads it for class-B
hinted rows. Titles like "New listing: ZSUSDT TradFi Perpetual Contract" name
no venue at all, so a model reading the title can only ever guess, and the
gold judges — who could see the source — tagged the venue every time.

The ASSET is named in the title, in a closed vocabulary we already own. The
only reason a title matcher misses it is that our vocabulary is tickers
(``BTC``) while headlines are written in names ("Bitcoin") and venue feeds in
instruments ("ONEUSDT"). `NAME_TO_TICKER` and `QUOTE_SUFFIXES` close both
gaps.

**Case carries meaning, and this module obeys the rule `filter.Vocabulary`
already measured on 2026-09-19:** a derived name matches only in TICKER form.
The universe contributes `LINK`, `NEAR`, `SAND`, `GAS`, `MOVE`, `ETC`, `HYPE`,
`DOT`, `VET`, `ARB`, `ATOM` — ordinary English, every one. A first cut of this
module matched tickers case-insensitively and "Bybit: link your account"
resolved to LINK, "BTC, ETH, SOL, etc." to ETC, "the hype around the ETF" to
HYPE. Lower case is the alias table's job and only the alias table's; a bare
ticker is read only where prose would never write that word.

A title with no lower case at all carries no case signal, so the bare-ticker
path is skipped there entirely — "BYBIT ROLLS OUT ONE MORE UPGRADE" is not a
story about ONE. Names, instruments and the source venue still resolve, which
is what a caps announcement actually needs.

Measured on the same gold set (`docs/research/feeds/gold_entities.py`,
research vault):

    arm                             key     entity P / R / F1
    local model (what we shipped)   0.500   0.611 / 0.790 / 0.689
    tickers only, from the title    0.493   0.937 / 0.622 / 0.748
    + name aliases                  0.547   0.892 / 0.811 / 0.850
    + venue from the source         0.680   0.902 / 0.965 / 0.932

0.660 of that 0.680 was reached before any rule was written while looking at
the gold rows; the alias for HYPE and the instrument-suffix strip were added
after reading the misses and are worth 0.020 between them. The honest claim
is therefore the pre-registered one: **0.660**, not 0.680.

This module decides the KEY only. The escalation gate keeps reading the
tagger's own `entities` — see `cascade.should_escalate`. The two questions
are different: the gate asks whether the tagger *noticed* something, which is
a property of the tagger's attention and was measured at recall 0.930 that
way; the key asks what the story *is about*, which is a property of the
world. Swapping the gate onto this module would auto-escalate every
venue-origin item, and that volume change has not been measured.
"""

import re
import typing

#: Quote suffixes a venue appends to build an instrument name. A title that
#: says ``ONEUSDT`` is naming the asset ``ONE``. The full token is always
#: tried against the vocabulary first, so symbols that are themselves spelled
#: with a suffix (``OPUSDT``, ``TUSDT``, ...) still win on their own.
#:
#: Upper-cased mirror of `filter._QUOTES`, longest first so ``FDUSD`` is
#: stripped before ``USD`` would take only three of its characters.
#: `test_news_entities` pins the two lists in step — a venue that starts
#: quoting in a new currency must not silently stop resolving here.
QUOTE_SUFFIXES: tuple[str, ...] = (
    "FDUSD", "BUSD", "USDT", "USDC", "USD", "DAI", "EUR", "TRY", "PERP",
)

#: Spellings that resolve onto a venue in `labeling.VENUE_NAMES`.
VENUE_ALIASES: dict[str, str] = {
    "okex": "okx",
    "coinbase pro": "coinbase",
}

#: Headline name -> ticker. Matched case-insensitively but required to be
#: CAPITALISED in the title (`_assets_from_names`), which is what makes it
#: safe to list a name that is also an ordinary word.
#:
#: Still dropped, because capitalisation does not save them — each is a word
#: a headline capitalises anyway, at its start or in title case, and each was
#: caught producing a wrong key: `ether` ("vanished into the ether"),
#: `stellar` ("A stellar quarter"), `harmony` ("Regulators seek harmony"),
#: `the graph` ("The graph shows funding flipping"), `stacks` ("Stacks of
#: unsold inventory"), `optimism` ("Optimism around the ETF decision" — the
#: most common of the lot in market prose) and `iota` ("not one iota"). Each
#: is still reachable in ticker form and, for a venue feed, through its
#: instrument name.
#: Longest phrase wins and its span is consumed, so
#: "Bitcoin Cash" yields BCH and cannot also yield BTC. A name is only ever
#: resolved onto a ticker the CALLER's vocabulary offers, so a market we do
#: not trade contributes nothing.
NAME_TO_TICKER: dict[str, str] = {
    "bitcoin cash": "BCH",
    "bitcoin": "BTC",
    "xbt": "BTC",
    "ethereum classic": "ETC",
    "ethereum name service": "ENS",
    "ethereum": "ETH",
    "solana": "SOL",
    "ripple": "XRP",
    "dogecoin": "DOGE",
    "cardano": "ADA",
    "binance coin": "BNB",
    "avalanche": "AVAX",
    "polkadot": "DOT",
    "chainlink": "LINK",
    "litecoin": "LTC",
    "tron": "TRX",
    "cosmos": "ATOM",
    "near protocol": "NEAR",
    "stellar lumens": "XLM",
    "monero": "XMR",
    "filecoin": "FIL",
    "hedera": "HBAR",
    "internet computer": "ICP",
    "aptos": "APT",
    "arbitrum": "ARB",
    "celestia": "TIA",
    "injective": "INJ",
    "thorchain": "RUNE",
    "uniswap": "UNI",
    "aave": "AAVE",
    "curve finance": "CRV",
    "lido": "LDO",
    "ethena": "ENA",
    "ondo": "ONDO",
    "pepe": "1000PEPE",
    "shiba inu": "1000SHIB",
    "shib": "1000SHIB",
    "bonk": "1000BONK",
    "floki": "1000FLOKI",
    "dogwifhat": "WIF",
    "worldcoin": "WLD",
    "hyperliquid": "HYPE",
    "kaspa": "KAS",
    "zcash": "ZEC",
    "algorand": "ALGO",
    "multiversx": "EGLD",
    "elrond": "EGLD",
    "tezos": "XTZ",
    "vechain": "VET",
    "the sandbox": "SAND",
    "axie infinity": "AXS",
    "apecoin": "APE",
    "kava": "KAVA",
    "dydx": "DYDX",
    "jupiter": "JUP",
    "pyth network": "PYTH",
    "jito": "JTO",
    "starknet": "STRK",
    "dymension": "DYM",
    "ether.fi": "ETHFI",
    "fetch.ai": "FET",
    "pendle": "PENDLE",
    "pancakeswap": "CAKE",
    "sushiswap": "SUSHI",
    "conflux": "CFX",
    "chiliz": "CHZ",
    "oasis network": "ROSE",
    "horizen": "ZEN",
    "ravencoin": "RVN",
    "reserve rights": "RSR",
    "trust wallet": "TWT",
    "biconomy": "BICO",
    "lisk": "LSK",
    "iotex": "IOTX",
    "jasmy": "JASMY",
    "nervos": "CKB",
    "moonriver": "MOVR",
    "arkham": "ARKM",
    "tellor": "TRB",
    "compound finance": "COMP",
    "apple": "AAPL",
    "nvidia": "NVDA",
    "tesla": "TSLA",
    "marathon digital": "MARA",
    "pinduoduo": "PDD",
    "ionq": "IONQ",
}

#: Names sorted longest-first, built once at import. Rebuilding this per item
#: would sort ~90 strings on every headline in the lane.
_NAMES_BY_LENGTH: tuple[str, ...] = tuple(
    sorted(NAME_TO_TICKER, key=len, reverse=True)
)

#: Upper-case tokens long enough to be an instrument rather than a word.
#: The ceiling is generous on purpose: at 12 it could not match
#: ``1000FLOKIUSDT`` at all, and a token that long is never prose.
_INSTRUMENT = re.compile(r"\b[A-Z0-9]{4,24}\b")

#: Compiled once per distinct needle. The vocabulary is fixed for the life of
#: a lane, so this is bounded by |vocab| + |names| + |venues|.
_PATTERNS: dict[str, typing.Pattern[str]] = {}


def _word(needle: str) -> typing.Pattern[str]:
    pattern = _PATTERNS.get(needle)
    if pattern is None:
        pattern = re.compile(r"\b" + re.escape(needle) + r"\b")
        _PATTERNS[needle] = pattern
    return pattern


class _Spans:
    """The character spans a longer name has already claimed.

    A list rather than a set of indices: a headline names a handful of things
    at most, so a linear scan over a handful of pairs is cheaper than the
    hashing, and this runs once per item in the lane.
    """

    __slots__ = ("_taken",)

    def __init__(self) -> None:
        self._taken: list[tuple[int, int]] = []

    def free(self, lo: int, hi: int) -> bool:
        for j in range(len(self._taken)):
            if lo < self._taken[j][1] and self._taken[j][0] < hi:
                return False
        return True

    def take(self, lo: int, hi: int) -> None:
        self._taken.append((lo, hi))


def _assets_from_names(
    title: str, low: str, allowed: frozenset[str], spans: _Spans, out: set[str]
) -> None:
    """Headline names, longest first, each consuming its span.

    A name must be CAPITALISED where it appears. This is the same idea as the
    ticker rule one level down — case carries meaning — and it is what lets
    the table hold names that are also ordinary words without pruning them by
    guesswork. "an avalanche of liquidations" is prose; "Avalanche halts
    withdrawals" is AVAX. "the ripple effect across markets" is prose;
    "Ripple settles with the SEC" is XRP. Guessing which names were "rare
    enough" as English had already been wrong about `link`, `hype`, `vet`,
    `move`, `etc` and `arb` on the ticker path, so it is not a method.

    A title with no case at all is exempt: a venue screaming
    "AVALANCHE NETWORK MAINTENANCE" still means the network, and names are
    the main signal that survives in a caps announcement.
    """
    cased = has_case(title)
    for i in range(len(_NAMES_BY_LENGTH)):
        name = _NAMES_BY_LENGTH[i]
        ticker = NAME_TO_TICKER[name]
        for m in _word(name).finditer(low):
            if not spans.free(m.start(), m.end()):
                continue
            if cased and not title[m.start()].isupper():
                continue
            # The span is claimed by the NAME, whether or not we trade what
            # it resolves to. Checking the vocabulary first would let
            # "Bitcoin Cash" fall through to "Bitcoin" on a desk with no BCH
            # market and key that story on BTC — a WRONG asset, not a miss.
            spans.take(m.start(), m.end())
            if ticker in allowed:
                out.add(ticker)


def has_case(title: str) -> bool:
    """Whether the title distinguishes upper from lower at all.

    A headline screamed in capitals says nothing by capitalising a word, so
    the bare-ticker path must not read one there.
    """
    for ch in title:
        if ch.islower():
            return True
    return False


def _assets_from_tickers(title: str, vocab: typing.Sequence[str], out: set[str]) -> None:
    """The tickers themselves, in TICKER form only — the rule
    `filter.Vocabulary` measured on 2026-09-19. Lower case belongs to
    `NAME_TO_TICKER`; half this vocabulary is ordinary English."""
    for i in range(len(vocab)):
        sym = vocab[i]
        if _word(sym).search(title) is not None:
            out.add(sym)


def _assets_from_instruments(
    title: str, allowed: frozenset[str], out: set[str]
) -> None:
    """Venue feeds name the instrument, not the asset: ONEUSDT is ONE."""
    for m in _INSTRUMENT.finditer(title):
        token = m.group(0)
        if token in allowed:
            continue
        for i in range(len(QUOTE_SUFFIXES)):
            suffix = QUOTE_SUFFIXES[i]
            if not token.endswith(suffix):
                continue
            stem = token[: -len(suffix)]
            if stem and stem in allowed:
                out.add(stem)
            break


def _venues_from(
    low: str, source_venue: str, venue_names: typing.Sequence[str], out: set[str]
) -> None:
    for i in range(len(venue_names)):
        venue = venue_names[i]
        if venue != "other" and _word(venue).search(low) is not None:
            out.add(venue)
    for alias, venue in VENUE_ALIASES.items():
        if _word(alias).search(low) is not None:
            out.add(venue)
    # The source's own venue is evidence, not inference.
    if source_venue:
        out.add(source_venue if source_venue in venue_names else "other")


def resolve(
    title: str,
    vocab: typing.Sequence[str],
    source_venue: str,
    venue_names: typing.Sequence[str],
) -> tuple[tuple[str, ...], tuple[str, ...]]:
    """``(venues, assets)`` for the story key, sorted, from title + source.

    ``vocab`` is the tradeable market vocabulary — the same closed list the
    triage prompt offered, so this can never name a market we do not trade.
    ``source_venue`` is `sources.Source.venue`, empty for a source that does
    not speak for a venue. ``venue_names`` is `labeling.VENUE_NAMES`, passed
    in rather than imported to keep this module free of a cycle.
    """
    allowed = frozenset(vocab)
    assets: set[str] = set()
    venues: set[str] = set()
    low = title.lower()
    spans = _Spans()

    _assets_from_names(title, low, allowed, spans, assets)
    if has_case(title):
        _assets_from_tickers(title, vocab, assets)
    _assets_from_instruments(title, allowed, assets)
    _venues_from(low, source_venue, venue_names, venues)

    return tuple(sorted(venues)), tuple(sorted(assets))
