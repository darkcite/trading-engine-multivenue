# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Tier 0 — the free filter every item passes before a model sees it (spec §7).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

Tier 0 is what makes the cascade affordable: it is pure, deterministic and
costs nothing, and roughly 60 % of a day's items stop here. The ORDER is
law — the first stop wins and is the verdict recorded on the row, so a
later reader can always say WHY an item never reached Haiku:

1. exact dedupe      the ``items`` PK (``INSERT OR IGNORE``) — not a verdict,
                     a re-polled feed simply stores nothing twice
2. near-duplicate    title-token Jaccard against the recent ring -> ``drop_dup``
3. class rule        class A/B/D, and class C from a venue-origin source,
                     pass unconditionally (``hits = -1``)
4. vocabulary        no derived asset/venue/keyword hit -> ``drop_vocab``
5. weight            a down-weighted mill needs two hits -> ``drop_weight``
6. caps              per-source hourly and the tier-1 daily ceiling -> ``drop_cap``

Corroboration counts through duplicates and model calls do not: a near-dup
lifts its survivor's ``item_count`` (and ``origins``, when the publisher is
new to that story) without ever buying a triage call.
"""

import dataclasses
import pathlib
import re
import typing

import claude_worker.cli
import claude_worker.features
import claude_worker.iv_digest
import claude_worker.news.sources
import claude_worker.news.store

TIER0_PASS: str = claude_worker.news.store.TIER0_PASS
TIER0_DROP_DUP: str = claude_worker.news.store.TIER0_DROP_DUP
TIER0_DROP_VOCAB: str = claude_worker.news.store.TIER0_DROP_VOCAB
TIER0_DROP_WEIGHT: str = claude_worker.news.store.TIER0_DROP_WEIGHT
TIER0_DROP_CAP: str = claude_worker.news.store.TIER0_DROP_CAP

#: A class-C item from a down-weighted source needs this many distinct
#: vocabulary hits to survive (spec §7.5): one hit is a passing mention.
MILL_MIN_HITS: int = 2
#: Verdict ``hits`` for an item that never reached the vocabulary step.
HITS_UNCHECKED: int = -1
#: Newest titles kept for the near-dup ring (spec §7.2).
RECENT_TITLES_MAX: int = 2_000
#: Shortest token the title tokenizer keeps.
TOKEN_MIN_LEN: int = 3
#: Shortest derived vocabulary entry kept — two-letter tickers collide with
#: ordinary English far too often to be worth their false positives.
VOCAB_MIN_LEN: int = 3
#: Until ``news-policy.toml`` is parsed (N3), the tier-1 ceiling the cycle
#: assumes. Mirrors ``[budget] tier1_calls_per_day`` in the example.
DEFAULT_TIER1_CALLS_PER_DAY: int = 600

#: The venue names are always in the vocabulary, whether or not the engine
#: currently carries an instrument from them.
VENUE_WORDS: tuple[str, ...] = (
    "binance",
    "okx",
    "deribit",
    "hyperliquid",
    "bybit",
    "polymarket",
    "kraken",
    "coinbase",
)

#: Quote currencies stripped from a joined descriptor to find its base asset
#: (``btcusdt`` -> ``btc``). Longest first: ``usdt`` must beat ``usd``.
_QUOTES: tuple[str, ...] = ("fdusd", "busd", "usdt", "usdc", "usd", "dai", "eur", "try")

#: 40 words the title tokenizer drops. Ordinary English that would otherwise
#: dominate a Jaccard between two unrelated headlines.
STOPWORDS: frozenset[str] = frozenset(
    (
        "the", "and", "for", "with", "that", "this", "from", "have", "has", "had",
        "will", "would", "can", "could", "should", "may", "might", "are", "was", "were",
        "its", "his", "her", "their", "our", "your", "not", "but", "all", "any",
        "new", "now", "how", "why", "what", "who", "when", "where", "which", "into",
    )
)

_PUNCT_RE: typing.Final[re.Pattern[str]] = re.compile(r"[^0-9a-z]+")
_WORD_RE: typing.Final[re.Pattern[str]] = re.compile(r"[0-9A-Za-z]+")
_SPLIT_RE: typing.Final[re.Pattern[str]] = re.compile(r"[-_:]+")


class Tier0Verdict(typing.NamedTuple):
    """Why an item did or did not reach tier 1.

    ``hits`` is the number of DISTINCT vocabulary entries found, or
    ``HITS_UNCHECKED`` when the verdict was reached before that step.
    """

    kind: str
    dup_of: str = ""
    hits: int = HITS_UNCHECKED

    @property
    def passed(self) -> bool:
        return self.kind == TIER0_PASS


def tokens(title: str) -> frozenset[str]:
    """Title -> comparison tokens: lowercase, punctuation stripped, short
    words and the stoplist dropped. The near-dup Jaccard's whole input."""
    out: set[str] = set()
    for part in _PUNCT_RE.split(title.lower()):
        if len(part) >= TOKEN_MIN_LEN and part not in STOPWORDS:
            out.add(part)
    return frozenset(out)


