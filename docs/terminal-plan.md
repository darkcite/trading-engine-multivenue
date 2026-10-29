# TERM — the operator's trading terminal: PLAN v2.1 (2026-10-19)

**Status:** PLAN. Nothing is built, nothing is committed, and the engine is untouched.

- **Lane:** TERM.
- **Authority:** this plan ranks below `CLAUDE.md`, then `docs/risk-policy.md`, then `docs/state.md`. Never take an armed or live state from this file: read `/state` first (`boot.git_sha`, `exec`, the slot blocks).
- **What v2 changes:** the frontend is **Rust compiled to WASM** (egui + eframe + wgpu), with **an all-Rust chart engine**. Lightweight Charts and every line of hand-written JS are gone.
- **Slots:** TERM moves to slots **10–13**, because the PVM branch claims slot 8 for `pine`.
- **Unchanged from v1.1:** the engine side, the door, the risk laws and the 17 review fixes (§10). The server-side sections are carried over, with the slot numbers updated.
- **v2.1** adds the 12 findings of a review of v2's frontend (§10). The reviewer checked the key claims by building a mirror workspace: Rust 1.88, eframe 0.33.3, wgpu 27, served under the plan's CSP in Chromium 141.

---

## 0. Summary

TERM is a local trading terminal. It offers:

- **Charts** in TradingView style: candles, panes, indicators, drawing tools, Pine overlays and the engine's own fills.
- **Order-book microstructure:** a DOM ladder showing our orders and their estimated queue position, a depth heatmap with trade bubbles, the tape, CVD, OFI, book imbalance, microprice, footprint bars and a volume profile.
- **Order entry** on Binance, Hyperliquid, Polymarket and Hypercall: a ticket, click-to-trade on the ladder, hotkeys, and TWAP / chase / iceberg algos.

**The whole stack is Rust.**

| layer | what it is |
|---|---|
| engine | unchanged, plus a new TERM member and door |
| `multivenue-terminal` | a cold server process |
| `term-ui` | the browser app: egui on one WebGPU/WebGL2 canvas, with the chart, heatmap, ladder and footprint drawn by our own wgpu pipelines |
| `term-proto` | a shared, dependency-free crate that defines every frame the server and the UI exchange, so the two cannot drift |

**The only JS in the browser** is the glue that `wasm-bindgen` generates at build time, plus `loader.js`, an external loader of 15 lines or fewer (CSP forbids inline script). Neither is ever edited by hand after it is written, and both are hash-pinned.

**The order path is unchanged.**

1. The browser sends an intent to the server. The server holds the trading gates.
2. The server passes it through the HMAC UDS door to a TERM member.
3. The member's pre-gate checks it.
4. The E6 risk gate checks it.
5. The slot's own arm sends it, with the cloid encoding the slot (E-9).
6. The ACK is the HTTP answer and the FILL is `userFills` (E-5).

**Secrets.**
- Venue keys stay in `.env`, read only by the engine.
- The door key lives in `~/multivenue/term.env` (R-T13).
- **The WASM is untrusted UI and holds no secret** (T-L13).

---

## 1. Operator rulings

### 1.1 Taken 2026-10-18 (v1)

| id | question | ruling |
|---|---|---|
| **O-T1** | How do orders reach venues? | **Through the engine.** A manual member on a reserved slot receives intents over an HMAC UDS door shaped like `ingress-ai`. Orders pass the E6 gate, the cloid encodes the slot, and there is one gateway per venue account. |
| **O-T2** | UI | A local web app. *Superseded in part by O-T9 … O-T11.* |
| **O-T3** | Venues for v1 order entry | **Binance, Hyperliquid, Polymarket CLOB, Hypercall options.** HyperEVM is out. |
| **O-T4** | Microstructure | **All four:** the ladder and our own orders, the heatmap with bubbles, the tape with flow metrics, and the footprint with VP. |
| **O-T5** | Accounts | **Mixed per venue.** The laws narrow this: see §4.4 and R-T2. |
| **O-T6** | Beyond the core order types | **Execution algos** (TWAP, chase, iceberg) and **ladder hotkeys**. Native stops, TP/SL, brackets and OCO are out. |
| **O-T7** | Overlays | **Built-in indicators, Pine via pine-vm, engine signals and fills.** |
| **O-T8** | Rollout | **Paper first, then live dust one venue at a time.** |

### 1.2 Taken 2026-10-19 (v2, the frontend)

| id | question | ruling |
|---|---|---|
| **O-T9** | Frontend language | **Rust compiled to WASM, not JS.** |
| **O-T10** | UI stack | **egui + eframe.** Immediate mode on one WebGPU/WebGL2 canvas. The chart, heatmap and ladder are custom wgpu paint callbacks; the ticket and blotter are egui widgets. |
| **O-T11** | Chart | **An all-Rust chart engine.** Lightweight Charts is dropped, and so is R-T3, the vendoring question. |
| **O-T12** | Targets | **Web first, native later.** The UI core stays free of web-only APIs, so a native eframe app can be added without a rewrite. |
| **O-T13** | Compute split | **The server computes and the WASM renders.** Books, bars, footprint and flow metrics are computed once, on the server. The WASM does only view-dependent work: visible-range VP, hit-testing, and indicators over the visible bars. |

---

## 2. Facts that shape the design

### 2.1 Market data

The engine's market data is too shallow for these views. Binance carries only `bookTicker`, `aggTrade` and `markPrice` (no depth). Hyperliquid carries `bbo` plus a top-5 `l2Book` capture. Polymarket carries the top of book. Hypercall carries RFQ provider quotes, not an L2. OKX and Deribit carry `DepthTopK` with K = 5.

So the terminal opens its own public sockets. That data is for display only (T-L5).

### 2.2 Per-IP limits shared with the engine

**Hyperliquid:**
- Per IP: 10 WebSocket connections, 30 new connections a minute, 1 000 subscriptions, 2 000 messages a minute, 1 200 REST weight a minute.
- Each gateway arm takes a share of 400 weight and 1 000 messages a minute (`exec_hyperliquid::gateway`).
- Slots 3 and 6 together would already use every message.

**Binance:**
- 300 connection attempts per 5 minutes, and 1 024 streams per connection.
- The engine opens 254 sockets in its boot burst, before `/state` answers.
- A 429 or 418 response is a sticky `BudgetFloor` / `VenueLock`.

**Hence:**
- TERM's budgets are hard caps in code, and TERM yields first (T-L6).
- T1a is blocked on T0's measurement.

### 2.3 What each arm can do today

| venue | what the arm supports | what limits it |
|---|---|---|
| **HL** (`exec-hyperliquid`) | GTC/ALO, modify, cancel, reduce-only, `scheduleCancel` | The gateway transport belongs to slots 3 and 6 only. |
| **Binance USDⓈ-M** | makers with a TTL, `order.modify`, `countdownCancelAll` | **BX-7 as built: one live Binance slot.** |
| **Binance spot, COIN-M, options** | IoC only | No dead-man. |
| **Binance Stocks** | — | Out: the engine refuses `equity`. |
| **Polymarket** (`exec-polymarket`) | FAK BUY only, on 15-minute up/down markets | Slot 9 alone. No cancel or modify. |
| **Hypercall** | operator verbs only | Not in `LIVE_ARM_VENUES` (a test asserts this) and not in `PNL_JUDGING_VENUES`. |

- `LIVE_ARM_VENUES` is `[Hyperliquid, HyperEvm, Binance, Polymarket]`. CLAUDE.md's Live-safety line is stale: it omits Polymarket.
- **Arms are slot-owned.** Each TERM slot needs its own arm, gateway and account.

### 2.4 Accounts, keys and venue minimums

**Hyperliquid:**
- The XMM and HYPARB precedents use their own HL accounts.
- Nonces are tracked per signer. Use one API wallet per process and per sub-account.
- Sub-accounts need $100 k of volume.

**Binance:**
- An account can hold 5 sub-accounts, each with up to 30 API keys.
- Options on a sub-account are UNVERIFIED.

**Polymarket:**
- A new signer means a new Deposit Wallet.
- pm15 has no account yet.

**Minimum order sizes:**

| venue | minimum |
|---|---|
| HL | $10 per order |
| Binance USDⓈ-M | `MIN_NOTIONAL` per symbol; BTCUSDT is about 100 USDT |
| Binance spot | `NOTIONAL` |
| Polymarket | the marketable-BUY floor |

### 2.5 Identity and precision

- `SymbolId`s are file-order ordinals.
- `universe_refresh` rewrites `[polymarket]` daily.
- So TERM keys instruments by descriptor, resolved per boot, and every `TermCmd` carries the boot epoch.
- Values are ×1e6 integers end to end. An instrument whose tick or lot is below 1e-6 is refused when it is loaded.

### 2.6 Slot map after PVM and PM15

