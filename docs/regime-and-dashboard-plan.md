# Market regime + regime labels + regime-aware AI aggregation + dashboard — plan (RG0–RG7)

**Status: IN PROGRESS — RG0–RG2 committed (`77f5ea5`); RG3 code-complete
2026-09-05 (uncommitted; operator commits — §12 entry). Next: the RG2/RG3
live smoke (operator: relink + `regime.toml` + restart; a labelled
ruleset staged/committed live — §7 RG3 exit tell), then RG5 (worker
regime lane) or RG4 (library + composer); §11 defaults apply until
overridden.** Pre-Stage-3 work by operator ruling ("before going to stage 3 we
need to implement the following"). Paper mode only; no
dispatcher/signer/RiskGate/live-ramp work — the Stage-3 ENTRY GATE
(`docs/arch/mvp-completion-plan.md` §7) is untouched. Owner doc for the
work; progress entries go to §12.

Operator decisions already taken (AskUserQuestion, 2026-09-03):

| # | decision | ruling |
|---|---|---|
| D1 | where the regime is computed | **Both** — MEASURED in the engine (zero-alloc, from live ticks) and DECLARED by the AI/worker; effective = declared if fresh else measured |
| D2 | how AI rulesets are regime-gated | **Row-level regime mask in the artifact** (wire-additive in `RuleRowV2._pad3`); regime change = NO table flip |
| D3 | what happens when a label turns OFF while positions are held | **Per-strategy choice** (`off = soft \| hard`) |
| D4 | dashboard form | **Both** — local web page (engine `/state` JSON + worker-served page) and TUI panels |

---

## 1. Why

1. Every strategy the engine can run today is regime-blind. The xv family
   was ruled dead on a ~300 ms lead the executor cannot capture; carry is
   dark; ICDP is negative at tier — and none of those verdicts say *in
   which market state* the edge existed. The nightly report has no regime
   axis, so "dead everywhere" and "dead in chop, alive in trend" are
   indistinguishable.
2. The AI strategist (semi-manual §4 today, `serve` later) proposes
   rulesets against the whole capture. It has no regime input, no regime
   output, and no library to compose from — every session starts from the
   grammar and the last thesis line in `state.db`.
3. Operational visibility is `/metrics` (73 names, 5 s mirror), a TUI
   whose market panel is permanently empty (`recent_tob` is never filled,
   `crates/cli/src/paper.rs` ~4106), the nightly `pnl-<day>.json`, and
   `grep` over boot logs. There is no single place that answers "what is
   running, under what mode, with which ruleset, making what".

## 2. Doctrine (laws for this work)

1. **The regime is a gate, not a signal.** It decides *whether* a
   strategy may enter; it never sizes, prices, or times an order. A
   strategy that wants the regime as a feature reads it through the
   feature engine (§4.7), not through the gate.
2. **Measured in the engine, declared by the AI, effective by law.**
   `effective = declared` while the declared word is fresh (its own
   TTL), else `measured`; while neither is valid (warm-up) the word is
   `UNKNOWN`. The engine never trusts a stale declaration and never
   waits for one.
3. **Fail-closed for labelled things, open for unlabelled things.** A row
   or strategy that carries a regime label does not trade under
   `UNKNOWN` unless its label says so. A row/strategy with no label
   (`mask == 0`, every artifact and every coded strategy that exists
   today) is unconstrained — bit-identical behaviour to today.
4. **Regime changes never flip a table.** VM rows carry their own mask;
   the composed table stays committed across regime changes. A
   re-commit happens only when the *membership* changes (§5.3), because
   a flip flattens VM position state and cold-starts every rolling
   window (`VmStrategy::on_table_flipped`).
5. **Exits are never gated.** Soft-off blocks entries and lets the
   strategy's own exit law drain the position; hard-off blocks entries
   and forces the strategy's flatten path. Halt stays the separate,
   sticky lever it is today.
6. **One integer law, two implementations, one parity fixture.** The
   engine's integer regime evaluator (`core-regime`) and the worker's
   Python reference (`claude_worker.regime`) compute the same words from
   the same minute closes; a committed fixture pins them (the ICDP
   feature-law precedent).
7. **Windows stay ≤ 2 h** (`docs/venue-time-capture-plan.md` §6.1). Regime
   *warm-up* comes from `candles.db` (existing derived data — a seed, not
   a capture window); gates are stated in windows N and per-window
   counts, never hours.
8. **Research substance stays out of git.** Labels, theses, library
   membership and per-regime P&L live under `~/multivenue/` and the
   research vault. This doc records laws and shapes only.
9. **No observability stack.** The dashboard is one static HTML file
   served on `127.0.0.1` by the worker plus one hand-rolled JSON route on
   the engine's existing `127.0.0.1:9191` server. No Prometheus, no
   Grafana, no CDN, no framework, no cloud.
10. **Hot-path cost budget:** one store per tick for regime-member
    symbols, O(members) work once per wall minute, one `AND + CMP` per
    row evaluation. Zero allocation after boot (bench gate).

## 3. The regime model

### 3.1 Dimensions (operator's formulas, integer form)

All prices are mid ×1e6 (`i64`). Returns are bps ×1e9
(`ret_bps_1e9(from, to)` — today in `strategy-icdp`; moves to
`core-regime` and ICDP imports it, no duplicate). Ratios are ×1e9. `W` is
the profile's window (§3.2); "minute close" = last mid of the wall minute
(the `strategy-vm::features::RollEntry` sampling law).

| dim | values (one-hot) | law | inputs |
|---|---|---|---|
| **TREND** | `BEAR / NEUTRAL / BULL` | `r = ret_W(BTC)`; `up = #{s : ret_W(s) > +thr}`, `dn = #{s : ret_W(s) < −thr}` over the breadth set (N syms). BULL ⇔ `r > +thr ∧ up·1e9 ≥ q·N`; BEAR ⇔ `r < −thr ∧ dn·1e9 ≥ q·N`; else NEUTRAL | BTC ref + breadth set minute closes |
| **SHAPE** (trend vs chop) | `CHOP / MIXED / TREND` | `ER_1e9 = |p_now − p_{now−W}| · 1e9 / Σ_k |p_k − p_{k−1}|`, k over 5-minute closes inside W (den 0 ⇒ ABSENT). CHOP ⇔ `ER < er_lo`, TREND ⇔ `ER > er_hi`, else MIXED | BTC minute closes (5 m = every 5th minute) |
| **VOL** | `LOW / NORMAL / HIGH` | `RV_W = isqrt(Σ r_k²)`, `r_k` = 1-minute bps return inside W (running sum, `i128`). LOW ⇔ `RV < p30`, HIGH ⇔ `RV > p70`; the percentiles are **params** (worker-computed over its lookback, §5.1), not engine state | BTC minute closes + params |
| **FUND_SIGN** | `NEG / POS` | sign of the latest funding print of the funding-reference instrument | `ChannelEvent{Funding}.v0` latch |
| **FUND_LEVEL** | `LOW / NORMAL / HIGH` | current rate vs p30/p70 of the last N prints (params, worker-computed) — "positioning / crowding" | latch + params |
| **STRETCH** | `EXT_DOWN / NEUTRAL / EXT_UP` | `stretch_1e9 = ret_W(BTC) · 1e9 / RV_W` (RV 0 ⇒ ABSENT). EXT_UP ⇔ `> +k`, EXT_DOWN ⇔ `< −k` | BTC |
| **REL** (per symbol, §3.4) | `LAGGING / INLINE / LEADING` | `rel_1e9(s) = ret_W(s) − ret_W(BTC)`; LAGGING ⇔ `< −thr_rel`, LEADING ⇔ `> +thr_rel` | breadth set |

Every threshold is a parameter of the profile (§3.2), with enter/exit
bands for hysteresis (§3.5). The reference instruments (BTC price ref,
funding ref) and the breadth set are descriptors in `regime.toml`,
resolved at boot against the live universe through
`ingress_ai::DescriptorTable` — the D-6 truth that `icdp.toml` already
uses.

### 3.2 Horizon profiles

"Интервалы зависят от стратегии" ⇒ the regime is evaluated at a small
fixed set of profiles; every label names the profile it reads.

| profile | id | TREND W | SHAPE W (5 m steps) | VOL W | STRETCH W | REL W | FUND N prints |
|---|---|---|---|---|---|---|---|
| `fast` | 0 | 1 h | 1 h / 12 | 1 h | 1 h | 1 h | 9 (3 d of 8 h) |
| `slow` | 1 | 4 h | 4 h / 48 | 4 h | 4 h | 4 h | 90 (30 d) |

Two profiles in v1 (`REGIME_PROFILES = 2`; the word array is sized 4 so
adding a third is a config change, not a layout change). The operator's
formulas name 1 h for ER and 4 h for stretch/relative; those are the
`fast`/`slow` defaults above — **open question §11-Q3**.

### 3.3 `RegimeWord` and `RegimeLabel` — encoding + match law

```rust
/// One-hot per dimension, one byte per dimension. `#[repr(transparent)]`.
pub struct RegimeWord(pub u64);
// byte 0 TREND      bit0 BEAR   bit1 NEUTRAL bit2 BULL       bit7 UNKNOWN (0x80)
// byte 1 SHAPE      bit0 CHOP   bit1 MIXED   bit2 TREND      bit7 UNKNOWN
// byte 2 VOL        bit0 LOW    bit1 NORMAL  bit2 HIGH       bit7 UNKNOWN
// byte 3 FUND_SIGN  bit0 NEG    bit1 POS                     bit7 UNKNOWN
// byte 4 FUND_LEVEL bit0 LOW    bit1 NORMAL  bit2 HIGH       bit7 UNKNOWN
// byte 5 STRETCH    bit0 EXT_DOWN bit1 NEUTRAL bit2 EXT_UP   bit7 UNKNOWN
// byte 6 SOURCE     bit0 MEASURED bit1 DECLARED bit2 UNKNOWN
// byte 7 reserved (zero)
pub const UNKNOWN: RegimeWord = RegimeWord(0x0004_8080_8080_8080); // every dim marked + SOURCE=UNKNOWN

/// Allowed set per dimension (any subset of bits), same byte layout.
pub struct RegimeLabel(pub u64);
pub const ANY: RegimeLabel = RegimeLabel(0);   // 0 = unconstrained (legacy law)

#[inline(always)]
pub const fn allows(label: RegimeLabel, eff: RegimeWord) -> bool {
    // `eff` is one-hot per populated byte; a label byte with the
    // effective bit set allows it. ANY short-circuits.
    label.0 == 0 || (eff.0 & label.0) == eff.0
}
```

Two loads, one AND, one compare, branch-predictable. **Per-dimension
unknown (RG0 amendment, landed):** a measured word marks a dimension it
cannot judge (warm-up, percentiles not yet pushed) with bit 7 instead
of leaving it empty. A label's omitted dimension is written as the full
legal mask *including* bit 7; an explicit list (`vol:low|normal`) is
known values only (`unknown` adds the mark, `!v` never includes it).
So `vol:low` refuses an unknown VOL while a row that omits VOL passes —
fail-closed *per dimension*, same formula. An EMPTY market byte is
legal only in a DECLARED word ("the declarer does not constrain it")
and passes every label. `UNKNOWN` (every dimension marked + SOURCE bit
2) is refused by every label that constrains anything — §2.3 falls out
of the encoding, no special case.

**Text grammar** (shared by `icdp.toml`, ruleset JSON, the library, the
dashboard, and the worker CLI):

```
regimes    = [ "trend:bull|neutral", "shape:trend", "vol:normal|high",
               "fund:pos", "level:*", "stretch:!ext_up", "source:measured|declared",
               "slow:trend:bull" ]        # "fast:"/"slow:" prefix selects the profile; unprefixed = fast
regime_off = "soft" | "hard"
rel        = "lagging|inline" | "slow:leading" | …
```

`dim:*` or an omitted dim = any value of that dim; `!v` = all but `v`;
`source:` omitted ⇒ `measured|declared` (never UNKNOWN) for a labelled
row, and `LABEL_ANY` when the `regimes` key is absent entirely. Parsing is
boot/side-path only (the engine's ruleset validator, `core-config`, the
worker); the hot path sees `u64`s.

### 3.3.1 A strategy fits MANY regimes — the three levels

A label is a *region* of the regime space, never a point. Multi-regime
fit is expressed at three levels, and each is a first-class shape:

1. **Within one label — per-dimension sets (product regions).** Every
   byte of a `RegimeLabel` is a subset: `trend:bull|neutral`,
   `vol:!high`. One label = the Cartesian product of its per-dim sets,
   e.g. `["shape:trend", "vol:normal|high"]` allows 3 × 1 × 2 × 2 × 3 × 3
   = 108 of the 162 possible words. Most real labels ("works when it
   trends, unless vol is extreme") are exactly this shape, and one
   `AND + CMP` evaluates it.
2. **Per profile — both horizons at once.** A row/strategy carries one
   mask **per profile** (`mask_fast`, `mask_slow`, §4.5) and is open only
   when *both* allow (a zero mask = any). "Fast trend inside a slow
   chop" and "any fast state while slow is bull" are both single rows.
3. **Across labels — unions (non-product regions) and regime-specific
   parameters.** A member of the library holds a *list* of labels
   (`labels: [ … ]`, ∃-semantics: it fits a word if any label allows
   it), and — more useful — a *row variant per regime*: the same signal
   with regime-specific `exit`/`horizon`/`max_risk`/`off`, each variant
   carrying a disjoint mask. Rule 8 is amended so disjoint-mask
   variants of one signal are legal (§4.5); the engine sees ordinary
   rows and at most one variant is ever open. Coded strategies get the
   same union shape through `RegimeLabelSet` (up to 4 terms, §4.2).

Labels are hypotheses; the **evidence is per regime word** (§5.2
`library_evidence.regime_word_mode` + the per-regime P&L of §4.8), so a
member's *fit set* — the regimes in which it has actually earned — is a
query, not an opinion, and the composer can select on either
(`--fit-from-evidence`, §5.3).

### 3.4 Per-symbol relative state

`REL` is not a market word — it is one byte per breadth-set symbol,
`RegimeState.rel[profile][slot]`, refreshed on the minute roll. It is
exposed (a) as a per-row gate field for VM rows (`rel:` in the label;
the row's action `sym` is the symbol judged) and (b) as a feature
(§4.7) so a row can use "coin lags BTC by ≥ x bps" as its signal.

### 3.5 Hysteresis — no flicker

- Continuous dims (SHAPE, VOL, STRETCH, FUND_LEVEL, REL, TREND's `r`)
  use enter/exit bands: leave a state only through the band's far edge
  (e.g. `er_hi_enter = 0.60`, `er_hi_exit = 0.50`).
- Every dim flips only after `confirm_min` consecutive minute
  evaluations agree (default 3). Flip counters per dim are metrics
  (`engine_regime_flips_total`), and the soak gate (§7 RG7) bounds them.
- A DECLARED word is never hysteresis-filtered — it is the AI's call.

### 3.6 Measured vs declared vs effective

```
declared_fresh(p) = declared_ts(p) != 0 && now − declared_ts(p) ≤ declared_ttl(p)
effective(p)      = declared_fresh(p) ? declared(p) | SOURCE_DECLARED
                  : measured_valid(p) ? measured(p) | SOURCE_MEASURED
                  : REGIME_UNKNOWN
```

**As landed in RG1 (amends the sketch above):** `effective =
merge(declared over measured) | DECLARED` while fresh — a declaration
overrides only the dimensions it names (each declared byte where
non-empty, the measured byte elsewhere), so a partial declaration is a
per-dimension override and §11-Q5 needs no separate mechanism; else
`measured | MEASURED` as soon as ANY dimension is known — dimensions
that cannot be judged yet carry the unknown mark and gate fail-closed
on their own (§3.3); else `UNKNOWN`. All three words per profile are
published to metrics, the snapshot, and `/state`;
`engine_regime_disagree_total` counts judged minutes on which a FRESH
declaration named a dimension the measurement disagreed with (the
dashboard's honesty meter). Freshness for both laws is judged at the
same instant — the timer tick that crossed the minute boundary; the
parity fixture caught a one-minute skew between the two in RG1 and it
was fixed in Rust.

## 4. Engine side (Rust)

### 4.1 New crate `crates/core-regime` (zero-alloc, `#[repr(C)]`, boot-boxed)

```rust
pub const REGIME_MAX_SYMS: usize = 32;      // breadth set + refs
pub const REGIME_RING_MIN: usize = 1536;    // ≥ 24 h of minute closes (slow W ≤ 4 h today; 24 h headroom)
pub const REGIME_PROFILES: usize = 2;       // words are stored [RegimeWord; 4]

#[repr(C, align(64))]
pub struct RegimeSym {                      // one per member symbol
    ring: [i64; REGIME_RING_MIN],           // minute close ×1e6; 0 = no sample
    last_mid_1e6: i64,                      // written per tick (the ONE hot-path store)
    newest_min: i64,
    sum_sq_ret: [i128; REGIME_PROFILES],    // running Σ r² per profile window
    sum_abs_5m: [i64; REGIME_PROFILES],     // running Σ|5 m move| per profile window
    sym: SymbolId, slot: u8, is_btc_ref: u8, is_fund_ref: u8, _pad: [u8; 5],
}

#[repr(C, align(64))]
pub struct RegimeState {
    syms: [RegimeSym; REGIME_MAX_SYMS],
    bucket: [u8; 64],                       // (sym ^ sym>>24) & 63 → slot | 0xFF (the engine's staleness-bucket law)
    params: RegimeParams,                   // from regime.toml (boot); §4.6 for live updates
    measured: [RegimeWord; 4], declared: [RegimeWord; 4],
    declared_ts: [NsTs; 4], declared_ttl: [u64; 4],
    effective: [RegimeWord; 4],
    rel: [[u8; REGIME_MAX_SYMS]; 4],
    raw: RegimeRaw,                         // last ret/ER/RV/stretch/funding per profile — for /state + metrics
    confirm: [[u8; 8]; 4],                  // hysteresis counters per dim per profile
    funding_rate_1e9: i64, funding_ts_ms: u64,
    last_minute: i64, n_syms: u8, valid_mask: u8, …
}

impl RegimeState {
    pub fn new_boxed() -> Box<Self>;                         // boot only (alloc_zeroed)
    pub fn configure(&mut self, p: &RegimeParams, anchor: WallAnchor) -> Result<(), RegimeErr>;
    pub fn seed(&mut self, rows: &[SeedRow]) -> u32;          // boot only (§4.3)
    #[inline(always)] pub fn on_tick(&mut self, t: &Tick);    // bucket lookup + 1 store; stale ticks ignored
    pub fn on_funding(&mut self, ev: &ChannelEvent);          // fund-ref latch
    pub fn on_minute(&mut self, mono_ns: NsTs) -> u8;         // roll rings, update running sums O(n_syms), re-judge; returns changed-profile mask
    pub fn set_declared(&mut self, p: u8, w: RegimeWord, now: NsTs, ttl_ns: u64);
    pub fn refresh_effective(&mut self, now: NsTs) -> u8;     // TTL law; returns changed-profile mask
    pub const fn effective(&self, p: u8) -> RegimeWord;
    pub fn rel_of(&self, p: u8, sym: SymbolId) -> u8;
}
```

Costs: `RegimeState` ≈ 32 × 12.5 KB ≈ 400 KB boxed at boot (the
`FeatureState` precedent). Per tick: one bucket load + one store for
member symbols, nothing for others. Per minute: ring write + two
running-sum updates per sym + one judge pass — a few µs, once a minute,
on the engine thread (accepted; measured in the bench).
`isqrt_i128` moves from `strategy-vm::features` into `core-regime` and
the VM imports it (no duplicate).

### 4.2 Ownership + call path

`StrategySet` owns `Box<RegimeState>` (it is the composer; the regime
gates members). Feeds:

- `on_tick` → `regime.on_tick(tick)` before the fan-out (one branch on
  the bucket).
- `on_venue_event(Funding)` → `regime.on_funding`.
- `on_timer`: `StrategySet::timer_period_ns` becomes `min(members, 1 s)`;
  on each timer, `regime.refresh_effective(now)` and, on a minute edge,
  `regime.on_minute(now)`. Any changed profile ⇒ recompute each slot's
  gate and fan out `on_regime` **edge-triggered** (only slots whose gate
  changed).
- `on_ai(SetRegime)` → `regime.set_declared(…)` then the same
  edge-triggered fan-out.

New trait surface in `strategy-core` (defaulted — no member is forced to
change):

```rust
/// Up to 4 product terms, each a (fast, slow) mask pair; a member is open
/// when ANY term allows both profiles' effective words. Term 0 = (0, 0)
/// means ANY. Evaluated by the set on regime change only (never per tick).
#[repr(C)] #[derive(Copy, Clone)]
pub struct RegimeLabelSet { pub terms: [[RegimeLabel; REGIME_PROFILES]; 4], pub off: u8 /*0 soft, 1 hard*/, _pad: [u8; 7] }

#[repr(C)] #[derive(Copy, Clone)]
pub struct RegimeGate { pub effective: [RegimeWord; 4], pub open: u8, pub hard: u8, _pad: [u8; 6] }

pub trait Strategy: StrategyCounters {
    …
    /// The member's static label set. Default = ANY / soft.
    fn regime_label(&self) -> RegimeLabelSet { RegimeLabelSet::ANY }
    /// Edge-triggered: called only when this member's gate changes.
    fn on_regime<C: Ctx>(&mut self, gate: RegimeGate, ctx: &mut C) { let _ = (gate, ctx); }
}
```

Coded strategies (slots 0–4, 6) implement `on_regime` as: store the gate;
skip entry paths while closed; if `hard` and closed, run their existing
flatten/exit path (`emit_exit`/IoC exit at the roll for ICDP — its
"exit at the roll regardless of staleness" law already exists; latency-arb
cancels its resting legs). Labels for coded strategies are constants in
each crate (`pub const REGIME_LABEL: …`) with an optional operator
override table in `regime.toml [labels]` applied at boot; ICDP's label is
a `[regime]` block in `icdp.toml` (it is the strategy's artifact; the
`core-config::icdp` parser gains a `[regime]` table with `terms` (up to 4)
and `off`, unknown-key-strict as today). Slot 4 (`ai-exec`) is gated at the slot level only — intents are
composed regime-aware on the worker side (§5.3), the engine does not
carry a per-intent mask (the 64-B `AiCmd` has no spare field that is not
shape-enforced zero).

`StrategyCounters` gains `regime_words() -> [RegimeWord; 12]`
(measured/declared/effective × 4) and `regime_gates() -> [u8; 8]`;
`paper.rs` mirrors them through the existing `mirror_*` pattern.

### 4.3 Warm-up + restart continuity: the regime seed

The engine restarts three times a day (T2 00:00/08:30/16:05Z). Without a
seed the `slow` profile is `UNKNOWN` for 4 h after every boot — and in
the harness a ≤ 2 h window can never warm a 4 h window at all. So:

- `~/multivenue/regime-seed.tsv` — `descriptor \t minute_ts \t close_1e6`
  for the last `REGIME_RING_MIN` minutes of every member, **written by
  the worker from `candles.db` (1 m, all sources)** in
  `scripts/engine-wrapper.sh` right before the `exec` (the
  `universe_refresh` precedent; best-effort — absent/short seed ⇒ the
  affected profile warms live, `regime: seed absent` boot tell).
- The engine reads it at boot (boot path may allocate/IO), fills the
  rings, primes the running sums, and judges once so the words are valid
  at `on_start`.
- The harness takes `--regime-seed <path>` (§4.8); `window_root.cut_run`
  gains a seed step (`regime.py` → `regime-seed.tsv` inside the window
  root, from `candles.db` for the `REGIME_RING_MIN` minutes before the
  window's first `ts_ns`) — derived data, not a capture window; §6.1 of
  the VT plan is respected.
- The DECLARED word does not survive a restart in the engine; the
  worker's post-boot lane (`recommit.py`, ruling #7b) re-pushes the last
  persisted declaration (`~/multivenue/worker/regime/declared.json`) after
  the re-commit lands — same waiter, one more frame.

### 4.4 Wire: `AiCmdKind::SetRegime = 12` (wire-additive)

| field | value |
|---|---|
| `kind` | 12 |
| `venue` | `Ai` |
| `strategy_id` | `STRATEGY_SLOT_NONE` (set-level command) |
| `param_id` | profile id (0 `fast`, 1 `slow`; `≥ REGIME_PROFILES` ⇒ shape error) |
| `px` | the DECLARED `RegimeWord` (bit-cast `i64`; SOURCE byte must be 0 — the engine stamps it) |
| `qty` | the WORKER-MEASURED word at send time (audit only — the capture shows AI-declared vs worker-measured divergence) |
| `ttl_ns` | declared validity (e.g. 15 min); **0 refused** (a declaration without expiry is exactly the stale-trust §2.2 forbids). Note the engine drain's TTL-on-pop law applies unchanged: a frame that sits in the ring longer than its TTL expires before it is applied |
| `flags` | bit0 `expire_on_silence` allowed (heartbeat-bound declarations) |
| `sym`, `side` | `SYMBOL_ID_NONE`, `AI_SIDE_NONE` |

`validate_shape` gains one arm; `frames.py` gains `KIND_SET_REGIME = 12`
+ `pack_frame` support; `docs/wire-format.md` `AiCmd` row + `docs/migration.md`
entry. Captured to `ai-cmds.pmlr` like every AiCmd, so the harness can
replay declarations (§4.8).

### 4.5 `RuleRowV2` — regime fields in the reserved tail (wire-additive)

`_pad3: [u8; 40]` (offset 88) is reserved-and-zero. Allocate:

| offset | bytes | field | law |
|---|---|---|---|
| 88 | 8 | `regime_fast: u64` (`RegimeLabel`, profile 0) | `0` = unconstrained on this profile |
| 96 | 8 | `regime_slow: u64` (`RegimeLabel`, profile 1) | `0` = unconstrained on this profile; both zero = the legacy unconstrained row (every existing artifact is bit-identical) |
| 104 | 1 | `regime_off: u8` | 0 soft, 1 hard; must be 0 when both masks are 0 |
| 105 | 1 | `regime_rel: u8` | allowed REL set per profile: bits 0–2 fast, bits 4–6 slow; 0 = any |
| 106 | 22 | `_pad3` | still zero (a third/fourth profile costs 8 B each here; ≥ 5 profiles = a table version) |

`ver` stays `RULE_ROW_VER_2` (the `Order.ttl_ns` precedent: new meaning
for previously-zero bytes, old rows unchanged). Ruleset JSON grammar
v2.1: the row keys `regimes` (array of strings, §3.3 grammar — each
string may be prefixed `fast:`/`slow:`; unprefixed = `fast:`), `regime_off`,
`rel` — parsed by the hand-written streaming parser in
`ingress-ai/src/ruleset.rs` (three more arms in the `else if` chain;
the unknown-key rejection is unchanged). Validator **rule 11**: label
grammar valid, `off` ∈ {soft, hard}, `regime_off`/`rel` present only
with `regimes`. `RulesetReject::Regime` appended.

**Rule 8 amendment (multi-regime variants of one signal, §3.3.1 level 3).**
Two rows that match on the v2 identity tuple (`ruleset.rs:1262-1275`)
are a duplicate **only if their regime regions intersect**:
`intersects(a, b) = ∀ profile p: (a_p == 0 || b_p == 0 || every dim byte of
(a_p & b_p) is non-zero)` — the same law per REL byte. Disjoint variants
(e.g. `bull` vs `bear` vs `neutral`, each with its own `exit`,
`horizon_ms`, `max_risk_usd`, `off`) are admitted; at most one of them
is ever open, so the double-fire hazard rule 8 exists for cannot arise.
A variant whose region overlaps another's is still rule-8 rejected. Two
legacy (both-masks-zero) rows keep today's behaviour exactly.
Rule 7's caps count every variant (conservative, unchanged).

Evaluator (`VmStrategy::on_tick`): the entry/refire path (`lib.rs:699`)
gains, before the horizon check:

```rust
// self.regime_eff: [RegimeWord; 4], self.regime_rel: per-profile per-sym bytes — set by on_regime
if !allows(RegimeLabel(row.regime_fast), self.regime_eff[0])
   || !allows(RegimeLabel(row.regime_slow), self.regime_eff[1])
   || !rel_allows(row.regime_rel, self.rel_of(row.sym)) {
    self.regime_blocked = self.regime_blocked.wrapping_add(1); i += 1; continue;
}
```

(four loads, two AND, two CMP; the row's 128 B are already in cache) and
the exit path (`lib.rs:669`) gains: `if row.regime_off == 1 && !allows(…) { emit_exit; continue; }`
(age-out stays first). Counters `regime_blocked`, `regime_hard_exits`
→ `engine_vm_regime_{blocked,hard_exits}_total`. `on_table_flipped` is
untouched — regime change never touches the tables (§2.4).

### 4.6 Params: `~/multivenue/regime.toml` (boot; restart-applied)

```toml
[refs]        btc = "binance-usdm:btcusdt"   fund = "binance-usdm:btcusdt"
[breadth]     members = ["binance-usdm:ethusdt", "binance-usdm:solusdt", …]   # ≤ 31, descriptors
[hysteresis]  confirm_min = 3
[profile.fast]  trend_w_min = 60   shape_w_min = 60  vol_w_min = 60  stretch_w_min = 60  rel_w_min = 60
                trend_thr_bps_1e9 = …  breadth_q_1e9 = 600000000
                er_lo_enter/exit_1e9 = …  er_hi_enter/exit_1e9 = …
                rv_p30_bps_1e9 = …  rv_p70_bps_1e9 = …       # worker-refreshed daily
                stretch_k_1e9 = 2000000000  rel_thr_bps_1e9 = …
                fund_p30_1e9 = …  fund_p70_1e9 = …           # worker-refreshed daily
[profile.slow]  …(4 h)…
[labels]      # optional overrides of the coded constants; `terms` = up to 4 product terms (∃-semantics), each may mix fast:/slow: prefixes
latency_arb = { terms = [["fast:vol:!high"]], off = "soft" }
icdp        = { terms = [["fast:shape:trend", "slow:trend:bull|neutral"], ["fast:shape:trend", "slow:trend:bear"]], off = "soft" }
```

Integer-only TOML subset, same parser family and strictness as
`core-config::icdp` (unknown/missing/duplicate keys fatal; refuses the
boot). `regime.toml.example` committed; the live file is data (outside
git). The worker rewrites the percentile lines daily (§5.1) and the next
T2 restart applies them — **no live parameter push in v1** (a
`SetRegimeParam` kind is listed under §10 deferred; the three daily
restarts make it unnecessary now).

### 4.7 Regime as a feature (RG3b, optional)

`FeatId::RegimeRel = 17` (append-only law): per-sym `rel_1e9` of the
row's profile (window = `win_a` mapped to the nearest profile; natural
unit = return fraction ×1e9). Lets a row *trade* the residual, not just
gate on it. Deferred to RG3b unless the operator wants it in RG3
(§11-Q8).

### 4.8 Harness (`backtest` / `audit-pnl`) — regime-aware replay

- `cli::backtest` instantiates the same `RegimeState`, seeds it from
  `--regime-seed`, feeds replay ticks/funding events, and replays
  `SetRegime` frames from the window's `ai-cmds.pmlr` (windowed roots
  must cut that file by `ts_ns` too — the events-file lesson, pitfall 17).
- `--regime off` (flag) evaluates every row as `LABEL_ANY` — the on/off
  delta is the first number any regime-labelled ruleset must show.
- `audit-pnl` JSON (additive, `detail_version` bump): per strategy ×
  effective regime word (decoded string) → `{orders, fills, net_usd,
  fee_ladder_net_usd[3], minutes}`; `regime_minutes` histogram per
  profile; `regime_blocked` counters. `pnl_report` merges them (§5.5).
- Schema-1 stdout stays frozen (worker contract). Harness numbers on a
  v2 root remain `stale-blind(v2)` upper bounds — unchanged law.
- G0 reminder stands: relink the release binary before any number matters.

### 4.9 Metrics (Prometheus text, names carry dimensionality)

`engine_regime_{fast,slow}_{measured,declared,effective}` (gauge = the
word), `engine_regime_{fast,slow}_source` (0/1/2), `engine_regime_declared_age_ns_{fast,slow}`,
`engine_regime_flips_total_{trend,shape,vol,fund,stretch}`,
`engine_regime_disagree_total`, `engine_regime_seed_rows`,
`engine_regime_raw_{fast,slow}_{ret_bps,er,rv_bps,stretch}` (×1e9 gauges),
`engine_strategy_regime_gate_{slot}` (0 open / 1 soft-closed / 2 hard-closed),
`engine_vm_regime_{blocked,hard_exits}_total`. ~40 names; the registry
ceiling (256 c / 384 g) holds.

## 5. Worker side (Python — full `import x` only)

### 5.1 `claude_worker/regime.py` — the reference evaluator + declared lane

- **Reference law**: the same integer evaluator as `core-regime`, over
  minute closes from `candles.db` (`candles` 1 m rows, every source)
  and prints from the `funding` table. A committed fixture
  (`tests/fixtures/regime/minutes-*.tsv` + expected words) is executed
  by both a Rust test and a pytest — parity is a gate, not a habit.
- **Percentiles**: RV and funding p30/p70 over the profile lookback
  (fast: 7 d of 1 h RV; slow: 30 d of 4 h RV / 90 funding prints),
  rewritten into `~/multivenue/regime.toml` daily (`--refresh-params`),
  applied at the next T2 restart.
- **Seed**: `--seed-out ~/multivenue/regime-seed.tsv` (§4.3), called by
  the engine wrapper and by `window_root` per window.
- **Measured (worker view)**: `python -m claude_worker.regime` prints the
  report — every dim's raw value, the word per profile, the last 24 h of
  words (from its own history under `~/multivenue/worker/regime/`), and
  the engine's current words read from `/state` (§6.1) — the AI's input
  in semi-manual mode.
- **Declared push**: `--declare fast="trend:bull,shape:trend,…" --ttl 900`
  persists `declared.json` and sends `SetRegime` (frames via
  `uds.py`, seq from `state.db` — the one-writer law). `--declare
  measured` declares the worker-measured word (the "AI confirms the
  measurement" case). The post-boot lane re-pushes `declared.json`
  after the re-commit (§4.3).
- Module lane, not a new Typer verb: the 8-verb surface stays as is
  (`pnl_report`/`candles` precedent); `push` is untouched.
- launchd: `com.multivenue.regime` every 5 min (`scripts/regime-cycle.sh`,
  worker-serialization `pgrep` guard as in `candles-cycle.sh`; label
  self-removal is not needed for `StartInterval` jobs). In `serve` the
  same function runs inside `ResearchCycle` (§5.4) instead of the timer.

### 5.2 Where AI strategies live today — and the library that fixes it

**Today (the answer to "clarify where we store current AI strategies"):**

| what | where | identity | gaps |
|---|---|---|---|
| installed artifacts (what the engine can stage) | `~/multivenue/artifacts/rulesets/<hash128>.json` (`$AI_RULESET_DIR`) | content hash of the canonical `{"rows":[…]}` bytes | no name, no labels, no status, no evidence; same rows ⇒ same hash (good), any edit ⇒ a new unrelated file |
| raw proposals (pass or fail) | `~/multivenue/worker/candidates/<utc-ts>-<hash128>.json`, rejects as `<ts>.rejected` | filename | not indexed anywhere; failed ones are never reused |
| catalog | `state.db` table `rulesets(hash, path, report_path, gates_passed, author_mode, model, thesis, staged_ts, committed_ts)` | full sha256 | **the thesis exists only here**; one row per *table*, no notion of a reusable *strategy* (row set); no regime, no window evidence |
| gate evidence | `R.report.json` beside wherever the session wrote `R.json` | path in the catalog | not canonical, not copied, not per window |
| research findings | `docs/research/` vault (git-excluded) | prose | not machine-readable |
| the ICDP member | `~/multivenue/icdp.toml` (params) + the vault's fit pipeline | sha256 of the file | a coded strategy with an artifact — not in any catalog |

Nothing today lets the worker ask "which validated strategies do I have
for regime X" — that is the gap the user's request names.

**Proposal — the strategy library** (all under `~/multivenue/`, never
git):

- **Unit = a *member*: a named row set** (1..n VM rows sharing a thesis)
  or a coded-member reference (`icdp@<sha256>`, `latency-arb`). A
  *table* is a composition of members (§5.3). Reuse happens at member
  granularity, which is why the table hash was never a usable identity.
- `~/multivenue/worker/library/<member_id>.json` — `{name, kind:
  "vm-rows"|"coded", rows:[…] (each with its own regimes/off/rel keys —
  regime-specific variants of one signal are separate rows in the same
  member, §3.3.1), labels: [ [label-strings], … ] (the member's declared
  fit set, ∃-semantics), thesis, origin: {author_mode, model,
  session_ts, source_hash128}, status: candidate|validated|retired}` —
  member_id = sha256 of the canonical rows (content-addressed like an
  artifact; labels/thesis are metadata and do not change the id).
- `state.db` new tables:
  `library(member_id PK, name, kind, path, status, labels_json TEXT,
  regime_off, thesis, origin_json, created_ts, updated_ts)`;
  `library_evidence(member_id, window_id, root, n_ticks, n_fills,
  net_usd_0, net_usd_tier, max_dd_usd, regime_word_mode, judged INTEGER,
  detail_version, ts, PK(member_id, window_id))` — one row per ≤ 2 h
  window the member has been run on, **judged (v3) or not**;
  `compositions(table_hash128 PK, member_ids_json, regime_word, profile,
  composed_ts, staged_ts, committed_ts)` — links a committed table back
  to its members.
- `python -m claude_worker.library` lanes: `add --from <ruleset.json>
  --name … --regimes … [--thesis …]` (splits a table into one member
  unless `--split-by-name-prefix`), `import-catalog` (one-time: every
  `rulesets` row + every candidate becomes a `candidate` member with
  `LABEL_ANY`, thesis carried over — nothing is lost), `list [--regime
  W]`, `label`, `retire`, `evidence <member> --window …` (runs the
  frozen `backtest` on a window root + `--regime-seed`, records the
  row). Existing committed hashes become `validated` members
  automatically; the operator labels them.
- The `rulesets` table and the frozen `stage-ruleset`/`commit-ruleset`
  verbs are untouched — the library sits *before* them in the pipeline.

### 5.3 `claude_worker/compose.py` — regime-aware aggregation

Input: the effective word per profile (from the engine's `/state`, else
the worker's own measured word), the library (`status = validated`
unless `--include-candidates`), caps.

1. **Select**: members that *fit* the effective words (both profiles) —
   fit = ∃ label in the member's `labels` list that allows them (default),
   or, with `--fit-from-evidence`, ∃ evidence rows for those words with
   judged net at tier > 0 over ≥ N windows — plus members that fit any
   word at Hamming distance 1 in dimension space (one dim differs) —
   the *neighbourhood*. A member that fits many regimes is selected
   once; all of its rows (every regime variant) go in, and the engine's
   row masks decide which variant is open, so a regime move within the
   neighbourhood needs no flip (§2.4).
2. **Fit the caps**: the validator counts every row, gated or not (rule
   7 is conservative by design). Sort by evidence (judged net at tier,
   then windows N), admit members until ≤ 256 rows and the $100k table /
   $20k per-symbol bounds hold; the rest wait.
3. **Emit** the composed artifact (`{"rows":[…]}` with per-row regime
   keys), canonical bytes, `hash128`.
4. **Gate**: the frozen `backtest.run_backtest` on **N ≥ 4 pooled
   disjoint ≤ 2 h windows that already exist**, each with its seed, LOWO
   as in the ICDP G1 shape; the composition passes iff the pooled OOS
   gates pass and the on/off delta (`--regime off`) is not negative.
   `library_evidence` rows are written for every member run.
5. **Promote** only if `hash128 != active` (from `/state`): install →
   `stage-ruleset` → `commit-ruleset` (the frozen verbs, in-process the
   way `daemon._try_promote` calls them). Re-compose triggers: the
   effective word leaves the current neighbourhood, a library change, or
   the daily refresh — never the bare minute-level regime flicker.
6. Intents (slot 4): the same select step filters signal-lane members
   before their cron pushes (`xv_signal`/`carry_signal` gain a
   `regime_allows()` check reading `declared.json`/`/state`) — the
   worker is the regime-aware composer for intents; the engine gates
   slot 4 as a whole.

### 5.4 The AI in the loop — semi-manual and `serve`

- **Semi-manual (`docs/prompts/ai-session.md` §4, additive steps; the
  pinned scripted test is extended in the same commit, deliberately):**
  0a. `python -m claude_worker.regime` — read the measured words + raw
  values; 0b. declare (`--declare …` or `--declare measured`) — the AI's
  ruling on the mode; 0c. `python -m claude_worker.library list --regime`
  — what exists for it; then author *new* members with `regimes` keys
  (§3.3) → `library add` → `compose --dry-run` → the existing steps 4–10
  unchanged. Rollback verb unchanged (disable-5 / re-commit prior).
- **`serve` (`ResearchCycle`)**: a `_REGIME` phase before `_FETCH`:
  compute → declare (auto-confirm the measurement unless the strategist's
  last ruling is fresher) → compose; the strategist digest gains a
  `REGIME` section (words, raw values, 24 h history, per-regime P&L
  from the last report) and a `LIBRARY` section (validated members and
  their labels); the prompt (`strategist-v3`) requires `regimes` on every
  proposed row and asks for a regime *verdict* line the worker parses
  into `--declare`. **No `serve` runs and no Anthropic calls until the
  operator opens the §7 gate — this phase only prepares the code path
  (tests mock the SDK boundary, as today).**
- The monitor (§8.3) becomes regime-aware in reporting only: the
  rollback triggers stay P&L-based; the event ledger records the
  effective word at trigger time.

### 5.5 Nightly report

`pnl_report` merges the harness's per-regime section: per strategy ×
regime word `{fills, net @0/@1/@tier, minutes}` and a per-profile regime
timeline (minutes per word, flips). `pnl` prints it. Evidence for the
library is written from the same run (the day's windows are the
`window_root` cuts already made).

## 6. Dashboard (D4: both)

### 6.1 Engine `GET /state` (JSON, `127.0.0.1:9191`)

- Served by the existing hand-rolled server thread beside `/metrics`
  and `/healthz`. The engine loop publishes an `EngineSnapshot` (POD,
  `#[repr(C, align(64))]`, ≈ 24 KB) into a second `SnapshotCell` **every
  1 s** (`SNAPSHOT_PERIOD_NS`, a second publish site — the 5 s metrics
  mirror is unchanged); the server thread copies it out under the
  seqlock and encodes JSON with a hand-written writer into the response
  buffer (`RESP_BUF_SIZE` 128 → 256 KiB). No serde, no allocation, no
  hot-path work.
- Sections: `boot` (pid, git sha/build ts via `env!`, boot ts, run dir,
  requested/configured/enabled mask, halted, artifact hashes: ruleset,
  icdp, regime.toml, seed rows); `regime` (per profile: measured /
  declared / effective decoded + raw, declared age + ttl, flips, disagree,
  rel per breadth sym); `slots[0..6]` (name, configured, enabled, label,
  gate, orders emitted/dropped, member counters); `vm` (active/staged
  hash, rows, epoch, per-row: idx, `name_h`, sym, state, side, entry px,
  age s, gate); `icdp` (12 counters, instruments); `ai` (ingress
  counters, heartbeat age); `ingress[venue]` (state, last-tick age,
  stale ticks, feed-delay EMA); `latency` (p50/p99 ×3); `recent`
  (last 64 orders + 64 fills — fixed rings in the snapshot: ts, slot,
  sym, side, px, qty, oid); `capture` (records, io errors).
- `DashboardState` (TUI) is replaced by a view over the same snapshot;
  the never-filled `recent_tob` goes away.

### 6.2 Worker-served page — `python -m claude_worker.dashboard`

- `http.server` (stdlib) on `127.0.0.1:9292` (§11-Q6), single-threaded,
  read-only. Routes: `/` → `dashboard.html` (one file, inline CSS+JS,
  vanilla, no CDN — the engine's offline law); `/api/worker` → JSON
  (rulesets catalog, library + evidence, compositions, regime history
  24 h + `declared.json`, latest `pnl-<day>.json` per strategy and per
  regime, candidates dir, events ledger tail 100, positions/P&L from the
  existing `positions` code path over the fills tail, config snapshot:
  `strategy.conf`, `fees.toml`, `regime.toml`, `icdp.toml` (hash +
  instruments), `universe.toml` summary, disk free of the Data volume —
  **never `.env`**); `/api/engine/state` and `/api/engine/metrics` →
  same-origin proxies to 9191 (no CORS, one page). Cadence: engine 2 s,
  worker 10 s, P&L 30 s.
- Panels: status bar (engine up/halted, mask, regime chips per profile
  with source + age, `ai.sock` heartbeat age, run dir, disk free);
  Regime (dims table with raw values and bands, 24 h word timeline,
  measured-vs-declared disagreement); Strategies (slot table: enabled,
  label, gate, orders, fires, positions, today's net per fee tier);
  Ruleset (active hash + thesis + members, rows table with per-row state
  and gate, staged hash); Library (members, labels, status, evidence N);
  P&L (per strategy today, per regime, day series from reports);
  Recent trades (orders/fills, slot-coloured); Ingress; Latency; AI plane
  (counters, ledger tail); Configs (read-only).
- launchd `com.multivenue.dashboard` (RunAtLoad, KeepAlive), log under
  `~/multivenue/logs/launchd/`. The HTML file lives at
  `claude-worker/src/claude_worker/dashboard/dashboard.html` (a
  licence-comment header; `make license-check` covers `.rs/.py/.sh` —
  §11-Q9 asks whether to extend it to `.html`).
- **Write controls (enable/disable/declare/halt) are NOT in v1** — a
  browser-reachable lever into the AI plane is a Stage-3-grade decision;
  listed under §10.

### 6.3 TUI

`crates/tui` reads the same `EngineSnapshot`: header gains the mask +
per-profile regime chips; the dead markets panel becomes **Strategies**
(slot / enabled / gate / orders / fires); a **Ruleset** line (active
hash, rows, epoch); recent orders replace the single last order. Render
cadence unchanged; the crate doc's "~10 ms" claim is corrected to the
real 1 s publish.

## 7. Phases + gates

Every phase: nextest green, `cargo test -p bench --test alloc_assertions
--release -- --test-threads=1` 0 B/op (fresh `Compiling bench` in the
log), worker pytest green with the 202 pin untouched, `make lint`,
`make license-check`, SPDX on every new file, `license.workspace = true`
on the new crate, `make license-deps` **not** needed (no new dependency
is planned on either side — `http.server` is stdlib), `cargo build
--release -p cli` before any live boot, explicit-path `git add`, commits
on operator ask, no push.

| phase | deliverable | tests / gates | exit tell |
|---|---|---|---|
| **RG0** freeze | this doc ruled; `AiCmdKind::SetRegime = 12` + shape arm; `RuleRowV2` tail fields + `static_assert_size` unchanged (128); `RegimeWord/RegimeLabel/allows` in `core-types`; `regime.toml.example`; `docs/wire-format.md` + `docs/migration.md` entries; label text grammar parser (`core-config::regime`) | shape unit tests (old rows bit-identical; `SetRegime` refuses `ttl_ns = 0`, bad profile, non-zero SOURCE); grammar proptest; `frames.py` pack/unpack parity test | `KIND_SET_REGIME` round-trips through `parse_frame` |
| **RG1** evaluator | `crates/core-regime` (§4.1) + `claude_worker/regime.py` reference + the parity fixture; `isqrt_i128` + `ret_bps_1e9` relocated (ICDP/VM import them) | proptest per dim (monotonic in inputs, hysteresis never flips inside the band, running sums == recomputed sums); parity fixture in both suites; bench: `on_tick` + `on_minute` 0 B/op, `on_minute` p99 < 20 µs @ 32 syms | fixture words identical Rust ↔ Python |
| **RG2** engine | `StrategySet` ownership, trait additions, coded labels + `regime.toml [labels]`, ICDP `[regime]`, seed loader + wrapper hook, `SetRegime` handling + effective law, edge-triggered `on_regime`, metrics, snapshot words, harness replay + `--regime-seed` + `--regime off`, audit-pnl per-regime JSON (additive), recommit re-push | set tests (gate edge-triggering, hard-off flatten path per coded member, UNKNOWN fail-closed, declared TTL expiry); harness determinism (same window + seed ⇒ same words); alloc 0 B/op on the set path (gate 41); **live smoke**: boot tell `regime: seed rows=… fast=… slow=…`, words moving in `/metrics`, a `--declare` visible within one report period, restart continuity (declared re-pushed) | first judged on/off delta printed for the live VM row |
| **RG2 live smoke (operator)** | `cargo build --release -p cli` (G0 relink); install `~/multivenue/regime.toml` from `regime.toml.example` (edit `[breadth] members` to descriptors that exist in `universe.toml`; the ref must be there too); optional: nothing else — the wrapper exports the seed itself; restart the standing engine through launchd; verify | boot log tells `regime: seed file read rows=…`, `regime: artifact configured … fast=… slow=… gates=[0,0,0,0,0,0,0,0]`, `strategy-set: composed …`; `/metrics`: `engine_regime_configured 1`, `engine_regime_fast_source 0` once measured (warm-up ≤ 1 min with a seed, else ≤ 4 h for `slow`), `engine_regime_minutes_judged_total` climbing 1/min, `engine_regime_seed_rows > 0`; `vm_rows_active 1` after the #7b recommit (unchanged law); no member behaviour change (all labels ANY) | words move in `/metrics` for ≥ 10 minutes, no restart loop |
| **RG3** VM rows | grammar v2.1 keys → tail fields (per-profile masks), validator rule 11 + `RulesetReject::Regime`, **rule 8 amendment** (disjoint regime variants of one signal admitted, intersecting ones refused), evaluator gate (entry, both profiles + REL) + hard exit, counters, strategist prompt v3 asks for labels + variants; **plus the harness replay moved here from the RG2 draft**: `cli::backtest` instantiates a `RegimeState`, seeds it from `--regime-seed`, feeds replay ticks + funding events, replays `SetRegime` frames from the window's `ai-cmds.pmlr` (`window_root` cuts that file by `ts_ns` too), hands the words to the bare `VmStrategy` (`on_regime`), `--regime off` evaluates every row as ANY, audit-pnl gains the additive per-regime section (§4.8) | validator tests (each rule-11 refusal; legacy artifact hash-stable and accepted; rule 8: disjoint variants pass, overlapping variants and legacy duplicates still refuse); evaluator proptest (a blocked row never enters, always exits; at most one variant of a signal open at any word; regime change never touches `tables`/positions); the existing `fuzz/fuzz_targets/ruleset_json.rs` target covers the new keys (≥ 300 s run recorded); alloc gate | a labelled ruleset stages, commits, and gates live without a flip |
| **RG3b** (opt.) | `FeatId::RegimeRel = 17` | feature tests; grammar accepts the name | — |
| **RG4** library + composer | `state.db` tables (schema migration additive), `library` + `compose` lanes, `import-catalog` of today's rulesets/candidates, `compositions` link, ai-session §4 steps 0a–0c + the pinned test extended | pytest: import preserves every hash + thesis; compose selects the neighbourhood, respects caps, is idempotent (same inputs ⇒ same hash), promotes only on hash change; gate runs on pooled windows only (N ≥ 4, each ≤ 2 h, LOWO) | first composed table committed live from ≥ 2 members |
| **RG5** worker regime lane | `regime` lane (report / percentiles → `regime.toml` / seed / declare / history), `regime-cycle.sh` + plist, `serve` `_REGIME` phase + digest/prompt sections (SDK mocked), intent-lane `regime_allows()`, `pnl_report` per-regime merge | pytest for each lane; declared re-push after a simulated boot; serve cycle test with the mock; day report shows per-regime rows | `pnl-<day>.json` carries `regimes` for a live day |
| **RG6** dashboard | `EngineSnapshot` + 1 s publish + `/state` writer; TUI panels; worker `dashboard` module + HTML + plist | `/state` encode test (fixed snapshot ⇒ byte-exact JSON, fits the buffer); server thread 0 alloc (bench); pytest for `/api/worker` shape; a manual live check of every panel populated (screenshot in the vault) | page live on 9292 showing regime, mask, rows, P&L, recent fills |
| **RG7** soak + close | 5 paper days with regime gating live (coincides with ICDP G2); flip bound (≤ `flips_max`/day per dim, default 24); per-regime P&L in 5 nightly reports; docs: `CLAUDE.md` CURRENT STATE, `ai-strategy-pipeline.md` (+svg), `research-universe.md`, `local-setup.md`, `risk-policy.md` cross-ref; close entry in §12 | stay-greens recorded | operator ruling |

Dependency order: RG0 → RG1 → RG2 → {RG3, RG5} → RG4 → RG6 → RG7. RG6's
engine half can start after RG2. Estimated size: RG0–RG2 ≈ the ICDP
I1–I5 footprint; RG4–RG6 mostly Python.

## 8. Hot-path + zero-copy accounting

- Regime `on_tick`: bucket load, compare, one `i64` store. No copy.
- Minute roll: in-place ring write + running-sum update; the judge reads
  `RegimeState` fields only.
- Row gate: two loads (`row.regime_mask`, `regime_eff[p]`), AND, CMP —
  the row is already in cache (128 B, loaded for evaluation anyway).
- `SetRegime`: the same 64 B frame path as every AiCmd (materialized
  once by `ingress-ai` from the recv buffer — the one documented copy of
  the AI plane, unchanged).
- Snapshot: one POD copy per second (`EngineSnapshot`, ≈ 24 KB,
  `SnapshotCell::publish`) — a deliberate, documented copy off the hot
  path; the reader copies once more under the seqlock on the server
  thread. JSON is written directly into the socket response buffer.
- Seed load, `regime.toml`, dashboard: boot / side-thread / Python —
  may allocate, each module carries the offline doctrine header.

## 9. Risks

| risk | mitigation |
|---|---|
| regime flicker gating strategies on/off every minute | hysteresis bands + `confirm_min`; `flips_total` metrics; RG7 flip bound; composition ignores minute-level changes (§5.3) |
| `slow` profile blind for hours after each of the 3 daily restarts | the seed (§4.3), boot tell + `engine_regime_seed_rows` gauge; UNKNOWN is fail-closed for labelled rows, so blindness costs opportunity, never risk |
| Rust/Python evaluator drift | the shared fixture is a test in both suites; the harness's words are the engine's code, not the worker's |
| composed neighbourhood breaches table caps | compose sorts by evidence and admits until caps; the validator's conservative count is the backstop |
| a re-commit still flattens VM state when membership changes | compose promotes only on hash change; the operator can pin `compose --freeze` during soak; hard-off members exit before the flip by their own law |
| declared word stale-trusted | `ttl_ns = 0` refused at the shape check; effective law expires it; disagree counter + dashboard chip make divergence visible |
| the pinned §4 scripted test | extended in the same commit as the prompt (deliberate, not drift — the test exists to catch *unintended* change) |
| `/state` JSON grows past the buffer | fixed 256 KiB buffer + an encode test with a full snapshot (256 rows, 64+64 recents); truncation is a test failure, not a runtime branch |
| the 1 s snapshot publish on the engine thread | ≈ 24 KB memcpy ≈ 2–3 µs; measured in the bench; if it shows in `decide` p99 the publish moves to every 2 s |
| harness windows lack `ai-cmds.pmlr` cuts | `window_root` cuts every capture file by `ts_ns` (extended in RG2); a window without it replays measured-only and says so on stderr |

## 10. Out of scope (deliberately)

- Stage-3 work of any kind: executor, RiskGate, signer, dispatcher,
  live ramp; the §7 entry gate stays closed.
- `serve` runs / Anthropic API calls — code path only, mocked.
- Dashboard write controls (enable/disable/declare/halt from the page).
- Live parameter push (`SetRegimeParam`) — daily `regime.toml` +
  T2 restarts cover it; revisit if the restart cadence changes.
- Engine-side per-strategy P&L accumulators — the dashboard reads the
  worker's positions/P&L (existing code path) and the nightly report;
  an in-engine paper book is a separate decision.
- More than two profiles, per-dimension AI overrides (v1 declares whole
  words per profile), per-intent regime masks on the wire.
- Any cloud, Prometheus, Grafana, CDN, framework, paid API.

## 11. Open questions for the operator (answer inline; defaults apply if unanswered)

- **Q1 references.** BTC price ref + funding ref = `binance-usdm:btcusdt`
  (default) — or OKX swap / a median across venues?
- **Q2 breadth set.** Default = every `binance.usdm` perp in
  `universe.toml` except the ref (≤ 31). Or an explicit list?
- **Q3 profile windows.** `fast` = 1 h everywhere, `slow` = 4 h
  everywhere (your ER-1 h and stretch/relative-4 h formulas as the
  anchors). Confirm or set per-dim windows.
- **Q4 hysteresis.** `confirm_min = 3`; ER bands 0.30/0.35 and 0.55/0.60;
  stretch `k = 2.0`; REL thr = 50 bps (fast) / 150 bps (slow). Confirm
  or override.
- **Q5 declared scope.** v1 declares a whole word per profile. Enough,
  or should the AI be able to override single dimensions?
- **Q6 dashboard port** `127.0.0.1:9292` (worker) — fine?
- **Q7 regime tick cadence.** `com.multivenue.regime` every 5 min as a
  separate launchd job (worker-serialized), or fold into the hourly
  candles cycle only?
- **Q8 RG3b** (`FeatId::RegimeRel` feature) in RG3 or later?
- **Q9 licence gate for `.html`** — extend `make license-check` to
  `.html` (one file today), or leave it at `.rs/.py/.sh`?
- **Q10 coded-strategy default labels.** All `ANY` at RG2 (behaviour
  unchanged) and labelled later from evidence — or do you want initial
  labels now (e.g. latency-arb `vol:!high`, ICDP `shape:trend`)?

## 12. Progress log

- 2026-09-03 — DRAFT written for operator review (no code). Operator
  decisions D1–D4 recorded in the header.
- 2026-09-03 — Review Q "does each strategy fit more than one regime?"
  → §3.3.1 added (three levels: per-dim sets, per-profile masks,
  unions/variants); `RuleRowV2` tail re-laid as `regime_fast`/`regime_slow`
  (profile byte dropped); rule 8 amended for disjoint regime variants
  of one signal; `RegimeLabelSet` (≤ 4 terms) for coded members and the
  ICDP `[regime]` block; library `labels` is a list; composer selects
  by ∃-label or `--fit-from-evidence`.
- 2026-09-03 — **RG0 code-complete** (uncommitted; operator commits).
  Landed: `core-types/src/regime.rs` (`RegimeWord`/`RegimeLabel`/
  `RegimeRel`/`RegimeTerm`/`RegimeLabelSet`, `allows`/`intersects`
  laws, text grammar `parse_label_term` + `RegimeLabelBuilder`, 13 unit
  tests); `AiCmdKind::SetRegime = 12` + shape arm + tests (`ALL_KINDS`
  13, first unassigned byte 13); `RuleRowV2` tail = `regime_fast` @88 /
  `regime_slow` @96 / `regime_off` @104 / `regime_rel` @105 / `_pad3`
  22 B (size 128 unchanged; `with_regime`, `regime_term`,
  `regime_fields_well_formed` = the rule-11 body; legacy rows
  bit-identical, tested); `audit-replay` AI-kind table sized 13 (was 10
  — a pre-existing OOB on captured seeds, fixed); `frames.py`
  `KIND_SET_REGIME` + `regime_word()`/`regime_word_dims()`/
  `regime_word_is_wire_declared()`; shared golden fixture +4 vectors
  (kinds 10/11/12), both suites assert coverage `0..=12`;
  `docs/wire-format.md` (AiCmd row, `RuleRowV2` tail, byte map),
  `docs/migration.md` entry, `regime.toml.example` (§4.6 shape, the RG2
  parser's contract). Gates: nextest core-types/ingress-ai/strategy-vm/
  strategy-set/cli 423/423; pytest frames 11/11; `make lint` green;
  `make license-check` OK (240 files); rustfmt clean on touched files;
  no mode changes. Not done in RG0 by design: no engine behaviour
  changes (nothing reads the new fields yet), no `core-config::regime`
  parser (RG2), no validator rule 11 wiring (RG3).
- 2026-09-03 — **RG0 amendment (pre-commit): per-dimension unknown
  mark.** Bit 7 of every market byte = "could not be judged"; omitted
  label dimensions fill with the legal mask INCLUDING the mark,
  explicit lists exclude it (`unknown` token adds it, `!v` never does);
  `RegimeWord::UNKNOWN = 0x0004_8080_8080_8080`. Same gate formula,
  fail-closed per dimension. `frames.py` mirrors it; wire-format +
  migration text updated. Reason: without it a measured word with an
  unjudgeable VOL either invalidated the whole word or let a `vol:low`
  row trade blind.
- 2026-09-03 — **RG1 code-complete** (uncommitted). Landed:
  `crates/core-regime` (`RegimeState` boot-boxed ≈ 400 KB: 32
  minute-close rings × 1536, linear-probe member map, `on_tick` = one
  compare + one store, `on_timer` rolls + judges once per wall minute,
  `seed()` from `candles.db` rows with a `2·confirm_min` warm replay,
  `set_declared`/`clear_declared`/`refresh_effective`, readers for
  measured/declared/effective/rel/raw/flips/disagree;
  `ProfileParams::{FAST,SLOW}_DEFAULT` = the example file; `math.rs`
  hosts `ret_bps_1e9`/`isqrt_i128`/`floor_div` — `strategy-icdp`
  re-exports, `strategy-vm` imports, duplicates deleted); the pure law
  as free functions (`close_at`/`ret_over`/`er_over`/`rv_over`/
  `judge_*`/`merge_declared`/`declared_disagrees`) mirrored
  function-for-function in `claude_worker/regime.py`
  (`RegimeEvaluator` over minute closes); the shared parity fixture
  `claude-worker/tests/fixtures/regime/parity-1.{input,expected}.tsv`
  (400 minutes × 4 symbols: uptrend / chop / divergent downtrend /
  high-vol / flat / holes, 3 funding prints, 2 declarations incl. an
  explicit unknown mark; 680 judged lines) written by
  `crates/core-regime/tests/parity.rs` under `REGIME_PARITY_WRITE=1`
  and asserted by both suites; bench **gate 41**
  (`regime_on_tick_and_minute_roll_are_zero_alloc`, 0 B/op in
  release). Design deltas vs the draft: no running sums (direct
  per-minute recompute ≈ 2k integer ops); tick→minute attribution by
  one compare against `minute_end_mono` (no division on the tick
  path); a measured word is usable as soon as any dimension is known;
  partial declarations merge per dimension (§3.6 as landed). Tests:
  core-regime 22 (unit + 2 proptests + parity), pytest
  `test_regime.py` 5.
- 2026-09-03 — **RG2 code-complete** (uncommitted; live smoke = the
  operator's, one-engine law). Landed: `core-config::regime`
  (`regime.toml` parser in the icdp scanner family — `[refs]`,
  `[breadth]`, `[hysteresis]`, `[profile.fast|slow]`, optional
  `[labels.<member>]` with `off` + `term1..term4` string arrays; the
  scanner gained string arrays; the committed `regime.toml.example`
  is a test fixture; `regime-seed.tsv` reader); `strategy-core`:
  `Strategy::{regime_label, set_regime_label, on_regime}` (defaulted —
  every existing strategy unchanged), `RegimeGate`, `RegimeCounters`;
  `strategy-set` owns `Box<RegimeState>` (`configure_regime` /
  `seed_regime` / `set_regime_label`, feeds `on_tick` → detector first,
  funding-ref events, the 1 s `REGIME_TIMER_NS` poll when configured,
  `SetRegime` at set level, edge-triggered `on_regime` fan-out to
  ENABLED members with gate re-sync on Enable, labelled members
  fail-closed from `configure` until the regime is known);
  `strategy-ai-exec` refuses intents while closed
  (`intents_refused_regime`); `strategy-icdp` blocks decisions while
  closed (`regime_blocked`) and on HARD close exits open positions at
  once with the bar's remaining TTL (`regime_exits`; the roll's exit
  law refactored into `exit_position`); cli: `--regime` /
  `--regime-seed` (absent DEFAULT artifact ⇒ unconfigured, explicit
  or invalid ⇒ boot refused), descriptor resolution via the D-6
  table, seed rows filtered to members, boot tells `regime: artifact
  configured hash=… seed_rows=… fast=… slow=… gates=…` /
  `regime: no artifact`, the `engine_regime_*` family (≈ 50 names:
  per-profile word/source/age/raw gauges, per-dim flip counters,
  disagree, `engine_strategy_regime_gate_{0..7}`, minutes/seed/
  declared/gate-change counters) + `engine_icdp_regime_{blocked,exits}_total`;
  worker: `python -m claude_worker.regime seed-out` (candles.db 1 m
  closes of the artifact's descriptors → `regime-seed.tsv`, atomic
  write) wired into `scripts/engine-wrapper.sh` before the exec.
  **Re-scoped:** the harness replay + per-regime audit-pnl (draft
  RG2-e) moves INTO RG3 — the harness drives a bare `VmStrategy`, so
  regime words only matter there once rows carry masks; the TUI
  snapshot words move to RG6 (the snapshot is replaced there); the
  `declared.json` post-boot re-push moves to RG5 with the declare
  lane. Coded labels: ANY at RG2 (§11-Q10) — `regime.toml
  [labels.*]` is the operator's lever, no code change needed. Not a
  behaviour change until `~/multivenue/regime.toml` exists. Gates at
  RG2 close: nextest 1555 · alloc 41/41 · lint green · license-check
  OK · pytest 633 (+ the known UDS-fixture flake green in isolation) ·
  no mode changes.
- 2026-09-03 — RG0–RG2 committed by the operator as `77f5ea5`.
- 2026-09-05 — **RG3 code-complete** (uncommitted; operator commits).
  Landed, per the §7 row: **(1) grammar v2.1** — `ingress-ai/src/ruleset.rs`
  gains the v2 row keys `regimes` (non-empty string array, §3.3 grammar,
  `fast:`/`slow:` prefixes, `rel:` terms allowed on rows; scanned into a
  64 B term buffer, folded through `core_types::regime::RegimeLabelBuilder`
  — 0 B/op, no `core-config` dependency), `regime_off` (`soft`|`hard`),
  `rel` (sugar for one `[fast:|slow:]rel:<values>` term) →
  `RuleRowV2::with_regime`; **rule 11** = term grammar + no duplicate
  `(profile, dim)` + `regime_off`/`rel` only beside `regimes` + the
  stored tail's `regime_fields_well_formed` (`RulesetReject::Regime`;
  JSON-shape faults stay rule 2); a profile constrained only by REL
  stores the new `RegimeLabel::LABELLED_ANY` (the omitted-dimension
  fill: every value, `measured|declared`) so REL-only rows are labelled
  rows (fail-closed on warm-up) instead of tail-law violations;
  **rule-8 amendment** = identity tuple AND
  `RegimeTerm::intersects` (disjoint variants admit — the three-way
  bull/neutral/bear split of one signal is one table; overlapping
  regions, different-dimension products, cross-profile pairs and
  legacy duplicates still refuse). **(2) vm row gate** — the set→vm
  seam is `core_regime::RegimeView` (`RegimeState::view()`: effective
  words + every member's REL byte per profile, 232 B, minute cadence)
  handed through `VmStrategy::set_regime_view` by
  `StrategySet::push_vm_regime_view` on configure / seed / every
  minute roll (REL can move with no word change — `on_timer` compares
  `minutes_judged`) / effective change / declaration, regardless of
  slot 5's own always-ANY gate; the vm re-judges every active row into
  one gate byte (`row_gate`: open / hard) on each push and on each
  flip (`on_table_flipped`), so the hot path pays ONE byte load per
  evaluated row: the entry/refire path skips closed rows
  (`regime_blocked`, per evaluated tick like `evals`), the exit path of
  a HARD-closed position row runs `emit_exit` at once after the age-out
  check and before min-hold (`regime_hard_exits`; ring-full retries);
  soft-closed rows drain by their own law; a view change touches
  neither `tables` nor `positions` (proptest); unlabelled rows judge
  open under every view (bit-identical). `StrategyCounters::vm_regime_{blocked,hard_exits}`
  → `engine_vm_regime_{blocked,hard_exits}_total` (paper.rs mirror +
  the §9 observability pin). Without a detector the vm keeps
  `RegimeView::UNKNOWN`: labelled rows fail closed, as live. **(3) the
  harness** — `cli::backtest::regime` (`RegimeMode` {`Auto` = the
  default artifact when it exists AND resolves on the root, else
  regime-blind with a stderr tell — the frozen worker argv can never
  fail on a default artifact; `Off` = tails stripped, every row ANY;
  `Artifact(path)` = refusals fatal}; `RegimeReplay` = the engine's own
  `RegimeState` anchored at (first virt, first wall), seeded from
  `--regime-seed` → the first run's own `regime-seed.tsv` → warm live,
  fed replay ticks + the fund ref's Funding/AssetCtx prints + the 1 s
  timer on the virtual clock + the window's `SetRegime` frames from
  `ai-cmds.pmlr` (`RecPayload::Regime`, lane 48; frames stamped before
  the run's first tick are clamped to it with the TTL shortened,
  expired ones dropped + counted), handing the vm the view exactly as
  the set does); `cli::regime_boot` = the ONE resolver the engine boot
  (strict) and the harness (members absent from the root dropped +
  counted; refs must resolve) share — moved out of the bin. `--regime`
  / `--regime-seed` on `backtest` and `audit-pnl`; `HarnessStats`
  `regime_*` + `BacktestOutput.regime` (`RegimeReport`: tells,
  per-profile minutes-per-word histogram, final words); stderr
  `regime: …` block (artifact/seed/mode tells, `regime fast:
  [word]=Nm …`, `regime end: fast=… slow=…`, counters);
  `--emit-detail` = `detail_version` **4** (additive `regime` block);
  `audit-pnl` gains funding events + `SetRegime` frames in its stream
  (class `CLASS_REGIME` between ticks and fills), one fill-model replay
  per `(profile, effective word at emit, strategy)`, the additive
  `regime` JSON section (`audit_pnl_version` stays 1) + stderr rows.
  **(4) worker** — `window_root.cut_run` cuts `ai-cmds.pmlr` like every
  file AND carries the pre-window `SetRegime` still in force at the cut
  (the LATEST frame per profile decides; ≤ 4096-frame scan), and writes
  the window's own `regime-seed.tsv` from `candles.db` when
  `seed=(regime.toml, candles.db)` is passed (`pnl_report` day mode
  passes `regime_seed_inputs()`); `strategist` **prompt v3**
  (`strategist-v3`: the keys, the gate law, VARIANTS on disjoint
  regions, worked examples A/B relabelled + example C = a bull/bear
  variant pair, rule-11 lines) and the structural mirrors
  `regime_term_ok`/`regime_rel_ok` in `parse_proposal` (keys optional in
  the parser, asked for by the prompt; canonical emission order puts
  them before `horizon_ms`). **(5) gates** — bench gate 42
  (`vm_regime_gate_and_view_rejudge_are_zero_alloc`: 256 labelled rows,
  view flips every cycle, hard exits + blocked entries, 0 B/op); fuzz
  `ruleset_json` reaches the keys through local corpus seeds
  `fuzz/corpus/ruleset_json/rg3-seed-*` (the corpus is gitignored; the
  ≥ 300 s run is recorded below). Tests: ingress-ai (tail landing,
  soft default + rel prefix forms, REL-only fill, every rule-11 refusal,
  the rule-8 amendment matrix), strategy-vm (gate blocks / legacy open,
  hard flatten vs soft drain, age-out-first + ring-full retry, REL from
  the view, flip re-judge, the variants/state proptest), strategy-set
  (view pushed on configure/seed/declaration/minute), core-regime
  (`view()`), core-types (`LABELLED_ANY`), cli (`regime_boot`,
  `backtest::regime` unit + the two `rg3_*` harness integrations:
  declared-gated on/off/refusals, expired-declaration fail-closed),
  worker (ai-cmds cut + carry, window seed, prompt v3 + 13 malformed +
  20 term-vocabulary cases). Not done by design (§7 rows): the
  `pnl_report` per-regime MERGE (RG5), the TUI snapshot words (RG6),
  `FeatId::RegimeRel` (RG3b, §11-Q8). **Gates at RG3 close (2026-09-05):
  nextest 1578 · release alloc 42/42 0 B/op (fresh `Compiling bench`) ·
  `make lint` green · `make license-check` OK (247 files) · worker
  pytest 671 (frozen 202 inside) · fuzz `cargo +nightly fuzz run
  ruleset_json -- -max_total_time=330`: 331 s, 37.6 M runs, cov 1732,
  no crash · `cargo build --release -p cli` relinked (G0) · `git diff
  --summary` shows no mode change.** Design notes worth keeping: the vm
  precomputes the per-row verdict on view change instead of evaluating
  `allows()` inline per tick (the same law, one byte per row on the hot
  path; a flip re-judges from the stored view); `RegimeMode`'s trait
  `Default` is `Off` so library callers and tests never consult the
  operator's home directory — only the bin maps an absent flag to
  `Auto`; the repo is not rustfmt-clean as a whole (pre-existing
  hunks), so only the new hunks were formatted.
- **RESUME POINT (for a fresh session):** RG3 is code-complete on disk
  (commit status = the operator's; gates at the end of this entry).
  Next = (a) the operator's live smoke of RG2+RG3 together — `cargo
  build --release -p cli` (G0 relink), `~/multivenue/regime.toml`
  from the example with members that exist in `universe.toml`, restart
  via launchd, check `regime: artifact configured`,
  `engine_regime_minutes_judged_total` climbing, `vm_rows_active 1`
  after #7b, then stage + commit ONE labelled ruleset (two disjoint
  variants of the live row is the smallest honest shape) and watch
  `engine_vm_regime_blocked_total` move with the effective word — the
  §7 RG3 exit tell; (b) then **RG5** (worker regime lane: report /
  percentiles / declare / `pnl_report` per-regime merge / serve
  `_REGIME` phase with the SDK mocked) or **RG4** (library + composer) —
  the operator picks. Harness numbers on a labelled ruleset are
  meaningful only on a ≤ 2 h v3 window WITH a seed (`window_root` now
  writes one) — a seedless window judges `slow` UNKNOWN for its whole
  span. Laws unchanged: regime is a gate not a signal; exits never
  gated; legacy rows bit-identical; no table flip on regime change;
  ≤ 2 h windows; research out of git; compile/test on the Mac
  (RustRover), never the sandbox; explicit-path `git add`, operator
  commits, never push.