def jaccard(left: frozenset[str], right: frozenset[str]) -> float:
    """|A n B| / |A u B|; 0.0 when either side is empty (an untokenizable
    title is never anyone's duplicate)."""
    if not left or not right:
        return 0.0
    return len(left & right) / len(left | right)


def base_asset(descriptor: str) -> str:
    """``binance-usdm:btcusdt`` -> ``btc``; ``okx:BTC-USDT-SWAP`` -> ``btc``;
    a Polymarket token id -> ``""`` (a number names no asset)."""
    tail = descriptor.split(":", 1)[1] if ":" in descriptor else descriptor
    tail = tail.strip().lower()
    if not tail:
        return ""
    head = _SPLIT_RE.split(tail)[0]
    if not head.isalnum() or head.isdigit():
        return ""
    if len(head) > VOCAB_MIN_LEN:
        for i in range(len(_QUOTES)):
            quote = _QUOTES[i]
            if head.endswith(quote) and len(head) - len(quote) >= VOCAB_MIN_LEN:
                return head[: -len(quote)]
    return head if len(head) >= VOCAB_MIN_LEN else ""


@dataclasses.dataclass(frozen=True, slots=True)
class Vocabulary:
    """The words that make an item worth a model call, derived at lane start.

    Derived, never hand-maintained apart from ``[keywords] event``: the
    assets and venues come from what the engine ACTUALLY carries, so a
    universe edit moves the vocabulary with it and a name we stopped
    trading stops buying triage calls.

    TWO patterns, because case carries meaning here (measured 2026-09-19 on
    the first live cycle over the real universe). Matching every derived name
    case-insensitively passed 88 % of items: the 122-perp universe
    contributes LINK, NEAR, SAND, PEOPLE, GAS and MOVE, which are ordinary
    English, and the market map contributes whole Polymarket question
    titles, which put ``the`` and ``will`` in the vocabulary. So:

    * the operator's keywords and the venue names match case-INsensitively —
      they are words, and prose capitalises them however it likes;
    * every DERIVED name matches only in TICKER form (uppercase), which is
      how financial prose writes one. "the link to move people" hits
      nothing; "LINK halted on Binance" hits three.
    """

    entries: frozenset[str]
    pattern: re.Pattern[str] | None
    ticker_pattern: re.Pattern[str] | None
    #: The DERIVED names in ticker form, sorted — the closed asset list the
    #: tier-1 prompt offers a model and `parse_triage_v2` validates its
    #: answer against (spec §9.1). Deliberately NOT ``entries``: that also
    #: carries the venue words and the operator's event keywords, and
    #: offering "delist" as an asset would be a prompt that lies. Defaulted
    #: so a test may build a vocabulary with no assets at all.
    #:
    #: Derived from the INSTRUMENT MANIFEST only, in ticker form, sorted,
    #: without the quote-suffixed duplicate of a base asset already present.
    #:
    #: Manifest-only is the load-bearing part. `entries` also derives from the
    #: market map, which carries whole Polymarket QUESTION TITLES — measured
    #: 2026-09-20 on the operator's real universe, offering `entries` as the
    #: tier-1 asset list put `BITCOIN UP OR DOWN ON AUGUST 22?` and the bare
    #: words `AFTER`, `AUGUST`, `DECREASE`, `MEETING`, `RATES` into a closed
    #: list of ASSETS — and `parse_triage_v2` would have accepted `AUGUST` as
    #: one, after which it becomes part of a story KEY and stories cluster on
    #: it. The manifest is where `base_asset` already strips properly: 130
    #: clean tickers instead of 159 entries of which 11 were sentences.
    #:
    #: Market-map names stay in `entries` and both patterns, untouched: tier 0
    #: SHOULD pass an item about the Fed, and that is what those titles buy.
    #: Quote-suffixed duplicates are dropped too (`BTCUSDT` when `BTC` is
    #: present): 122 of the first 281 entries were a base asset repeated with
    #: `USDT`, an ambiguity for a model asked to name "the asset" while
    #: offered both. A quote-suffixed name whose stem is too short to be
    #: derived at all (`ARUSDT`, stem `ar`) STAYS — it is the engine's only
    #: spelling for that market.
    assets: tuple[str, ...] = ()

    def hits(self, text: str) -> int:
        """Distinct vocabulary entries in ``text``. Word-boundary on BOTH
        sides is deliberate: it is what keeps ``listing`` out of
        ``delisting`` and ``eth`` out of ``ethics``."""
        found: set[str] = set()
        if self.pattern is not None:
            for match in self.pattern.finditer(text):
                found.add(match.group(0).lower())
        if self.ticker_pattern is not None:
            for match in self.ticker_pattern.finditer(text):
                found.add(match.group(0).lower())
        return len(found)


