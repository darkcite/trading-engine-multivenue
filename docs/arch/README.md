# docs/arch/ — closed historical documents

Everything here is **CLOSED**: superseded plans and finished progress logs.
Nothing in this folder receives further entries. Read only for archaeology;
the live doc set is `docs/` + `PLAN.md` + `CLAUDE.md`.

| file | what it was | closed |
|---|---|---|
| `phase-1-plan.md`, `phase-1b-plan.md`, `phase-1c-plan.md` | Phase 1 build-out plans | superseded by `PLAN.md` |
| `phase-2-plan.md` … `phase-6-plan.md` | Phase 2–6 plans | superseded by `PLAN.md` / `phase-8-plan.md` |
| `phase-8-progress.md` | Stage-1 (8a–8e) soak history incl. G1 soaks | closed 2026-08-15; never write there |
| `g1-remediation-and-resoak-plan.md` | G1 gap-monitor work order | CLOSED — G1 BLESSED 2026-08-15 (commit `9d473ca`) |
| `phase-8f-design.md` / `phase-8f-progress.md` | 8f AI ingress design + log | 8f closed at `7ca91be` |
| `phase-8g-design.md` / `phase-8g-progress.md` | 8g ruleset engine (`strategy-vm`) design + log | 8g closed at `39e6542` (G7) |
| `phase-8-plan.md` | Stage-2 parent plan (§8.2 worker/verbs, §8.7 loop, §12 stage table) | Stage 2 closed 2026-08-22 (H6b-SEMI); superseded by `docs/mvp-completion-plan.md` + `docs/stage2-finish-plan.md` |
| `phase-8h-design.md` / `phase-8h-progress.md` | 8h research-loop design (LOCKED) + log | 8h closed 2026-08-22; closure entry last in the progress log |
| `8h-kickoff.md` | frozen H0 authority prompt (was `docs/prompts/`) | Stage 2 closed; archaeology only |
| `mvp-progress.md` | M1 progress log (universe config, multi-market lanes) | M1 closed 2026-08-22 |
| `m2-progress.md` | M2 progress log (options ladder, options-select, iv_digest) | M2 closed 2026-08-22 |
| `m3-progress.md` | M3 progress log (launchd fleet C1–C5, C6 calendar phase, the remediation-era ops entries) | C6 CLOSED by operator blessing 2026-08-29 (exit entry last; streak 6 ≥ 3, 37/37 runs harness-clean) |
| `m4-progress.md` | M4 progress log (shadow-P&L attribution, D1–D3 rulings) | M4 closed 2026-08-23 |
| `capture-continuity-outage-2026-08-27.md` | Defect A/B/C findings (settlement deaths, PM dark, unnamed errors) | remediated via the plan below; WS2 live-proven 2026-08-29 |
| `capture-remediation-plan-2026-08-28.md` | remediation plan (T1/T2/D-lanes/#7a/#7b) | folded into `docs/stage2-finish-plan.md` WS0–WS13; live-proven 2026-08-29 |
| `remediation-run-phase.md` | remediation run-phase prompt (was `docs/prompts/`) | folded into WS13; superseded |
| `venue-instrument-support-gaps.md` | venue/instrument gap audit | §1 folded into stage2-finish-plan WS2–WS12 (coded 2026-08-29); §2+ = Stage-3+ material |
| `ws10-engine-plumbing-design.md` | WS10 funding/L2 lanes design | WS10 coded+gated+live-proven 2026-08-29 |
| `stage2-finish-plan.md` | Stage-2 finish authority (WS0–WS13, single numbering; WS13 gates+live entries) | Stage 2 finished 2026-08-29; archived at MVP close 2026-09-02 |
| `mvp-completion-plan.md` | M-phase plan M0–M6 | **MVP COMPLETE — M6 closed by operator ruling 2026-09-02** (close entry last in `mvp-progress.md`). **§7 (Stage-3 ENTRY GATE) and §9 (data-pipeline law) remain FORWARD-BINDING from here** |
| `vm2-plan.md` | ruleset-VM v2 plan V0–V9 (§8 log, §9 parity runbook) | V0–V9 all closed 2026-09-02; engine-only after the operator-ordered bootout; stay-greens 1420/39/600 |
| `m5-runbook-notes.md` | M5 runbook (external strategies, carry/xv crons, session notes) | M5 closed with the 2026-09-02 bootout (crons decommissioned; xv lives on the VM) |
| `binance-stocks-plan.md` | BST bStocks/TradFi-perps/PM-equity-dailies plan | landed 2026-08-29 (§8); PM ≤6-token cap finding standing |
| `license-audit-2026-08-27.md` | Apache-2.0 audit + application record | archived 2026-09-02 (operator order); **REMAINS the authority for CLAUDE.md's Licensing rules + the Makefile gates** |
| `research-tools-exclusion-plan.md` | research one-shots exclusion policy | archived 2026-09-02 (operator order); **REMAINS the owning authority doc for the `tools_` class** (Makefile naming-rule owner) |
| `architecture.md` + `architecture.svg` | one-page architecture orientation | archived 2026-09-02 (operator order); last actualized 2026-08-29 |
| `options-support-plan.md` | Phase 9+ options-execution candidate | archived 2026-09-02 (operator order); P&L-gated, never scheduled |

Note: this archive grew in waves (2026-08-16, 2026-08-29, 2026-09-02). In-tree doc
comments and older docs cite pre-move paths (`docs/phase-8-plan.md`,
`docs/phase-8h-*`, `docs/prompts/8h-kickoff.md`,
`docs/capture-remediation-plan-2026-08-28.md`, …) — those are historical
citations, left as written BY DESIGN; any such reference resolves here.