| slot | owner |
|---|---|
| 8 | **`pine`** — the `pvm-pine-engine` branch (`strategy_set`: `SLOT_PINE`, `BIT_PINE`; branch `BUILT_MASK` 0x03FF) |
| 9 | `pm15` |
| **10–13** | **TERM:** 10 `term-hl`, 11 `term-bn`, 12 `term-pm`, 13 `term-hc` |
| 14–15 | stay reserved |

- If TERM lands before PVM, `BUILT_MASK` = 0x02FF | 0x3C00 = **0x3EFF**. Once all three lanes are on main, it is **0x3FFF**. The strategy-set tests pin both values.
- `all` stays 0x00FF.

### 2.7 The Rust UI stack, as of 2026-10-19

**The repo pins Rust 1.88.0** (`rust-toolchain.toml`: targets `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`; workspace edition 2021).

| version | rust-version | wgpu |
|---|---|---|
| **eframe 0.33.x (0.33.3, 2025-12-11)** | **1.88** | 27 |
| eframe 0.34.0 | 1.92 | 29 |
| eframe 0.37.0 (2026-10-15) | 1.95 | 30 |

- **The newest egui that fits the pin is 0.33.3.**
- eframe on the web compiles to WASM. It uses WebGPU where the browser has it and falls back to WebGL2. It needs the `wasm32-unknown-unknown` target and the `wasm-bindgen` CLI for the JS glue.
- **Safari 26 (macOS 26) ships WebGPU by default.** So does Chrome.
- **Licences:**
  - egui, eframe, wgpu and naga are MIT OR Apache-2.0.
  - **egui's default fonts are not.** The `epaint_default_fonts` crate declares `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`.
  - The repo's `deny.toml` allow-list has neither OFL-1.1 nor Ubuntu-font-1.0 (R-T19).
  - With `default_fonts` off, `epaint_default_fonts` leaves the graph entirely, kittest included (verified).
- **The dependency graph** of eframe 0.33.3 + wgpu 27 (verified in the mirror workspace):
  - 118 crates for wasm32 and 335 `Cargo.lock` entries.
  - **The lock picks up `tokio`.** wasm-bindgen-futures 0.4.79 (paired with wasm-bindgen 0.2.129) declares a target-gated dependency on it, under `cfg(all(target_os = "emscripten", wasm_bindgen_unstable_tokio))`.
  - `cargo tree -i tokio` is empty, but the repo's `deny.toml` bans `tokio` and cargo-deny reads every target.
  - **Fix:** pin `wasm-bindgen = "=0.2.128"`, which pairs with -futures 0.4.78 and has no tokio in its graph.
- **Build-time host tools:**
  - The `wasm-bindgen-cli` binary itself depends on serde_json, ureq and rouille.
  - Neither it nor `wasm-opt` is in any crate graph we ship, and neither is linked into a binary.
  - They are build tools, like `cargo-deny` itself; R-T18 records that.
- **Browser behaviour:**
  - Chrome 133+ with Energy Saver **freezes** a tab that has been hidden for more than 5 minutes and is CPU-heavy, and its network handlers stop.
  - Hidden tabs pause rAF and slow chained timers to about once a minute.
  - Headless Chromium 141's software-WebGL fallback is deprecated and needs `--enable-unsafe-swiftshader`.

### 2.8 What is reused

**Engine and server side:**
- The `ingress-ai` frame law
- the `exec-gateway` kit
- `core-net` and `core-crypto`
- `core-io::PmlrReader` and the captures
- `instrument-manifest.tsv`
- the candles store
- `core_fill` and its queue law
- `book_builder::ladder`
- `/state` and `/api/worker`

**UI side:** nothing; there is no UI precedent in the repo. The dashboard is 750 lines of HTML with no framework.

---

## 3. Architecture

```text
 BROWSER  http://127.0.0.1:9393                       multivenue-terminal (cold; never sources .env)
 ┌─────────────────────────────────────────────┐      ┌───────────────────────────────────────────────┐
 │ index.html + loader.js (≤15) + term_ui.js   │      │ web (mio): HTTP/1.1 + RFC 6455; cookie        │
 │   (wasm-bindgen glue, generated, never      │      │   session; exact Host/Origin; CSP; serves the │
 │    hand-edited)                             │      │   embedded wasm bundle; trading gates HERE    │
 │ term_ui_bg.wasm  — egui/eframe app          │ WS   │ md-bn · md-hl · md-pm · md-hc (public only)    │
 │   term-ui-core: state, chart engine,        │◀────▶│   deep books · bars · footprint · flow metrics │
 │     ladder, ticket, blotter, hotkeys        │ bin  │   heatmap columns (u8, log-quantized)          │
 │   term-render: wgpu pipelines + WGSL        │ frames│ hist: candles store (ro) · REST · PMLR prints │
 │   term-ui-web: canvas, WebSocket, focus,    │(term-│ state: /state 1 Hz · tail engine-*.pmlr        │
 │     visibility, clipboard  (the only        │proto)│ door: UDS client · HMAC · seq · heartbeat      │
 │     web-sys user)                           │      │ term-proto: the SAME crate as the UI           │
 └─────────────────────────────────────────────┘      └────────────────────┬──────────────────────────┘
                                             ~/multivenue/term.sock        │  82 B TermCmd ▼ ▲ TermEvt (HMAC)
 multivenue-engine ────────────────────────────────────────────────────────┴──────────────────────────
   term-door (only when STRATEGY names a TERM slot) → reduce ring | grow ring
   ENGINE THREAD: slot 10 term-hl · 11 term-bn · 12 term-pm · 13 term-hc
       reduce ring first (no pre-gate) · grow ring → pre-gate → E6 → the slot's own arm
```

### Trust boundaries

**1. Browser ↔ server.**
- **Login.**
  1. `scripts/terminal-open.sh` mints a one-time token and opens `/#t=<token>`.
  2. **The WASM** reads `location.hash`, POSTs the token to `/login` with web-sys `fetch` (allowed by `connect-src 'self'`), then removes it with `history.replaceState`. No hand-written JS is involved.
  3. The server answers with an HttpOnly, SameSite=Strict cookie and burns the token.
- **Request checks.** `Host` and `Origin` must match exactly; a null Origin is refused.
- **Browser profile.** The terminal runs in a **dedicated browser profile with no extensions**. Under the same user, an extension or page automation could send valid intents.
- **CSP:**

  ```text
  default-src 'none'; script-src 'self' 'wasm-unsafe-eval';
  connect-src 'self' ws://127.0.0.1:9393; img-src 'self';
  style-src 'self'; base-uri 'none'; form-action 'self';
  frame-ancestors 'none'
  ```

  - `base-uri` and `form-action` do not fall back to `default-src`, so they are set explicitly.
  - `'wasm-unsafe-eval'` is the narrowest directive that lets a page compile WASM (Chrome; Safari since WebKit bug 197759 was fixed). `'unsafe-eval'` is never granted.
  - Because `script-src 'self'` blocks inline scripts, the loader is an external file, `loader.js`: ≤ 15 lines, SPDX header, hash-pinned in the bundle manifest.
  - Checked: eframe 0.33.3 loads under this exact CSP in Chromium 141 with no violations, because eframe sets styles through the CSSOM.
- **Response headers.**
  - `.js` is served as `text/javascript` and `.wasm` as `application/wasm`, both with `X-Content-Type-Options: nosniff`.
  - `Cache-Control: no-store`, so a rebuilt bundle is never mixed with a cached one.
- **Every trading gate is server-side** (T-L13). A modified bundle, an extension or any automation can submit only what passes those gates. **The typed live confirmation is a UX step, not authentication.**

**2. Server ↔ engine.** HMAC in both directions, using a key that lives outside `.env`.

**3. Engine ↔ venue.** Unchanged.

**Residual:** a process running as the same user can read the key file or the cookie store. T-L2 binds sessions, not the OS.

---

## 4. Components

### 4.1 The member: `crates/strategy-term`

**Unchanged from v1.1 §4.1 except the slot numbers.**

**Structure**
- One member type, `Term<V>`, with four `member_table!` rows on **slots 10–13**.
- The `STRATEGY` alias `term`.
- A descriptor allow-list in `term.toml`. An unresolved entry is dropped and reported, never a boot refusal.

**Command classes**
- **Reducing:** cancel, cancel-all, halt, flatten's cancel leg and algo-cancel. These skip the pre-gate, ride their own ring, and are drained first. Under pressure the door applies backpressure; it never discards one.
- **Growing:** these go through the pre-gate:
  - the age check, against `sent_wall_ns`
  - the band, against the **engine's** touch
  - the venue minimum
  - notional and position limits
  - one rate bucket shared by commands and algo children, inside the slot's per-IP share
  - capability

**FLATTEN** cancels first, then exits reduce-only IoC at the touch ± slip. With a stale touch or a halted slot, the exit leg is refused.

