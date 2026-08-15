# Phase 8f — Progress log (stage2/8f-ai-ingress worktree)

Session notes for the Stage-2 worktree ONLY. `docs/phase-8-progress.md`
belongs to the Stage-1/soak session — never write there from here.

## 2026-08-15 (first entry) — Phase 0 COMPLETE: design written, reviewed, decisions locked; no code

- Worktree created at `b931c59` (8e commit), branch `stage2/8f-ai-ingress`;
  `.env` copied 0600; RustRover MCP attached to the worktree project.
- Required reading done: CLAUDE.md; plan §3, §6.5–6.6, §7–§14;
  wire-format.md; phase-8-progress seventh entry (**no eighth entry exists
  at the branch point** — the 2026-08-15 kickoff directive carries the
  delta: §8.2 rewrite, §8.2.1 dual-mode, §8.7 attribution, §9
  ANTHROPIC_API_KEY serve-only, §12 "both modes proven", Python 3.14);
  old `claude-worker/` fully read as reference (1 012 lines incl. tests).
- Wrote `docs/phase-8f-design.md`. Operator review completed same day;
  **§13 decisions locked**: (1) ingress-ai rewrites `ts_ns` to engine
  monotonic at accept — TTL clock-coherent; (2) `push --kind order-intent`
  allowed in semi-manual, paper-only pre-8i; (3) hash128 single-frame
  Stage/Commit; (4) new crate `strategy-set`; (5) Typer stays;
  (6) heartbeat 5 s / staleness 15 s compile-time constants.
- Review-driven design amendments (already in the doc):
  `fetch --news` (mechanical news pull for semi-manual — session is the
  triage/labeling brain); engine-thread fills capture `engine-fills.pmlr`
  (kind 2) added to 8f item 6 — positions/P&L reach the AI via replay
  (8f emits, 8h consumes); read-only `positions` verb (≤~1 s-stale live
  view off the running engine's capture; engine is a producer, never a
  server); §12.1 session plan S1–S7 (estimate 6–8 sessions); §12.2
  mandatory session-handoff protocol (every session ends with status +
  interim state + exact resume point + next-session kickoff prompt here).
- Tree state: `docs/phase-8f-design.md` + this file uncommitted; nothing
  else touched. No git ops beyond the operator-authorized worktree add.

**RESUME POINT — Session S1 (design §12.1), checklist items 1–4.**
First act: commit the two Phase-0 docs on `stage2/8f-ai-ingress`
(authorized). Then item 1: `uv python install 3.14` + SDK/tooling import
check (no live calls). Kickoff prompt for S1 issued to operator in chat
per §12.2.
