# G1 remediation + re-soak plan (post first 6 h soak)

**STATUS 2026-08-15: CLOSED — G1 BLESSED.** All three work items
shipped in commit 9d473ca (every-row trade-seq checking, 1:1
TradeGap/BookGap pairing events + audit-replay pairing section,
metrics accept nonblocking-inherit fix via event sink → tracing,
capture_records gauge on the 1 s flush cadence; gates 836/836 +
30/30 alloc 0 B/op). The 4 h re-soak (run-1786779891499577000)
passed every §6.6 criterion on the amended basis — deribit
gaps_total==gap_events==0 across 453 k msgs — and the operator
blessed G1 the same day (ninth progress entry; plan §12 G1 row).
Kept as the work-order record.

2026-08-15. Basis: eighth progress entry; capture
`~/multivenue/logs/run-1786742370972151000` (142 MB, keep — it is the
regression-fixture source). Operator decisions baked in: Alchemy key
POSTPONED (none available); re-soak is 4–6 h, NOT 24 h (laptop
constraint — operator amends the G1 basis, §12 G1 row precedent);
OKX object-extent trade slicing stays deferred to post-G1 hardening.

## Scope

IN: (1) deribit gap-monitor root-cause + event pairing, (2) metrics
accept-loop EAGAIN nit, (3) capture_records gauge cadence nit,
(4) 4–6 h re-soak and §6.6 judgment on the amended basis.
OUT: Alchemy/rpc closure (leg stays keyless, parse errors
EXPECTED+tapped); OKX trade-slicing hardening; anything else. Diff
stays minimal ahead of a gate run.

## Work item 1 — deribit gap monitor (the blocker)

Evidence from the 6 h soak: gaps_total=67 while raw tap = header-only
(0 rejects), deribit reconnects/resubscribes = 0, and audit-replay
re-derivation shows trade_holes=0 / chain_breaks=0 on BOTH
instruments. Increments arrived in bursts inside the two
network-weather windows (21:43–21:51Z, 23:32–23:49Z). Conclusion:
runtime monitor counts discontinuities the captured stream disproves.

Ranked hypotheses (verify, do not assume):
1. Cross-stream trade_seq comparison — ticker-implied last-trade seq
   racing the trade channel during stalls.
2. Cross-instrument interleave edge in DeribitTradeSeq (two
   instruments share the ingress thread).
3. Heartbeat/test_request path counting into gaps_total.

Tasks:
- Locate every `gaps_total++` site in `crates/ingress-deribit`; write
  a synthetic unit repro of the observed pattern (red), fix (green).
- EVERY surviving gap increment must emit a paired ChannelEvent
  (existing 64-B POD, SlotKind::Event=5 — no new types) carrying
  channel, symbol, expected vs observed seq; plus a rate-limited log
  line. Zero-alloc, monomorphized capture hook, as per 8e patterns.
- `audit-replay` gains a pairing section: runtime gap counter vs gap
  ChannelEvents in <label>-events.pmlr — §6.6's "every increment
  paired with a logged venue event" becomes mechanically checkable.
- Regression fixture: prefer bytes/sequences extracted from
  run-1786742370972151000; synthesize the ticker/trade interleave
  otherwise. Golden-run test updated.

Acceptance: nextest workspace green (Mac ONLY — CLAUDE.md pitfall 10);
release alloc gate 0 B/op incl. live capture; next soak shows
deribit gaps_total==0 under normal weather OR every increment 1:1
event-paired AND corroborated (or refuted with evidence) offline.

## Work item 2 — metrics accept EAGAIN

Observed twice (01:23:22Z, ~01:46Z): single scrape fails, engine
prints raw untimestamped `metrics: connection error: Resource
temporarily unavailable (os error 35)`. Fix: treat EAGAIN/EWOULDBLOCK
as retry in the accept path; route real errors through the standard
log macro (timestamp + target). Acceptance: curl hammer test; no raw
eprintln remains.

