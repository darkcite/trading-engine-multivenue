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

## 2026-08-15 — Session S1 (items 1–4)

Phase-0 docs committed: `d2b0be2` ("8f Phase 0: design + progress log").

### Item 1 — Python 3.14 toolchain: VERIFIED

- `uv` was **absent from the Mac** (CLAUDE.md assumed it); operator
  installed it manually mid-session: **uv 0.12.5** (Homebrew,
  `/opt/homebrew/bin/uv`).
- `uv python install 3.14` → **CPython 3.14.7**
  (`cpython-3.14.7-macos-aarch64-none`, uv-managed).
- Scratch venv `/tmp/stage2-py314-check`; all imports clean under 3.14.7,
  **no live calls** (import + `__version__` only). Exact versions:

| package | version |
|---|---|
| anthropic | 0.122.0 |
| httpx | 0.28.1 |
| typer | 0.27.1 |
| structlog | 26.1.0 |
| pytest | 9.1.1 |
| ruff | 0.16.3 |
| mypy | 2.3.1 |

- ruff accepts `--target-version py314`; mypy accepts
  `--python-version 3.14` — **no version pinning workaround needed**
  (design §5.5 contingency unused). Typer imports and runs under 3.14 —
  decision Q4's argparse fallback not needed.
- Scratch venv deleted after verification (disposable per design).

### S1 CLOSED — §12.2 handoff

**1. Status.** Items 1–4 of design §12 complete, one commit each, all
gates green on the Mac:

