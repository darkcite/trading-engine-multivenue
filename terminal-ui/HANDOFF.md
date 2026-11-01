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
- **Owed**: M5 (member + door, paper, in-process door tests only), M6
  (gates: lint, nextest, license-check, copy-audit, license-deps, fuzz 60 s
  per touched target on nightly). Follow-ups: HL coarse `nSigFigs` book for
  the heatmap; footprint and VP frames from the server; drawing tools,
  crosshair, panes; kittest; the glue byte-compare gate; `build.rs`
  embedding under `web-bundle`; T0's Mac half (Safari 26, a real GPU).
- **Local notes for the cloud box**: build with `RUSTFLAGS=-C
  target-cpu=native` in a separate target dir (the repo's `znver3` flag
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