## Work item 3 — capture_records gauge cadence

Observed: gauge publishes only after a run-loop exit (okx froze at
its first-cycle 11 232; never-cycled venues report 0 despite growing
pmlr files). Fix: publish on the existing 1 s maybe_flush cadence.
Acceptance: gauges advance within ~2 s in steady state; alloc gate
unchanged.

## Re-soak (4–6 h, operator basis)

- MUST RE-PROBE outcomeMeta before launch: outcome 1081 settles
  2026-08-15T06:00Z — the BTC 1d priceBinary id WILL be a successor;
  `#enc` = new_id × 10. Never reuse #10810 blindly.
- Same CLI shape as the first soak (PM Xi-2027 YES token, okx 3, dbt
  2, hl BTC,ETH,SOL,#<new-enc>, `--polygon-path /`,
  `--raw-tap deribit,rpc`). rpc parse errors remain EXPECTED.
- Window: if practical, launch 03:00–04:30Z (10:00–11:30 local +07)
  so the 06:00Z settlement lands ~T+2–3 h → finally observes
  outcomeMetaUpdates lifecycle + successor id (8d open half) inside
  the soak. Wired link preferred — both weather episodes last run
  were local-path.
- Monitoring: ~25 min cadence, corrected `_total` grep (see session
  facts), scrape retry guard until item 2 lands, same intervention
  rules as the first soak.
- Judgment: §6.6 criteria on the 4–6 h amended basis + the new
  pairing check. NINTH progress entry, uncommitted. STOP for operator
  verdict (G1 blessing → Stage 2 go).

## Git discipline

No commits, branches, or any git op without explicit operator word.
Suggested: Phase-A diff review → operator approves a commit (eighth
entry + this plan may ride along or go separate — operator choice) →
soak runs on the committed tree.

## Session mechanics facts (carry-over from the 6 h soak session)

- RustRover MCP first action: `get_project_modules` (liveness).
  `execute_terminal_command` MUST set executeInShell=true; keep each
  call ≤45 s — a wait-loop call >45 s times out at the MCP layer but
  KEEPS RUNNING in the IDE terminal (it armed a delayed pkill last
  time; check before re-firing).
- NEVER run cargo in the Cowork Linux sandbox (false greens —
  pitfall 10). Sandbox is for greps/python/sleep only. Sandbox sleep
  calls cap at ~178 s (use ~170 s chunks as the between-sample timer).
- Mac rmeta weirdness after edits → `cargo clean -p <crates>`.
- Live metric names carry `_total`: sample grep is
  `(state|msgs_total|parse_errors_total|gaps_total|ring_drops_total|resubscribes_total|reconnects_total|capture_io_errors|capture_records) [0-9]`.
- Metrics scrape may one-shot fail until item 2 lands — retry once
  after 3 s before recording.
- Stop the engine by pid (`kill -INT <pid>`), or pkill -INT -f
  pattern with care: a zsh -c wrapper whose text contains the pattern
  self-matches. Last shutdown: ONE SIGINT, all loops Stopped in 36 ms.
- Engine: `./target/release/multivenue-engine`; metrics
  127.0.0.1:9191/metrics; run dir `~/multivenue/logs/run-<ns>`;
  capture grows ~142 MB/6 h; audit-replay on that is <20 s.
- Prior-run artifacts: /tmp/soak-6h-notes.md, /tmp/soak-6h-audit.txt,
  /tmp/soak-6h.log — /tmp may not survive reboot; the capture run dir
  is durable and the audit is reproducible from it.
- HIP-4 probe pattern (no /tmp script needed):
  `curl -s -X POST https://api.hyperliquid.xyz/info -H 'Content-Type: application/json' -d '{"type":"outcomeMeta"}'`
  then find the dict with description containing `underlying:BTC` +
  `period:1d` + `class:priceBinary`; coin = `#` + (outcome_id × 10).
- Paper orders fire off live PM ticks (known, economically
  meaningless — seventh entry note 7); not a soak concern.