| item | commit | content |
|---|---|---|
| Phase 0 | `d2b0be2` | design + progress log (branch's first commit) |
| 1 | `8feab93` | Python 3.14.7 toolchain verified (versions table above) |
| 2 | `4f086f3` | claude-worker deleted (1 243 lines) + rescaffolded: pyproject ≥3.14, Base/Serve config split, imports-full + config tests |
| 3 | `71bbcae` | `core-types::AiCmd` + kinds + shape validator + offset asserts + `AI_RING_SIZE` + `read_le`; `core-io SlotKind::AiCmd = 4` decode + reader roundtrip; wire-format.md AiCmd section + header row |
| 4 | `0984b79` | `core-crypto::hmac_sha256_tag16` + `ct_eq` (constant-time), RFC 4231 case-5 truncated KAT |

Gates last run: `cargo nextest run -p core-types -p core-io` 63/63;
`-p core-crypto` 19/19; `cargo check --workspace` clean (5 s, warm);
`claude-worker: uv run pytest` 18/18 + ruff check + ruff format + mypy
strict clean. NOT run in S1 (nothing hot-path touched): full workspace
nextest, release alloc assertions, fuzz — item-17 gates.

**2. Interim state / deviations (all within design intent, recorded for
review):**

- **uv was absent from the Mac**; operator installed **uv 0.12.5**
  (Homebrew, `/opt/homebrew/bin/uv`) mid-session. CPython **3.14.7**
  uv-managed.
- Item 2 scaffold: `[project.scripts]` entry point deliberately omitted
  until `cli.py` exists (item 12) — no dangling entry point per the
  unused-code rule. Dev deps are pytest/ruff/mypy only (old
  pytest-asyncio/pytest-cov/tomli-w dropped: no asyncio by design, no
  TOML round-trip anywhere in the new worker).
- Config semantics pinned in code (design §10 left them implicit):
  `CLAUDE_WORKER_REPLAY_DIR` is **required** (only §10 worker key with
  no default); `AI_INGRESS_HMAC_KEY` is required + validated (64 hex →
  32 B) in **BaseConfig for all verbs** — even read-only `positions`/
  `fetch` (fail-fast; loosen at item 12 only if operator asks);
  `RSS_FEEDS` optional → empty allowlist tuple; both secrets are
  `dataclasses.field(repr=False)` with leak-tests.
- Item 3 validator strictness beyond the bare §3 table (documented in
  rustdoc): `MAX_STRATEGY_SLOTS = 8` (u8 enable-mask ceiling) bounds
  Enable/Disable/SetParam slots; `OrderIntent` requires `px > 0`,
  `qty > 0`, real market venue (never `Ai`, never undecodable);
  `SetFairValue` requires `px >= 0` (bias is the signed channel);
  `flags` bit 0 legal on SetFairValue/SetBias only, all other bits ⇒
  malformed; `_pad` must be zero (canonical capture bytes).
  `AiCmd::read_le` added for unaligned rx-buffer materialization — its
  single stack copy is documented inline per the zero-copy doctrine;
  little-endian compile guard added.
- No stale-rmeta incidents; no worktree cargo contention observed.
- Open defects: none known.

**3. Exact resume point.** Design §12 **item 5 — `ingress-ai` crate**
(S2, the biggest single Rust item). First actions: `cargo new`-style
skeleton at `crates/ingress-ai` (workspace membership: check root
`Cargo.toml` members list), then study `core-ring` producer API +
`core-io::PmlrCapture` wiring + an existing mio ingress loop
(`ingress-rss` is the simplest reference — it still exists; RSS removal
is item 16) before writing the UDS listener.

**4. S2 kickoff prompt (ready to paste):**

```
Stage 2 — 8f AI-Ingress, IMPLEMENTATION SESSION S2 (checklist item 5:
ingress-ai crate), ISOLATED WORKTREE, parallel to the soak/remediation
session. NEVER touch /Users/darkcite/trading-engine-multivenue.
WORKTREE: /Users/darkcite/trading-engine-stage2, branch
stage2/8f-ai-ingress (now at 0984b79). Verify get_project_modules
against the worktree path FIRST; if the MCP won't attach, stop.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED) + latest
phase-8f-progress.md entry (S1 handoff) supersede the committed plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S1 handoff (state, deviations, this prompt)
2. docs/phase-8f-design.md §3, §4 (ALL), §10, §11 (Rust rows), §13
   decisions 1/5/6; §7 only for the side-path seam context
3. CLAUDE.md (pitfalls #10/#11)
Committed S1 surface to build on: core_types::{AiCmd, AiCmdKind,
AiCmdShapeError, AI_RING_SIZE, read_le, validate_shape, consts};
core_io::SlotKind::AiCmd; core_crypto::{hmac_sha256_tag16, ct_eq}.
Reference (do not modify): core-ring producer/consumer API,
core-io::PmlrCapture, ingress-rss mio loop shape (RSS dies in item 16 —
reference only), core-metrics counter patterns.
S2 SCOPE — item 5, split commits allowed (5a listener / 5b policy+tests):
crates/ingress-ai: mio UDS listener (AI_INGRESS_SOCK; parent dir 0700,
sock 0600, stale unlink at bind); single client, second conn
accepted-then-closed + rejected_conns_total; peer-cred euid check
(LOCAL_PEERCRED macOS xucred / SO_PEERCRED Linux ucred); preallocated
4 KiB rx buf; §4.4 accept order EXACTLY: len==80 check → full 82-B frame
→ hmac_sha256_tag16 + ct_eq (fail ⇒ drop conn) → validate_shape
(fail ⇒ frame discarded, conn kept) → seq policy (regress discard /
gap count) → ts_ns := now_ns rewrite (decision 1; original ts to
capture only… capture carries the REWRITTEN slot — worker send time is
in the capture record because capture happens at accept: re-read §4.4
step 6 and §13.1 wording and implement what §13.1 says: rewrite happens
BEFORE capture-and-push; PMLR keeps the original ONLY via the raw-tap…
if ambiguous, ASK OPERATOR before coding this line) → PmlrCapture
(SlotKind::AiCmd, capture BEFORE push) → try_push (full ⇒
ring_drops_total). Stage/Commit kinds additionally routed to a side-path
seam (fn hook; validation stub itself is item 14). Metrics:
engine_ingress_ai_{cmds,hmac_fail,protocol_err,malformed,seq_gap,
seq_regress,ring_drops,expired,rejected_conns}_total,
engine_ingress_ai_last_heartbeat_age_ns gauge, capture pair. Tests per
§11: UDS loopback integration suite (good frame, bad HMAC, short/torn,
oversize len, seq regress/gap, second-conn reject, heartbeat cadence,
every counter asserted); proptest (arbitrary bytes never panic;
pack→parse roundtrip); fuzz target ai_cmd_frame registered in
fuzz/Cargo.toml (cargo check only); bench alloc assertion
accept→verify→capture→push 0 B/op (release, --test-threads=1).
Hot-path rules absolute: zero alloc, no dyn, no tokio, no serde_json,
mio only. Tests override AI_INGRESS_SOCK=/tmp/stage2-ai-<pid>.sock,
METRICS_BIND stage2-local (never 9191), MULTIVENUE_LOG_DIR
stage2-local. HARD ISOLATION unchanged (S1 prompt rules). Cargo on the
Mac ONLY; RustRover MCP execute_terminal_command executeInShell=true
≤45 s, long builds nohup > /tmp/stage2-build.log & + poll. uv 0.12.5 at
/opt/homebrew/bin/uv (Python 3.14.7) — not needed for item 5. One
commit per sub-item, tests green on the Mac before the next, one-line
status after each. STOP after item 5: append §12.2 handoff (status,
interim state, resume point, S3 kickoff for items 6–7) to
docs/phase-8f-progress.md. All notes to phase-8f-progress.md ONLY. If
context runs short: write interim state + exact resume point + relaunch
prompt, then tell me.
```

Note for S2 (flagged during S1, needs no decision now): §4.4 step 6
("capture BEFORE push so ring-dropped commands remain auditable") and
§13.1 ("worker send time preserved in the PMLR capture record only")
are in tension — if capture happens after the rewrite, the captured
slot carries engine time, and the worker send time survives nowhere
structured. S2 must reconcile (likely: capture the pre-rewrite slot,
push the rewritten one — zero extra copies, both truths kept) and ask
the operator if the reconciliation changes observable behavior.
→ RESOLVED in S2, see below: operator chose the literal §4.4 ordering.

## 2026-08-15 — Session S2 (item 5: `ingress-ai`)

### S1 open question RESOLVED — operator decision (same day)

Asked before coding the rewrite/capture lines, as flagged. Options
presented: (a) capture the pre-rewrite slot (worker ts in capture,
§13.1 verbatim) vs (b) literal §4.4 step-6 sequence (rewrite → capture
→ push; capture byte-identical to the pushed slot). **Operator chose
(b).** Consequences, now in force:

- PMLR capture (`ai-cmds.pmlr`, kind 4) carries the **rewritten** slot
  — engine-clock coherent, byte-identical to what the ring consumer
  sees.
- The worker's original send time survives ONLY in the optional
  `--raw-tap` payload capture (which `ingress-ai` does not host in
  8f); structured recovery of send times = the worker's own SQLite
  event log (item 9).
- Docs amended in the 5a commit: design §3 (dated amendment block),
  §4.4 step 6 (clarifying note), §13.1 (strikethrough + amendment),
  wire-format.md AiCmd section, `core-types` AiCmd rustdoc.

### Item 5 — `ingress-ai`: DONE (two commits)

| commit | content |
|---|---|
| `a43f87f` | 5a: crate (workspace member + `core-crypto`/`ingress-ai` added to workspace deps); `frame.rs` (82-B frame consts/parse/pack, `SeqPolicy`), `capture.rs` (`AiCmdCapture` single-file PMLR sink, sticky-disable), `status.rs` (`AiIngressStatus` D7-pattern slot), `listener.rs` (`bind_uds` 0700/0600/stale-unlink, `peer_euid` LOCAL_PEERCRED/SO_PEERCRED, mio run loop, `admit_frame` §4.4 hot core, Stage/Commit seam hook); doc amendments above; 26 unit+prop tests |
| `10fc7c8` | 5b: `tests/uds_loopback.rs` (8 scenarios: good/batched frames, bad HMAC, short+oversize len, torn frame, seq regress/gap, second-conn reject, malformed-keeps-conn, heartbeat cadence, Stage+Commit seam; FULL counter vector asserted incl. zeros); `ai_cmd_frame` fuzz target (raw-frame, valid-tag→shape, seq-policy paths) in `fuzz/Cargo.toml`; `ai_ingress_admit_frame_is_zero_alloc` in bench (10 000 frames pack→accept→verify→capture→push) |

**Gates last run (all on the Mac):** `cargo nextest run -p ingress-ai`
34/34; targeted sweep `-p ingress-ai -p core-types -p core-io
-p core-crypto -p bench` 148/148; `cargo check --workspace` clean;
fuzz `cargo check` clean; `cargo test -p bench --test alloc_assertions
--release -- --test-threads=1` **31/31, new AI path 0 B/op**. NOT run
(item-17 gates): full workspace nextest, fuzz time-boxed runs, pytest.

### Interim state / deviations (all recorded for review)

- **Capture sink shape:** `PmlrCapture` (core-io) is the 3-file venue
  sink; AI needs one file, so `AiCmdCapture` lives in `ingress-ai`
  wrapping `PmlrWriter` with the same sticky-disable/flush-cadence
  policy. If item 6's `engine-fills.pmlr` capture wants the same
  shape, hoist a generic single-file sink into core-io THEN (do not
  duplicate a third time).
- **`expired_total` writer:** lives in `AiIngressStatus` so the whole
  `engine_ingress_ai_*` family mirrors from one slot, but its writer
  is the ENGINE drain site (`inc_expired`, item 6) — per-field single
  writer, documented in the struct.
- **Metrics exposition** (registry names, heartbeat AGE gauge derived
  as `now - last_heartbeat_ns`) is deliberately left to the cli
  mirror in item 6 — the crate exposes the status slot + capture-pair
  getters only (IngressStatus/D7 pattern; no core-metrics dep).
- **SeqPolicy:** primes on the FIRST frame of each connection without
  counting a gap (handles both worker schemes: restart-at-1 and the
  persistent SQLite allocator surviving reconnects); per-connection
  reset; `seq_gap_total` counts gap EVENTS, not missing frames;
  regress does not move the high-water mark.
- **Counter semantics pinned:** `cmds_total` = frames passing
  len+HMAC+shape+seq (ring-dropped still counted; `ring_drops_total`
  separate). Malformed/regressed frames are NOT captured. Torn-frame
  residue at EOF/transport-error counts `protocol_err_total`.
  `rejected_conns_total` covers second-conn AND peer-cred mismatch
  (the euid-negative path needs a second uid — not testable
  in-process; it shares the tested reject code path).
- **Seam order:** side-path seam invoked AFTER `try_push` (literal
  §4.4 step order; `AiCmd` is Copy so the push doesn't consume it).
- **`bind_uds` chmods the parent dir 0700 unconditionally** — fails
  fast on a shared parent like `/tmp` (correct for production
  `~/multivenue/run/`). Tests therefore use
  `/tmp/stage2-ai-<pid>/<tag>.sock` (own parent dir; still inside the
  stage2 namespace).
- **macOS landmines (cost ~30 min, now encoded in tests):**
  (1) `setsockopt(SO_RCVTIMEO)` returns EINVAL on a UDS whose peer
  already closed — the loopback EOF probe uses `set_nonblocking` +
  bounded read poll instead of `set_read_timeout`.
  (2) `std::thread::scope` + a panicking scenario = 60 s+ hang (scope
  joins the still-running ingress thread; the real assert never
  prints). Fixed with a `StopOnDrop` guard that flips the stop flag
  during unwind. Diagnosed live with `sample <pid>`; the three stuck
  test PIDs were killed by PID (never by name).
- No stale-rmeta incidents. Open defects: none known.

### Exact resume point

Design §12 **items 6–7** (S3). Item 6 first actions: read
`crates/engine` tick loop + `strategy-core::Strategy` trait, then add
the `ai_cons: Consumer<AiCmd, AI_RING_SIZE>` lane — budgeted drain in
`Engine::tick()`, TTL-on-pop (`now - cmd.ts_ns > ttl_ns ⇒ drop +
status.inc_expired()` — ts_ns is engine-monotonic since accept,
decision 1), `Strategy::on_ai` defaulted method; then engine-thread
fills capture (`engine-fills.pmlr`, kind 2 — decide the core-io hoist
here); then cli wiring (spawn `ingress-ai` thread with
`Ring::new().split()`, env `AI_INGRESS_SOCK` + `AI_INGRESS_HMAC_KEY`
64-hex→32 B, core-4 pin note, mirror `AiIngressStatus` + capture pair
into the metrics registry incl. the heartbeat-age gauge). Item 7:
new crate `strategy-set` per §7 (§13 decision 3).

### S3 kickoff prompt (ready to paste)

```
Stage 2 — 8f AI-Ingress, IMPLEMENTATION SESSION S3 (checklist items
6–7: engine AI lane + strategy-set), ISOLATED WORKTREE, parallel to
the soak/remediation session. NEVER touch
/Users/darkcite/trading-engine-multivenue.
WORKTREE: /Users/darkcite/trading-engine-stage2, branch
stage2/8f-ai-ingress (now at 10fc7c8 + the S2 handoff commit). Verify
get_project_modules against the worktree path FIRST; if the MCP won't
attach, stop.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED, §3/§13.1 capture
amendment 2026-08-15 in force: capture carries the REWRITTEN slot;
worker send time via --raw-tap only) + latest phase-8f-progress.md
entry (S2 handoff) supersede the committed plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S2 handoff (state, deviations, this
   prompt)
2. docs/phase-8f-design.md §2 (engine/strategy-set/fills-capture/cli
   rows), §3 (TTL drain paragraph + amendment), §4.3–4.4 (consumer
   side), §5.4, §7 (ALL — StrategySet semantics), §10, §11 (engine
   drain + StrategySet rows), §13 decisions 1/3/6
3. CLAUDE.md (pitfalls #10/#11)
Committed S2 surface to build on: ingress_ai::{run, AiIngressCfg,
AiIngressStatus (incl. inc_expired for the drain site), AiCmdCapture,
admit_frame, pack_frame, parse_frame, SeqPolicy, FRAME_LEN,
AI_CMDS_FILE}; S1 surface unchanged (core_types::AiCmd etc.).
Reference (do not modify): ingress-ai internals, core-ring, existing
engine tick loop shape, cli spawn wrappers for venue ingresses.
S3 SCOPE — item 6 then item 7, one commit each (6 may split
6a drain / 6b fills-capture+cli):
item 6: engine `ai_cons` lane — budgeted drain in Engine::tick()
(budget per tick, design §2 row), TTL-on-pop vs engine-monotonic
ts_ns (drop ⇒ AiIngressStatus::inc_expired — the slot's designated
drain-side writer), malformed re-check at drain (defense in depth,
counter), Strategy::on_ai defaulted method (monomorphized, no dyn);
engine-thread fills capture engine-fills.pmlr (SlotKind::Fill = 2,
alloc-asserted; consider hoisting a generic single-file PMLR sink
into core-io and porting AiCmdCapture onto it — S2 deviation note);
cli spawn wiring: ingress-ai thread (core 4 note — RSS still owns its
core until item 16), env keys AI_INGRESS_SOCK /
AI_INGRESS_HMAC_KEY (64 hex → 32 B, .env only, never logged),
metrics mirror for AiIngressStatus + capture pair +
engine_ingress_ai_last_heartbeat_age_ns gauge (= now −
last_heartbeat_ns, 0 ⇒ report absent-or-sentinel, pick and document).
item 7: new crate strategy-set per §7: static members
latency-arb/ev/cross-arb/rule-tree/ai-exec-placeholder? NO — ai-exec
is item 8; wire the slot indices (0–5, 4=ai-exec bit reserved until
item 8, 5=vm bit reserved for 8g) WITHOUT dead members (unused-code
rule: only built members exist as fields; mask bits for 4/5 are
reserved constants). Enable refused while halted (sticky halt flag
from HaltRequest until 8i), Disable always honored,
engine_ai_enable_refused_total, --strategy → initial mask; unit
suite + alloc assertion (fan-out steady state).
Tests per §11 rows: engine drain (TTL-expiry-on-pop, budget
respected, malformed rejected); StrategySet (mask fan-out,
enable-while-halted refused, disable always, initial mask). Alloc
gates in bench: StrategySet fan-out steady state (ai-exec on_tick
gate arrives with item 8).
Hot-path rules absolute: zero alloc, no dyn, no tokio, no
serde_json. Tests override AI_INGRESS_SOCK under
/tmp/stage2-ai-<pid>/ (OWN parent dir — bind_uds force-chmods the
parent to 0700 and must not touch shared /tmp), METRICS_BIND
stage2-local (never 9191), MULTIVENUE_LOG_DIR stage2-local; never
~/multivenue/logs, never /tmp/soak*; no kill/pkill by name (by-PID
of own test processes only, after diagnosis).
HARD ISOLATION unchanged (S1/S2 prompt rules): all work ONLY under
/Users/darkcite/trading-engine-stage2; NEVER run `multivenue-engine
run`, bind 127.0.0.1:9191, or connect live venues. Git: small
commits ONLY on stage2/8f-ai-ingress inside the worktree; NO
merge/rebase/push; no git ops in the main checkout.
Cargo on the Mac ONLY (CLAUDE.md pitfall #10; sandbox = greps only).
Stale-rmeta pitfall: impossible unresolved-import errors after edits
= cargo clean -p <touched crates> and retry.
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long builds via
nohup > /tmp/stage2-build.log & then poll; projectPath = the
worktree. macOS test landmines from S2: SO_RCVTIMEO EINVAL on
peer-closed UDS (use nonblocking EOF probes); std::thread::scope +
panicking scenario hangs unless a StopOnDrop guard flips the stop
flag (pattern in crates/ingress-ai/tests/uds_loopback.rs); `sample
<pid>` is the hang diagnostic. uv 0.12.5 at /opt/homebrew/bin/uv
(Python 3.14.7) — not needed for items 6–7. One commit per
(sub-)item, tests green on the Mac before the next, one-line status
after each; ask before anything ambiguous.
STOP after item 7: append §12.2 handoff (status, interim state,
resume point, S4 kickoff for items 8–9) to docs/phase-8f-progress.md.
All session notes to phase-8f-progress.md ONLY — phase-8-progress.md
belongs to the other session. If context runs short: write interim
state + exact resume point + relaunch prompt into
phase-8f-progress.md, then tell me.
```