def _longest_first(entry: str) -> tuple[int, str]:
    return (-len(entry), entry)


def _alternation(entries: typing.AbstractSet[str], upper: bool) -> re.Pattern[str] | None:
    """One word-boundary alternation over the entries, longest first so a
    multi-word entry wins over its own first word."""
    if not entries:
        return None
    ordered = sorted(entries, key=_longest_first)
    parts: list[str] = []
    for i in range(len(ordered)):
        parts.append(re.escape(ordered[i].upper() if upper else ordered[i]))
    flags = re.NOFLAG if upper else re.IGNORECASE
    return re.compile(r"\b(?:" + "|".join(parts) + r")\b", flags)


def _keep_derived(token: str) -> bool:
    """A derived name must carry information: a stopword does not, and a
    Polymarket token id (a 77-digit number) can never appear in prose."""
    return len(token) >= VOCAB_MIN_LEN and token not in STOPWORDS and not token.isdigit()


def build_vocabulary(
    market_names: typing.Iterable[str],
    descriptors: typing.Iterable[str],
    keywords: typing.Iterable[str],
) -> Vocabulary:
    """Assemble the vocabulary from the three derived sources plus the
    operator's event keywords (spec §7.4)."""
    derived: set[str] = set()
    tradable: set[str] = set()
    words: set[str] = set()
    for name in market_names:
        for part in _SPLIT_RE.split(str(name).strip().lower()):
            if _keep_derived(part):
                derived.add(part)
    for descriptor in descriptors:
        if _keep_derived(base_asset(str(descriptor))):
            derived.add(base_asset(str(descriptor)))
            tradable.add(base_asset(str(descriptor)))
    for i in range(len(VENUE_WORDS)):
        words.add(VENUE_WORDS[i])
    for keyword in keywords:
        cleaned = str(keyword).strip().lower()
        if cleaned:
            words.add(cleaned)
    # A name that is BOTH a venue and a derived token stays a word: the venue
    # is what an article means by it.
    derived -= words
    tradable -= words
    return Vocabulary(
        entries=frozenset(derived | words),
        pattern=_alternation(words, upper=False),
        ticker_pattern=_alternation(derived, upper=True),
        assets=asset_list(tradable),
    )


def asset_list(derived: typing.AbstractSet[str]) -> tuple[str, ...]:
    """[`Vocabulary.assets`] from the derived tokens: ticker form, sorted,
    without the quote-suffixed duplicate of a base asset already present."""
    out: list[str] = []
    for entry in sorted(derived):
        keep = True
        for i in range(len(_QUOTES)):
            quote = _QUOTES[i]
            stem = entry[: -len(quote)]
            if entry.endswith(quote) and len(stem) >= VOCAB_MIN_LEN and stem in derived:
                keep = False
        if keep:
            out.append(entry.upper())
    return tuple(out)


def vocabulary_from(
    paths_market_map: pathlib.Path,
    replay_dir: pathlib.Path,
    keywords: typing.Iterable[str],
) -> Vocabulary:
    """[`build_vocabulary`] over the operator's real inputs. Every read is
    best-effort: a missing market map or run dir narrows the vocabulary, it
    never stops a cycle."""
    market_names: list[str] = []
    try:
        market_names = list(claude_worker.cli.load_market_map(paths_market_map).markets)
    except (OSError, ValueError):
        market_names = []
    descriptors: list[str] = []
    run_dir = claude_worker.features.latest_run_dir(replay_dir)
    if run_dir is not None:
        manifest = claude_worker.iv_digest.read_manifest(run_dir)
        if manifest is not None:
            descriptors = sorted(set(manifest[0].values()))
    return build_vocabulary(market_names, descriptors, keywords)


