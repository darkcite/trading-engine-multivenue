<!-- SPDX-License-Identifier: Apache-2.0 -->
<!-- Copyright 2026 Anton (darkcite) -->
# TERM branch `term-t0-spike` — handoff and work plan

Read this first in any session that continues TERM on this branch. It ranks
below `CLAUDE.md`, `docs/risk-policy.md` and `docs/state.md`, and follows the
TERM plan v2.1 (project copy `claude/term-plan-v2-2026-10-19.md`).
Never take an armed or live state from this file.

## Operator rulings for this branch (2026-11-01)

| id | ruling |
|---|---|
| **O-T14** | **Git override, scoped to this branch.** The CLAUDE.md git law (no branches, no session pushes, git write-ops on the Mac) is waived for `term-t0-spike` only: sessions may commit and push to it from the cloud container. `main` is never touched; no force-push, rebase or history rewrite; staging is explicit-path only; one commit per milestone, authored `darkcite`. The token is the operator's and is never written to disk or config. |
| **O-T15** | **Scope includes the money path.** This run covers T1a (server), T1b (UI) and the engine side: the `strategy-term` member and the `ingress-term` door. R-T8, R-T18 and R-T19 (a) are taken as recommended. T0's Mac half (WebGPU, Safari 26) is still owed and does not block. |
| **O-T17** | **TERM slots amended (R-T1 was stale: PMJ owns 11).** term-hl **12**, term-bn **13**, term-pm **14**, term-hc **15**. Slot 10 stays reserved. |
| **O-T18** | **M5 scope from the cloud box:** the member and the door in paper, tested only against an in-process door with a per-run key. alloc-assert, bench-check and the worker's pytest pin stay owed to the Mac. |
| **O-T16** | **Root workspace and live public feeds.** `term-proto` and `crates/terminal` join the root workspace on this branch. The cloud box may open live **public** market-data feeds (no keys), inside the plan's budgets. |

Laws that still hold in full: T-L1 (the terminal holds no venue key and opens
no order socket), **T-L2 (Claude never sends a frame to the live door; tests
use an in-process door with a per-run key)**, T-L5, T-L6 (stop a venue's feed
on its first 429/418; HL ≤ 40 weight/min, Binance ≤ 4 connections), T-L8,
T-L12, T-L13, T-L14, and "research never enters git" (measurements and
status docs go to the project, not here). No venue key exists in the cloud
box, so nothing built here can trade.

## State (2026-11-01, after M4)

- **M0** done: T0 smoke PASS on WebGL2; WebGPU over SwiftShader also runs
  every phase. On WebGPU the device reports the browser's limits, so the
  no-storage-buffer guarantee rests on the naga shader test.
- **M1** done: term-proto, 23 kinds, wire v1, fuzz targets.
- **M2** done: `crates/terminal` (multivenue-terminal): auth, CSP, WS,
  synthetic, recorder, replay. The bundle is public on loopback (no
  secret, T-L13); `/ws` is the only door to data.
- **M3** done: `--live` HL + Binance USDⓈ-M public feeds, scanners
  (fuzz `term_live_scan`), Binance sync, HL candle history at boot (≈ 26
  weight), heat every 250 ms. From the cloud box Binance answers 451 and
  stops (geo-block); TLS there needs `--extra-ca` (the sandbox re-signs).
- **M4** done (first cut): the app — login form, WebSocket link with the
  one COPY, chart / heatmap / ladder / tape / flow from server frames.
  `scripts/app_smoke.py` signs in through the form; PASS on synthetic and
  on live HL.