**MODIFY** follows E-7. Its fallback is sequential: cancel, wait for the terminal event, then place.

**Order lifetime**
- `max_rest_s` sets the TTL.
- The venue dead-man is a boot law for any slot that rests orders.
- The watchdog stops the algos and cancels resting orders when the heartbeat is lost or the UI is lost (R-T15).
- E-8 governs HIP-4 rolls.

**Positions** are adopted at boot. No TERM order survives a restart.

**HALT is slot-scoped.**
- It never maps to the engine-wide `AiCmdKind::HaltRequest`, and a test proves that no other bit is touched.
- On a paper slot it is the member's own latch.
- On a live slot it also writes an `exec.HALT` line. That line survives the daily restart and is cleared only by removing that one line and restarting.

### 4.2 The door: `crates/ingress-term`

**Unchanged from v1.1 §4.2.**

**The thread.** One `term-door` thread, spawned only when `STRATEGY` names a TERM slot.

**Accept order**
1. length
2. HMAC
3. shape (the slot must be in the TERM mask)
4. seq
5. **boot epoch**
6. **`sent_wall_ns` age**
7. `ts_ns` rewrite
8. PMLR capture to `term-cmds.pmlr`
9. hand-off to the reduce ring or the grow ring

Intents are never queued across a disconnect.

**Frames.** `TermCmd` and `TermEvt` are 64 B each; layout as in v1.1 §4.2. Their `slot` field must be one of 10–13.

**The new PMLR kind** is registered in every reader: `PmlrReader`, the worker's `pmlr.py`, `archive.py`, capture-catalog and audit-replay. The change is checked against the 202-test pin, and the old binary must be able to read the new kind (rollback-safe).

### 4.3 Risk

**Unchanged from v1.1 §4.3.**

- **E6 per slot.** Caps equal paper's. Cancels are never blocked.
- **Live caps** are the operator's call, set above each venue's minimum. Proposal:

  | slot | per order | per instrument | per day |
  |---|---|---|---|
  | `term-hl` | $12 | $40 | $100 |
  | `term-bn` | only symbols with a low minimum notional | — | — |
  | `term-pm` | $2 | $10 | $25 |
  | `term-hc` | $5 | $10 | $25 |

- **E7 per single-venue slot.** Hypercall has no E7 judge, so it needs either one or an explicit unbounded ruling.
- **New risk-policy section:** "TERM — slots 10–13".

### 4.4 Accounts per venue (O-T5, for R-T2)

| venue | slot | recommendation | the shared variant |
|---|---|---|---|
| Hyperliquid | 10 `term-hl` | **Dedicated, forced.** `HYPERLIQUID_TERM_MASTER_ADDR` and `HYPERLIQUID_TERM_AGENT_KEY`. | Not possible. Another slot's listed order marks the account shared and `admit` refuses. The sweep cancels every MV cloid. Positions would net, and nonces would race. |
| Binance | 11 `term-bn` | **A dedicated sub-account.** `BINANCE_TERM_*`. BX-7 allows one live Binance slot either way (R-T14). | Only while `term-bn` is the sole Binance slot. Otherwise ghosts and Orphans appear, and `SpotStray` / `OptStray` refuse the boot. |
| Polymarket | 12 `term-pm` | **A dedicated signer.** `PM_TERM_*`. | Not possible: the Polymarket arm is "slot 9's alone". |
| Hypercall | 13 `term-hc` | **A dedicated wallet and agent.** | Only if the operator's verbs stay unused while the slot is armed. |

- Venue keys live in `.env`, written by the operator.
- The door key lives in `~/multivenue/term.env` (0600). Only `engine-wrapper.sh` and the terminal's launcher source it.
- No session reads either file.

### 4.5 Arm work per venue

**Unchanged from v1.1 §4.5, slots renumbered.**

- **HL:**
  - A third gateway, for slot 10, which needs an `exec_boot` law line.
  - **The per-IP ledger:** slot 3 frozen at 400 / 1 000; slot 10 at 100 / 200, taken from slot 6's share and the candles lane. The algos are throttled inside that share. The sum must be proven against the venue's limit before T7a (R-T17).
- **Binance:**
  - Routing keyed by (venue, account), plus a second `BnGateway`.
  - **A BX-7 amendment** (R-T14).
  - USDⓈ-M makers; IoC everywhere else (R-T4).
- **Polymarket:**
  1. SELL FAK.
  2. GTC/GTD, cancel and heartbeat.
  3. Amend the slot-alone law.
- **Hypercall:**
  - The options E6 row.
  - Add Hypercall to `LIVE_ARM_VENUES` (and invert its test).
  - E7, or an unbounded ruling.
  - Long-only first. This is shared with slot 7's ruling.

### 4.6 The terminal server: `crates/terminal` → `multivenue-terminal`

**Unchanged from v1.1, with three differences:**

1. **It serves the embedded UI bundle** under the `web-bundle` feature:
   - `index.html`, `loader.js`, `term_ui.js` (generated) and `term_ui_bg.wasm`, embedded by `build.rs` from the git-ignored `terminal-ui/dist/`
   - the CSP and headers from §3

   Without the feature it serves a stub page, so every gate builds on any checkout.
2. **Every frame is a `term-proto` type** (§4.11.2). The server encodes into its final tx buffer in place.
3. **It owns all computation** (O-T13): deep books, bars at every timeframe, footprint cells, the volume-at-price arrays, CVD, OFI, imbalance, microprice, the queue estimate, built-in indicators and Pine outputs. The UI never derives market state on its own.

**Per-client output**
- **Coalescing:** books at 20 Hz or less, trades every 50 ms, heatmap columns every 100 ms.
- **Backpressure:** each client has a bounded tx buffer.
  - A slow client is moved to **snapshot mode**: intermediate deltas are dropped, and it gets the latest snapshot per subscription when it drains.
  - So is a client that has **reported** `Hidden`, `Frozen` or `Stale` (§4.11.3).
  - The server cannot infer visibility by itself, because a hidden tab still drains its socket.
- **UI liveness:**
  - The server sends a `Heartbeat` frame every 1 s.
  - The WASM answers with an `Echo` from inside its `onmessage` handler. That works in a hidden tab and stops in a frozen one.
  - Three missed echoes, or a socket close, is **UI lost** (R-T15). The server then sends `UiLost` through the door, and the member stops its algos and cancels resting orders.
  - WebSocket ping/pong is **not** used for this, because the browser answers it without the page.

**Replay mode (T1a)**
- `multivenue-terminal --replay <spool-dir> --no-door` serves recorded market data with order entry disabled.
- **The recorder ships in T1a**, not with the heatmap spool in T2. It writes every outgoing md frame to a spool file whose header carries the `term-proto` version. A replay of another version is refused.
- **`--synthetic <seed>`** is a deterministic synthetic-market generator, written as code, not data, so nothing needs to be in git. It produces books, trades, bars and liquidations for a few named instruments, plus **stub `EngineStatus` and candle history**.
- With these, the UI work and the cloud browser smoke tests need neither the network, the Mac's spool, the engine, `/state` nor the candles store.

### 4.7 Market-data plane

**Unchanged from v1.1 §4.7.**

| venue | streams |
|---|---|
| Binance | `@depth@100ms` + `@aggTrade` (and `@forceOrder` on futures), with REST snapshot and U/u sync |
| HL | `l2Book` at 20 levels plus a coarser `nSigFigs` book, and `trades` |
| Polymarket | `book` / `price_change` / `last_trade_price` / `tick_size_change` |
| Hypercall | provider quotes and trades |

**Budgets**
- **HL:** 1 socket, 40 subscriptions or fewer, 40 weight a minute or less.
- **Binance:** 4 connections or fewer, at most 1 attempt every 10 s. **No attempts while `/state` is dark or within 5 minutes of a boot.**
- On the first 429 or 418, that venue's feed stops for the session.

**Testing:** every new scanner gets a proptest and a fuzz target.

### 4.8 History and backfill

**Unchanged from v1.1 §4.8.**

- **Bars:** the candles store, read-only, then REST klines within budget, then live bars.
- **Footprint, VP and CVD backfill:** 24 h of `ChannelId::Trade` PMLR rows.
- **Heatmap:** spooled for 24 h.
- **SQLite:** `rusqlite`.

### 4.9 Microstructure: the maths (server)

**Unchanged from v1.1 §4.9.**

- **Queue estimate:**
  - At placement, the queue ahead is the displayed size.
  - Trades at our price reduce it.
  - Size decreases at the level reduce it pro rata.
  - This is the law `core_fill::queue` uses.
- **OFI:** Cont–Kukanov–Stoikov, at level 1 and over the top K.
- **Imbalance:** (B − A)/(B + A).
- **Microprice:** (P_a·Q_b + P_b·Q_a)/(Q_a + Q_b).
- **Footprint:** bid × ask volume and delta per price bucket. Diagonal imbalances of 3:1 or more are flagged.
- **VP:** POC and VAH/VAL at 70 %.