class RecentTitles:
    """The near-dup ring: the tokenized titles of recent tier-0 survivors.

    Bounded by both time and count so a busy day cannot make tier 0 O(n^2)
    over a growing table.
    """

    def __init__(self, max_entries: int = RECENT_TITLES_MAX) -> None:
        self._max = max_entries
        self._rows: list[tuple[int, str, frozenset[str], str]] = []

    def add(self, ts: int, key: str, title_tokens: frozenset[str], story_id: str = "") -> None:
        self._rows.append((ts, key, title_tokens, story_id))
        if len(self._rows) > self._max:
            del self._rows[: len(self._rows) - self._max]

    def __len__(self) -> int:
        return len(self._rows)

    def survivor(
        self, title_tokens: frozenset[str], threshold: float, since_ts: int
    ) -> tuple[str, str] | None:
        """The best match at or above ``threshold`` within the window, as
        ``(key, story_id)``; ``None`` when the item is new."""
        best_score = threshold
        best: tuple[str, str] | None = None
        for i in range(len(self._rows) - 1, -1, -1):
            ts, key, other, story_id = self._rows[i]
            if ts < since_ts:
                continue
            score = jaccard(title_tokens, other)
            if score >= best_score:
                best_score = score
                best = (key, story_id)
        return best


@dataclasses.dataclass(slots=True)
class Caps:
    """The two ceilings tier 0 enforces, as remaining allowances.

    Held as REMAINING rather than as limits so the cycle can seed them from
    what today already spent and the filter stays pure.
    """

    per_source_remaining: dict[str, int] = dataclasses.field(default_factory=dict)
    tier1_remaining: int = DEFAULT_TIER1_CALLS_PER_DAY

    def available(self, source: str) -> bool:
        return self.tier1_remaining > 0 and self.per_source_remaining.get(source, 0) > 0

    def take(self, source: str) -> None:
        self.tier1_remaining -= 1
        self.per_source_remaining[source] = self.per_source_remaining.get(source, 0) - 1


def is_unconditional(item: claude_worker.news.sources.Item) -> bool:
    """Spec §7.3 — structure and venue-origin prose never need a keyword.

    A venue announcing its own delisting must reach tier 1 whether or not
    the wording happens to contain a word we thought of in advance.
    """
    return item.class_ != "C" or bool(item.venue)


def tier0(  # noqa: PLR0913
    item: claude_worker.news.sources.Item,
    *,
    vocab: Vocabulary,
    recent: RecentTitles,
    caps: Caps,
    now_ts: int,
    near_dup_jaccard: float,
    near_dup_window_s: int,
) -> Tier0Verdict:
    """The verdict for one item. Pure and deterministic — the same inputs
    give the same verdict, which is what makes the funnel auditable.

    Wide by design: every input is a collaborator the caller owns, and
    passing them explicitly is what keeps this function free of hidden
    state — the property the determinism test rests on.
    """
    title_tokens = tokens(item.title)
    match = recent.survivor(title_tokens, near_dup_jaccard, now_ts - near_dup_window_s)
    if match is not None:
        return Tier0Verdict(TIER0_DROP_DUP, match[0], HITS_UNCHECKED)
    if is_unconditional(item):
        return Tier0Verdict(TIER0_PASS, "", HITS_UNCHECKED)
    hits = vocab.hits(item.title + " " + item.text)
    if hits == 0:
        return Tier0Verdict(TIER0_DROP_VOCAB, "", 0)
    if item.weight < claude_worker.news.sources.ORIGIN_WEIGHT_MIN and hits < MILL_MIN_HITS:
        return Tier0Verdict(TIER0_DROP_WEIGHT, "", hits)
    if not caps.available(item.source):
        return Tier0Verdict(TIER0_DROP_CAP, "", hits)
    return Tier0Verdict(TIER0_PASS, "", hits)


def title_words(text: str) -> list[str]:
    """Word-ish runs of a title (a diagnostic surface for `report`)."""
    return _WORD_RE.findall(text)