- **M5a/b** done: `crates/ingress-term` (the door: frames, accept order,
  rings, the 0600 UDS thread) and `crates/strategy-term` (the member: reduce
  first, pre-gate, watchdog, slot-scoped halt, flatten, modify), both under
  copy-audit. **M5c part 1 done**: `strategy-set` builds TERM on 12–15
  (`Term<SLOT>`, names term-hl/-bn/-pm/-hc, outside `all`, BUILT_MASK
  0xFBFF, slot 10 the only reserved slot); an unwired member is inert
  (timer period u64::MAX); the AI is refused EnableStrategy(12–15) like
  pine. **M5c part 2 done**: `cli::term_boot` (only when the mask names a TERM
  slot: `~/multivenue/term.env` must be 0600, one ring lane per enabled
  slot, the door on `~/multivenue/term.sock`, the epoch in
  `~/multivenue/term.epoch`), called from `engine_loop_set`; `/state`
  names 12–15 (engine-snapshot); TERM is PAPER ONLY in `exec_boot`
  (`--arm-live 12` and a live `[exec.slot.12]` are refused, like pine);
  the terminal's door client (`--door/--door-key/--door-epoch/--route
  instr:slot:sym`, beats while a UI is connected, events → views).
  **Still owed from M5**: the PMLR kind for `term-cmds.pmlr` (every reader +
  the worker pin, Mac), publishing the boot epoch in `/state`, algos over
  the door, a `term.toml` for per-slot caps, refusals shown in the UI.
- **M6 done on the cloud box** (2026-11-01): `make lint` PASS (whole
  workspace), `license-check` OK, `copy-audit` new=0 (TERM crates under the
  ratchet), `cargo test --workspace --no-fail-fast`: 415 test binaries, all
  pass after M6a/M6b (two TERM regressions found and fixed: a stack overflow
  in the set built by value, and tui's slot-row pin). Fuzz on nightly, 60 s
  each, clean: term_proto_frame, term_proto_intent, term_proto_dec,
  term_live_scan, term_door_frame. `license-deps` not triggered (no new
  third-party crate).
- **Owed to the Mac**: alloc-assert, bench-check, the worker's pytest pin
  (`claude-worker/tests/test_news_cascade.py` asserts "8..15 are reserved":
  already stale before TERM, now 12–15 are TERM's), the `term-cmds.pmlr`
  PMLR kind (every reader + the worker), publishing the boot epoch in
  `/state`, Safari 26 + a real GPU for T0, and a first paper session through
  the real door (`STRATEGY=…+term-hl`, `term.env` 0600, the terminal with
  `--door … --route …`).
- **Follow-ups**: HL coarse `nSigFigs` book for the heatmap; footprint and
  VP frames; drawing tools, crosshair, panes; the blotter and refusal
  display; algos over the door; `term.toml` caps; kittest; the glue
  byte-compare gate; `build.rs` embedding under `web-bundle`.
- **Local notes for the cloud box**: build with `RUSTFLAGS=-C
  target-cpu=native` in a separate target dir; the sandbox caps disk at
  ~19 GB, so test with `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`; detached jobs die between
  turns, so poll long runs within one turn (the repo's `znver3` flag
  traps on this CPU); kill the server with `pkill -x multivenue-term`, never
  `pgrep -f` (it matches the calling shell).

## Work plan (milestones = commits)

- **M0** Re-run the smoke on the post-clippy tree; try WebGPU over
  SwiftShader (`smoke.py out webgpu`).
- **M1 term-proto complete.** Every frame kind of plan §4.11.2 (BookSnap /
  BookDelta chunked, TradeBatch, Bar, FootprintCells, VapArray, FlowPoint,
  HeatCol, IndicatorPoint, Marker, Order/Position/FillView, EngineStatus,
  Intent, Control, Heartbeat/Echo, Layout/Drawing/KeyMap, LiveConfirm);
  chunk reassembly; round-trip proptests; fuzz targets `term_proto_frame`,
  `term_proto_intent`, `term_proto_dec` in `fuzz/`. Join the root workspace.
- **M2 `crates/terminal` server.** mio HTTP/1.1 + RFC 6455; one-time token →
  cookie; exact Host/Origin; CSP and headers; `web-bundle` embed with
  MANIFEST check; `--synthetic <seed>`; the md recorder; `--replay`.
- **M3 market data.** HL `l2Book` + `trades`, Binance USDⓈ-M `@depth@100ms`
  + `@aggTrade` with snapshot and U/u sync; books, bars, OFI, imbalance,
  microprice, footprint, VP (plan §4.9); proptest + fuzz per scanner. A short
  live public smoke from the cloud inside the budgets.
- **M4 UI T1b.** WS client with the preallocated ring and the one documented
  copy; chart engine panes, crosshair, time labels, price scale; ladder;
  heatmap fed by `HeatCol`; tape. kittest with raw pointer events.
- **M5 money path, paper.** `strategy-term` member on slots 10–13 and the
  `ingress-term` HMAC UDS door shaped like `ingress-ai`; intents → pre-gate
  → E6; the cloid encodes the slot (E-9). Tests against an in-process door
  with a per-run key only. No armed path, no live slot.
- **M6 gates.** `make lint`, `cargo nextest run --workspace`,
  `make license-check`, `make copy-audit`, `make license-deps`, fuzz 60 s per
  touched target (nightly), plus `make terminal-ui-gates`. Record which gates
  are Mac-only (alloc-assert, bench-check, the pytest pin) and owed.

Each session ends with: commit, push to this branch, a progress note to the
operator, and this file's "State" section updated.