**Rendering of all of these lives in §4.11.**

### 4.10 Indicators and overlays (server)

- **Built in:**
  - EMA and SMA
  - VWAP ± 1σ and 2σ
  - Bollinger (20, 2)
  - RSI (14, Wilder)
  - ATR (14)
- **Engine overlays:**
  - every slot's orders and fills, read from the tailed `engine-*.pmlr`
  - HAR, regime and funding, from `/api/worker` and `/state`
- **Pine via pine-vm:** run on the server, over its own bars. Blocked until PVM lands on main. No TradingView traffic.

---

### 4.11 The frontend: Rust → WASM (new in v2)

#### 4.11.1 Workspace and toolchain (R-T18)

The UI lives in a **separate cargo workspace**, `terminal-ui/`, inside the repo. The main workspace excludes it.

| path | what |
|---|---|
| `terminal-ui/rust-toolchain.toml` | channel **1.88.0** (the repo's pin) plus target **`wasm32-unknown-unknown`**. The root toolchain file is untouched. |
| `terminal-ui/Cargo.toml` | `resolver = "3"` with `[workspace.package] rust-version = "1.88"`, so cargo picks dependency versions compatible with it. Its own `Cargo.lock` and its own `deny.toml`. |
| `terminal-ui/crates/term-ui-core` | platform-free: app state, protocol client, layout, chart engine, ladder, ticket, blotter, hotkeys |
| `terminal-ui/crates/term-render` | wgpu pipelines and WGSL shaders (§4.11.4) |
| `terminal-ui/crates/term-ui-web` | the only crate that touches `web-sys`: canvas start, WebSocket, focus and visibility, clipboard, `fetch` for login. `#[wasm_bindgen] start(canvas)`. `crate-type = ["cdylib", "rlib"]`. **eframe, wasm-bindgen, web-sys, js-sys and wasm-bindgen-futures sit under `[target.'cfg(target_arch = "wasm32")'.dependencies]`**, so a host build never compiles native eframe or winit (on a Linux host with default features off, winit has no x11 or wayland and refuses to compile). |
| *later:* `terminal-ui/crates/term-ui-native` | eframe native and a WebSocket client (O-T12) |
| `crates/term-proto` (main workspace) | the shared crate: `#![no_std]`, **zero dependencies**, `#[repr(C)]` frame layouts, encode and decode in place. The server depends on it; the UI path-depends on it. It must build on both toolchains. |

**Why a separate workspace.** egui, eframe and wgpu bring 118 crates for wasm32 and 335 `Cargo.lock` entries (measured). In the main workspace they would:

- enter the engine's `Cargo.lock`
- enter the engine's cargo-deny graph
- slow `cargo build --workspace`

The engine stays exactly as it is. The UI workspace gets its own gate targets (§4.11.8).

**Pins**
- **eframe / egui / egui-wgpu 0.33.3** with **wgpu 27**: the newest whose MSRV is at most 1.88. Moving to egui 0.37 means a repo-wide toolchain bump to 1.95, which is the operator's decision.
- **wasm-bindgen `=0.2.128`** (with wasm-bindgen-futures 0.4.78), the newest pair whose graph has no `tokio` (§2.7).
- **The `wasm-bindgen-cli` binary must equal the crate's version.** `scripts/terminal-ui-build.sh` reads the version from `terminal-ui/Cargo.lock` and installs the CLI with `cargo install --locked wasm-bindgen-cli --version =<it>`.

**Features**

| setting | why |
|---|---|
| default features off | start from nothing |
| `wgpu` with both the `webgpu` and `webgl` backends | WebGPU where available, WebGL2 fallback |
| `persistence` off | it pulls in `ron` and `serde`; layouts are stored on the server |
| `accesskit` off | not needed in the shipped build. `egui_kittest` turns it on in test builds; that is harmless, but the custom-painted ladder has no AccessKit nodes, so ladder tests drive raw pointer events |
| `default_fonts` off | replaced by our own fonts (R-T19) |

**The ban check (T0, then a gate):**
- **grep `terminal-ui/Cargo.lock`** for every banned name (`tokio`, `serde_json`, `reqwest`, `async-std`, `ethers`, `alloy`, `openssl`), expecting zero.
- `cargo deny check bans`, with the UI `deny.toml` carrying the root's `deny` list and `[graph] targets = ["wasm32-unknown-unknown", "aarch64-apple-darwin", "x86_64-unknown-linux-gnu"]`.
- `cargo tree -i` alone is not enough: it missed the target-gated `tokio`.
- The host tools (`wasm-bindgen-cli`, `wasm-opt`) are outside the graph (R-T18).

**Build** (`scripts/terminal-ui-build.sh`):
1. `cargo build -p term-ui-web --target wasm32-unknown-unknown --release`
2. `wasm-bindgen target/wasm32-unknown-unknown/release/term_ui_web.wasm --target web --no-typescript --out-name term_ui --out-dir terminal-ui/dist`, giving `term_ui.js` and `term_ui_bg.wasm`. (No `snippets/` directory is produced; checked.)
3. Optionally `wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-reference-types --enable-multivalue --enable-sign-ext --enable-mutable-globals`. These are the features Rust 1.88 turns on for wasm32. `wasm-opt` is a build tool only.
4. Copy `web/index.html` and `web/loader.js` into `dist/`, then write `dist/MANIFEST` (SHA-256 per file).

The terminal's `build.rs` embeds `dist/` under `web-bundle` and refuses when a hash does not match `MANIFEST`. `dist/` is git-ignored.

**Detecting a hand edit:** the manifest cannot, because the same script writes it. **The gate regenerates the glue from a clean build and compares `term_ui.js` byte for byte** against what is embedded.

**Release profile:** `opt-level = "s"`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`. A minimal eframe + wgpu app measured **4.46 MB before `wasm-opt`**; it loads once from localhost.

#### 4.11.2 `term-proto`: the shared wire

**Frame header**

| field | size | notes |
|---|---|---|
| `kind` | u8 | |
| `ver` | u8 | |
| `len` | u32 | the body length; a frame is still capped at 64 KiB (the WS slot) |
| `seq` | u32 | per subscription: gap ⇒ resync |
| `ts_ns` | u64 | |

Then a fixed-layout body, little-endian.

**Chunking.** Large payloads go as chunked kinds carrying `(offset, count, last)`:
- `BookSnap` with many levels;
- a 24 h `VapArray`, for example 30k buckets × 16 B ≈ 480 KB at a 0.1 tick;
- a busy bar's `FootprintCells`.

The client assembles them into its preallocated arrays and applies the result only on `last`.

**Frame kinds**

| kind | contents |
|---|---|
| `BookSnap` | K ≤ 512 levels per side, i64 ×1e6 |
| `BookDelta` | |
| `TradeBatch` | ≤ 256 prints |
| `Bar` | per timeframe, plus volume and delta |
| `FootprintCells` | per bar: bucket, bid vol, ask vol |
| `VapArray` | |
| `FlowPoint` | CVD, OFI, imbalance, microprice |
| `HeatCol` | u8 log-quantized intensities over a price window, plus the window's base tick and scale |
| `IndicatorPoint` | |
| `Marker` | |
| `OrderView` / `PositionView` / `FillView` | the TermEvt mirror |
| `EngineStatus` | PAPER/LIVE per slot, from `/state` |
| `Intent` | UI → server: place, modify, cancel, flatten, algo, halt — carried as ×1e6 integers |
| `Control` | subscribe; trading-enable; confirm-nonce; **visibility: `Hidden`, `Visible`, `Frozen`, `Resumed`, `Stale`** |
| `Heartbeat` / `Echo` | server → UI every 1 s; UI → server from `onmessage` (§4.6 UI liveness) |
| `Layout` / `Drawing` / `KeyMap` | typed binary records: panel tree, per-instrument drawings as typed points, hotkey bindings. Saved on the server |
| `LiveConfirm` | the typed live confirmation: a fixed-length ASCII field compared on the server |

- **No free-form text is ever interpreted.** Every payload is a typed binary record. The only strings are bounded, length-prefixed ASCII fields (instrument labels, layout names, the confirmation phrase), and the server checks each against an allow-list or an exact value.
- **Decimal entry is parsed inside the WASM**, using the same `term-proto::dec` routine the server uses: more than 6 decimals is refused, never rounded. The server parses and checks again.
- **Decoding is zero-copy:** a frame is a borrowed view over its bytes, with alignment-safe accessors and no `transmute` of unaligned data.
- **Testing:** fuzz targets `term_proto_frame`, `term_proto_intent` and `term_proto_dec`; a round-trip proptest for every kind; a chunk-reassembly proptest.

#### 4.11.3 The browser boundary

**WebSocket**
- `web-sys::WebSocket` with `binaryType = arraybuffer`.
- `onmessage` copies the `ArrayBuffer` into a slot of a preallocated ring in WASM memory with `Uint8Array::copy_to`.
- That is **the one copy across the JS↔WASM boundary**, documented `// COPY: ws payload ≤ 64 KiB — JS heap → linear memory — unavoidable on the web; a JS-side decoder rejected (O-T9)`.
- Frames are decoded in place from the slot.

**Paint loop**
- The app drains at most N frames per paint, bounded.
- The repaint policy is 30 Hz while streaming idle and 60 Hz during interaction. egui does not repaint without input or `request_repaint_after`.

**Hidden and frozen tabs**
- **Hidden.** The browser pauses `requestAnimationFrame` and slows chained timers to about once a minute. The `onmessage` handlers still run, so the ring keeps filling while nothing paints.
- **Frozen.** Chrome 133+ Energy Saver freezes a tab hidden for over 5 minutes that is using a lot of CPU, and its network handlers stop.
- **What the client reports.** It listens for `visibilitychange`, `freeze` and `resume`, and sends `Control::Hidden` / `Visible` / `Frozen` / `Resumed`. When its ring overflows it sends `Stale`. The server moves any client that is not visible to snapshot mode and stops sending deltas until it reports `Visible` or `Resumed`.
- **UI lost** is defined in §4.6: three missed `Echo`es or a socket close. A frozen tab stops echoing, so its algos stop and its resting orders are cancelled (R-T15). A tab that is only hidden keeps echoing and loses nothing.
- No rAF- or timer-driven heartbeat exists anywhere, so throttling cannot fake a UI loss.
- **Order state is never at risk from this:** the member and the server own it (T-L13).

**Keyboard and focus**
- egui takes keys from the canvas, with its hidden text agent for text fields.
- Browser-reserved shortcuts (⌘W/T/N/Q/L) cannot be captured, so TERM hotkeys use **⌃⇧ chords on macOS** and plain keys only while the ladder has focus.

**Threads**
- Single-threaded: egui on the main thread.
- No SharedArrayBuffer, so no COOP/COEP isolation is needed.
- The server does the heavy work (O-T13).

#### 4.11.4 The renderer: `term-render`

The wgpu pipelines run inside egui through `egui_wgpu::CallbackTrait` paint callbacks. Every buffer is allocated at view creation and updated in place each frame with `queue.write_buffer` / `write_texture`, sized to the view's maximum. No GPU allocation happens per frame.

**Pipelines**

| # | pipeline | draws | how |
|---|---|---|---|
| 1 | instanced rects | candle bodies and wicks, OHLC bars, histograms (volume, delta, VP), footprint cell backgrounds, ladder bars | one instance buffer |
| 2 | polylines | line and area series, indicator bands, the CVD/OFI panes | **instanced segments**: the point vertex buffer is bound 4 times at offsets i−1, i, i+1, i+2, with an instance step, and the vertex shader builds the miter or bevel join from those neighbours. **No storage buffer is used.** Fallback: points in an `R32Float` data texture read with `textureLoad` |
| 3 | SDF circles and rings | trade bubbles and liquidation rings | area ∝ size, colour by aggressor |
| 4 | heatmap | a **texture ring** | below |
| — | text (egui's painter, **not** a `term-render` pipeline) | prices, axes, footprint numbers, ladder | epaint glyph atlas and galleys, our fonts. Its cost with thousands of labels in WASM is measured by the T0 spike; labels are cached per value and culled to the view |

**The heatmap texture ring**
- `R8Unorm`, filterable on both WebGPU and WebGL2.
- Columns × price rows, for example 3 000 × 1 024 = 3 MB. One `write_texture` per new column; the read offset wraps.
- Zoomed out, the server down-samples by max.
- A colour LUT is a 256×1 texture.
- A per-view contrast/gamma uniform tunes the picture without re-sending data.
- **Each column has its own base tick**, because `HeatCol` windows move with the market. The bases live in a second 1-D texture, `R32Sint` (3 000 × 1), holding each column's base relative to the view's origin, written alongside the column. The fragment shader reads it with `textureLoad` and offsets the row lookup.

**Precision**
- **The GPU never sees an absolute price or time.** The values themselves do not fit an f32: ×1e6 prices reach about 1e11 and timestamps are ns since the epoch, far past the 24-bit mantissa.
- **What a view keeps on the CPU:** its origin as **i64 ticks plus an f32 fraction of a tick**, and an origin bar index.
- **What the vertices carry:** offsets in **whole ticks and whole bars**. The fraction is a uniform.
- **Re-basing:** when the view pans past 2¹⁶ ticks, so offsets stay exact and sub-tick panning does not jitter.
- Axis labels are formatted from the i64 values with integer arithmetic. No f64 `Display` is used for a price.

**Backends**
- WebGPU first, WebGL2 fallback.
- The pipelines use only the WebGL2-compatible subset of wgpu (`Limits::downlevel_webgl2_defaults()`, asserted at device creation):
  - **no storage buffers** (WebGL2 has none);
  - no storage textures and no compute;
  - uniform buffers of 16 KiB or less;
  - **a vertex stride of 255 B or less**;
  - textures of 8 192 or less per side.
- `R8Unorm` is filterable on both backends.
- The T0 spike runs every pipeline on WebGL2 and on WebGPU before T1b builds on them.

#### 4.11.5 The chart engine (`term-ui-core::chart`)

| area | v1 scope |
|---|---|
| **time scale** | bar-index x axis (no gaps for closed sessions, as TradingView does); right offset; wheel and pinch zoom anchored at the cursor; drag to pan; double-click to reset; visible range kept per pane group |
| **price scale** | auto-fit to the visible range with a margin; manual drag-scale; optional log scale; tick-size-aware labels; a last-price label; labels for our orders and positions on the axis |
| **time labels** | calendar-aware steps (s / m / h / D / W / M); UTC or a fixed offset (Asia/Bangkok +7 has no DST) |
| **panes** | stacked panes sharing the time scale, resizable separators; the main pane plus CVD, OFI, imbalance, RSI and so on |
| **series** | candles, OHLC bars, line, area, histogram, baseline; markers (per-slot fills, Pine shapes); price lines (our working orders, position average, liquidation) |
| **custom series** | **footprint**: cells per bar from `FootprintCells`, numbers drawn when zoomed in, a delta bar when zoomed out. **VP**: a pane primitive on the right edge, visible-range or session; POC, VAH and VAL lines |
| **crosshair** | snaps to OHLC; a legend with OHLC, volume and indicator values; a tooltip on markers |
| **drawing tools** | horizontal line, ray, trend line, rectangle, measure. Kept per instrument on the server. Hit-testing, handles, snapping, delete |
| **order lines** | **dragging a working-order line sends a MODIFY intent**. While the drag is in progress the original line stays until the server confirms. On a live slot the drop opens a confirm when the size is at least `confirm_usd`; the server issues the nonce |
| **heatmap mode** | the main pane switches to the heatmap with bubbles on the same time and price scales; candles can be overlaid |

**Testing:** host-target unit tests and proptests for scales, tick generation, label formatting, re-basing, hit-testing and drawing geometry. No GPU is needed.

#### 4.11.6 The app (`term-ui-core::app`)

**Layout**
- A left watchlist: allow-listed instruments plus chart-only symbols. PAPER/LIVE per slot is read from `EngineStatus`, never from config.
- The chart in the centre.
- The ladder and ticket on the right.
- Bottom tabs: Orders, Positions, Fills, Algos, Tape, Engine events.
- egui panels, resizable. A layout preset is saved as a typed `Layout` record on the server (`~/multivenue/terminal/layouts/`).

**Ladder**
- An egui custom widget over `term-render`'s rects and text.
- Fixed rows at the tick or an aggregation step.
- Columns: our bids · bid size · price · ask size · our asks · session volume at price · flash of the last prints · the queue estimate under our order.
- Mouse: left-click sends a limit; shift-click an IoC; drag a MODIFY; right-click a cancel.
- An **IoC-only** badge on products that cannot rest.
- Auto-centre, with a toggle.

**Ticket**
- Venue, slot, instrument, side, kind (limit or IoC), price, quantity (contracts or USD notional), reduce-only, post-only.
- An estimated fee, labelled as an estimate.
- The decimal parse uses `term-proto::dec`.
- Quantisation to tick and lot is conservative: buy prices down, sell prices up, quantity down. The server and the arm stay authoritative.

**Algos panel:** TWAP, chase and iceberg parameters, progress, and cancel.

**Safety**
- Trading is off at login.
- A live slot needs a typed confirmation (`LiveConfirm`), checked on the server. It is a UX step against slips, not authentication (§3).
- Every gate is held on the server (T-L13).
- When the engine is down: a red banner and order entry disabled.

**Hotkeys** (`keys.toml` on the server):

| key | action |
|---|---|
| 1–5 | size |
| B / S | IoC at the touch ± slip |
| Esc | cancel all on the instrument |
| ⌃⇧F | flatten the instrument |
| ⌃⇧K | KILL this slot |

The server rate-limits all of them.

**Testing:** `egui_kittest` 0.33.3 drives widgets headless on the host: the ladder's click → intent, the ticket's validation, hotkeys, the drag-to-modify state machine. The ladder is custom-painted and has no AccessKit nodes, so **its tests send raw pointer events at computed row coordinates**. Text widgets are found by label.

#### 4.11.7 Native later (O-T12)

- **What stays out of `term-ui-core` and `term-render`:** they never name `web-sys` or `js-sys`.
- **The platform seam:** `term-ui-web` provides a `Platform` implementation, the socket, the clock, the clipboard and visibility. It is generic and monomorphized, with no `dyn`.
- **What a later `term-ui-native` adds:** eframe native, the same protocol over a WebSocket to 127.0.0.1:9393, and a token read from its own 0600 file.
- **What it removes:** the browser attack surface (cookies, Origin, extensions).
- **What it needs:** a ruling, R-T20, which is not in v1's scope.

#### 4.11.8 Gates for the UI workspace

| gate | how |
|---|---|
| lint | Host: `cargo clippy --workspace --all-targets -D warnings`. Wasm: `cargo clippy --workspace --lib --target wasm32-unknown-unknown -D warnings` (**`--lib` only**, because dev-deps do not build for wasm32: proptest pulls getrandom 0.4, which refuses `wasm32-unknown-unknown`). proptest and kittest sit under `[target.'cfg(not(target_arch = "wasm32"))'.dev-dependencies]`. **Both reproduced in the mirror workspace.** |
| tests | `cargo nextest run` on the host: the chart engine, the app logic, kittest and the `term-proto` proptests. `term-ui-web` has no host tests; its eframe dependencies are wasm-only |
| glue | the clean-build regeneration of `term_ui.js` and its byte-for-byte compare (§4.11.1) |
| bans | the `Cargo.lock` grep and `cargo deny check bans` (§4.11.1) |
| fuzz | `term_proto_frame`, `term_proto_intent`, `term_proto_dec` (in the main workspace's `fuzz/`, because `term-proto` lives there) |
| licences | `cargo deny check licenses` and `cargo about` on the UI workspace, both with `[graph] targets` set. The output merges into `THIRD-PARTY-NOTICES.md`, because the wasm is embedded in the shipped terminal binary. `make license-check` asserts the merge. The fonts follow R-T19 |
| SPDX | every `.rs`, `.sh`, `.wgsl` and `loader.js` starts with the two-line SPDX record; `index.html` carries it as an HTML comment |
| research guard | unchanged: nothing under `terminal-ui/` is research |
| browser smoke | The server in **`--synthetic <seed>`** mode, deterministic, with stub engine status and candles, so it runs in the cloud container. Headless Chromium (Playwright, preinstalled) is launched with **`--enable-unsafe-swiftshader`**, because software WebGL is otherwise a deprecated fallback. The run: load, log in, subscribe, render N frames, screenshot the chart, ladder and heatmap for visual review; check that the console has no CSP violation and that `Hidden`/`Visible` reporting works. Headless covers the **WebGL2** path only. **WebGPU and Safari 26 are checked on the Mac by the operator**, or in the desktop app's built-in browser at 127.0.0.1:9393, with the server in `--replay` over the Mac's spool. Not a gate the cloud can pass alone |
| **not applicable** | `alloc-assert` and `copy-audit` bind the engine's crates. The UI is a cold process; buffers are preallocated anyway (§4.11.4). The one boundary copy carries its `// COPY:` |

All of these are wired into `make terminal-ui-gates`. The engine's own gates are unchanged.

---

## 5. Laws for the lane (T-L1 … T-L14)

**T-L1 — the terminal is not an engine.**
- It holds no venue key.
- It opens no order socket.
- It never sources `.env`.

**T-L2 — the TERM key belongs to the operator.**
- It lives in `term.env` (0600).
- Only the engine wrapper and the terminal read it.
- No worker, script or session reads it.
- **Claude never sends a frame to the live door.** Automated tests use an in-process door with a test key generated per run (§6, T5/T6).

**T-L3 — the browser holds no secret.**
- A one-time token, then a cookie.
- An exact Host/Origin match; a null Origin is refused.
- The CSP from §3.
- The server enforces every gate.

**T-L4 — reducing commands are never blocked.**
- Growing commands pass the pre-gate and E6.
- E-1, E-5, E-7 (with the sequential fallback), E-8 and E-9 hold.
- Fees are measured.

**T-L5 — TERM's market data never feeds the engine.**

**T-L6 — per-IP budgets are hard caps, and TERM yields first.**
- No Binance attempts while `/state` is dark or within 5 minutes of a boot.
- A venue's feed stops on its first 429.

**T-L7 — no TERM order outlives its supervisor.**
- `max_rest_s`, the venue dead-man and the watchdog enforce this.
- A sweep runs before every restart (`daily-restart.sh`, with the operator's word).

**T-L8 — prices are ×1e6 integers from the keystroke to the venue.**
- More than 6 decimals is refused.
- Quantisation is conservative.
- An instrument below 1e-6 is refused.
- The GPU sees only whole-tick and whole-bar f32 offsets from an i64 origin (§4.11.4).

**T-L9 — HALT is slot-scoped.** It is cleared by removing its own `exec.HALT` line and restarting.

**T-L10 — every engine-side phase ships under the §6.1 checklist.**

**T-L11 — identities are checked at every boundary.**
- Descriptors.
- `boot_epoch`.
- A manifest reload on each engine boot before order entry re-enables.

**T-L12 — repo laws.**
- SPDX headers (`.wgsl` included).
- `license.workspace = true` in the main workspace; an equivalent `[workspace.package] license` in `terminal-ui/`.
- No research in git.
- Gates are counted in fills, ticks and windows of 2 h or less.

**T-L13 — the WASM is untrusted UI (new).**
- It decides nothing that matters. Every check that guards money runs on the server, then in the member, then in E6.
- A modified or stale bundle, a browser extension or any page automation **can submit only what passes the server's gates**. It can submit valid intents, so the gates and caps are the protection, not the UI.
- The typed live confirmation is a UX step, not authentication.
- The terminal runs in a dedicated browser profile with no extensions.
- The UI's own validation exists for feedback only.

**T-L14 — no JS except the generated glue and the loader (new).**
- The browser runs only the `wasm-bindgen`-generated glue (regenerated each build, never edited, compared byte for byte by a gate) and `loader.js`, the one hand-written file (15 lines or fewer, SPDX header, hash-checked).
- No JS dependency, no npm, no CDN.

---

## 6. Phases

Sizes: S ≈ 1 session, M ≈ 2–3, L ≈ 4+.

| phase | deliverable | restart | blocked on | size |
|---|---|---|---|---|
| **T0** | Rulings R-T1 … R-T20. The per-IP ledger, measured. The `terminal-ui/` skeleton: toolchain, `deny.toml` with `[graph] targets`, the feature set, the cfg-gated dependencies, the wasm-bindgen pin. The ban check (§4.11.1). **The render spike:** one pipeline of each kind (rects, instanced polyline, SDF circles, heatmap ring plus base texture) inside `CallbackTrait`, and **a timed frame with 5 000 egui labels**, run on WebGL2 (Chromium) and on WebGPU and Safari 26 (the Mac, the operator present). It is the go/no-go for the egui text and render path. Draft example files and wire-format. | no | — | M |
| **T1a** | `term-proto` (fuzz, proptests, chunking). The server: auth, login, CSP and headers, bundle serving with the glue check, **the md recorder, `--replay`, `--synthetic`** with stubs, the candles store and REST history, live bars, books for HL perps and USDⓈ-M, the `state` thread. The cloud browser smoke harness. | no | T0, R-T8, R-T18 | L |
| **T1b** | **The UI core:** the web shell, the protocol client with visibility reporting and `Echo`, the chart engine core (time and price scales, candles, volume, crosshair, zoom and pan, panes, re-basing), the read-only ladder, the tape, engine status. The operator reviews on the Mac. | no | T1a | L |
| **T2** | Microstructure on all four venues. Server: deep books, footprint, VaP (chunked), flow metrics, heatmap columns, the 24 h backfill, the new scanners with proptests and fuzz. UI: the heatmap texture ring, base texture and LUT, bubbles, the footprint custom series, the VP primitive, the flow panes. | no | T1b | L+ |
| **T3** | Overlays and tools: the built-in indicators (server) and their series (UI); every slot's markers; HAR, regime and funding; **drawing tools** with server-side storage. | no | T1b | M+ |
| **T4** | **Engine side, paper.** `strategy-term` on slots 10–13, `ingress-term`, the PODs, the PMLR kind everywhere, the `/state` `term` block, risk-policy, `term.env`. Gates: alloc, copy-audit, fuzz, proptest, latency, the slot-3 proof, a risk review. | **yes** | R-T1, R-T5, R-T6, R-T11, R-T13 | L |
| **T5** | Order entry, paper: the ticket, ladder click-to-trade, order lines with drag-to-MODIFY, hotkeys, the blotter, KILL and FLATTEN, confirmations, the watchdog (`Echo`, `UiLost`), kittest coverage. **Exit:** an automated harness drives ≥ 50 paper round trips over ≥ 3 venue classes, every reject reason, 5 watchdog firings (socket close, missed echoes, a simulated freeze) and 0 desyncs. It uses an in-process engine, the paper matcher and **a test-key door**, never the live door. The operator then makes ≥ 10 round trips through the real UI. | no | T4 | M+ |
| **T6** | Algos, paper. **Exit:** the harness makes 10 runs of each algo in each of four endings (completed, cancelled, halted, UI lost), 120 in all, with every child inside its caps. The operator then runs each algo once through the real UI. | yes | T5 | M |
| **T7a** | **HL live** (`term-hl`, slot 10): a dedicated account, the third gateway, the ledger proof, `[exec.slot.10]`, `--arm-live 3,10`, `term-flip.sh`. The dust battery, run by the operator. | yes | T6, R-T2, R-T5, R-T17 | M |
| **T7b** | **Binance live** (`term-bn`, slot 11): the BX-7 amendment or the sole-slot ruling, account-keyed routing, the sub-account; USDⓈ-M first, then IoC on spot, COIN-M and options. | yes | T7a, R-T4, R-T14 | M–L |
| **T7c** | **Polymarket** (`term-pm`, slot 12): extend the arm, paper first, then live dust. | yes | T5, a PM account | L |
| **T7d** | **Hypercall** (`term-hc`, slot 13): the dedicated wallet, `LIVE_ARM_VENUES`, the options E6 row, E7 or the unbounded ruling; long-only dust. | yes | slot 7's ruling | M |
| **T8** | Pine overlays. | no | PVM on main | M |
| **T9** | Resting orders on spot, COIN-M and options behind an external watchdog (only if R-T4 asks for it). | no | T7b | M |
| **T10** | *Later:* `term-ui-native`. | no | R-T20 | M |

### 6.1 The restart checklist (T-L10)

Slot 3 is LIVE.

1. **G0:** `cargo build --release -p cli`, plus `stat`, against the last `crates/cli` change.
2. **Gates:** the full set green.
3. **The slot-3 proof:** replay one window of 2 h or less, with and without the TERM slots. Slot 3's route and its `engine-orders.pmlr` must be **byte-identical**, and p99 must be within noise.
4. **Smoke:** `exec-smoke.sh`.
5. **Review:** the risk reviewer (Opus, xhigh) on the diff.
6. **Restart** at the operator's safe minute, off the 15-minute grid. Then check:
   - `vm_rows_active` ≥ 1
   - slot 3's `exec` unchanged
   - `term` present
7. **Rollback:** keep the previous binary and wrapper. The new PMLR kind must be readable by the old binary.

UI-only changes (T1a–T3, T5's UI half) **never** need an engine restart. They need only a terminal restart and a browser reload.

---

## 7. Rulings needed (R-T1 … R-T20)

| id | question | recommendation |
|---|---|---|
| **R-T1** | TERM slots | **One per venue on 10–13:** 10 `term-hl`, 11 `term-bn`, 12 `term-pm`, 13 `term-hc`. Slot 8 is pine's on the PVM branch; 14–15 stay reserved. |
| **R-T2** | Accounts | Per §4.4. |
| ~~R-T3~~ | ~~LWC vendoring~~ | **Withdrawn by O-T11.** There is no third-party JS. |
| **R-T4** | Resting orders where the venue has no dead-man | **IoC-only in v1.** |
| **R-T5** | Live caps | The operator's numbers, above the venue minimums (§4.3). |
| **R-T6** | HALT | Slot-scoped. In paper, a latch. In live, an `exec.HALT` line that survives the daily restart. |
| **R-T7** | Algos across a restart | Not resumed. |
| **R-T8** | Port, authentication and budgets | 127.0.0.1:9393, §3 authentication, budgets after T0. |
| **R-T9** | The allow-list | Keyed by descriptor, a subset of the universe. An unresolved entry is dropped and reported. |
| **R-T10** | Polymarket | SELL FAK first, then GTC/GTD plus heartbeat. Neg-risk markets out. |
| **R-T11** | Stage-3 gate | **Waived for TERM**, as O-E1 did for BIN15, while T-L2 holds. |
| **R-T12** | Where the plan lives | `docs/terminal-plan.md` (untracked) plus the project copy. |
| **R-T13** | The TERM key | `~/multivenue/term.env`, 0600, following the `s3.env` precedent. |
| **R-T14** | BX-7 | Amend it to ownership per account, in T7b. |
| **R-T15** | UI lost | `cancel`. |
| **R-T16** | Exits on a halted slot | Halt stays cancel-only. |
| **R-T17** | HL ledger | Slot 3 frozen at 400 / 1 000, slot 10 at 100 / 200, market data at 40 weight. The rest comes from slot 6 and candles. |
| **R-T18** *(new)* | UI workspace, toolchain and build tools | **A separate `terminal-ui/` workspace on the repo's 1.88.0 plus `wasm32-unknown-unknown`, with:**<br>• eframe/egui 0.33.3, wgpu 27, **wasm-bindgen `=0.2.128`**<br>• `resolver = "3"`<br>• cfg-gated dependencies, and `[graph] targets` in its `deny.toml` and `about.toml`<br><br>**Build tools.** The `wasm-bindgen-cli` binary (its own dependencies include serde_json) and `wasm-opt` count as **host build tools outside the shipped graph and outside the ban**, like `cargo-deny`. Confirm that the forbidden-crates hook and the bans apply to crates we compile and ship, not to installed tools.<br><br>Moving to egui 0.37 (Rust 1.95) is a later repo-wide toolchain decision. |
| **R-T19** *(new)* | Fonts | Two options; both are the operator's call.<br><br>**(a) Recommended: keep the fonts in crates.io crates.** Use egui's `default_fonts` (`epaint_default_fonts`), or another font crate from crates.io. Add a crate-scoped exception to the UI `deny.toml`:<br>`{ crate = "epaint_default_fonts", allow = ["OFL-1.1", "Ubuntu-font-1.0"] }`<br>Nothing third-party is then committed, and cargo-deny and cargo-about see the licences. The repo's `deny.toml` is untouched.<br><br>**(b) Our own fonts** (for example JetBrains Mono and Inter, OFL-1.1) committed under `terminal-ui/assets/fonts/`. That **is vendoring** under the repo law, so it needs an explicit ruling that font files are assets exempt from it. The embedding crate then declares `license = "Apache-2.0 AND OFL-1.1"`. cargo-deny never reads `include_bytes!` assets, so the crate field is what carries the licence. |
| **R-T20** *(new, later)* | Native app | Not in v1. Open it when the web build is live (T10). |

---

## 8. Risks and residuals

- **The HL per-IP ledger** is the tightest constraint. R-T17 and T0 gate it.
- **The terminal's view is not the engine's view.** A band refusal carries both prices.
- **Book depth is uneven:** HL gives 20 levels, Hypercall has no L2, and Binance options are partial.
- **Binance options on a sub-account** are unverified.
- **BX-7 and the Polymarket arm** are money-path work, size L, with BX-style review rounds.
- **New for v2:**
  - **egui pinned a year back.** 0.33.3 is from 2025-12 because of the 1.88 pin. Fixes after it are out of reach until the toolchain bump.
  - **Two renderer backends.** A visual bug can be WebGPU-only or WebGL2-only. Headless CI covers WebGL2, through SwiftShader, which is deprecated and needs a flag. The operator's Mac covers WebGPU and Safari.
  - **WebGL2 has no storage buffers**, so the polyline and heatmap designs avoid them (§4.11.4). The T0 spike proves this on both backends.
  - **Text cost and quality.** epaint galleys for thousands of ladder and footprint labels per frame in WASM are the main performance unknown; the T0 spike times it. egui's glyph atlas is not browser-native subpixel text, so it needs the operator's eye in T1b–T2. A font-size setting ships from T1b.
  - **Our own chart engine.** It is about 8–12k lines that Lightweight Charts would have given us: axes, zoom feel, label collisions, drawing-tool UX. This is the largest UI risk and needs the most operator review.
  - **Bundle size:** 4.46 MB before `wasm-opt` for a minimal app (measured). The full app is re-measured in T1b; that is fine on localhost.
  - **Hidden and frozen tabs** (Chrome's Energy Saver):
    - handled by visibility reporting, snapshot mode and the `Echo` watchdog;
    - a frozen tab is UI lost (resting orders cancelled);
    - order state itself is never touched.
- **The same-user residual** (§3).
- **A wall-clock step** fails closed on age.

---

## 9. Files (planned)

| path | state |
|---|---|
| `crates/term-proto/` | new, main workspace, `no_std`, zero dependencies |
| `crates/strategy-term/`, `crates/ingress-term/`, `crates/terminal/` (with `synthetic.rs`, the deterministic market generator, and the md recorder) | new (SPDX headers, `license.workspace = true`) |
| `terminal-ui/` (`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `deny.toml`, `about.toml`, `crates/term-ui-core`, `crates/term-render` with its `.wgsl`, `crates/term-ui-web`, `web/index.html`, `web/loader.js`; `assets/fonts/` only under R-T19 (b)) | new, a separate workspace; `dist/` git-ignored |
| `Cargo.toml` (root) | add `terminal-ui` to `exclude` |
| `crates/strategy-set`, `crates/core-types`, `crates/core-io`, `claude-worker/…/pmlr.py`, `archive.py` | TERM rows on 10–13, PODs, the PMLR kind |
| `crates/cli` | the door thread, the TERM boot, account-keyed arms, `exec_boot` laws |
| `crates/exec-hyperliquid`, `exec-binance`, `exec-polymarket`, `exec-hypercall` | the §4.5 deltas |
| `scripts/terminal-ui-build.sh`, `scripts/terminal-open.sh`, `scripts/term-flip.sh`, `launchd/…terminal.plist` | new |
| `scripts/engine-wrapper.sh`, `scripts/daily-restart.sh` | sources `term.env`; the TERM sweep (the operator's word, other lanes' files) |
| `Makefile` | `terminal-ui-gates`; `license-check` asserts the UI notices merge |
| `term.toml.example`, `term.env.example`, `.env.example`, `exec.toml.example` | names only |
| `docs/risk-policy.md`, `docs/wire-format.md`, `docs/repo-map.md`, `docs/state.md` | updated at each phase close |
| `fuzz/` | `term_frame`, `term_http`, `term_ws`, `term_proto_frame`, `term_proto_intent`, `term_proto_dec`, `bn_depth_diff`, `hl_l2book_full`, `pm_book_full` |

---

## 10. History

| version | date | change |
|---|---|---|
| v1 | 2026-10-18 | First plan: a JS frontend with Lightweight Charts. |
| v1.1 | 2026-10-18 | 17 risk-review findings folded in: 2 BLOCKs, 11 MAJOR, 4 MINOR (the key outside `.env`, reducing vs growing commands, slot-scoped HALT, `sent_wall_ns`, the dead-man and watchdog, the HL ledger, budgets before T1, descriptor identity, Hypercall and BX-7 facts, venue minimums, the restart checklist, web security, licensing, the PMLR kind, precision, the modify race). All carried into v2. |
| v2 | 2026-10-19 | **The frontend in Rust/WASM** (O-T9 … O-T13): egui 0.33.3 + eframe + wgpu 27 in a separate `terminal-ui/` workspace on Rust 1.88; an all-Rust chart engine; the shared `term-proto` crate; replay mode; CSP; T-L13, T-L14; R-T3 withdrawn; R-T18 … R-T20 added. **TERM slots moved 8, 10–12 → 10–13**, because PVM claims 8. |
| **v2.1** | **2026-10-19** | **12 findings from a review of the frontend** (7 MAJOR, 5 MINOR). The reviewer built a mirror workspace (Rust 1.88, eframe 0.33.3, wgpu 27, Chromium 141). Changes: hidden/frozen tabs and an `Echo`-based UI-lost; the gates rewritten (cfg-gated deps, wasm lint `--lib`, kittest pointer events); the target-gated `tokio` (wasm-bindgen pin, lockfile grep, `[graph] targets`); fonts (crates.io exception vs vendoring ruling); WebGL2 (no storage buffers: instanced polylines, a per-column base texture); replay in T1a with a recorder and a synthetic generator; the T0 render spike and the T1a/T1b split; the build script (names, cdylib, wasm-opt features, MIME, glue compare); CSP `base-uri`/`form-action` and WASM-driven login; T-L13/T-L14 wording; the f32 reasoning corrected; chunked frames and typed records; `BUILT_MASK` for both landing orders; the calendar restated from one named pace. |

---

## 11. Size, against PVM and PM15

Line counts include tests and were measured with `wc` on 2026-10-19.

| lane | lines | how long it took |
|---|---|---|
| PM15 | ~18k | about a day of sessions |
| PVM | ~165k (10 pine crates, 160k) | about 4 weeks, still on its branch |
| BX (`exec-binance`) | 37k | ~3 weeks |

**TERM v2.1, estimated:**

| part | lines |
|---|---|
| terminal server (Rust, cold), incl. replay, recorder, synthetic generator | 16–22k |
| **frontend in Rust:** `term-ui-core` app 7–10k + chart engine 8–12k + `term-render` 3–4k + `term-ui-web` ≈ 1k + `term-proto` 1–1.5k | **20–28k** (v1.1's JS frontend: 6–10k) |
| engine side, paper | 9–13k |
| arm deltas: HL 1–2k, Binance 3–6k, PM 4–6k, Hypercall 2–4k | 10–18k |
| Pine overlay | ~1k |
| **total (the parts' sum)** | **≈ 56–82k** (v1.1: 45–60k): about 3–4.5× PM15 and 35–50 % of PVM |

**Calendar, from one named pace.**

The pace used is BX's, about 12k lines a week (37k in ~3 weeks, with review rounds and Demo batteries). PM15's ~18k in a day came from heavy reuse that TERM does not have, so it brackets nothing.

| scope | lines | calendar at BX pace |
|---|---|---|
| paper terminal (T0–T6) | 45–63k | **≈ 4–5¼ weeks** |
| everything, all four venues | 56–82k | **≈ 4¾–7 weeks** |

Faster is possible if the cold parts (server and UI, about 37–51k) run quicker than money-path work. That is plausible but unproven. **The T0 spike and T1a measure the real pace**, and the calendar is restated after T1a.

**Operator time, on top of the calendar:**

| when | what |
|---|---|
| T0 spike | ~1 session on the Mac: WebGPU and Safari 26 |
| T1b and T2 | one visual review each |
| T5 and T6 | ≥ 10 round trips and 3 algo runs through the real UI; the 120-run coverage is the harness's |
| each T7 venue | the mainnet dust battery |

This calendar replaces the earlier chat estimate for v1.1 ("3–4 weeks to all four venues"). That estimate was optimistic on the same pace.

**What v2 buys for its extra 12–18k lines over v1.1:**
- one language, and the repo's own gates (clippy, nextest, proptest, fuzz) over the UI's logic;
- the server/UI frame drift class removed by `term-proto`;
- no third-party JS and no vendoring ruling;
- a native app later from the same core.

## Sources

- egui/eframe: [eframe on docs.rs](https://docs.rs/eframe) · [versions and dependencies](https://docs.rs/crate/eframe/latest) · [0.37.0 Cargo.toml (rust-version 1.95)](https://docs.rs/crate/eframe/latest/source/Cargo.toml) · [0.34.0 Cargo.toml (1.92, wgpu 29)](https://docs.rs/crate/eframe/0.34.0/source/Cargo.toml) · [0.33.0 Cargo.toml (1.88, wgpu 27)](https://docs.rs/crate/eframe/0.33.0/source/Cargo.toml)
- WebGPU: [Safari 26 ships WebGPU](https://app.cinevva.com/news/2025-09-15-safari-webgpu) · [WebGPU in major browsers (web.dev)](https://web.dev/blog/webgpu-supported-major-browsers?hl=en)
- Hyperliquid: [rate limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits) · [nonces and API wallets](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/nonces-and-api-wallets) · [sub-accounts](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/sub-accounts)
- Binance: [sub-account FAQ](https://binance.com/support/faq/360020632811) · [SBE market data streams](https://developers.binance.com/docs/binance-spot-api-docs/sbe-market-data-streams)
- Polymarket: [wallets and auth](https://docs.polymarket.com/trading/wallets-auth.md)
- Repo (read-only): `rust-toolchain.toml`, root `Cargo.toml`, `deny.toml`, `crates/cli/src/exec_boot.rs`, `crates/exec-hyperliquid/src/{gateway,event}.rs`, `crates/core-types/src/lib.rs`, `docs/wire-format.md`, `docs/risk-policy.md` ("S16", "HYPERCALL", "BX"); the PVM worktree's `crates/strategy-set/src/lib.rs` (`SLOT_PINE`)
- Project: `claude/term-plan-2026-10-18.md` (v1.1), `claude/pvm-resume-2026-10-18.md`, `claude/pm15-build-plan-2026-10-17.md`, `claude/pm15-status-2026-10-17.md`, `claude/bx-status.md`
