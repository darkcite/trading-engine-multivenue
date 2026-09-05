# Market regime + regime labels + regime-aware AI aggregation + dashboard — plan (RG0–RG7)

**Status: IN PROGRESS — RG0–RG2 committed (`77f5ea5`); RG3 committed
(`81ed263`, 2026-09-05); RG5 committed (`8defc26`, 2026-09-05); RG2/RG3/RG5
live smoke PASSED 2026-09-05 (§12); RG4 code-complete 2026-09-05
(uncommitted — §12); RG6 committed (`ef75c91`, 2026-09-05 — engine
`/state` + TUI + the worker page on 9292, all LIVE; §12 last entry).
RG6 CLOSED by operator ruling 2026-09-05 (§7 exit tell met: page live on
9292 showing regime, mask, rows, P&L, recent fills). RG7 restated under
the ≤ 2 h law (§7.1) and OPEN: the soak = N pooled ≤ 2 h windows with
regime gating live, judged by `regime soak`; §11 defaults apply until
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

**As landed (RG6 engine half, 2026-09-05 — the `/state` contract,
`"v": 1`; pinned byte-exact by `crates/engine-snapshot/tests/encode.rs`).**
Crate `engine-snapshot` (deps: `core-types`, `strategy-core`) holds
`EngineSnapshot` (≈ 21 KB, `#[repr(C, align(64))]`, `Copy`), the generic
seqlock `SnapshotCell<T>` (the tui cell generalized — `publish(&T)`,
`read()`, `read_into(&mut T)`), `RecentRing<T, N>` (the engine keeps
`recent_orders`/`recent_fills` beside its captures) and
`encode_state_json` (sticky-overflow cursor: complete body or
`JsonOverflow`, never truncated; `STATE_JSON_MAX` = 160 KiB, the
server's `RESP_BUF_SIZE` = 256 KiB). `core-metrics` stays
dependency-free: `serve_metrics(addr, registry, state: Option<F>, stop,
on_event)` takes the writer as an `FnMut(&mut [u8])` (`None::<StateFn>`
⇒ 404; encode error ⇒ 500). The cli publishes every `SNAPSHOT_PERIOD_NS`
= 1 s (`fill_snapshot` + one `publish` — a second gate beside the 5 s
`next_report`; the T1(c) tick-age stamps moved to that gate); the cell
exists whenever `/metrics` does; the TUI (`--tui`) reads the same cell.
`crates/cli/build.rs` records `MULTIVENUE_GIT_SHA` (soft — `unknown`
without git; re-runs only when `.git/HEAD`/its ref moves); the binary's
own mtime is the relink tell.

*Number law:* counters, masks, small ids, ×1e6 prices/qtys = JSON
numbers; ns stamps, 64-bit ids/hashes/words = strings (decimal for
stamps/ids, lower-hex for hashes/words); `*_age_s` derived against the
snapshot's `now.mono_ns` (`-1` = never). Body timestamps are
engine-monotonic — `wall = now.wall_ns + (t − now.mono_ns)`.

| key | content |
|---|---|
| `v`, `seq` | schema 1; publish counter |
| `now` | `mono_ns`, `wall_ns` (anchor arithmetic), `uptime_s` |
| `boot` | `pid`, `git_sha`, `binary_mtime_ns`, `boot_wall_ns`, `run_epoch_ns`, `run_dir`, `strategy` (the `--strategy` name), `strategy_kind`, `paper`, `requested_mask`, `configured_mask`, `enabled_mask`, `halted`, `ruleset_hash`, `ruleset_staged_hash` (hash128 hex, zero = none), `icdp_hash`, `regime_hash` (sha256 hex), `regime_configured` |
| `counters` | `iterations`, `ticks`, `signals`, `fills`, `events`, `depths`, `opts`, `orders_emitted`, `orders_dropped`, `ai_dispatched`, `ai_drain_malformed` |
| `latency` | `ingest`/`decide`/`ack` × `{p50_ns, p99_ns}` |
| `regime` | `configured`, `minutes_judged`, `seed_rows`, `declared_total`, `gate_changes`, `gates[8]` (0 open / 1 soft / 2 hard), `profiles[2]` = `{name, measured, declared, effective}` each a word `{hex, dims[7]}` (raw dimension bytes — decode with the §3.3 byte map) + `declared_age_s`, `declared_ttl_s`, `disagree`, `flips[8]`, `raw{present, ret_bps_1e9, er_1e9, rv_bps_1e9, stretch_1e9}`; `rel{syms[], fast[], slow[]}` (REL byte per breadth sym, 255 = unknown) |
| `slots[8]` | `slot`, `name`, `configured`, `enabled`, `gate`, `label_terms`, `label_off`, `orders_emitted`, `orders_dropped` |
| `vm` | `active_hash`, `staged_hash`, `rows_active`, `epoch`, `fires`, `orders_emitted`, `orders_dropped`, `commit_dropped`, `regime_blocked`, `regime_hard_exits`, `rows[]` = `{i, name_h (hex), sym, ref_sym, flags, family, gate (bit 0 open, bit 1 hard), regime_off, state (1 = entered), side, entry_sign, entry_px_1e6, qty_sym_1e6, entry_ts_ns, age_s}` |
| `icdp` | `configured`, `hash`, `instruments`, the 14 `IcdpCounters` |
| `ai` | the 15 `AiIngressStatus` counters (+ `drain_malformed`, `enable_refused`), `heartbeat_age_s` |
| `ingress[7]` | `venue` (pm, bn, okx, deribit, hl, bybit, rpc), `state` (0 down / 1 connecting / 2 up / 3 backoff), `last_tick_age_s`, `ticks`, `msgs`, `reconnects`, `ring_drops`, `stale_ticks`, `parse_errors`, `gaps`, `sub_drops`, `feed_delay_ema_ms` |
| `capture` | `fills_records`, `fills_io_errors`, `orders_records`, `orders_io_errors` |
| `recent` | `orders_total`, `orders[≤64]` oldest-first `{ts_ns, age_s, slot, venue, sym, side, kind, px_1e6, qty_1e6, oid, ttl_ns}`; `fills_total`, `fills[≤64]` `{ts_ns, age_s, sym, side, px_1e6, qty_1e6, oid}` (a `Fill` carries no slot — join on `oid` against the orders) |

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
| **RG7** soak + close (RESTATED 2026-09-05 under the ≤ 2 h law — §7.1; the original "5 paper days / per day / 5 nightly reports" wording is VOID) | the soak = **N ≥ 8 complete ≤ 2 h windows** with regime gating LIVE (detector configured + a labelled table active), pooled from runs that already exist (single-run pools admissible — never a wait); per-window flip bound (≤ `FLIPS_MAX_PER_WINDOW` = 2 per market dimension per profile per window — the old 24/day rate restated for 2 h); per-regime P&L present in the nightly reports covering the pooled windows; the seed-hole fix (candles tail refreshed before `seed-out`); docs: `CLAUDE.md` CURRENT STATE, `ai-strategy-pipeline.md` (+svg), `research-universe.md`, `local-setup.md`, `risk-policy.md` cross-ref; close entry in §12 | `python -m claude_worker.regime soak` (the judge: history samples + the engine's own flip counters per window; pytest); stay-greens recorded | `soak` verdict PASS on N ≥ 8 windows + operator ruling |

Dependency order: RG0 → RG1 → RG2 → {RG3, RG5} → RG4 → RG6 → RG7. RG6's
engine half can start after RG2. Estimated size: RG0–RG2 ≈ the ICDP
I1–I5 footprint; RG4–RG6 mostly Python.

### 7.1 RG7 under the ≤ 2 h law (restated 2026-09-05)

Operator law (2026-09-05, verbatim): *"in any scenario any test time /
soak time / protect time MUST NOT EVER BE MORE THAN 2 hours."* A soak
is therefore never a calendar span. It is a COUNT of disjoint, complete,
≤ 2 h windows that already exist, judged together:

- **Window.** One `≤ 2 h` `ts_ns` slice of one run (the RG4 pool law:
  `~/multivenue/worker/windows/`, newest K = 8 complete v3 cuts,
  count-pruned). A window counts for RG7 only if regime gating was LIVE
  through it: the engine's detector configured (`regime_configured 1`,
  seed applied) AND a labelled table active (`vm_rows_active ≥ 1` with
  regime masks) — i.e. windows cut from runs after the RG5 live smoke
  (2026-09-05 ≈ 05:11Z) and the RG6 restart (08:02Z).
- **N.** `N ≥ 8` windows (the pool size). Fewer = the verdict is
  `INSUFFICIENT`, never a wait — the next lane cut adds windows as the
  engine keeps running.
- **Flip bound per window.** For each profile × market dimension
  (trend, shape, vol, fund, level, stretch): flips inside the window ≤
  `FLIPS_MAX_PER_WINDOW` = 2 (the old "24/day" restated: 24 × 2 h / 24 h).
  Source of truth = the engine's own cumulative
  `engine_regime_<p>_flips_<dim>_total` counters, sampled every 5 min
  into the regime history by `com.multivenue.regime` (RG7 extends the
  history entry with the engine's `/state` regime block: flips,
  `minutes_judged`, effective words, pid); the per-window count is the
  counter delta between the last sample at/after the window's start
  and the last sample before its end, valid only when the pid is
  unchanged inside the window (a restart resets the counters; such a
  window is judged from the worker's 5-min mirror — word changes at
  5-min resolution — and marked `mirror`).
- **Coverage.** ≥ 20 history samples inside the window (24 nominal).
- **Per-regime P&L.** The nightly report of each window's UTC day
  (`pnl-<day>.json`) carries the `regime` section with ≥ 1 word holding
  fill-model strategy rows per profile — the RG5 tell, now required
  over the pooled windows.
- **Seed hole.** The fast profile must not be UNKNOWN for the first
  hour after a restart: the wrapper refreshes the 1 m candles tail of
  the regime's own descriptors right before `seed-out` (`regime
  seed-out --refresh-tail`, ≤ 8 instruments × 1–2 REST pages), so the
  seed reaches the boot minute.
- **Verdict.** `python -m claude_worker.regime soak` prints one line
  per window (`PASS` / `FAIL <dim> flips=n` / `mirror` / `short`) and
  the pooled verdict: PASS when every counted window passes and
  N ≥ 8; the JSON goes under `~/multivenue/worker/regime/soak-<utc>.json`
  (worker state, never git). The operator's close ruling follows a
  PASS.

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
- **2026-09-05 — RG5 CODE-COMPLETE (the worker regime lane, §5.1 +
  §5.4 + §5.5; RG3 was committed as `81ed263` earlier the same day).**
  The operator picked RG5 over RG4 (AskUserQuestion). What landed —
  **(1) `claude_worker.regime` lanes** (module, never a verb; the
  `seed-out` lane from RG2 kept): `report` (worker-measured words + raw
  values per profile, the declaration in force, the engine's words from
  `/metrics` gauges `engine_regime_<profile>_{measured,declared,effective}`,
  the 24 h timeline), `history`, `refresh-params [--dry-run]` (RV +
  funding p30/p70 over the §5.1 lookbacks — fast 7 d of 1 h RV, slow
  30 d of 4 h RV, funding prints — rewriting ONLY the six percentile
  lines of `regime.toml`, comments kept, `.bak` beside it; too few
  samples keep the zeros = dimension ABSENT, honestly), `declare --fast
  "<decl>|measured" --slow … --ttl N --source operator|strategist
  [--no-send]` (persists `declared.json` FIRST, then one `SetRegime`
  frame per profile over `uds.py`; `qty` carries the worker-measured
  audit word), `cycle` (the 5-minute launchd job: measure + history +
  once-per-UTC-day refresh via a stamp file; NEVER declares), `repush`
  (post-boot: every still-fresh entry of `declared.json` re-sent with
  ITS OWN remaining TTL). **Measurement law**: `measure()` is the
  engine's seed law over `candles.db` judged at the LAST minute the
  store holds for the BTC ref and reports `age_min` — the candles lane
  is hourly, so a lag is normal, not an error. State lives under
  `~/multivenue/worker/regime/` (`$CLAUDE_WORKER_REGIME_DIR`;
  config-carrying callers use `regime_dir_for(db_path)` = the worker dir
  + `regime`, so tests never touch the operator's directory).
  `recommit` re-pushes the declaration after every boot's re-commit on
  the same connection (best-effort; a transport error never fails the
  re-commit). `scripts/regime-cycle.sh` + `launchd/com.multivenue.regime.plist`
  (StartInterval 300, worker-serialization pgrep guard, absent artifact
  = exit 0) are installed by `install-launchd.sh`. **(2) label mirror +
  intent lanes**: `label_masks`/`label_allows`/`regime_allows` mirror
  the §3.3 grammar (omitted dims of a touched profile fill with the
  any-mask incl. the unknown mark; SOURCE defaults measured|declared);
  `lane_gate(label, now_ms, words)` = the coded lanes' entry gate —
  empty label = ANY with no engine/file touch (bit-identical to
  pre-RG5), else the `current_words` chain engine → fresh declaration →
  UNKNOWN (a constrained profile fails closed). `xv_signal` and
  `carry_signal` carry `REGIME_LABEL` (default empty) and gate ENTRIES
  only (CVFC + S1 entry passes, the xv entry arm); exits drain by their
  own law; `run_cycle(..., regime_words=)` injects words for tests. **(3)
  nightly**: `pnl_report.merge_reports` folds every run's additive
  `regime` section into a merged `regime` key — mode counts,
  minutes_judged / declared_applied / set_regime_frames / expired sums,
  per profile × word (keyed by hex `bits`, ordered by minutes desc)
  minutes + per-strategy fills/orders/trades/net/ladder; a pre-RG3
  report contributes nothing; `regime_head_lines` adds the `regime …`
  summary lines the `pnl` verb prints; `latest_report_regimes` feeds the
  digest. **(4) serve `_REGIME` phase** (§5.4; the §7 gate is untouched —
  no `serve` runs, the SDK stays mocked): `ResearchCycle` gains
  `regime_inputs=(regime.toml, candles.db)` (production =
  `regime.regime_inputs()` from the environment,
  `$CLAUDE_WORKER_REGIME_TOML` override; a test that injects
  `research_env` gets NO phase unless it passes
  `research_regime_inputs`) and a synchronous `_REGIME` phase between
  capture selection and fetch: `serve_regime_step` = measure → history →
  AUTO-CONFIRM (declare the measured word, source `serve-measured`) for
  every profile whose declaration in force is not a FRESHER ruling by
  someone else (operator `declare` / the strategist's verdict win while
  fresh; serve's own prior confirm is overwritten); never raises —
  absent inputs / transport are counted skips (`ResearchStats.regime_*`)
  and events `regime_measured` / `regime_verdict` land in the ledger.
  The digest gains the REGIME section (`build_digest(regime=)`:
  `regime_digest_from` = the report text + the latest nightly report's
  per-regime P&L rows). **Prompt `strategist-v4`**: the output contract
  gains the OPTIONAL `"regime": {"fast": "<decl>|measured", "slow": …}`
  verdict (rule only when the digest contradicts the measurement; never
  invent a regime to fit the rows); `parse_proposal` accepts it
  (`Proposal.regime`, structural: `regime.parse_verdict`), the cycle
  applies it right after the parse — independent of the gate result —
  as a declaration with source `strategist` (a `measured` verdict without
  a measurement is a counted failure, never a declaration of nothing).
  Semi-manual `ai-session.md` §4 steps 0a–0c and the LIBRARY section are
  RG4's (the library does not exist yet). **Gates at RG5 close
  (2026-09-05): worker pytest 692 (frozen 202 inside; +21 over RG3:
  `test_regime_lane` 9, research-cycle 3, strategist 3, pnl 2, xv 1,
  carry 1, window_root/prompt pins) · `make license-check` OK (249
  files) · `make lint` green (Rust untouched by RG5 — the lint is the
  standing gate) · no Rust change ⇒ nextest/alloc/fuzz numbers stand
  from RG3 · `ruff` finding counts on the touched files did not grow
  except new E501s in the same hand-wrapped style the files already
  carry (py-lint is not a gate).**
- **2026-09-05 — RG2+RG3+RG5 LIVE SMOKE PASSED (run by the session on
  the operator's "you do the smoke first"; RG5 hash entry = `61b9ab8`).
  Every §7 exit tell observed on the standing launchd engine:**
  - **Install.** `~/multivenue/regime.toml` from the example with
    `[breadth] members` = the six `binance.usdm` crypto majors that
    exist in `universe.toml` (`eth sol xrp doge ada ltc`; the example's
    `bnbusdt` is not in the universe and would have refused the boot;
    §11-Q2's all-usdm default was NOT taken — stock perps and micro-alts
    are not a BTC-trend breadth set; the operator's pre-existing
    `regime.toml.candidate` had the first four); `refresh-params` filled
    the six percentile lines from candles.db (fast RV p30/p70 26.4/39.8
    bps on 167 samples, slow 65.2/103.0 on 93, funding 9/90 prints) so
    VOL + FUND_LEVEL are judged, not ABSENT. `./scripts/install-launchd.sh`
    = graceful restart + `com.multivenue.regime` installed (StartInterval
    300). Release binary verified current (`cargo build --release -p cli`
    = `Finished … 0.20s`, nothing recompiled since RG3).
  - **Boot tells (pid 60169, 04:55:25Z, run `run-1788584102583092000`):**
    wrapper `regime seed-out: 10346 rows for 7 descriptors`; engine
    `regime: member resolved` ×6, `regime: seed file read rows=10346
    dropped=0`, `regime: artifact configured hash=8be4364c… members=6
    confirm_min=3 seed_rows=10332 … gates=[0,…]`, `strategy-set:
    composed mask=112`; `/metrics` `engine_regime_configured 1`,
    `engine_regime_seed_rows 10332`, `engine_regime_minutes_judged_total`
    1/min (18 at 05:13Z); #7b recommit re-staged/re-committed
    `bfbc5349…` (seq 39189/39190) ⇒ `vm_rows_active 1`; `regime repush:
    no fresh declaration persisted — nothing to re-send` (the RG5
    post-boot lane ran). Fast TREND judged `neutral` after the 3-minute
    confirm (word `0x0001808080808002`, source 0 = measured).
  - **Worker lane.** `regime report` = fast `neutral/mixed/low/pos/
    normal/neutral`, slow `neutral/chop/low/pos/normal/neutral` (candles
    lag 3 min); `declare --fast measured --ttl 900` → seq 39422 →
    `engine_regime_fast_declared 0x0000020202010202`, effective =
    declared (`0x0002020202010202`, `fast_source 1`, `declared_age_ns`
    0.9 s); slow untouched (`slow_source 0`). The 5-minute cycle ran at
    04:59/05:09 (history rows written; the 05:04 slot skipped itself on
    the pgrep guard while a worker backtest was live) and did the
    once-per-UTC-day percentile refresh at 04:59 (stamp
    `params-refreshed-utc-day`; `.bak` = the file the engine booted
    with) — the engine keeps `8be4364c…` until the next T2 restart, the
    harness reads the refreshed file (`1f57d25a…`): by design, noted.
  - **Labelled ruleset (the RG3 exit tell).** Two disjoint variants of
    the live xv row (`xv-okx-bnspot-vlow` `regimes:["vol:low"]` /
    `xv-okx-bnspot-vnot` `regimes:["vol:!low"]`, everything else
    identical incl. `exit 1.0`, both `regime_off` soft) = artifact
    `fde6f733e72649e0c6452b009d0f7c3c…` (installed by hand as
    `<hash128>.json` + `.report.json` — the frozen `stage-ruleset` verb
    binds gates and sends the frame but does NOT install the artifact:
    the first stage (seq 39424) targeted a missing file and the engine
    ignored it silently). Gate evidence per the ≤ 2 h law: a research
    one-shot of the git-excluded class (owner doc
    `docs/arch/research-tools-exclusion-plan.md`) pooled 11 seeded 2 h
    windows (4+3+2+2 from the four v3 runs of 2026-09-04
    00:01Z → 2026-09-05 04:54Z, 9.8 GB, RSS ≤ 8.6 GB) under
    `~/multivenue/research/rg-smoke/root`; the frozen worker `backtest`
    on that root: **PASS** — OOS legs 78, round trips 14, net +$4.51
    (0-fee argv, an upper bound), DD $9.26, 2 trading days, bounds
    $3,002/$15,006/$20,964. A single-window harness run (`--emit-detail`
    = detail_version 4) proves the replay is judged, not regime-blind:
    `regime: artifact … seed_rows=10745 fast=trend=neutral shape=chop
    vol=low fund=unknown level=unknown stretch=neutral`, `labelled_rows
    2, blocked 267948, minutes_judged 120` (FUND dims unknown: no
    Binance funding events reach this host — ops debt (d)). Stage seq
    39426 + commit seq 39428 → `vm_rows_active 2`, `engine_vm_table_epoch
    2`, `engine_vm_regime_blocked_total` 0 → 37 within 4 s and ≈ 100–900
    per 10 s thereafter (variant B gated under the declared `vol:low`;
    the rate follows the syms' tick bursts). `declare --fast "vol:high"`
    (seq 39430) → effective `0x0002808080048080` (only VOL overridden,
    the rest = the measurement), the counter kept climbing (variant A
    now gated), `table_epoch` STAYED 2 and `vm_rows_active` 2 = **no
    table flip on a regime change**; `declare --fast "vol:unknown"`
    (seq 39432) accepted (both variants closed = fail-closed); restored
    with `declare --fast measured --ttl 2400` (seq 39434) so variant A
    trades until the engine's own VOL judgement lands.
  - **Findings to carry (not fixed here):** (1) **seed hole** — the
    wrapper's `seed-out` ran at 04:54 from candles.db ending 03:55 (the
    hourly candles lane refreshed minutes later), so the ring holds a
    03:56–04:54 hole; `close_at` walks ≤ 5 min over holes, hence fast
    TREND was known 04:58–05:00 (m−60 within 5 min of 03:55) and then
    UNKNOWN again until live minutes cover 60 min (~05:55Z); fast VOL /
    SHAPE need ≥ 80 % of their windows live (~05:45Z). Every T2 restart
    repeats this (up to ~1 h of fast-profile blindness ×3/day ⇒ labelled
    rows fail closed then). Fix shape for RG7: refresh the candles tail
    in the wrapper BEFORE `seed-out` (or seed the last hour from the
    previous run's own ticks). (2) FUND_SIGN/FUND_LEVEL are UNKNOWN live
    and in the harness on this host (Binance markPrice unreachable, ops
    debt (d)) — `fund:`/`level:` labels are unusable until the `.env`
    lever. (3) A stage frame for a missing artifact is dropped without a
    counter — RG6's `/state`/metrics should expose `stage_refused`.
    (4) `regime_blocked` counts per evaluated tick of a closed row, so
    its rate is tick-bursty; a per-row gate byte in `/state` (RG6) is
    the readable form. (5) The labelled table `fde6f733…` is now the
    ACTIVE row the daily recommit carries forward — the operator rules
    whether it stays (the RG7 soak shape: "5 paper days with regime
    gating live") or `bfbc5349…` is re-committed.
  - **Gates:** no source change (a git-excluded research one-shot only);
    stay-greens stand from RG3/RG5 (nextest 1578 · alloc 42/42 · pytest
    692 · lint · license-check).
- **2026-09-05 — RG4 CODE-COMPLETE (library + composer, §5.2–§5.4; the
  operator picked RG4 over RG6, then ruled by AskUserQuestion: keep
  `fde6f733…` live; import validates ONLY the active table; gate =
  pooled + on/off delta + LOWO; the carry legs come in as a CANDIDATE
  member; pool = 8 windows; words from `/metrics` with the measured
  fallback — and, verbatim, "in any scenario any test time / soak time /
  protect time MUST NOT EVER BE MORE THAN 2 hours", which is now a
  standing law: retention by window COUNT, a 2 h WALL BUDGET on every
  gate, soaks stated as window counts).** Python only, no Rust change.
  What landed — **(1) `state.py`**: additive tables `library`
  (`member_id` PK, name, kind vm-rows|coded, path, status
  candidate|validated|retired, `labels_json`, `regime_off`, thesis,
  `origin_json`, stamps), `library_evidence` (PK member × window:
  root, `n_ticks`, `n_fills`, `net_usd_0`, `net_usd_tier`, `max_dd_usd`,
  `regime_word_mode`, judged, `detail_version`, ts) and `compositions`
  (`table_hash` PK, hash128, `member_ids_json`, `words_json` — both
  profiles, the plan's `regime_word, profile` pair folded into one
  column —, path, `gate_json`, composed/staged/committed stamps); the
  typed readers `RegistryRow` / `LibraryMember` / `EvidenceRow` /
  `CompositionRow`; `library_insert` is insert-or-ignore (an import
  re-run never downgrades a status the operator set). **(2)
  `claude_worker/library.py`** (module lane): `member_id` = sha256 of
  the canonical rows (`strategist.parse_row` — a new public seam over
  the structural parser — + `artifact_bytes`), so a single-member
  composition's table hash IS the member id; labels derived from the
  rows' `regimes` (+ the `rel` sugar) — an unlabelled row makes the
  member ANY; ∃-semantics `label_fits`, `rel:` terms stripped from the
  word-fit law; member files `~/multivenue/worker/library/<id>.json`
  (`$CLAUDE_WORKER_LIBRARY_DIR`, `library_dir_for(db)`), drift-checked
  on read (rows must re-hash to the id); lanes `add` (`--split-by-name-
  prefix`, `--regimes` override, `--status`), `import-catalog` (registry
  rows via the new `State.rulesets_all()` + `candidates/*.json`, thesis
  + source hash in `origin`, dedup by file hash, the coded
  `icdp@<sha256>` from `icdp.toml` with `[labels.icdp]` of
  `regime.toml`; idempotent; `--dry-run`), `list [--regime current|
  "<decl>[;<decl>]"] [--status] [--all] [--json]` (a query's unnamed
  dimensions are UNKNOWN — fail-closed like the engine),
  `label … --regimes … | --any [--regime-off]`, `validate | retire |
  candidate`, `evidence <member> --window … [--fees]` (one run per
  window through the ADDITIVE `backtest.run_harness_extra` — the frozen
  argv + `--fee-bps` tier + `--emit-detail`, the carved `0/100` split
  so the WHOLE window is scored; the row reads `net_usd_0` from the fee
  ladder's zero rung, `net_usd_tier` from the report, `judged` from the
  detail's stale lanes, the dominant fast word from the regime block).
  **(3) `window_root.py`**: the standing POOL — `pool_candidates`
  (every complete 2 h window of every v3+ run, newest first;
  `run_pmlr_version` refuses stale-blind v2), `pool_ensure(logs, pool,
  k, seed)` (reuse existing cuts by name, cut the missing with their
  seeds, prune beyond the newest k BY COUNT), `pool_windows`,
  `complete_windows`, `symlink_root` (the LOWO roots). **(4)
  `claude_worker/compose.py`** (module lane): `effective_words` (query
  → RG5 chain engine → declaration → the worker's own `measure()` →
  UNKNOWN), `neighbourhood` (Hamming-1: one market dimension of one
  profile; an unknown byte neighbours every value), the rule-8 mirror
  (`row_identity` over the validator's tuple + `exit` presence,
  `row_region` = per-profile label masks + REL nibbles,
  `regions_intersect` = `RegimeLabel/RegimeRel::intersects`), the rule-7
  mirror (`cap_usage`: both legs of a position row, 256 rows, $10k /
  $20k / $100k), `select_members` (fit word | neighbour | any |
  evidence; retired never, candidates on `--include-candidates`; sorted
  by judged tier net, windows, name) → `admit` (rule 5 names, rule 8,
  caps) → `compose` → `write_composition` (canonical bytes at
  `<worker>/compositions/<hash128>.json`; idempotent) → `gate`
  (per-member evidence for every pool window it lacks, the FROZEN
  `run_backtest` on the pool = the binding report, `--regime off` on
  the pool = the on/off delta ≥ 0, LOWO = every pool-minus-one symlink
  root keeps OOS net > 0; every run charged to `WallBudget` 7200 s —
  `BudgetExceeded` is a FAIL reason, never a wait; a harness error is a
  verdict, not a crash) → `promote` (`FREEZE` pin refuses; hash ==
  active, or the active table's CANONICAL rows == the composition's,
  is a no-op — the smoke-era hand-written `fde6f733…` would otherwise
  flip to byte-different identical rows; else `install_candidate` →
  the frozen `stage_ruleset`/`commit_ruleset` pair in-process with the
  composed thesis as attribution → `engine_vm_table_epoch` watched).
  CLI: `--dry-run | --promote`, `--include-candidates`,
  `--fit-from-evidence`, `--regime`, `--pool/--pool-size/--no-refresh`,
  `--no-lowo`, `--fees`, `--budget-s ≤ 7200`, `--freeze/--unfreeze`,
  `--json`; exit 0 PASS · 2 nothing fits / usage · 3 gate FAIL · 4
  promote transport. **(5) `ai-session.md`** §4 steps 0a (`regime
  report`), 0b (`declare`), 0c (`library list --regime current` →
  author members → `library add` → `compose --dry-run` → steps 4–10, or
  `compose --promote`) + the new §8 LIBRARY section; the pinned
  scripted test extended in the same change (0a's honest exit 2 without
  an artifact, 0c on an empty library, `library add` of a REAL v2 row,
  `compose --dry-run --json` whose single-member table hashes to the
  member id, then backtest → install → stage → commit → push on the
  COMPOSED artifact). **Tests:** `tests/test_library.py` (8) +
  `tests/test_compose.py` (10) — state round-trips, canonical id /
  labels / drift, import preserves every hash + thesis and validates
  only the active table, query words, the evidence row from a fake
  additive-path run, every lane through `main`; neighbourhood count,
  rule-8 (ANY vs variant, disjoint variants, REL nibbles), caps,
  selection order + rule-8 dedup + idempotent hash, the gate on a fake
  harness (evidence caching, frozen pooled argv + binding report,
  off/LOWO/pooled/floor/budget/harness-error verdicts), promotion
  (freeze, same hash, canonical no-op, the frozen frames on the fake
  UDS server, registry + composition stamps), count-only pool pruning
  on crafted v3 runs (v2 refused), the CLI. **Gates (Mac):** worker
  pytest **709** (692 + 17; the frozen 202 inside; scripted pin green)
  · `make license-check` OK (252 files — the smoke entry above was
  re-worded: it named a one-shot file, pitfall 16) · `make lint` stands
  (no Rust) · ruff on the new files: E501 in the files' hand-wrapped
  style only.
- **2026-09-05 — RG4 LIVE (the §7 exit tell, run on the real worker
  db against the standing engine):** `import-catalog` → 6 members
  (`954fba8da621` = the live `fde6f733…` rows, VALIDATED; `bfbc5349`
  xv, h6/h6b dip-rip fades and the g7 smoke floor as candidates — all
  four RETIRED by the session as superseded/smoke-era; `icdp@407e064b…`
  coded). The carry legs of `b9883c1a` (10 cvfc deribit/hl + 7 s1
  bybit) added as the candidate member `cvfc-carry` at **$2,500/leg**
  (the $2,750 of the merged table no longer fits beside TWO xv
  variants under the leg-counted $100k cap: 17 × 2 × 2,750 + 2 × 2 ×
  3,000 = $105.5k; at $2,500 it is $97k), unlabelled because FUND dims
  are unknown on this host. Pool: 8 windows under
  `~/multivenue/worker/windows/` (7 reused from the smoke's cuts by
  name, 1 cut fresh — the newest complete 2 h windows of the four v3
  runs 2026-09-04 08:31Z → 2026-09-05 04:54Z). **Finding (structural,
  carried to RG7/harness): a funding-carry member cannot be evidenced
  under the 2 h law with today's harness** — `apr24` needs 24 h of
  funding prints and the backtest warm-up is TABLE-GLOBAL, so one
  evidence run of `cvfc-carry` on a 2 h window emitted 0 orders
  (`orders.emitted 0`, 6.7 M records replayed, judged) and any table
  carrying it would block its OTHER rows for the first 24 h of the
  pooled timeline (16 h) — the fix shape is a per-window FUNDING seed
  the way `regime-seed.tsv` warms the detector (window_root writes it
  from candles.db's `funding` table, the harness's `--funding-seed` /
  `FundingSeed` replay honours it) — a `crates/cli` change, out of
  RG4's Python scope. The composition therefore ran validated-only:
  words from the engine (`fast` = trend neutral · shape mixed · vol low
  · fund/level unknown · stretch neutral, measured — the seed hole had
  filled by 06:0xZ), member `954fba8da621` fit "word", table
  `954fba8da621d769d2bc9607e95854b9` (2 rows, $12k), evidence rows
  written for all 8 windows (whole-window, tier `okx 8:10`): fills
  16–283 per window, zero-fee nets −73.4 … +3.1 (Σ ≈ −$97), tier nets
  −40 … −417 (Σ ≈ −$1,343) — the xv verdict of 2026-09-02 restated
  per window: nothing survives the okx tier. Pooled frozen gate on the
  8-window pool: **PASS** — OOS legs 76, RT 13, net +$4.09 (0 fee),
  DD $9.26, 2 days, bounds $3,002 / $14,870 / $20,787. **`--regime
  off` on the same pool: +$4.46 ⇒ on−off = −$0.36 — the vol labels
  are WORSE than their absence; LOWO: +2.42 / +4.09 ×4 / +2.42 /
  −8.86 (without the 2026-09-05 00:01Z window) / −0.70 (without the
  02:01Z window) — the OOS edge sits in two windows. Composer verdict:
  FAIL (838 s of the 7 200 s budget; `compositions.gate_json` holds
  it).** So the labelled table the smoke committed is NOT confirmed by
  its own gate: the label does not earn its keep and the edge is not
  robust to leaving one window out — the 2026-09-02 xv verdict, now
  measured per window and per label. No promotion was asked (a single
  member; its rows ARE the live table — the canonical no-op law) —
  the "≥ 2 members committed live" tell stays honestly OPEN until a
  second member can be evidenced (carry needs the funding seed; an
  authored member needs its own windows). Operator decision pending:
  keep `fde6f733…` live as the RG7 soak shape regardless (paper), or
  re-commit `bfbc5349…` now that the on/off delta says the split adds
  nothing.
- **2026-09-05 — operator rulings after RG4 (AskUserQuestion): keep
  `fde6f733…` live as the soak shape; next = RG6 (dashboard); the
  smoke's leftover cuts under the research vault deleted (23 GiB free).
  RG6 was NOT started in the RG4 session (context budget) — the map
  below is the session's hand-off, gathered read-only from the tree so
  the next session starts at the design, not the archaeology.**
  - **Server (`crates/core-metrics/src/server.rs`)**: `serve_metrics`
    (`:62`) = a plain non-blocking `TcpListener` accept loop
    (`ACCEPT_TIMEOUT` 200 ms), one connection at a time, `handle_one`
    (`:141`) routes `/metrics` (`:150`) and `/healthz` (`:168`) — the
    `/state` branch goes beside `/healthz`; body written in place past
    `HEADER_BUDGET = 256` into `resp` (`RESP_BUF_SIZE = 128 * 1024` at
    `:56` → raise to 256 KiB), `write_headers`/`format_u64` reusable;
    the data source today is the live `Arc<MetricsRegistry>` atomics
    (`registry.rs:232` `encode_prometheus` with its private `Cursor`
    `put/put_u64/put_i64` = the precedent for the hand-written JSON
    writer). `/state` is the first consumer that needs a snapshot cell
    handed to the server thread (`bin/multivenue-engine.rs:1809-1832`
    spawns `metrics-http` with `serve_metrics(bind, reg, stop, on_event)`
    — clone the new cell into that closure). Loopback tests:
    `crates/core-metrics/tests/server_loopback.rs`.
  - **Seqlock**: `tui::SnapshotCell` (`crates/tui/src/lib.rs:144-233`,
    `#[repr(C, align(64))]`, `publish`/`read`, single-writer
    debug_assert) carries `DashboardState` (≈ 640 B, `:57-100`); the ONLY
    publish site is `paper.rs:4366-4403` inside `engine_loop_full`'s
    `if now >= next_report` (5 s, `REPORT_PERIOD_NS` `paper.rs:108`);
    the cell exists only under `--tui` (`Observability::build`
    `paper.rs:2898`, field `snapshot` `:2999`). RG6: make the cell
    generic over its POD (or add `StateCell<EngineSnapshot>`), publish
    every 1 s (`SNAPSHOT_PERIOD_NS`, a second `next_state` gate beside
    `next_report`; `now_ns()` in hand at `:4156`), create it regardless
    of `--tui`; the TUI reads the same snapshot.
  - **TUI** (`crates/tui/src/lib.rs`): `run_dashboard(cell, stop)`
    `:253`, 30 Hz frame (`FRAME_PERIOD` `:244`), panels `render_header`
    `:329` / `render_markets` `:349` / `render_last_order` `:391` /
    `render_latency` `:412` / `render_ingest_health` `:441` (labels bits
    0..6 only — bybit at bit 7 is invisible); `recent_tob*` AND
    `last_order_*` are never filled (dead); the "~10 ms" claim at `:12`
    and `:204` is wrong (5 s) — correct both. Alignment test
    `dashboard_state_is_cache_aligned`.
  - **The generic boundary**: `engine_loop_full` is generic over
    `S: Strategy`; every strategy datum crosses through
    `strategy_core::StrategyCounters` (`crates/strategy-core/src/lib.rs:
    83-204`, default-0 accessors: `enabled_mask` `:593`, `icdp_counters`
    `:192`, `regime_counters` `:201`, `vm_*`). New accessors needed
    (default-empty, so every other strategy is untouched): `is_halted`
    (`StrategySet::is_halted` `strategy-set:436` exists, not exposed),
    per-slot orders emitted/dropped (today only the 7-member sum
    `strategy-set:554-572` + the vm breakout `:609`), vm
    `active_hash128()`/`staged_hash128()`/`active_epoch()`/`rows_active()`
    (exist, `strategy-vm:360-378`), the per-row view (`positions:
    [VmPosition; RULE_TABLE_ROWS]` `:216` + `row_gate: [u8; …]` `:220`
    are private — add a `vm_row(i) -> VmRowView {idx, name_h, sym,
    state, side, entry_px_1e6, entry_ts_ns, gate}` reading
    `tables[active].rows[i]` (`RuleRowV2`) + `positions[i]` +
    `row_gate[i]`), icdp `params_hash()`/`instruments()`
    (`strategy-icdp:505/511`, boot-logged only), regime `rel` per
    breadth sym (`RegimeView::rel_of` `core-regime:467`; not in
    `RegimeCounters` `strategy-core:211-246`).
  - **Sources already reachable from the loop**: `AiIngressStatus`
    (`ingress-ai/src/status.rs:26`, `eng.ai_status()`), `IngressStatus`
    ×venue (`core-metrics/src/ingress_status.rs:177-237`, 128 B each,
    `feed_delay_ema_ms`, `stale_ticks_total`; last-tick AGE is derived
    only in the loop — `tick_age_track` `paper.rs:4138`, venue order
    pm/bn/okx/deribit/hl/bybit/rpc), latency `Engine::{ingest,decide,
    ack}_{p50,p99}_ns()` (`engine/src/lib.rs:666-692`), capture
    `fill/order_capture_records/io_errors` (`:815-848`), regime words
    via `RegimeCounters`.
  - **Boot info**: NOTHING exists — no `build.rs` in the workspace, no
    pid/git-sha/build-ts anywhere; `requested_mask` and `configured`
    are computed in `engine_loop_set_full` (`paper.rs:2459-2475`) and
    dropped (only `mask` survives); `run_dir`/`epoch_ns` come from
    `cli::new_capture_run_dir` (`bin:715`, `paper.rs:221`) and are not
    threaded in; artifact hashes = `vm.active_hash128()`,
    `IcdpStrategy::params_hash() -> &[u8;32]`, `RegimeBoot.hash`
    (`paper.rs:2598`), seed rows = `RegimeCounters.seed_rows`. RG6:
    a `BootInfo` POD threaded `bin` → `engine_loop_set_full` →
    `engine_loop_full` (+ `Observability`), a workspace `build.rs` for
    the cli crate (`git rev-parse` + build ts via `option_env!`
    fallbacks — never a hard dependency on git at build time).
  - **Orders / fills — no in-memory ring exists** (only counters and
    the PMLR write-through). Choke points: `EngineCtx::submit`
    (`engine/src/lib.rs:908`, `Order` 64 B carries ts/sym/side/kind/
    px/qty/client_oid/venue/strategy_id/ttl — the `Ok` arm beside
    `cap.append(&order)`), and the two fill drains in `Engine::tick`
    (`:436-465` fill lanes, `:467-496` dispatcher pump; `Fill` 64 B has
    NO strategy_id — join by `order_id == client_oid` against the order
    ring, or leave the slot out). `EngineCtx` is rebuilt per callback at
    11 sites — the rings live on `Engine`, reborrowed like
    `order_capture`.
  - **Conventions**: `static_assert_size!` (`core-types:2584`);
    `#[repr(C, align(64))]` + explicit `_pad` + `const fn empty()`;
    alloc gate = `crates/bench/tests/alloc_assertions.rs` (copy
    `dashboard_snapshot_read_is_zero_alloc` `:999-1020` for the
    publish/read AND the JSON encode of a FULL snapshot — 256 rows,
    64 + 64 recents — into a 256 KiB buffer; truncation = test
    failure, not a runtime branch).
  - **Worker half (Python, §6.2)**: `python -m claude_worker.dashboard`
    — stdlib `http.server` on 127.0.0.1:9292, `/` = one
    `dashboard.html` (inline CSS+JS, no CDN; a licence comment header —
    §11-Q9 open), `/api/worker` (rulesets catalog, library + evidence +
    compositions via the RG4 readers, regime history 24 h +
    `declared.json`, latest `pnl-<day>.json` per strategy and regime,
    candidates, events tail 100, positions from the existing code
    path, config snapshot — `strategy.conf`, `fees.toml`, `regime.toml`,
    `icdp.toml` hash + instruments, universe summary, Data-volume free
    — never `.env`), `/api/engine/state` + `/api/engine/metrics`
    same-origin proxies to 9191; cadence engine 2 s / worker 10 s /
    P&L 30 s; `launchd/com.multivenue.dashboard.plist` (RunAtLoad,
    KeepAlive) + `install-launchd.sh` + a `local-setup.md` line; pytest
    for the `/api/worker` shape on a tmp worker dir. Write controls are
    NOT in v1 (§10).
  - **Order of work**: engine half first (snapshot POD + accessors +
    BootInfo + rings + 1 s publish + `/state` writer + tests + alloc
    gate + `make lint`), relink + launchd restart + `curl /state`, then
    the TUI, then the worker page, then the live check of every panel
    (screenshot into the vault, never git). Exit tell (§7): page live
    on 9292 showing regime, mask, rows, P&L, recent fills.
- **RESUME POINT (for a fresh session):** RG4 is code-complete on disk
  (commit status = the operator's; gates at the end of the RG4 entry);
  the live composition run and the rulings are recorded above. Next =
  **RG6** per the hand-off map directly above (§6 is the spec; add
  `stage_refused` and the per-row gate bytes the smoke asked for).
  Alternative the operator may order first: the harness FUNDING SEED
  (the carry member's blocker — `window_root` writes `funding-seed.tsv`
  per window from candles.db, `crates/cli backtest --funding-seed`
  replays it before the first tick) to close the RG4 "≥ 2 members"
  tell. RG7's "5 paper days" wording is VOID under the 2026-09-05
  ruling — restate it as N ≤ 2 h windows when RG7 opens. Laws unchanged
  (see the RG3 resume point) + the 2 h test/soak/protect law. Relaunch
  prompt: "Continue the regime lane in trading-engine-multivenue. Read
  CLAUDE.md CURRENT STATE (regime bullet), then
  `docs/regime-and-dashboard-plan.md` §12 last two entries (the RG6
  hand-off map + RESUME POINT). Start RG6 — engine half first. Laws:
  regime is a gate not a signal; exits never gated; legacy rows
  bit-identical; no table flip on regime change; capture windows and
  every test/soak/protect time ≤ 2 h; research never in git;
  compile/test only on the Mac via RustRover (nohup + poll for > 45 s);
  Python full `import x` only; SPDX header on every new file;
  explicit-path `git add`, commit only on my ask, never push; tell me
  when short on context with a resume prompt."
- **2026-09-05 — RG6 ENGINE + TUI halves CODE-COMPLETE (uncommitted —
  operator commits; explicit paths listed below). Engine half, per the
  hand-off map:** new crate `crates/engine-snapshot` (`license.workspace
  = true`; deps `core-types` + `strategy-core`; no new external
  dependency ⇒ no `license-deps` run) = `EngineSnapshot` (≈ 21 KB POD,
  `#[repr(C, align(64))]`, size-pinned ≤ 32 KB) + `BootInfo` + the
  section PODs + `RecentRing<T, N>` + the generic seqlock
  `SnapshotCell<T>` (moved out of `tui`, `_pad` so `data` starts on its
  own line for any `T`; `publish(&T)` copies once, `read_into(&mut T)`
  for the 24 KB reader) + `encode_state_json` (sticky-overflow cursor,
  the §6.1 number law, `STATE_JSON_MAX` 160 KiB) + tests (`tests/
  encode.rs`: a FULL snapshot — 256 rows, 64 + 64 recents, every text
  field at capacity, every scalar at its widest — fits and is balanced;
  one byte short is refused; a fixed snapshot renders BYTE-EXACT (the
  schema pin); recents render oldest-first with ages). `strategy-core`:
  `SlotCounters` / `VmRowView` (48 B) / `RegimeRelView` (`REGIME_REL_SYMS`
  = 32, const-asserted against `core_regime::REGIME_MAX_SYMS` in the set)
  + default-empty accessors `is_halted` / `slot_counters(slot)` /
  `vm_active_hash128` / `vm_staged_hash128` / `vm_rows_view(&mut [..])` /
  `icdp_params_hash` / `icdp_instruments` / `regime_rel_view`;
  `strategy-set` overrides all eight; `strategy-vm::rows_view` (row +
  position + gate byte per active row). `engine`: `recent_orders` /
  `recent_fills` rings on `Engine` (push in `EngineCtx::submit`'s Ok arm
  beside the intent capture — accepted orders only — and in both fill
  drains; `EngineCtx` reborrows the ring at all 11 sites). `core-metrics`
  stays dependency-free: `serve_metrics(addr, registry, state:
  Option<F>, stop, on_event)` with `F: FnMut(&mut [u8]) -> Result<usize,
  EncodeErr>` (`StateFn` = the `None` spelling), `GET /state` →
  `application/json` (404 without a writer, 500 on overflow — never
  truncated), `RESP_BUF_SIZE` 128 → 256 KiB, `write_body` shared with
  `/metrics`; loopback tests for all three outcomes. `cli`:
  `Observability.state` (the cell, created whenever `/metrics` is) +
  `.boot: BootInfo` (`with_boot_info`; `boot_info()` = pid, wall anchor,
  binary mtime, `MULTIVENUE_GIT_SHA`, run dir/epoch, `--strategy`,
  paper); `engine_loop_set_full` stamps `requested_mask`/
  `configured_mask`; the set arm stamps `regime_hash`/`regime_configured`
  from `RegimeBoot`; `run_engine_loop` gains the 1 s `next_state` gate
  (before the 5 s gate) = `update_tick_age` (the T1(c) stamps moved here
  — the 5 s gauges read them) + `fill_snapshot` (every source: the
  `StrategyCounters` UFCS route, engine counters/percentiles/captures/
  rings, `AiIngressStatus`, the seven `IngressStatus` slots) + one
  `publish`; `state_writer(cell)` = the server thread's boxed scratch +
  `read_into` + encode; `crates/cli/build.rs` (git sha, soft; re-runs
  on `.git/HEAD` + its ref only). **TUI half (§6.3):** `crates/tui`
  rewritten over `EngineSnapshot` — `DashboardState`, `MAX_TOB_SLOTS`,
  the dead `recent_tob`/`last_order_*` and the "~10 ms" claim are GONE;
  panels = header (pid / strategy / masks / halted / uptime / git / seq;
  counters; two regime chips decoded through the §3.3 byte map with
  declared age/TTL + minutes judged), Strategies (slot / member / cfg /
  on / gate / orders / drop), Recent orders (newest first, 8 of 64),
  Ruleset (hash / rows / epoch / staged / fires / blocked / hard_exits +
  every active row with gate, position, side, entry px, age), Latency,
  Ingress (7 venues: state / last tick / ticks / stale / delay / reconn);
  `run_dashboard(&SnapshotCell<EngineSnapshot>, stop)`; the bin spawns
  it on `obs.state` under `--tui`; `Observability::build(enable_metrics)`
  lost its `enable_tui` parameter and the 5 s `DashboardState` publish
  block is deleted; tui deps = `core-types` + `engine-snapshot` (+
  `strategy-core` dev). **Gates (2026-09-05, all on the Mac):** nextest
  1593 passed / 1 skipped (was 1578); alloc gate **42/42 0 B/op** with a
  fresh `Compiling bench` (the old `dashboard_snapshot_read_is_zero_alloc`
  is superseded by **gate 43** `state_snapshot_publish_read_encode_is_
  zero_alloc` — a FULL snapshot published + read back + encoded into a
  256 KiB buffer 1 000×, 0 B); `make lint` green; `make license-check`
  OK; `cargo build --release -p cli` relinked 14:57:16 local with the
  head sha `61b9ab865ab4` embedded. **LIVE `/state` CHECK PASSED
  2026-09-05 08:02Z (operator-approved restart: `pkill -TERM` → KeepAlive
  reboot on the relinked binary, pid 75758, run
  `run-1788595347123757000`):** boot tell `metrics: HTTP server starting
  bind=127.0.0.1:9191 state=true`; `/state` 15 s after boot = `"v": 1`,
  `boot.git_sha 61b9ab865ab4`, `binary_mtime_ns` = the 14:57 relink,
  `strategy ai+icdp` / `set`, masks req 112 / cfg 113 / on 112, `paper`
  1, `regime_configured` 1 with `seed_rows 10738`, both profiles
  measured (`dims [2,1,1,128,128,2,1]` = trend NEUTRAL · shape CHOP · vol
  LOW · fund/level UNKNOWN · stretch NEUTRAL · source MEASURED — the
  raw one-hot bytes, decoded by bit index), `rel` = 7 syms (BTC ref 255
  + six members INLINE), `icdp` configured `407e064b…` 8 instruments
  with `decisions` climbing (16 → 48), all six WS venues `state 2` with
  last-tick ages 0–2 s and feed-delay EMAs (deribit 134 ms, hl 147 ms),
  rpc down (as always), captures 0 errors, body 5.5 KB. After the #7b
  recommit (seq 39436/39437) at ~70 s: `vm.active_hash fde6f733…`,
  `rows_active 2`, `epoch 1`, and the two labelled rows carry THEIR OWN
  GATE BYTES — row 0 (`vol:low`) `gate 1` = open, row 1 (`vol:!low`)
  `gate 0` = soft-closed under the live LOW vol — the per-row gate the
  RG5 smoke asked for, now observable; `ai.cmds 159`, `ruleset_staged
  1`, `ruleset_committed 1`, `heartbeat_age_s 8`; `/metrics`
  `engine_vm_rows_active 2` (the post-restart law holds).
  **WORKER PAGE (§6.2) CODE-COMPLETE + LIVE-CHECKED the same session
  (08:16Z):** `claude_worker/dashboard.py` (module, `python -m
  claude_worker.dashboard`; stdlib `http.server` on 127.0.0.1:9292,
  single-threaded, read-only; `Inputs` resolved once from env —
  `CLAUDE_WORKER_DB` / `_REPLAY_DIR` / `_REPORTS_DIR` / the new
  `CLAUDE_WORKER_MULTIVENUE_DIR` / `CLAUDE_WORKER_ENGINE_URL` /
  `CLAUDE_WORKER_DASHBOARD_PORT`; `worker_payload()` = rulesets
  (`rulesets_all`), library + per-member evidence roll-ups, compositions
  (the RG4 readers), regime = 24 h `history_tail` + `load_declared` +
  the `regime.toml` bands (`read_regime_params`) + the byte map for the
  page, pnl = the latest `pnl-<day>.json` minus `runs_detail` + a
  14-day series, candidates (`*.json` only), the events ledger tail
  (100, read-only SQLite), positions from the CURRENT run's fills
  (`features.read_fills` → `reconstruct_positions` → `position_views`
  with marks carried at cost — no tick scan at cadence), config
  snapshot (`strategy.conf`, `fees.toml`, `regime.toml`, `icdp.toml`
  sha256 + `[[instrument]]` count, `universe.toml` per-list counts,
  `retention.conf`; `.env` is never read and the test asserts no
  `sk-ant`/`.env` in the document), Data-volume free; server-side cache
  5 s (positions 30 s); `/api/engine/state` + `/api/engine/metrics` =
  allow-listed same-origin proxies to 9191 (502 when down); `--once`
  prints the document); `dashboard/dashboard.html` (one file, inline
  CSS+JS, vanilla, no CDN — the test asserts no `<script src`/`<link`/
  URL; SPDX comment header; engine 2 s / worker 10 s; panels: status
  bar, Regime (effective/measured/declared per profile with raw ret/ER/
  RV/stretch + flips, rel per breadth sym, slot gates, the 24 h
  per-dimension timeline strips, the bands table, declared.json with
  in-force/expired), Strategies (slot table + today's net @0/1/tier
  from the report), P&L (per strategy, per ruleset, per regime word,
  day series), Ruleset (registry thesis/author + composition link + the
  rows with gate/position), Library (+ candidates), Recent trades
  (orders newest-first with slot colour, fills joined by oid), Ingress,
  Latency & loop, AI plane (+ ledger tail), Positions, Configs);
  `tests/test_dashboard.py` (document shape on a tmp worker dir incl.
  the no-db case, loopback routes 200/200/502/404, `--once`); worker
  pytest **713** (frozen 202 inside), ruff clean. **CMDLINE LAW
  (found while wiring launchd):** every lane's overlap guard is `pgrep
  -f 'claude[-_]worke[r]'`, which a long-running server whose cmdline
  carried `claude_worker`/`claude-worker` (a `-m` module path, the
  venv's own `claude-worker/.venv/bin/python3`) would trip FOREVER —
  the boot-time recommit `while pgrep …` would never restore the live
  table. Hence `scripts/dashboard.sh` execs `~/multivenue/venv/bin/
  python3 scripts/dashboard-serve.py` (a directory symlink alias of the
  venv — Python finds `pyvenv.cfg` through the unresolved path — and a
  repo-root launcher), verified live: `pgrep -f 'claude[-_]worke[r]'`
  prints nothing while the page serves. `launchd/com.multivenue.
  dashboard.plist` (RunAtLoad, KeepAlive, `dashboard.log`) +
  `install-launchd.sh` (both loops) + `docs/local-setup.md` (uninstall
  list + a Dashboard paragraph). **LIVE CHECK 08:16Z (hand-run
  wrapper, not yet bootstrapped in launchd):** `/ 200 28.7 KB`,
  `/api/worker 200 47.7 KB in 21 ms`, `/api/engine/state 200 12.1 KB`,
  `/api/engine/metrics 200`, `/nope 404`; every panel populated from
  the live engine + worker (35 history points 04:59→08:15Z, 7 library
  members, 3 day reports, 40 recent orders, 6 venues UP, the two vm
  rows with their gate bytes) — the frozen record (page + both JSON
  documents, self-replaying `index.html`) is in the vault at
  `docs/research/rg6-dashboard-live-2026-09-05/` (git-excluded; the
  §7 "screenshot in the vault" tell). **`com.multivenue.dashboard`
  BOOTSTRAPPED 08:19Z on operator approval (pid 77305, `state =
  running`, guard silent, all routes 200) and RG6 COMMITTED as
  `ef75c91` (35 files; the RG4 worker set stays unstaged for the
  operator); `make license-check` 262 files OK post-commit.** Open: the
  RG6 close ruling (§7 exit tell met). Pitfall-10 note for the next
  session: three "impossible" unresolved-import errors in a row today
  were stale rmeta after edits to `strategy-core` / `core-metrics` —
  `cargo clean -p <crate>` fixed each; and a sandbox-side `git status`
  left a stale `.git/index.lock` (removed on the Mac; use the Mac lane
  for every git read too).
- **2026-09-05 — RG6 CLOSED (operator ruling "go do the RG6 close
  ruling (§7 exit tell met)"): the §7 exit tell — page live on 9292
  showing regime, mask, rows, P&L, recent fills — was observed live at
  08:16Z (entry above; frozen record in the vault), `/state` + TUI +
  the worker page are committed (`ef75c91`, `634f740`) and
  `com.multivenue.dashboard` runs under launchd. Carried out of RG6 into
  RG7: the seed-hole fix (§7.1), `stage_refused` (RG5 finding 3 — NOT
  exposed by RG6: a stage frame for a missing artifact is still dropped
  without a counter; the ingress side path owns it), the `.html`
  licence-gate question (§11 Q9, still open, default = leave at
  `.rs/.py/.sh`). RG7 is restated in §7 + §7.1 (the "5 paper days"
  wording VOID under the ≤ 2 h law) and opens now.**
- **2026-09-05 — RG7 OPENED (operator: "then RG7 restated as N ≤ 2 h").
  Restated in §7 (row) + §7.1 (procedure); code + docs landed the same
  session (uncommitted at writing — operator commits):** `claude_worker.
  regime` gains (1) `state_url` / `engine_state` / `engine_regime_sample`
  — the cycle now samples the engine's `/state` regime block (pid,
  cumulative flips per profile x market dim, minutes judged, effective
  words, vm rows, hard exits) into every `history.ndjson` line
  (`engine` key; absent when the engine is down — the log line says
  `engine=unreachable`); (2) the **`soak` lane** — `SoakWindow` /
  `WindowVerdict`, `soak_windows_from_runs` (every complete ≤ 2 h window
  of every v3 run under the replay root inside the history horizon —
  the pool's candidate law WITHOUT cutting) or `--pool` (the standing
  cuts), `judge_window` (coverage ≥ 20 samples → `short`; gating live at
  every engine sample else `ungated`; flips from the engine counters
  when the pid is constant across the window — baseline = the last
  sample before the window, same pid — else the worker mirror's 5-min
  word changes; hard-exit delta; `pnl_regime_present` = the window's
  day report holds fill-model rows for both profiles), `run_soak`
  (pooled verdict PASS / FAIL / INSUFFICIENT, needs `SOAK_MIN_WINDOWS` =
  8 counted; `FLIPS_MAX_PER_WINDOW` = 2; JSON under the regime dir;
  exit 0 only on PASS, else 3); (3) **`seed-out --refresh-tail
  [--universe]`** = `refresh_tail`: the §9.6 gap-fill of the 1 m candles
  for the artifact's own descriptors only (forward + OKX-backward lanes,
  demand-sized budget) before the export — the seed-hole fix; the
  wrapper passes it. Tests `tests/test_regime_soak.py` (sample shape,
  cycle with a mocked `/state`, every window verdict branch, the pooled
  lane incl. INSUFFICIENT/exit codes, refresh-tail over a fake venue
  reaching the boot minute). Worker pytest **718**, ruff clean on the
  new hunks (the file's pre-existing E501/RUF002 lines untouched —
  format only your own hunks), `make license-check` OK. **Live:**
  `regime soak` on the real history + runs = `INSUFFICIENT (windows 8,
  counted 0)` — every existing pool/run window predates the regime
  going live (the history starts 04:59Z today; the 08:30Z T2 restart
  reset the counters at pid 78186) — the honest verdict, never a wait;
  the cycle's history lines carry `engine` since 08:35Z; `seed-out
  --refresh-tail` by hand: 7 descriptors, 36 bars each, 1.5 s, the seed's
  last minute lag 1 min (was up to 60). Docs: `ai-strategy-pipeline.md`
  (§7b the regime gate + the slot-6/mask-112 correction + §9 `/state` /
  dashboard / soak rows + §15 seams and kind 12 + §16) and the SVG (lane
  F: 7 slots, the REGIME GATE and DASHBOARD rows, lanes G/H shifted,
  `/metrics + /state`), `research-universe.md` (§5 the ≤ 2 h law + the
  regime-gate law, §4 the live inventory incl. `fde6f733…`/icdp/library,
  §6 the v2.1 regime keys), `risk-policy.md` (the day-floor note
  superseded + a "Regime gate" section: entries only, fail closed,
  bounded declarations, no flicker, read-only observability),
  `local-setup.md` (soak + refresh-tail), CLAUDE.md. **RG7 exit = a
  `soak` PASS on N ≥ 8 counted windows (gating live throughout) + the
  operator ruling — accumulates as the engine runs; nothing to wait
  for by hand.**
- **RESUME POINT (for a fresh session):** RG6 is COMMITTED (`ef75c91`)
  and LIVE (engine `/state`, TUI, `com.multivenue.dashboard` on 9292).
  RG7 is OPEN with its judge landed (entry above; the RG7 set =
  `claude-worker/src/claude_worker/regime.py`, `claude-worker/tests/
  test_regime_soak.py`, `scripts/engine-wrapper.sh`, `docs/
  ai-strategy-pipeline.{md,svg}`, `docs/research-universe.md`, `docs/
  risk-policy.md`, `docs/local-setup.md`, this plan, `CLAUDE.md` —
  uncommitted until the operator commits). NEXT: run `uv run python -m
  claude_worker.regime soak` (from `claude-worker/`, `.env` sourced)
  when ≥ 8 complete ≤ 2 h windows of regime-gated runs exist (≈ 16 h of
  engine time after 08:35Z 2026-09-05 — the T2 restarts at 16:05Z/00:00Z
  split runs but windows are per run, so they still count; a restart
  INSIDE a window makes it `mirror`-judged, never lost); on PASS the
  operator rules RG7 closed. The RG6 path list below is the historical
  staging record (paths: `Cargo.
  toml` `Cargo.lock` `crates/engine-snapshot/` `crates/cli/build.rs`
  `crates/cli/Cargo.toml` `crates/cli/src/{lib.rs,paper.rs,bin/
  multivenue-engine.rs}` `crates/core-metrics/{src/lib.rs,src/server.rs,
  tests/server_loopback.rs}` `crates/engine/{Cargo.toml,src/lib.rs}`
  `crates/strategy-core/src/lib.rs` `crates/strategy-set/src/lib.rs`
  `crates/strategy-vm/src/lib.rs` `crates/tui/{Cargo.toml,src/lib.rs}`
  `crates/bench/{Cargo.toml,tests/alloc_assertions.rs}` `docs/regime-
  and-dashboard-plan.md` `CLAUDE.md` — the RG4 worker files are a
  SEPARATE uncommitted set, stage them separately; the worker page adds
  `claude-worker/src/claude_worker/dashboard.py`, `claude-worker/src/
  claude_worker/dashboard/dashboard.html`, `claude-worker/tests/
  test_dashboard.py`, `scripts/dashboard.sh`, `scripts/dashboard-
  serve.py`, `launchd/com.multivenue.dashboard.plist`, `scripts/
  install-launchd.sh`, `docs/local-setup.md`). The live `/state` check
  PASSED and the worker page is LIVE-CHECKED (entry above). Remaining
  for RG6: bootstrap `com.multivenue.dashboard` (operator: `launchctl
  bootstrap gui/$UID ~/Library/LaunchAgents/com.multivenue.dashboard.
  plist` after rendering the template, or re-run `install-launchd.sh`
  which also restarts the engine), then the §7 close ruling. Then RG7
  (restated as N ≤ 2 h windows). Historical note — the order of work
  was: (2) the worker page
  `python -m claude_worker.dashboard` (§6.2 + the hand-off map's worker
  bullet: stdlib `http.server` on 127.0.0.1:9292, `dashboard.html`
  inline CSS+JS no CDN, `/api/worker`, same-origin proxies to 9191,
  `launchd/com.multivenue.dashboard.plist` + `install-launchd.sh` +
  `local-setup.md`, pytest for the `/api/worker` shape; the page decodes
  `regime.profiles[].*.dims` with the §3.3 byte map — mirror
  `claude_worker/regime.py`'s names); (3) the live check of every panel
  (screenshots into the vault, never git) = the §7 RG6 exit tell.
  Alternative the operator may order first: the harness FUNDING SEED
  (RG4's carry blocker). Laws unchanged (RG3/RG4 resume points + the 2 h
  law). Relaunch prompt: "Continue the regime lane in
  trading-engine-multivenue. Read CLAUDE.md CURRENT STATE (regime
  bullet), then `docs/regime-and-dashboard-plan.md` §12 last two entries
  (RG7 landing + RESUME POINT), §7.1 (the ≤ 2 h soak law) and §6.1 'As
  landed' (the `/state` contract). Continue RG7: run `regime soak`; if
  INSUFFICIENT, report the window count and stop (never wait); if PASS,
  write the close entry for the operator's ruling. Laws: regime is a gate not a
  signal; exits never gated; legacy rows bit-identical; no table flip on
  regime change; capture windows and every test/soak/protect time ≤ 2 h;
  research never in git; compile/test only on the Mac via RustRover
  (nohup + poll for > 45 s); Python full `import x` only; SPDX header on
  every new file; explicit-path `git add`, commit only on my ask, never
  push; tell me when short on context with a resume prompt."
