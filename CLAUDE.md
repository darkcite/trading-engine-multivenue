# CLAUDE.md — Multivenue Trading Engine

This file front-loads context for any Claude session working in this repo.
It is deliberately self-sufficient: a fresh session should be able to start
from **this file + the current phase's design/progress docs** without
rereading the whole doc set. `PLAN.md` remains the architectural deep-dive.

## What this is

A pure-Rust, zero-allocation, zero-copy, single-writer, lock-free engine that executes latency-arbitrage trades on Polymarket's CLOB. v1 runs locally on a MacBook Pro M4 using only free-tier external APIs. Claude (via the `claude-worker` Python process) acts as an offline strategy researcher — never in the hot path.

## CURRENT STATE (updated 2026-08-29 — keep this section current at every phase boundary)

- **ACTIVE AUTHORITY: `docs/stage2-finish-plan.md` (2026-08-29 operator ruling — read it FIRST).** Single WS0–WS13 numbering. **WS2–WS12 CODED + the WS13 GATES PHASE GREEN + ALL COMMITTED (2026-08-29 session, commits `a9f5b4b`…`46f9610` + CLAUDE.md close):** WS2 drop/establishment semantics (OKX/Deribit/Bybit) · WS3–WS8 venue expansions · WS9 Bybit sixth venue (data-only, lane 5/venue 6) · WS11 worker refdata/funding/D5 fold-ins · WS12 `make lint` green. Gates run on operator order: **nextest 1316 · alloc 38 0 B/op · pytest 477 (frozen 202 inside) · fuzz 5×301 s clean · fmt/lint/license green — the NEW stay-greens 1316/38/477**; 6 regression fixes landed during the run (WS9 seventh-venue desyncs: `venue_records[6]` OOB ×2 files, binance `parse_filters` Truncated-vs-BadRow, 3 test/alias fixes) — record in the plan's WS13 entry. **WS10 CODED + GATED + COMMITTED (2026-08-29, same session): operator approved D-A1…D-B3 as recommended via AskUserQuestion; A = funding venue-event lanes (ChannelEvent carrier, `EVENT_LANE_FUNDING` mask, `on_venue_event`, event_ring_drops) commits `1ffc10c`/`e5b26ed`/`b74577a`; B = L2 depth (DepthTopK 192B = the FIRST kind-determined PMLR slot size — kinds 0–6 stay 64B, kind 7 = 192B, container v2 unchanged; book-builder ladder; okx/deribit level walks; `on_depth`; STALE-on-gap; audit-replay depth stream; 2 new fuzz targets) commits `2bc356d`/`66e3ff9`/`bf30681` + docs `c22e10f` + fixup `aabc5af`. Stay-greens moved to 1345/38/477 (nextest/alloc/pytest; fuzz okx_book_levels + deribit_book_levels 301s clean).** **WS13 LIVE PHASE RUN 2026-08-29 (operator amended the soak 7 days → 1 HOUR): soak GREEN.** run-1787988833603587000 (07:33:53→08:30:53Z, the 0830 T2 slot closed it on the minute): **08:00Z settlement CROSSED ALIVE on okx+deribit (the WS2 proof — okx carried 1 441 non-fatal SubDrop events instead of dying)**; audit-replay integrity ZERO on pm/bn/okx/hl/bybit (deribit: 6 venue-side trade holes, chain_breaks 0); **WS10-B depth LIVE (okx 9 678 + deribit 8 687 snapshots, STALE=0)**; Bybit sixth venue clean throughout; D1 fetch conflicts=0/unresolved=0-in-mapped-universe; #7b armed+fail-safe-refused as designed. Two live catches fixed+committed during smoke (`98d4db2`): Bybit spot discovery sub-1e-9 basePrecision truncation (BTTUSDT-class rows killed the page) + GaugedCapture depth-hook forwarding. **C6: streak 6 ≥ 3 PASSED, root tidied; the step-3 `oos.trading_days≥2` sub-gate is operator-pending (full-root merge ~27 GB > 24 GiB RAM; every feasible OOS tail lands on PM-dark Aug-28; bounded rerun viable Aug-31).** **§5.4 okx churn ROOT-CAUSED + FIXED + GATED (2026-08-29, after `7d6518a`; entry = stage2-finish-plan WS13 tail): NOT the venue, NOT the subscribe — rustls' 16 KiB received-plaintext cap signals backpressure via `ErrorKind::Other` ("received plaintext buffer full") and `core-net::TlsTransport::drive_tls` treated it as fatal ⇒ any >16 KiB burst (okx books400 ≈25 KiB/frame, opt-summary ≈600 KiB) killed the session; Aug-25 onset = OKX's `_UM` option-family listings growing the burst. Fix: backpressure=break in the pump + pull-through in `read()` (edge-triggered-safe, zero alloc, heals all six venues; deribit's churny sessions were the same bug). Red→green pinned by `core-net/tests/tls_burst_loopback.rs` (256 KiB burst reproduced the EXACT production error pre-fix). Stay-greens moved to 1349/38/477. **LIVE-PROVEN same session** (commit `00c13bf`, revive 10:28Z → run-1787999341626410000): okx run-loop churn lines 0 since boot, okx AND deribit reconnects_total 0 on unbroken Steady sessions, opt-summary capture ~9× (1.26 MB/8 min vs ~874 KB/57-min soak hour) — §5.4 CLOSED.** **C6 CLOSED by operator blessing 2026-08-29 ⇒ M3 CLOSED (exit entry = `docs/arch/m3-progress.md` last; Aug-31 bounded rerun optional, non-gating). REMAINING before Stage 3: (a) BN markPrice = venue-side unreachable from this network (RE-probed 2026-08-29 incl. live-SUBSCRIBE: ack-then-silent; eapi WS 404s; `.env` actualized to the full key surface same day on operator grant — activation stays one value + restart); (b) M5 on explicit go; (c) the §7 entry gate.** The earlier remediation commits (`24d545a`…`09a7bbb`) are live-UNTESTED until WS13's live phase; WS0 operator levers unchanged (restart-lane revive: `echo 19700101 > ~/multivenue/state/last-restart-utc-0000`; #7b arms at next boot unless disarmed). Background (ARCHIVED 2026-08-29): `docs/arch/capture-continuity-outage-2026-08-27.md` → `docs/arch/capture-remediation-plan-2026-08-28.md` §12 → `docs/arch/remediation-run-phase.md` (folded into WS13, live-proven).
- **VM2 (ruleset-VM v2) V0–V8-OPENING LIVE (2026-08-30; authority `docs/vm2-plan.md`, §8 log newest entries + §9 runbook — read those FIRST for VM2 work).** Commits `70bc59b` V5 · `342eccc` V6 · `6cc1ba5` V7 · `2a283a5` V8-prep · `928fc99` MVP-tempo gate amendment (**`GateThresholds.min_trading_days` 2→1**, operator-ruled, D1-pattern; + xv-v2 retune: hl pair dropped after per-pair probes isolated dd $129/$59.6k stacking there; okx-only 3.0/1.0 bps, $3,000/leg → ALL GATES PASS, OOS +$8.93/83 legs/17 rt) · `d0be3a7` V8 opening act (xv-v2 `bfbc5349…` committed, seq 48/50) — **but the VM traded ONLY Aug-30 08:55→16:05Z (+$32.71 realized / 156 orders / dd $247.88, legs flat) and then ran INERT ~2.5 days: every boot's #7b recommit died on the STALE ai.sock inode (existence-wait satisfied, connect refused, no retry). FIXED + REVIVED 2026-09-02 ~13:05Z (recommit main retries transport within the wait budget, 2 pins; seed-push skip → bounded wait; revive seqs 18618/18619, vm_rows_active 1/epoch 1, 1,625 seed frames re-pushed). **BOOTOUT EXECUTED on operator order 2026-09-02 ~15:20Z (V8 CLOSED): parity stopped GREEN (truncated window, 0 misses, semi-vacuous cron side), com.multivenue.carry + com.multivenue.xv booted out + plists DELETED. Engine-only now: xv on the VM (`bfbc5349…`, rows 1). CARRY IS DARK by operator order — merged xv+carry table `b9883c1a…` exists (carry legs rescaled to $2,750 for the Rule-7 leg-counted $100k cap) but its bounded-root gate ran OOS-0-trades (outage tail) and §6 stage refuses failing reports (NO override); revisit = healthy Aug-29→31 root + that candidate. The xv hl pair ruled dead at bootout.** **DISK: the Data volume ran 100 % FULL Sep-2 (~360 GB non-project data; capture ENOSPC-wedged all lanes → restart recovered); retention PROTECT_DAYS now 5 (conf), 11 GB free — the host still needs real headroom freed by the operator. The 00:20Z nightly pnl timer is DEAD since Aug-23 (unfixed).** After ANY restart, verify `vm_rows_active 1` before trusting VM state.** The general grammar (17 features / 4 combines / positions / groups / holds / confirms), engine opt+event+depth lanes, seeds (kinds 10/11), multi-channel backtest with D-7 mark-fill + dead-descriptor drop, depth_digest/coverage_audit/channel_map, per-host candle budgets (live starvation fix), parity comparator; cvfc-v2 `f7d79ce5…`/s1-v2 `0cf7433e…` stage when their hold/warmup-law gates clear; merged-v2 `79eaceec…` = the one-table combination. Stay-greens **1420 / 39 / 553**. The M5 crons (com.multivenue.carry/xv) stay RUNNING until bootout. V9 closes (research-universe §6 grammar section landed 2026-08-30).
- **Stage 1 (8a–8e) DONE**, G1 soak blessed 2026-08-15 (history: `docs/arch/phase-8-progress.md`).
- **Stage 2 CLOSED (this commit, 2026-08-22, under the operator-AMENDED §12):** 8f CLOSED (`7ca91be`) · 8g CLOSED (`39e6542`, G7) · **8h CLOSED** (H0 LOCKED · H1 `3ad40a9` · H2 `1ed6017` · H3 `76680db` · H4 `7bd0e42` · H5 `0e47429` · H6a `044c398` · H6b-SEMI = this commit). The autonomous research loop (harness + fill model + data_fetcher + strategist + §8.1 auto-promotion + §8.3 monitor/§8.5 rollback) is CODE-COMPLETE and E2E-proven; the closure record is the last entry of `docs/phase-8h-progress.md`.
- **What was demonstrated LIVE (semi-manual lane, rulings below):** H6a — Fable-5-authored `d8aea5f4…` gates-passed on the §8.5-sanctioned capA and promoted through the FROZEN verbs onto the live engine (vm_rows 2, epoch 1, vm_fires 1, paper). H6b-SEMI — second Fable-5 ruleset `92feb9ea…` (levels 0.45/0.58, $8 caps) gates-PASS first try (OOS +$60.30 / 63 trades / 2 days), promoted (Stage seq=20 / Commit seq=22); then the manual §8.5-shaped ROLLBACK: Disable-5 (mask 49→17) → restage/commit of the prior from its bound paths (staged 2 / committed 3) → operator-act re-enable + re-commit (mask →49, **epoch →2, prior live, vm_fires 0→1**); audit-replay renders the whole chain (seqs 19–32, gaps 0) with ZERO integrity violations. **Standing finding:** Commit is mask-gated at the vm member (8g gating pin) ⇒ the post-rollback operator procedure is **enable + re-commit** (staged prior survives the disabled window; applies without restaging).
- **OPERATOR RULINGS (2026-08-22, standing):** `ANTHROPIC_API_KEY` will NOT be provisioned until Stage 3 — NO `serve`, NO Anthropic API calls, EVERYTHING semi-manual via the ai-session §4 verbs. §12 was AMENDED accordingly: the §8.1 auto-promotion + §8.3 monitor-rollback LIVE proofs are **deferred to the Stage-3 ENTRY GATE** (`docs/mvp-completion-plan.md` §7: key → one keyed serve cycle + one monitor rollback observed BEFORE any executor work). `AI_INGRESS_HMAC_KEY` is PERMANENT in `.env` (engine dotenvy + worker BaseConfig read it; worker verbs need `.env` sourced into the shell — see the H6b wrapper pattern in the closure entry; NEVER read/print `.env`).
- **M1 CLOSED (2026-08-22, commits `c477bb9`+`bad65d6`+close):** universe config file (`~/multivenue/universe.toml`, TOML-subset parser in core-config, M1 SymbolId law: PM[0]→42/BN-spot[0]→7 anchors + namespaced ordinals, USDM base 512) · PM multi-market (one connection, N-id subscribe) · Binance multi-connection lane (one thread, N single-stream conns, spot+`BINANCE_FUT_WS_HOST`) · BN exchangeInfo discovery audit · worker universe-file seeding at the fetch seam (`CLAUDE_WORKER_UNIVERSE_FILE`). Exit proven live: ONE zero-flag boot ran PM 4 tokens + BN 3 conns + OKX/Deribit/HL; audit integrity zero all five venues; map unresolved=0; nextest 1139 / alloc 36 / pytest 363 / fuzz +2 targets clean. Legacy flag boots stay byte-identical. **Operational: the up/down dailies expire 16:00Z — refresh `universe.toml` via the Gamma lane before each boot (M3 automates).** Log: `docs/arch/mvp-progress.md`.
- **M2 CLOSED (2026-08-22, commits `d0e14d4` M2.1 · `485cba1` M2.2 · `1d3670f` M2.3 · `0e98bc0` M2.4 · close commit = this session):** options ladder LIVE on Deribit + OKX (capped E2/K8 chains; exit boot 192 instruments discovered/selected across 3 venues; OptSummary mark/IV channel = PMLR SlotKind 6). **Binance options WS operator-ruled TEMPORARILY UNREACHABLE from this network** — eapi REST live-proven, the code-complete lane retries harmlessly; **activation lever = `BINANCE_EAPI_WS_HOST` in `.env`** (one value + engine restart, NO code change); revisit at/before the M6 soak. M2-close additions (operator-ruled this session): `crates/options-select` (the selection LAW's single home — the venue `select_capped_chain` wrappers keep their frozen signatures, tests, proptest pins and fuzz targets), the per-run **`options-manifest.tsv`** sidecar (bin-written sym→instrument-name map — options ordinals reshuffle per boot BY DESIGN, so every offline venue+descriptor consumer resolves through it; format in `docs/wire-format.md`), and **`claude_worker.iv_digest`** (§9.8: per-sym 1m/1h IV snapshots in the `iv_digest` table BESIDE candles inside `candles.db`; descriptors `deribit:`/`okx:`/`binance-opt:` + instrument name; pre-manifest runs honestly skipped; a MODULE, never a verb; serialized like every worker invocation). Log: `docs/arch/m2-progress.md` (close entry last).
- **M3 CLOSED — C6 CLOSED by operator blessing 2026-08-29** (exit entry = last entry of `docs/arch/m3-progress.md`): closing catalog trailing streak **6 ≥ 3** (ends Aug-28) · 37 runs / 338.2M ticks / 23.2 GB · **37/37 harness-clean, whole-root replay OK** · monitor floor crossed (23h55m ≥ 6h). The step-3 `oos.trading_days≥2` fill sub-gate was BLESSED on streak+machinery (full-root merge ~27 GB > 24 GiB RAM; every feasible OOS tail = PM-dark Aug-28; the fill machinery itself proven in WS13's 27-run/657-fire step-3 run); the Aug-31 bounded rerun stays available, non-gating. The launchd fleet REMAINS live/unattended (T2 restarts 00:00/08:30/16:05Z + 00:20Z pnl timer, hourly candles+iv, retention, caffeinate). C6+ owed items all landed pre-close (D2 `9b062c1` · D3 `f3bd448` · WS11 D5 `065df0b`). The M2/M3 ownership split is DISSOLVED; one-engine + serialized-worker-verbs + explicit-path staging remain standing laws.
- **M4 CLOSED (2026-08-23, commits `89d9348` M4.1 · `c107157` M4.2 · close commit; log `docs/arch/m4-progress.md`, operator rulings D1–D3 recorded there):** shadow-P&L attribution is LIVE end-to-end. M4.1 = the enabling migration (the audit found paper mode captured NEITHER intents nor fills): `Order.strategy_id` @41 + **`engine-orders.pmlr`** intent log (SlotCapture, capture-what-was-accepted) + the set's `StampCtx` per-member attribution (live proof: 503 intents all stamped slot-0). M4.2 = **`audit-pnl`** subcommand (fill law REUSED from `backtest::fill`, boundary-0 books; §6 LAW: books keyed by DESCRIPTOR via per-run manifests — never bare SymbolId across runs; manifest-less runs per-run-namespaced) + **`instrument-manifest.tsv`** (D3: EVERY instrument, final §9.4 descriptors, every boot; options file kept one release) + per-day buckets + the paper view beside it (live: 35 runs/10 days, latency-arb modeled net −$32.55, and the **211,840 §4.1 caps-rejection finding** — emit-cadence-vs-open-order-lifecycle now visible). M4.3 = `claude_worker.pnl_report` module (spawns audit-pnl by the §14 PATH contract, writes `pnl-<day>.json`+`.summary.txt`, schema-checked) + the **`pnl` verb — the D1-unfrozen ONE additive verb** (thin reader; the frozen verb-surface pin amended to 8 with the ruling cited in-test; frozen 202 otherwise byte-untouched). **D2 deferral:** the nightly launchd timer lands at M3's C6+ window (manual `python -m claude_worker.pnl_report` until then — §4-M4's "lands automatically" transfers there). Descriptor continuity begins at each lane's first M4.2+-binary boot (standing lane: its next 00:00Z restart).
- **M5 RUNNING since 2026-08-29 (session 1 = external-strategies onboarding; log `docs/m5-runbook-notes.md`):** CVFC-1 + S1-pilot live in paper via the s4 Intent lane + the hourly `com.multivenue.carry` cron (COTI position open); `docs/research-universe.md` = the loop agent's catalog. **BST STOCKS LANDED same day** (binance-stocks-plan §8): 3 bStocks + 8 TradFi perps (+`TRADIFI_PERPETUAL`→Perpetual discovery fix) + NVDA PM daily (pair 2:2) + BST3 equity-dailies refresh family (ET date law, NYSE holiday table, 20:15Z/21:15Z slots) — equity capture live 24/7. **LATENT M1 CAP found live: PM ≤6 tokens total (token 7 collides with the btcusdt anchor id 7) ⇒ TSLA/SPY dailies wait on an allocation-base slice (operator-ruled).** Stay-greens **1351 / 38 / 497**. Then M6. Plan §9 (PMLR canonical, candles.db keyed venue+descriptor, fetch 1m/1h/1d + derive, backtest reads PMLR only) stays BINDING.
- **HARD OPERATOR REQUIREMENT (standing): Stage-2 completion has been notified (H6b-SEMI close). Do NOT start ANY Stage-3 work (executor, risk/8i+, venue dispatchers, live ramp — code, plans, or designs) without the operator's explicit confirmation; the Stage-3 ENTRY GATE is `docs/mvp-completion-plan.md` §7 and it is the operator's to open. M4 also starts only on explicit operator go (design entry first).**
- **Authority chain now:** `docs/stage2-finish-plan.md` (WS0–WS13, its WS13 entries = the latest word) → `docs/mvp-completion-plan.md` (M-phases; §9 binding) → this file. For 8h archaeology (all ARCHIVED 2026-08-29): `docs/arch/8h-kickoff.md` → `docs/arch/phase-8h-design.md` (LOCKED) → `docs/arch/phase-8h-progress.md` (closure entry last).
- **Frozen contract (unchanged):** `claude-worker/src/claude_worker/backtest.py` — argv `multivenue-engine backtest --ruleset R --replay-dir D --split 70/30`, schema-1 JSON on stdout, GateThresholds numbers; the frozen 202 worker tests pin it. **The harness conforms to the worker, never vice versa.** `backtest.py` and `cli.py` byte-untouched through H6b-SEMI (five sessions).
- **Baselines at M4 close (all run 2026-08-23):** workspace nextest **1240/1240** (+1 skipped fixture-regen; 1 informational leaky on a pre-existing real-binary harness test), release alloc **38/38** 0 B/op (`--test-threads=1`, corrected clean + fresh-`Compiling bench` guard; gate grew 37→38 with the M4.1 orders-capture/stamp assertion), worker pytest **439** (frozen 202 with the ONE D1-sanctioned verb-pin amendment; release binary on PATH), fuzz: standing clean — M2-close re-ran the 3 discovery-family targets ≥300 s (`deribit_instruments` 8.77M · `okx_instruments` 5.71M · `binance_eapi` 11.84M); M4 added no untrusted-bytes parser. Stay-greens: **1349 / 38 / 477 since the 2026-08-29 §5.4-fix gates run** (WS10-era 1345, WS13-gates-era 1316 and M4-era 1240/38/439 superseded).
- **LICENCE PASS LANDED + VERIFIED 2026-08-27** (`2dd88d5` metadata · `3989d63` gate+audit · `9780d42` SPDX headers 194 files +525/−0 · `a0b9159` CLAUDE.md rules · `c2cd66e` license-deps run + 3 tooling fixes + `THIRD-PARTY-NOTICES.md` · `64aa755` notices byte-exact). Apache-2.0 is applied per-file now, not just at the root. **Stay-greens re-run on the Mac and GREEN: nextest 1240 · alloc 38 (test bin freshly re-linked) · pytest 439**; `uv sync` clean; the wheel now ships LICENSE+NOTICE; `cargo deny check licenses` ok; notices = 131 packages / 60 licence sections (ring alone contributes 18). Tooling installed: `cargo-deny`, `cargo-about` (**needs `--features cli`**). **PRE-EXISTING, proven not caused by the pass, left for their own commits: `cargo fmt --check` fails on ~88 files, `cargo clippy -D warnings` fails on ~40 lints ⇒ `make lint` does NOT currently pass.** Rules in "Licensing" below are binding; authority `docs/license-audit-2026-08-27.md` §3.4.
- **Pushes are the OPERATOR's, done manually.** `origin/main` advancing without any session having pushed is NORMAL. (The old "push anomaly KNOWN / origin-main divergence" note was a misreading — operator-corrected 2026-08-27. `origin/main`'s reflog is full of `update by push` because the operator pushes by hand.) Sessions still NEVER push: that rule is unchanged.
- **Git discipline:** NO push, NO rebase, NO history rewrite, NO new branches, NO git ops without operator ask. Do NOT touch `.env`.
- If context runs short: write interim state + exact resume point + relaunch prompt to the active M-phase progress doc, then tell the operator.

## Parallel M2/M3 session protocol (BOTH CLOSED — C6 closed 2026-08-29; the M2/M3 OWNERSHIP SPLIT IS DISSOLVED. The one-engine law, the serialized-worker-verbs law, and explicit-path staging remain STANDING LAWS for every session; the rest of this section is historical record)

Two Claude sessions shared this ONE checkout during M2/M3. (Standing inheritance for every session since: the one-engine law, the worker serialization law, and explicit-path staging.)

- **Git staging is explicit-path ONLY.** `git add <your owned paths>` — NEVER `git add -A`/`-u`. Check `git status` first; the other lane's dirty files are NOT yours to stage, commit, or clean. Commit messages prefixed `M2:` / `M3:`. Commits remain operator-authorized (checkpoint pattern).
- **Ownership.** M2 owns: `crates/ingress-deribit`, `crates/ingress-okx`, `crates/ingress-binance` (eapi), their tests + fuzz targets, `docs/wire-format.md` + `docs/migration.md` (M2.3), `docs/m2-progress.md`. M3 owns: the cli capture-catalog module + bin arm, `claude-worker` (candles.db, refresh automation), `docs/local-setup.md` runbook additions, `~/Library/LaunchAgents` plist, `docs/m3-progress.md`. SHARED — small additive edits, sequential commits, note in your log: `crates/cli` (bin + paper.rs), `crates/core-config`, `crates/core-io`, `universe.toml.example`, `.env.example`, `docs/mvp-progress.md`.
- **ONE ENGINE EVER** (9191 + ai.sock are singletons). Once M3 installs the launchd instance, it is THE standing engine; any smoke boot first `pgrep -f multivenue-engine`, stops the standing instance (`launchctl`), and restarts it after. G0 relink law applies per boot; a relink under a running engine takes effect at its next restart.
- **Worker verbs globally serialized** (one SQLite seq namespace across sessions): `pgrep -f claude-worker` before any verb; never overlap fetch/push/stage/commit with the other session.
- **Cargo shares one target dir** — concurrent builds/tests BLOCK on the file lock. Wait; never kill the other session's build. Long-runners: nohup with per-lane prefixes (`/tmp/m2-*`, `/tmp/m3-*`).
- **Sequencing pin:** M2.3 (mark/IV wire-format migration) starts only AFTER M3's capture-catalog first commit lands (M2's ladder order makes this natural). Whichever side lands second extends the other (the catalog gains the new channel's row).

## Build / test / run

```sh
# build (debug)
cargo build --workspace

# build (release — what we actually deploy)
cargo build --release --workspace

# run the full test suite (unit + proptest + integration)
cargo nextest run --workspace

# run only fast tests (skip fuzz/bench compile)
make test-fast

# run fuzz targets for 5 minutes each (CI default)
cargo fuzz run polymarket_clob_frame -- -max_total_time=300
# cargo-fuzz v0.13.2 installed; runs `+nightly`; in-repo `cargo install`
# trips the 1.88.0 toolchain pin — install `+stable` from $HOME if needed.

# run allocation assertions — this MUST show 0 B/op on hot paths
# --test-threads=1 is REQUIRED: CountingAllocator is process-global;
# parallel threads pollute each other's AllocGuard deltas (or use `make alloc-assert`)
# False-green guard: confirm a fresh `Compiling bench` in the log, or
# `cargo clean -p bench --release` and rerun (H2 correction: plain
# `-p bench` does NOT remove the release test bin on this toolchain).
cargo test -p bench --test alloc_assertions --release -- --test-threads=1

# criterion benches
cargo bench --workspace

# python claude-worker tests
cd claude-worker && uv run pytest

# licence gates — run license-check before every commit (offline, ~1 s:
# SPDX coverage on all tracked .rs/.py/.sh, claude-worker LICENSE/NOTICE
# drift, the fuzz manifest's non-inheritable license key).
make license-check
# only when a dependency changed (needs cargo-deny + cargo-about installed);
# regenerates THIRD-PARTY-NOTICES.md, which is COMMITTED, not release-time.
make license-deps

# start the engine locally (paper mode)
# G0 law: test gates build rlibs/test bins but NEVER relink the release
# binary — ALWAYS `cargo build --release -p cli` before any live boot.
# M1: the universe comes from ~/multivenue/universe.toml (zero flags);
# legacy fallback: no config file ⇒ --polymarket-asset-id is REQUIRED
# (boot refuses venue-blind). For AI-cmd work use the set path:
# --strategy all (bare latency-arb can't express AI toggles).
cargo build --release -p cli
cargo run --release -p cli -- run --paper --strategy all
# legacy (no universe.toml):
cargo run --release -p cli -- run --paper --polymarket-asset-id <TOKEN_ID>

# audit a capture run (every run writes PMLR capture to
# <MULTIVENUE_LOG_DIR>/run-<epoch_ns>/ — per-venue ticks/events/signals
# + optional --raw-tap payload tap)
cargo run --release -p cli -- audit-replay --dir ~/multivenue/logs/run-<ns>
```

## Universe config — adding markets/instruments (M1 runbook)

- The boot universe lives in `~/multivenue/universe.toml` (TOML subset; grammar documented in `universe.toml.example` + `core-config::universe`; `--universe <path>` overrides; file absent ⇒ legacy flag boot). Read ONCE at boot — **changes apply on restart** (brief capture gap, one new run dir; M3 automates the cadence).
- **Polymarket** (crypto up/down binaries only — M1-R1): resolve `clobTokenIds` via the Gamma lane (`https://gamma-api.polymarket.com/markets?slug=<slug>`), append `"<yes>:<no>"` (pair) or `"<token>"` (single leg) to `[polymarket] markets`. Caps: 64 market entries / 128 tokens. Latency-arb wiring: `[pairs] map = ["P:B"]` (market index × `binance.spot` index, 0-based file order).
- **Append, never reorder.** SymbolIds are file-order ordinals (PM token[0]→42, BN spot[0]→7, everything else `make_symbol_id(venue, ordinal)`; USDM base 512). Reordering a still-listed instrument re-syms it next boot, and the worker map keeps the OLD sym by design (conflict reported on every fetch until the stale `market-map.json` entry is pruned). Wholesale replacement is clean — the daily up/down refresh (markets expire 16:00Z) drops old ids from the observed universe (dead map names are harmless) and adds fresh ones.
- **Binance**: append to `spot` / `usdm` (lowercase stream symbols) — the multi-connection lane grows one conn; the exchangeInfo audit validates at boot. **OKX/Deribit/HL**: append to their instrument/coin lists; 8e discovery validates.
- After restart, run `claude-worker fetch` once — the `CLAUDE_WORKER_UNIVERSE_FILE` seam seeds map names (§9.4 descriptors), Gamma meta and YES/NO pairs; `unresolved=0` in the fetch output is the done-tell.
- One-off boots without editing the file: the per-venue CLI flags still override (`--polymarket-asset-id X` etc.).

## Hard architectural rules (do not violate — the build will fail if you do)

### Rust
- **Zero allocations in hot paths.** No `Vec::push`, no `format!`, no `to_string`, no `Box::new`, no `Vec::from`. Preallocate at boot, reuse forever. Enforced by `core-alloc::CountingAllocator` in tests.
- **No `dyn Trait` in hot paths.** Strategies are monomorphized via `Engine<S: Strategy>`. Generic dispatch, not virtual.
- **No `tokio` on hot path.** Tokio allocates and is cooperative-scheduled. Bootstrap only, if at all.
- **No `serde_json` on hot path.** Every WS/HTTP parser is a handwritten byte scanner over `&[u8]`.
- **No `ethers` / `alloy` full stacks.** We use `secp256k1` + `tiny-keccak` directly.
- **No `async-std`, no `reqwest`.** Hyper + rustls only for HTTP/2.
- **No `foreach`/iterator overhead in hot loops.** Raw indices, `get_unchecked` inside safe wrappers.
- **No bounds checks in hot loops.** Hot loops use `unsafe` blocks with `// SAFETY:` comments; safe wrappers uphold invariants.
- **No panics in release hot paths.** Use `debug_assert!`. Release builds have `panic = "abort"`.
- **Every POD struct in hot path is `#[repr(C)]` + `#[derive(Copy, Clone)]`.**
- **Every ring / cache-sensitive struct is `#[repr(align(64))]`.**
- **Strategies implement the `Strategy` trait from `strategy-core`.** No exceptions.
- **Every ingress parser has a property test + a fuzz target.** See §21.3 and §21.4 of PLAN.md.
- **Every public function has at least one happy-path and one failure-mode unit test.**
- **Offline paths (audit-replay, backtest) MAY allocate** — they are not hot paths; each such module carries a doctrine header saying so.

### Python (`claude-worker/`)
- **Full `import x` only. Never `from x import y`.** This is a codebase-wide preference.
- **No live Anthropic API calls in tests.** Mock at the SDK boundary.
- Anthropic SDK is constructed inside `serve` only; strategist model is `MODEL_STRATEGIST = "claude-fable-5"`.

### Licensing — EVERY new file (enforced by `make license-check`)
- **Every new `.rs`, `.py` and `.sh` file MUST carry the two-line SPDX record as its first lines.** No exceptions — tests, fuzz targets, one-off scripts, throwaway harnesses. Adding a file without it fails `make license-check`, which is a gate, not advice.
  ```rust
  // SPDX-License-Identifier: Apache-2.0
  // Copyright 2026 Anton (darkcite)
  ```
  ```python
  # SPDX-License-Identifier: Apache-2.0
  # Copyright 2026 Anton (darkcite)
  ```
- **Placement is exact:** Rust — above the `//!` inner-doc block and above any `#![...]` inner attribute (comments may legally precede both). Python — above the module docstring (`__doc__` still resolves). Shell — **after** the shebang, never before it.
- **Every new crate's `Cargo.toml` gets `license.workspace = true`.** The one manifest that cannot inherit is `fuzz/Cargo.toml` (workspace-`exclude`d by cargo-fuzz convention) — it carries a literal `license = "Apache-2.0"`; keep the two in sync.
- **Adding/changing/removing a dependency changes the license surface of the shipped binary.** Run `make license-deps` (cargo-deny + cargo-about) and commit the regenerated `THIRD-PARTY-NOTICES.md` with that change. If `cargo deny check licenses` rejects a new license, that is a deliberate decision in `deny.toml` — read what the license obliges before appending a line, never rubber-stamp it.
- **Never vendor third-party source into this tree.** Deps are unmodified crates.io/PyPI packages, which is what makes the `NOTICE` claim true. If material of any provenance must land in-tree, record it in `NOTICE` first.
- **No binary leaves the build host without `LICENSE` + `NOTICE` + `THIRD-PARTY-NOTICES.md` beside it.** Stage-3 / Phase-7 gate.
- **Repo-wide file rewrites: verify `git diff --summary` is empty of mode changes.** A `> tmp && mv` pass creates new inodes at the umask and silently strips exec bits — it did exactly that to the five `scripts/*.sh` launchd scripts on 2026-08-27. `--numstat` does NOT show mode changes and will not catch it.
- Authority: `docs/license-audit-2026-08-27.md`. Contributor-facing copy: `CONTRIBUTING.md`.

### Secrets
- **`.env` file only.** No macOS Keychain, no AWS KMS, no Vault, no Secrets Manager.
- **`.env` is `chmod 600` and in `.gitignore`. `.env.example` is committed.**
- **Signing key loaded into an `mlock`'d page, zeroized on drop.**

### Deployment
- **No cloud services, at any phase.** Even Phase 7 EC2 is a plain Linux VM — no KMS, no SSM, no CloudWatch, no Terraform, no Ansible.
- **No observability stack.** TUI (`ratatui`) + log files + a trivial `/metrics` endpoint on `127.0.0.1`. No Prometheus, no Grafana.

## Directory guide

- `PLAN.md` — full architecture, phased roadmap, testing strategy.
- `docs/stage2-finish-plan.md` — the ACTIVE authority (WS0–WS13; WS13 gates+live entries carry the latest word).
- `docs/mvp-completion-plan.md` — M-phase plan; §7 = the Stage-3 entry gate; §9 data-pipeline law (BINDING).
- `docs/m5-runbook-notes.md` — M5 accumulator. (The M3 log closed with C6 2026-08-29 → `docs/arch/m3-progress.md`.)
- `docs/prompts/ai-session.md` — semi-manual AI-session prompt (pinned by `claude-worker/tests/test_session_scripted.py` — do not move or drift it).
- `docs/wire-format.md` — PMLR v2 ring-slot/replay-log formats. `docs/migration.md` — format/schema migration log.
- `docs/risk-policy.md` — kill-switch and cap rules.
- `docs/license-audit-2026-08-27.md` — Apache-2.0 compliance audit + application record; the authority for the "Licensing" hard rules. `CONTRIBUTING.md` — the contributor-facing copy. `deny.toml` / `about.toml` / `about.hbs` — the dependency licence gate.
- `docs/architecture.md` (+ `.svg`, `phase-8-architecture.svg`) — one-page orientation.
- `docs/local-setup.md` — Mac toolchain setup. `docs/hot-path-latency.md` — standing latency audit (referenced by PLAN.md + bench).
- `docs/options-support-plan.md` — Phase 9+ candidate, P&L-gated; nothing lands in 8h.
- `docs/arch/` — **CLOSED history** (phase 1–6 plans, Stage-1 progress, 8f/8g/8h design+progress, Stage-2 parent plan, M1/M2/M4 logs, the Aug-27/28 outage+remediation docs, WS10 design, archived prompts). See its README index. Never write there; read only for archaeology.
- `crates/core-*/` — OS-agnostic primitives (rings, time, config, alloc, io, net, parse, simd, crypto).
- `crates/core-crypto/` — handwritten SHA-256 / HMAC-SHA256 / base64 (RFC 4648); no external crypto stacks.
- `crates/core-io/` — PMLR replay log writer/reader + `PmlrCapture` (per-ingress §6.5 capture sink) + raw tap (`PMRT`).
- `crates/ingress-*/src/discovery.rs` — per-venue boot REST discovery (8e): instrument universes, tick/lot metadata, coverage audit.
- `crates/ingress-*/` — one per external source (polymarket, binance, okx, deribit, hyperliquid, rpc). `crates/ingress-ai/` — AI command plane (UDS+HMAC, ruleset validate/stage/commit).
- `crates/strategy-*/` — strategies implementing the `Strategy` trait; `strategy-vm` — the 8g ruleset VM; `strategy-set` — composed strategy set (mask-49, compose-if-configured).
- `crates/signer-eip712/` — audited-C-backed signer; do not replace with `ethers`.
- `crates/clob-dispatcher/` — persistent H/2 client; preallocated buffers.
- `crates/cli/` — the main binary (`multivenue-engine`: run / audit-replay / backtest-in-progress).
- `crates/tui/` — read-only dashboard; snapshot-page driven.
- `crates/bench/` — criterion + dhat; allocation assertions live here too. `#[global_allocator]` CountingAllocator is process-global — keep new tests of other crates OUT of the bench crate.
- Integration tests live per-crate under each crate's `tests/` directory. No workspace-level `tests/` is used.
- `fuzz/` — cargo-fuzz targets.
- `claude-worker/` — Python 3.14 worker: `serve` daemon + operator verbs (fetch/backtest/push/positions/stage-ruleset/commit-ruleset); Anthropic SDK constructed inside `serve` only; never in the hot path. **Research one-shots (`claude-worker/tools_*.py`) are deliberately git-excluded** — `tools_` is a reserved prefix there; findings go to `docs/`, outputs are the sha256-named artifacts in `~/multivenue/artifacts/rulesets`, and anything that earns a caller moves into `src/claude_worker/`. Authority: `docs/research-tools-exclusion-plan.md`.
- `.claude/` — subagents, slash commands, settings.

## Common pitfalls — if you're about to do one of these, stop

1. **Adding `tokio` to any `crates/core-*` or `crates/ingress-*`.** Use `mio` + handwritten state machines.
2. **Adding `serde_json` to an ingress parser.** Write a byte scanner; see `core-parse`.
3. **Adding `.collect::<Vec<_>>()` in a hot loop.** Preallocate a fixed-size array or InlineArray-equivalent.
4. **Using `String` in a hot path.** Symbols are `type SymbolId = u32;`. News payloads are `&[u8]`.
5. **Using `async fn` on anything the engine loop touches.** Hot path is synchronous.
6. **Adding a `from x import y` in `claude-worker/`.** Full `import x` only.
7. **Proposing to add Prometheus, Grafana, Terraform, AWS SDK, or any cloud service.** Deliberately excluded.
8. **Proposing to add paid API integrations (X, Benzinga, Blocknative) before Phase 6.** Gated on demonstrated P&L.
9. **Proposing to skip tests "because it's a small change".** Zero-alloc assertions exist because small changes have regressed them before.
10. **Trusting `cargo` runs inside the Cowork Linux sandbox.** The mounted-repo fingerprints go stale and produce FALSE GREENS (observed 2026-08-15). Compile and test on the Mac only. Corollary on the Mac: impossible-looking unresolved-import errors right after file edits = stale rmeta — `cargo clean -p <touched crates>` and retry.
11. **Trusting probe fixtures over live boots.** Venue wire drifts (OKX `preopen` empties, 27-byte XPERP ids, Deribit starbase reorder + sci-notation floats were all caught LIVE in 8e). New parsers get a live smoke run before being declared done; `--raw-tap` exists for exactly this.
12. **Long commands through the RustRover MCP terminal.** `execute_terminal_command` executeInShell=true has a ≤45 s window — long runs: `nohup … > /tmp/log &` then poll. zsh eats bare `===` in echo.
13. **Modifying the worker's frozen surfaces.** `backtest.py` argv/schema-1 and the verb surface are FROZEN; the harness conforms to the worker. The 202 pytest baseline stays untouched-green.
14. **Creating a new `.rs`/`.py`/`.sh` without the SPDX header, or a new crate without `license.workspace = true`.** See "Licensing" above. `make license-check` fails on it, and a header-less file lifted out of the repo carries no license signal at all — §4(c) obliges downstream to retain notices that then do not exist. Write the two lines when you create the file, not later.
15. **Adding a dependency without re-running `make license-deps`.** A new crate changes what the shipped binary must attribute. `THIRD-PARTY-NOTICES.md` is committed, not generated at release time, precisely so it cannot be missing when a binary ships.
16. **Committing a `claude-worker/tools_*.py` research one-shot — or naming a specific one in a permanent doc.** The class is git-excluded by policy, and `make license-check` fails on BOTH: a tracked one-shot (`git add -f` is the only way one comes back), and any tracked file that names a concrete one-shot, or the external research corpus, outside that class's owning authority doc. Naming the class pattern to state the law is fine; naming a file you do not ship is how a permanent doc comes to point at nothing — cite the owner doc instead (`docs/research-tools-exclusion-plan.md`; external corpus → `docs/license-audit-2026-08-27.md` G8). If a one-shot genuinely needs tracking it becomes a module in `src/claude_worker/` with tests, not a forced add.

## macOS session facts (hard-won)

- AF_UNIX `sun_path` length cap bites long socket paths.
- `SO_RCVTIMEO` returns EINVAL on peer-closed UDS.
- `std::thread::scope` panic hangs without a StopOnDrop guard.
- `sample <pid>` is the go-to for diagnosing hangs.
- RustRover MCP must attach (`get_project_modules`) against the main checkout FIRST; if it won't attach, stop.
- A REUSED RustRover MCP terminal can return STALE/MIXED output from earlier commands (burned two M2.3 investigation loops on phantom lines that were in no file). Evidence-critical reads: `reuseExistingTerminalWindow=false`; files are the only ground truth.
- The RustRover MCP terminal window is ~45 s REGARDLESS of the requested timeout — long runs: `nohup … > /tmp/<lane>-*.log 2>&1 &` then poll the log file.
- Concurrent pytest runs across sessions COLLIDE (shared socket/tmp fixtures; twice-observed in M2). Treat pytest like a serialized worker verb: `pgrep -f pytest` first, never overlap lanes.
- `tests/test_recommit.py::test_recommit_restages_and_recommits_active_row` is INTERMITTENTLY FLAKY under the full suite — observed once on 2026-08-30 as `len(fake_uds.frames) == 0` (no frame reached the `FakeUdsServer`), i.e. the UDS fixture, not the assertion under test. Characterized immediately: 6/6 green in isolation, 598/598 green on the two full reruns that followed. Same family as the AF_UNIX/`SO_RCVTIMEO` facts above. Re-run before believing it; a single red here is not a regression signal.
- `pkill -f <pattern>` can match a POLLING shell that quotes the same string (cosmetic exit 143) — bracket one letter of the pattern (`multivenue-engin[e]`).

## Preferred Claude models for tasks in this repo

- **Bulk artifact generation** (topic tagging): Haiku 4.5.
- **Reasoning** (rule parsing, news labeling): Sonnet 4.6.
- **Strategy proposals** (`claude-worker` serve strategist, ruleset drafts): Fable 5 (`MODEL_STRATEGIST = "claude-fable-5"`).
- **Hard work** (backtest review, architectural changes): Opus 4.6.

## When in doubt, read (in this order)

1. This file's CURRENT STATE section — where we are, what's next.
2. `docs/stage2-finish-plan.md` — the active authority; WS13 entries = the latest word.
3. `docs/mvp-completion-plan.md` — M-phases, §7 Stage-3 entry gate, §9 binding data law.
4. `PLAN.md` — everything architectural.
5. `docs/wire-format.md` — ring slot layouts, replay log format.
6. `docs/risk-policy.md` — kill-switch and cap rules.
