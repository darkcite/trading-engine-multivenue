# Phase 8f — Progress log (stage2/8f-ai-ingress worktree → main)

Session notes for Phase 8f ONLY. `docs/phase-8-progress.md` is closed
soak history — never write there. As of S4 (2026-08-15) the worktree
isolation era is over: `stage2/8f-ai-ingress` merged to `main` and all
8f work continues in the main checkout
`/Users/darkcite/trading-engine-multivenue`.

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

> **AMENDED 2026-08-15 (post-S3, operator directive):** the S4 kickoff
> prompt stored at the end of this entry is **superseded**. S4 begins
> by MERGING `stage2/8f-ai-ingress` into `main` and continues items
> 8–9 in the main checkout `/Users/darkcite/trading-engine-multivenue`
> — the stage2 worktree isolation era ends. The operative S4 prompt
> was issued in chat; its merge preconditions: no running engine, both
> trees clean, divergence surveyed, conflicts beyond mechanical ⇒ ask.

## 2026-08-15 — Session S3 (items 6–7: engine AI lane + strategy-set)

### S3 CLOSED — §12.2 handoff

**1. Status.** Items 6 and 7 of design §12 complete, three commits,
all gates green on the Mac:

| item | commit | content |
|---|---|---|
| 6a | `bb70c46` | engine `ai_cons` lane: budgeted drain in `Engine::tick()` (`AI_DRAIN_BUDGET = 8`, after fills / before timer), TTL-on-pop vs engine-monotonic `ts_ns` (⇒ `AiIngressStatus::inc_expired`), drain-site shape re-check counter, `Strategy::on_ai` defaulted method; `Rings`/`Consumers` carry the AI lane + status slot |
| 6b | `e80bf1d` | `core_io::SlotCapture<R>` hoisted (S2 note honored), `AiCmdCapture` ported (API unchanged); engine fills capture `engine-fills.pmlr` on both fill paths; cli `spawn_ai` + env keys + full `engine_ingress_ai_*` mirror + `engine_fills_capture_*` pair; alloc gate fills-append 0 B/op |
| 7 | `1a45165` | new crate `strategy-set` per §7: slots 0–3 built, bits 4/5 reserved consts (no dead members), Enable/Disable/Halt semantics, `mask_for_name`, cli `--strategy all` → `engine_loop_set_full`; `engine_ai_enable_refused_total` via new defaulted `StrategyCounters::ai_enable_refused`; alloc gate fan-out 0 B/op |

Gates last run: targeted `cargo nextest run` over core-io / ingress-ai /
engine / strategy-core / strategy-set / core-config / cli / core-types /
bench **215/215**; `cargo check --workspace` clean; fuzz `cargo check`
clean; release alloc assertions `--test-threads=1` **33/33, 0 B/op**
(31 prior + `engine_fills_capture_append` + `strategy_set_fanout`;
`engine_tick_with_latency_record` now includes the empty AI lane).
NOT run (item-17 gates): full workspace nextest, fuzz time-boxed runs,
pytest.

**2. Interim state / deviations (all within design intent):**

- **`AI_DRAIN_BUDGET = 8`, its own const** (design §2 named no number):
  bounds worst-case tick contribution; drains a full 1024 ring in 128
  iterations. Drain sits after the fill pump, before the timer —
  control plane yields to market data within an iteration.
- **Drain-site malformed re-check** counts into an engine-local field
  (`Engine::ai_drain_malformed`, mirrored as
  `engine_ai_drain_malformed_total`), NOT into
  `AiIngressStatus::malformed_total` — preserves the per-field
  single-writer discipline documented in `status.rs`. Deliberately no
  `debug_assert` on that branch: §11 requires the "malformed rejected"
  drain test, and the asserting boundary is the ingress.
- **engine deps grew**: `ingress-ai` (the status-slot type; drain site
  is the designated `expired_total` writer) and `core-io`
  (`SlotCapture<Fill>`). mio rides along compile-time only.
- **Fills capture semantics**: fills stage BEFORE `on_fill` (a strategy
  panic cannot lose the record); flush cadence is the cli 5 s report
  tick (`Engine::maybe_flush_fill_capture`, called outside the metrics
  gate) + unconditional drain in `stop()`; `Observability` carries the
  sink to the engine loop (`Option::take` at boot). Registry pair
  `engine_fills_capture_{io_errors,records}` is mirrored centrally —
  the engine owns this capture, unlike per-thread venue sinks.
- **AI capture pair** (`engine_ingress_ai_capture_*`) mirrors from
  inside the spawn wrapper only after `run()` returns / on rebind /
  at exit — venue-wrapper parity; stale mid-run by design (live AI
  health = the centrally mirrored status counters).
- **Heartbeat-age sentinel picked**: gauge = `now − last_heartbeat_ns`;
  `last_heartbeat_ns == 0` ⇒ **-1** ("no heartbeat ever") — a literal
  0 would read as maximally fresh. Documented on the field.
- **Key handling**: `AI_INGRESS_HMAC_KEY` absent/empty ⇒ ingress-ai not
  spawned (info log, producer dropped — §3.3 unspawned shape);
  present-but-invalid ⇒ **fatal boot error** (typo ≠ absence). Parsed
  in cli (`parse_ai_hmac_key`, error strings carry no key material);
  deliberately NOT a `core_config::Config` field (`print-config`
  debug-prints Config). `AI_INGRESS_SOCK` IS a Config field
  (tilde-expanded like `log_dir`).
- **`spawn_ai` runs unpinned** — core 4 note honored: RSS owns core 4
  until item 16; rebind-on-error loop with 500 ms backoff; ruleset
  seam is a documented no-op closure until item 14.
- **`Consumers` carries `ai_cmds` + `ai_status`** — one struct change
  instead of a 7-wrapper signature ripple; documented on the field.
- **StrategySet interpretation calls** (all from §7 text, recorded for
  review): member sizes per the §7 sketch (`LatencyArb<64>`, ev 8,
  cross-arb 8×8, rule-tree 8); `on_start` validates
  **initially-enabled members only** (fail-fast preserved; members
  outside the mask boot unvalidated + inert — safe because all four
  have validation-only `on_start`; rustdoc'd invariant);
  **HaltRequest clears the enable mask** in addition to setting the
  sticky flag (kill-switch reading; no wire Resume);
  `enable_refused` counts BOTH halted refusals and reserved/unknown-
  slot enables (one counter per spec; capture stream disambiguates);
  `--strategy all` = requested ∩ **configured** mask (latency-arb from
  mandatory pairs; ev/cross-arb/rule-tree only when their flags are
  present) — every enabled member still validates; composed set is
  **paper-only until 8i**; single-name `--strategy` values keep their
  existing monomorphized paths (back-compat per §7).
- **`StrategyCounters::ai_enable_refused`** added as a defaulted trait
  method (0 for plain strategies) so the generic engine loop mirrors
  `engine_ai_enable_refused_total` without set-specific plumbing;
  `engine_strategy_set_active` gauge added so kind `"set"` is visible
  in the active-strategy family.
- **Config-helper dedup**: `configure_{latency_arb,ev,cross_arb,
  rule_tree}` extracted in `paper.rs`; the four standalone
  `engine_loop_*` fns and the set builder share them (no third copy of
  the ev loader exists).
- **Stale-rmeta incidents: 3** (strategy-core after the `on_ai` trait
  addition, core-io→engine after the `SlotCapture` hoist,
  strategy-core again after `ai_enable_refused`) — all cleared with
  `cargo clean -p <crate>` + retry per CLAUDE.md pitfall #10 corollary;
  no false greens (sandbox never used for cargo).
- Open defects: none known.

**3. Exact resume point.** Design §12 **item 8 —
`strategy-ai-exec`** (S4). First actions: read design §7
`strategy-ai-exec` paragraph + §5.4 staleness semantics; then new
crate `strategy-ai-exec`: fixed `[FairEntry; 64]` keyed by sym
(open-addressed linear probe, no hashing), entries
`{px_1e6, set_ns, ttl_ns, bias_1e6}` fed by SetFairValue/SetBias via
`on_ai`; deviation quoting vs venue book beyond edge param;
OrderIntent honor (paper; slot 4 target enforced by the shape table);
staleness = `now - last_accepted_frame_ns > 15 s` ⇒ pull quotes +
refuse intents (heartbeat liveness derived at the consumer, §4.3;
compile-time consts per §13 d6); `expire_on_silence` flag ties
entries to heartbeat liveness. Then wire it into `strategy-set` as
the slot-4 member (BIT_AI_EXEC becomes built; `--strategy` name
`ai-exec`?— check §7/§6 for the operator-facing name before
inventing one) + bench alloc gates ai-exec `on_tick`/`on_ai`. Item 9
(worker core I: frames.py/uds.py/state.py + fake UDS fixture +
golden vectors shared with Rust) follows in the same session if
context allows.

**4. S4 kickoff prompt (ready to paste):**

```
Stage 2 — 8f AI-Ingress, IMPLEMENTATION SESSION S4 (checklist items
8–9: strategy-ai-exec + worker core I), ISOLATED WORKTREE, parallel
to the soak/remediation session. NEVER touch
/Users/darkcite/trading-engine-multivenue.
WORKTREE: /Users/darkcite/trading-engine-stage2, branch
stage2/8f-ai-ingress (now at 1a45165 + the S3 handoff commit).
Verify get_project_modules against the worktree path FIRST; if the
MCP won't attach, stop.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED, §3/§13.1 capture
amendment in force) + latest phase-8f-progress.md entry (S3 handoff)
supersede the committed plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S3 handoff (state, deviations,
   StrategySet interpretation calls, this prompt)
2. docs/phase-8f-design.md §2 (ai-exec row), §3 (per-kind table:
   SetFairValue/SetBias/OrderIntent rows, expire_on_silence flag),
   §5.1+§5.3 (worker subsystems for item 9), §5.4 (staleness — BOTH
   sides), §7 (ai-exec paragraph), §10, §11 (ai-exec + Python rows),
   §13 decisions 1/2/6
3. CLAUDE.md (pitfalls #6/#10/#11)
Committed S3 surface to build on: engine ai lane
(Strategy::on_ai, AI_DRAIN_BUDGET, ai_status getter, fills capture,
ENGINE_FILLS_FILE), strategy_set::{StrategySet, mask_for_name,
BIT_*/SLOT_* consts, *_mut accessors, enable_refused_total},
StrategyCounters::ai_enable_refused, cli::{spawn_ai,
parse_ai_hmac_key, open_fills_capture, engine_loop_set_full},
core_io::SlotCapture, core-config ai_ingress_sock; S1/S2 surfaces
unchanged.
S4 SCOPE — item 8 then item 9, one commit each (8 may split
8a crate / 8b set+cli wiring):
item 8: new crate strategy-ai-exec per §7: [FairEntry; 64] fair
table keyed by sym, open-addressed LINEAR PROBE (no hashing in hot
path), entries {px_1e6, set_ns, ttl_ns, bias_1e6}; on_ai consumes
SetFairValue/SetBias (slot-4-agnostic kinds fan in via StrategySet)
+ OrderIntent (honor in paper: submit via ctx at intent px/qty/side/
venue; strategy_id already validated = 4 by the shape table);
deviation quoting: quote/take when venue book deviates beyond edge
param vs fair±bias; TTL per entry (engine-monotonic ts_ns base,
decision 1); staleness per §5.4: now − last_accepted_frame_ns >
15 s ⇒ pull AI quotes + refuse intents, recover on next valid
frame (heartbeat/any-frame liveness derived from popped frames —
NO extra atomics, §4.3; 5 s/15 s compile-time consts, env-tunable
in tests only, decision 6); expire_on_silence flag (bit 0) ties
entries to heartbeat liveness in addition to their TTL; then wire
as the slot-4 member of StrategySet (BIT_AI_EXEC becomes built,
mask_for_name gains the member name — take the operator-facing
name from the design, do not invent), --strategy all picks it up;
unit suite per §11 row (fair-table TTL, staleness pull 15 s,
expire_on_silence, OrderIntent paper flow) + bench alloc gates
ai-exec on_tick AND on_ai 0 B/op (release, --test-threads=1).
item 9: worker core I per §5.1/§5.3: claude-worker/src/
claude_worker/{frames.py, uds.py, state.py} — frames.py: AiCmd
pack (struct.pack_into into ONE preallocated 82-B bytearray per
connection), kinds, HMAC tag16, seq alloc via state.py; uds.py:
UDS client, connect, heartbeat-precedes-payload, send_frame,
single-writer; state.py: SQLite WAL (dedupe, seq allocator
surviving reconnects, event log incl. send timestamps — the
capture amendment made this the only structured send-time record);
tests: fake UDS server fixture (accept, len check, HMAC verify
with test key, record frames), golden frame vectors CHECKED IN and
shared with Rust (fixture bytes + test key; add a Rust-side test
in ingress-ai consuming the same fixture file so the two packers
cannot drift), imports-are-full test extended. Python: 3.14.7 via
uv 0.12.5 (/opt/homebrew/bin/uv), full `import x` only, no live
SDK calls (llm.py does not exist yet — item 11).
Tests: cargo on the Mac ONLY (pitfall #10; sandbox = greps only);
stale-rmeta playbook: 3 incidents in S3 — impossible unresolved-
import/method errors after edits ⇒ cargo clean -p <touched> and
retry. uv run pytest for claude-worker.
Hot-path rules absolute: zero alloc, no dyn, no tokio, no
serde_json. Tests override AI_INGRESS_SOCK under
/tmp/stage2-ai-<pid>/ (OWN parent dir — bind_uds force-chmods the
parent 0700), METRICS_BIND stage2-local (never 9191),
MULTIVENUE_LOG_DIR stage2-local; never ~/multivenue/logs, never
/tmp/soak*; no kill/pkill by name (by-PID of own test processes
only, after diagnosis).
HARD ISOLATION unchanged: all work ONLY under
/Users/darkcite/trading-engine-stage2; NEVER run `multivenue-engine
run`, bind 127.0.0.1:9191, or connect live venues. Git: small
commits ONLY on stage2/8f-ai-ingress inside the worktree; NO
merge/rebase/push; no git ops in the main checkout.
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long builds via
nohup > /tmp/stage2-build.log & then poll; projectPath = the
worktree. macOS test landmines (S2): SO_RCVTIMEO EINVAL on
peer-closed UDS (nonblocking EOF probes); std::thread::scope +
panicking scenario hangs without a StopOnDrop guard (pattern in
crates/ingress-ai/tests/uds_loopback.rs); `sample <pid>` is the
hang diagnostic. One commit per (sub-)item, tests green on the Mac
before the next, one-line status after each; ask before anything
ambiguous.
STOP after item 9: append §12.2 handoff (status, interim state,
resume point, S5 kickoff for items 10–11) to
docs/phase-8f-progress.md. All session notes to
phase-8f-progress.md ONLY — phase-8-progress.md belongs to the
other session. If context runs short: write interim state + exact
resume point + relaunch prompt into phase-8f-progress.md, then
tell me.
```

## 2026-08-15 — Session S4 (PHASE 0: merge to main; then items 8–9)

### Phase 0 — MERGE COMPLETE: stage2/8f-ai-ingress → main

Operator amendment (recorded at `ab46ee4`) supersedes the stored S4
prompt: S4 opens by merging the stage2 branch into main; the worktree
isolation era ends; all further 8f work happens in the main checkout.

- **Preconditions verified**: `pgrep -fl multivenue-engine` empty; main
  clean at `518679d` (G1 BLESSED — exactly the required minimum);
  worktree clean at `ab46ee4` on `stage2/8f-ai-ingress`.
- **Divergence at merge time** (merge base `b931c59`, the 8e commit):
  - `main..stage2/8f-ai-ingress`: **14 commits** — `d2b0be2` (Phase 0
    docs), `8feab93`/`4f086f3` (items 1–2), `71bbcae`/`0984b79`
    (items 3–4), `6e42132` (S1 handoff), `a43f87f`/`10fc7c8` (item 5),
    `0acb74a` (S2 handoff), `bb70c46`/`e80bf1d` (item 6), `1a45165`
    (item 7), `e124298` (S3 handoff), `ab46ee4` (amendment).
  - `stage2/8f-ai-ingress..main`: **2 commits** — `9d473ca` (G1
    remediation) + `518679d` (G1 BLESSED, progress-log only).
- **Merge commit**: `0ed0bfe`
  (`git merge --no-ff stage2/8f-ai-ingress`).
- **Conflicts (2, both mechanical; no hot-path/CLAUDE.md/design-doc
  conflicts — resolved without operator escalation per the conflict
  policy):**
  1. `crates/cli/src/paper.rs` — import block only. Both sides
     appended to the same `use` lines: main (`9d473ca`) added
     `Capture, ChannelEvent, NsTs` to the `core_types` import; stage2
     added `AiCmd, AI_RING_SIZE` + the `core_io::{SlotCapture,
     SlotKind}` / expanded `engine` / `ingress_ai` imports.
     Resolution: **union** (multiline `core_types` import carrying
     both sides' additions; stage2's other lines verbatim).
  2. `claude-worker/README.md` — main's remediation-era edit was a
     `3.12`→`3.14` version bump inside the old intro paragraph; the
     stage2 8f rewrite (item 2) replaced that paragraph wholesale.
     Resolution: **stage2 side** (the rewrite is authoritative;
     nothing of main's edit survives to lose — the rewritten worker
     is 3.14-native).
- **Stale-rmeta incident #4 (new failure mode — post-merge):** first
  post-merge `cargo check --workspace` failed in `ingress-ai` (11
  errors) + `strategy-set` (7) with E0425/E0432/E0599. First clean
  pass (`cargo clean -p` over the ten crates the merge obviously
  touched) still failed — `ingress-ai` down to 4 errors: `cannot find
  hmac_sha256_tag16 / ct_eq / HMAC_TAG16_LEN in core_crypto` while
  the merged source plainly contains them. Cause: `core-crypto` (and
  friends) kept pre-merge fingerprints. Playbook addendum: **after a
  merge, clean every workspace-local crate**, not just the visibly
  touched ones (`for d in crates/*/; do cargo clean -p $(basename
  $d); done`). Second run clean.
- **Post-merge gates — ALL GREEN on the Mac:**
  - `cargo check --workspace` clean
  - `cargo nextest run --workspace` **926/926**
  - `cargo test -p bench --test alloc_assertions --release --
    --test-threads=1` **33/33, 0 B/op**
  - fuzz `cargo check` clean
  - `claude-worker` `uv run pytest` **18/18**
- `.env` untouched in both checkouts. stage2 worktree left in place
  (operator decides its removal at session end).

From here: small commits directly on `main`, one per (sub-)item,
gates green before the next. Items 8–9 follow.

### S4 CLOSED — §12.2 handoff

**1. Status.** Phase 0 (merge) + items 8 and 9 of design §12 complete,
five commits on `main` (the worktree isolation era is over):

| step | commit | content |
|---|---|---|
| merge | `0ed0bfe` | `--no-ff` stage2/8f-ai-ingress → main (14 commits; 2 mechanical conflicts, §above) |
| record | `4ed29a3` | S4 Phase-0 progress entry |
| 8a | `3e0bace` | `strategy-ai-exec` crate: `AiExec<N>` fair table + staleness + intents, 24-test suite |
| 8b | `4ab1771` + `8b10ee4` | slot-4 wiring (BIT_AI_EXEC built, `mask_for_name` "ai-exec", cli `--strategy ai-exec`/`all`), bench alloc gates; follow-up pins last-writer-wins flag policy (missed the 8b add; no amend per session git rules) |
| 9 | `36ea4da` | worker core I: `frames.py`/`uds.py`/`state.py`, fake UDS server, golden vectors shared with Rust (`ingress-ai/tests/golden_frames.rs`) |

Gates at close (all on the Mac): `cargo check --workspace` clean;
`cargo nextest run --workspace` green (958 pre-item-9; final run
includes ingress-ai 37/37 with the 3 golden tests); release alloc
assertions `--test-threads=1` **35/35, 0 B/op** (33 + `ai_exec_on_tick`
+ `ai_exec_on_ai`); fuzz `cargo check` clean; claude-worker
`uv run pytest` **40/40** + ruff check + ruff format + mypy strict
clean.

**2. Interim state / deviations (all within design intent, recorded
for review):**

- **Item 8 staleness mechanics** (§5.4 + §4.3 + decision 1):
  `last_frame_ns` is fed by `cmd.ts_ns` — the ingress accept stamp —
  not the drain-time clock; before the first frame ever the strategy
  is stale by definition (fail-safe; table necessarily empty). The
  frame that ENDS a silence window is itself evaluated against the
  pre-frame gap: an `OrderIntent` in that position is **refused**
  (counted `intents_refused_stale`) yet still restores liveness —
  which is precisely why §5.4's heartbeat-precedes-payload exists;
  a well-behaved worker never hits the branch. The
  `expire_on_silence` sweep runs once, on the recovery frame (cold,
  bounded at N); while stale, quoting is pulled globally, so
  sweep-at-recovery is sufficient — no timer scan exists.
- **`expire_on_silence` is PERMANENT** once a silence window closes:
  flagged entries do not resurrect on recovery (unflagged ones do,
  TTL permitting); only a fresh upsert revives the symbol. Upserts
  are **last-writer-wins for the whole entry policy** (`set_ns`,
  `ttl_ns`, the flag): a follow-up unflagged SetBias clears the
  flag. Pinned by `upsert_flag_is_last_writer_wins` after the
  ai_exec_on_ai alloc gate caught exactly that interaction on its
  first run ("sweep must have run").
- **Fair table**: one TTL per entry, refreshed by either kind (§7
  sketch has one `set_ns`/`ttl_ns`); bias-only entries are held but
  never quote (no fair to deviate from); quote target =
  `fair + bias`, `target <= 0` ⇒ skip; probe home = `sym % N`,
  upsert scans the full chain before claiming a reusable slot, dead
  slots are re-keyed (never emptied) so probe chains cannot shorten
  — invariants rustdoc'd, collision/reuse/full covered by tests.
- **No boot symbol config for ai-exec**: MultiBook slots are claimed
  lazily on the first tick of a symbol with an upsert-live fair
  entry (the AI publishes the universe); `book_track_failed`
  counter when N=64 is exhausted. `on_start` validates parameters
  only (edge > 0, qty > 0) — the set's late-enable invariant holds.
  Consequently `engine_loop_set_full` marks ai-exec **configured
  unconditionally** and `--strategy all` now includes it;
  `--strategy ai-exec` is the single-bit set path (no pre-8f
  standalone arm existed; §7 "single name = single bit"); both
  paper-only until 8i.
- **Decision 6 reading**: `AI_STALENESS_NS = 15 s` is a compile-time
  const with NO runtime/env knob at all — tests exercise it with
  synthetic clocks (frame `ts_ns` and `ctx.now_ns()` are test
  inputs), which is stronger than "env-tunable in tests only".
- **Quoting style**: ev-convention post-only at mid,
  `DEFAULT_EDGE_1E6 = 20_000`, `DEFAULT_QTY = 10`, 250 ms
  `CooldownGate` keyed by book slot.
- **Naming**: operator-facing name `ai-exec` taken from the §7 slot
  enumeration — the same list the four existing `mask_for_name`
  names come from; no invention needed. `BIT_AI_EXEC_RESERVED` →
  `BIT_AI_EXEC` (grep-verified no external users).
- **Item 9**: `frames.py` packs verbatim — per-kind §3 argument
  validation belongs to the push verb (item 12) and,
  authoritatively, the engine accept path. Heartbeat-precedes-
  payload is enforced in code per connection (`send_cmd` refuses
  until a heartbeat succeeded on THIS connection; resets on
  reconnect). A seq allocated for a frame whose `sendall` fails is
  burned — the engine counts a gap, never faults (§3 semantics).
  `events.ts` carries the wall-clock send stamp, detail
  `seq=<n> kind=<k>` — the only structured send-time record (§3
  capture amendment). Full §5.3 schema created up front (items
  10–12 add consumers, not migrations); `ai_seq` reseeds on open if
  the row vanished. Ruff: tests-only PLR2004 ignore + inline
  PLR0913 noqas on the three 12-field wire functions.
- **Golden vectors**: `claude-worker/tests/fixtures/`
  `ai_frame_golden.txt` — 10 vectors, one per kind (negative px,
  eos flag, hash128 halves included), fixture-only key, format
  documented in-file; consumed by `test_frames.py` (pack
  byte-identity, buffer reuse, pad re-zeroing) AND
  `crates/ingress-ai/tests/golden_frames.rs` (kind coverage, pack
  byte-identity, parse round-trip) — the packers cannot drift
  silently.
- **Stale-rmeta incident #4** (Phase 0 above): post-merge, clean
  EVERY workspace-local crate, not just visibly-touched ones.
- **macOS landmine (NEW, S4)**: `AF_UNIX` `sun_path` caps at ~104
  bytes — pytest `tmp_path` socket paths overflow it
  ("AF_UNIX path too long"). Fixture sockets live in short
  `/tmp/cw-ai-<pid>-*/` dirs (`tests/conftest.py::short_sock_dir`,
  0700, hygiene-compliant). Joins the S2 list (SO_RCVTIMEO EINVAL,
  scope-hang StopOnDrop).
- **Push anomaly (operator attention)**: `origin/main`
  (github.com/darkcite/trading-engine-multivenue) advanced to
  `4ed29a3` mid-session. This session performed NO pushes (none are
  authorized). Most likely the operator pushed while following
  along, or an IDE auto-push fired. Nothing was done about it here;
  commits after `4ed29a3` (8a/8b/9) were local-ahead at close.
- `.env` untouched in both checkouts; stage2 worktree still in
  place (operator decision pending at session end).
- Open defects: none known.

**3. Exact resume point.** Design §12 **item 10 — worker core II**
(S5): `claude-worker/src/claude_worker/{pmlr.py, features.py,
feeds.py, labeling.py, backtest.py, commander.py}` per §5.1, all
tests with the LLM mocked at the (not-yet-existing) `llm.py` seam via
injected `complete_fn`, canned feeds/reports per §11. First actions:
read §5.1 subsystem specs + §9.1 carried patterns (tagger vocab,
rule-extractor schema, FakeClient conftest pattern) + §11 Python
rows (pmlr golden fixture FROM THE RUST WRITER — generate with
core-io, check in; canned pass/fail backtest reports; fetch --news
httpx-mock transport). `commander.py` drives `uds.py`/`state.py`
from item 9 — the fake UDS server fixture and golden vectors are
already in place. Item 11 (`daemon.py` + `serve` + `llm.py` seam +
serve-loop test) follows in-session if context allows.

**4. S5 kickoff prompt (ready to paste):**

```
Stage 2 — 8f AI-Ingress, SESSION S5 (checklist items 10–11: worker
core II + daemon/serve), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue — the worktree isolation
era ended in S4 (merge 0ed0bfe); small commits directly on main,
one per (sub-)item, tests green on the Mac before the next. NO
push, NO rebase, NO history rewrite, NO new branches. Do NOT touch
.env. Do NOT write phase-8-progress.md (closed soak history).
Verify get_project_modules against the main checkout FIRST; if the
MCP won't attach, stop.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED, §3/§13.1 capture
amendment in force) + latest phase-8f-progress.md entries (S4
handoff) supersede the committed plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S4 handoff (state, deviations incl.
   staleness/eos interpretation calls, landmines, push anomaly)
2. docs/phase-8f-design.md §5.1 (all six subsystems), §5.2, §5.3
   (prompt_cache consumers arrive now), §6 (verb surface — context
   for commander/backtest gates), §9.1 (carried patterns: tagger
   vocab, rule-extractor schema, FakeClient), §11 Python rows
   (pmlr golden fixture from the RUST writer, canned backtest
   reports, fetch --news httpx-mock, serve-loop test is item 11)
3. CLAUDE.md (pitfalls #6/#10/#11)
Committed S4 surface to build on: strategy_ai_exec::AiExec (slot-4
member, BIT_AI_EXEC built, --strategy ai-exec/all), worker item-9
core: claude_worker.frames (pack_frame/tag16/kind+sentinel consts),
claude_worker.uds (UdsClient, heartbeat-precedes-payload enforced,
UdsError), claude_worker.state (State: next_seq/mark_seen/
record_event/record_frame_sent/events; full §5.3 schema),
tests/conftest.py (FakeUdsServer + short_sock_dir + TEST_KEY),
tests/fixtures/ai_frame_golden.txt (+ Rust twin test in
ingress-ai/tests/golden_frames.rs — extend BOTH sides if frames
change).
S5 SCOPE — item 10 then item 11, one commit each (10 may split by
module group):
item 10: pmlr.py (read-only mmap PMLR v1/v2 reader; golden fixture
bytes produced by the Rust core-io writer, checked in), features.py
(replay-log features + rate-budgeted REST secondary; positions/P&L
reconstruction from engine-fills.pmlr per §2/§5.1 — 8f emits fill
data + fetch position summary only), feeds.py (httpx RSS/Atom
poll, dedupe via state.mark_seen, triage→escalate via INJECTED
complete_fn — llm.py does not exist until item 11), labeling.py
(Sonnet labeling prompt + strict parse, §9.1 schema), backtest.py
(subprocess seam to `multivenue-engine backtest` — mockable, canned
reports; GATES per §5.1 with thresholds in worker config),
commander.py (labeled events + policy → AiCmd frames via item-9
frames/uds; TTL from half-life; Heartbeat cadence 5 s in serve).
Tests: all LLM calls mocked (injected complete_fn / FakeClient
pattern), fake UDS server for commander, canned feed XML, canned
pass/fail reports, pmlr golden fixture v1+v2, dedupe against
pre-seeded SQLite.
item 11: llm.py (THE SDK seam: make_client()/complete(); serve-only
construction), daemon.py (serve composition loop: cadences off a
monotonic clock, single-threaded, SIGTERM/SIGINT → flush SQLite,
close UDS, exit 0), `serve` frontend in a minimal cli.py entry IF
required by the serve-loop test (full verb surface is item 12 —
do not build verbs early); serve-loop test: one composed iteration
with canned feeds + mocked LLM + fake UDS (dedupe honored, cache
hit on second identical prompt, heartbeat emitted, clean SIGTERM).
ANTHROPIC_API_KEY read in ServeConfig ONLY; verbs never construct
clients (asserted by existing config split).
Python: 3.14.7 via uv 0.12.5 (/opt/homebrew/bin/uv); full
`import x` only (no aliases); ruff format + check + mypy strict +
pytest green before each commit; no live SDK calls, no live feeds
(httpx mocked), no live engine.
Rust gates only if Rust files are touched (none expected in 10–11).
Cargo on the Mac ONLY (pitfall #10; sandbox = greps only);
stale-rmeta playbook incl. the S4 post-merge addendum (clean ALL
workspace-local crates after big tree rewrites).
Test hygiene: sockets via tests/conftest.short_sock_dir()
(/tmp/cw-ai-<pid>-*/ — macOS sun_path ~104-B cap, S4 landmine);
METRICS_BIND test-local (NEVER 9191); MULTIVENUE_LOG_DIR test-local
(NEVER ~/multivenue/logs); NEVER run `multivenue-engine run` or
connect live venues (8f live demo stays operator-gated); no
kill/pkill by name (by-PID of own test processes only, after
diagnosis).
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long runs via
nohup > /tmp/8f-build.log & then poll; projectPath =
/Users/darkcite/trading-engine-multivenue. macOS landmines: AF_UNIX
sun_path cap (short_sock_dir), SO_RCVTIMEO EINVAL on peer-closed
UDS, std::thread::scope panic hangs without StopOnDrop, `sample
<pid>` for hang diagnosis. One-line status after each commit; ask
before anything ambiguous.
STOP after item 11: append the §12.2 handoff (status, interim
state, resume point, S6 kickoff for items 12–15) to
docs/phase-8f-progress.md. If context runs short: write interim
state + exact resume point + relaunch prompt there, then tell me.
```

## 2026-08-15 — Session S5 (items 10–11: worker core II + daemon/serve)

### S5 CLOSED — §12.2 handoff

**1. Status.** Items 10 and 11 of design §12 complete, four commits on
`main` (item 10 split by module group as authorized), all gates green
on the Mac before each:

| item | commit | content |
|---|---|---|
| 10a | `a45db8a` | data plane: `pmlr.py` (mmap v1/v2 reader, torn-tail tolerance) + `features.py` (tick features → per-sym JSON, marks, RestBudget, positions/P&L from `engine-fills.pmlr`, HIP-4 netting) + Rust-writer golden fixtures |
| 10b | `cf8925b` | news plane: `labeling.py` (§9.1 vocab + strict parsers) + `feeds.py` (httpx RSS/Atom, dedupe, jittered cadence, triage→escalate over injected `complete_fn`) + `state.py` prompt-cache consumers (`cached_complete`) |
| 10c | `f5db691` | action plane: `backtest.py` (mockable subprocess seam, STRICT harness contract, §5.1 gates in code, report written pass AND fail) + `commander.py` (labels→SetBias, TTL from half-life clamped, 5 s heartbeat cadence) |
| 11 | `163e9f9` | `llm.py` (THE SDK seam, serve-only construction) + `daemon.py` (single-threaded serve loop, signal-clean shutdown) + conftest `FakeClient`; §11 serve-loop test (dedupe ✓ cache-hit ✓ heartbeat ✓ SIGTERM ✓) |

Gates at close (Mac): claude-worker `uv run pytest` **131/131**
(40 → 72 → 100 → 124 → 131), `ruff format` + `ruff check` + `mypy
--strict` clean before every commit. Rust gates NOT run — **zero Rust
files touched** (per the S5 prompt); the only cargo invocation was the
scratch fixture generator in `/tmp` (never the workspace target dir).
`.env` untouched. No push/rebase/branch ops.

**2. Interim state / deviations (all within design intent, recorded
for review):**

- **PMLR golden fixtures** (`claude-worker/tests/fixtures/pmlr/`):
  bytes produced by the REAL `core-io::PmlrWriter` via a scratch crate
  OUTSIDE the workspace (unused-code rule keeps throwaway Rust out of
  the tree); generator source checked in verbatim as
  `generator.rs.txt` + regen steps in the fixture README. The **v1
  fixture** is the v2 writer's bytes with the header version patched
  to 1 and bytes 48..64 of every slot filled `0xAA` (v1's undefined
  padding) — the same crafted-v1 pattern `core-io`'s own reader tests
  use; no v1 writer exists to invoke.
- **`pmlr.py` torn-tail tolerance is a DOCUMENTED divergence** from
  `core-io::PmlrReader` (which rejects ragged payloads): the worker
  tails files the engine is still flushing (§11 positions row —
  "reader stops cleanly mid-flush"), so a trailing partial slot is
  ignored and surfaced as `Reader.torn`. Rust auditor semantics are
  unchanged.
- **HIP-4 pairing interpretation call**: SymbolId ordinals are
  boot-allocation order, NOT HIP-4 encodings — yes/no pairing is not
  derivable from the fill log. `features.hip4_pair_views` takes
  EXPLICIT `(yes_sym, no_sym)` pairs; netting mirror = `|yes − no|`
  marked at the yes leg, `flattened_qty = min(yes, no)` when both
  long. The pairs' config source is an **open item-12 wiring
  question** (below).
- **Positions math**: integer 1e12 cost-basis accounting; closes
  remove basis pro-rata by floor division with the remainder RETAINED
  in open cost — value is conserved exactly (pinned by
  `test_position_rounding_conserves_value`); full closes zero the
  basis exactly. No-mark symbols are carried at cost (unrealized 0,
  exposure |basis|) — no phantom P&L from truncated averages.
- **v1 tick files pin venue 0** in features (Phase-1 capture was
  Polymarket-only; v1 slots carry no venue byte).
- **Commander policy v1 doctrine**: labels are directional PRESSURE →
  `SetBias` only (the signed channel); bias = `bias_scale_1e6 ×
  confidence` signed by direction (default scale 20 000 = 2 ¢ at
  conf 1.0); TTL = half-life clamped to [1 s, 3600 s]; below
  `min_confidence` 0.7 ⇒ refused + counted, never sent;
  `expire_on_silence` default ON; SetBias row per §3: strategy_id/side
  0xFF, param_id 0, venue Ai. Commander holds no socket state;
  `UdsError` bubbles to the daemon (reconnect owner);
  `reset_cadence()` on fresh connections.
- **Backtest harness stdout contract defined NOW** (the 8h binary must
  conform): JSON with `schema_version = 1`, `ruleset_hash` REQUIRED
  equal to the worker's own sha256 of the ruleset file (anti-drift
  bind), `split`, `oos{net_pnl_usd,trades,trading_days,
  max_drawdown_usd}`, `bounds{max_order/symbol/total_notional_usd}`.
  Untrustworthy output (bad schema/hash/types) ⇒ `BacktestError`,
  NOTHING written; gate fail ⇒ worker report still written next to R
  (`R.report.json`) with `gates.all_passed = false` (verb maps to
  exit 3, item 12). **DD-cap default $200** taken from risk-policy's
  max-realized-loss-per-day line; bounds defaults 100/250/1000 per
  risk-policy. `ruleset_hashes()` also returns hash128 = sha256[..16]
  for §13-d5 Stage/Commit frames.
- **Daemon calls**: labels produced while the engine socket is down
  are DROPPED + counted (`labels_dropped_disconnected`) — TTL'd news
  pressure must not queue for a returning engine (§5.4 fail-safe
  reasoning). `symbol_map` (market name → SymbolId) is a `serve()`
  parameter; empty map = valid triage-only degraded mode. **Operator
  wiring for the market map is an open item-12 question** (below).
  `iterations=` bound exists for tests only; prod runs unbounded.
- **`cli.py` deliberately NOT created**: the §11 serve-loop test
  drives `daemon.serve` directly; the full verb surface is item 12
  (do-not-build-early rule held).
- **prompt_cache consumers** (first ever): `State.cache_get/cache_put`
  + `cached_complete(model, prompt_version, prompt, complete_fn)` —
  the single LLM-call gate; key = (model, sha256(version),
  sha256(content)) so template bumps invalidate cleanly. Serve-loop
  test proves a cache hit costs zero SDK calls.
- **conftest `FakeClient`** (§9.1 pattern) returns REAL
  `anthropic.types.TextBlock` instances so `llm.complete`'s isinstance
  narrowing is honest; no SDK network anywhere (client construction in
  `test_llm.py` is offline by SDK design).
- **Toolchain notes**: ruff format under `target-version = py314`
  now applies **PEP 758 unparenthesized multi-except** — expect
  `except ValueError, TypeError:` shapes; they are valid 3.14. The
  Cowork sandbox's repo mount went stale again mid-session
  (pitfall #10 corollary held: greps only, all gates on the Mac).
- **Push anomaly CONTINUES (operator attention)**: `origin/main`
  advanced to `a45db8a` (item 10a) mid-session — this session
  performed NO pushes (none authorized). Same signature as the S4
  anomaly (operator following along, or IDE auto-push). At close,
  `main` is ahead of origin by 10b/10c/11 + this handoff commit.
- **OPEN QUESTIONS for the operator (answer before/at S6 kickoff):**
  1. **`strategist.py` has NO §12 checklist item** (it exists only in
     the §2 file tree). Scope call: build strategist + its serve
     cadence + backtest-cadence composition in S6, or defer to 8h?
     Until then the daemon deliberately does not compose a
     strategist/backtest cadence (no dead code).
  2. **Market-map + HIP-4-pairs config source** for serve and the
     verbs (labeling universe `{market name → SymbolId}`, and
     `(yes,no)` pairs for the positions netting view): file format /
     env key / CLI flag? Item 12 needs the decision to wire `fetch`,
     `positions`, and serve.
- Open defects: none known.

**3. Exact resume point.** Design §12 **item 12 — operator verbs in
`cli.py`** (S6): Typer app per the §6 EXACT surface (`serve`, `fetch
[--replay-dir|--symbols|--no-rest|--news]`, `backtest --ruleset
[--replay-dir|--split]`, `push --kind …` with per-kind §3 required-arg
validation and 1e6 float scaling, `positions [--run-dir|latest]
[--json]` read-only (no UDS, no heartbeat, no seq), `stage-ruleset
--ruleset --report` (GATE BINDING: recompute sha256, require
`REP.ruleset_hash` match + `gates.all_passed` + our schema version;
record in `rulesets` with author_mode; send RulesetStage{hash128}; NO
OVERRIDE FLAG EXISTS), `commit-ruleset --hash|--ruleset`); global exit
codes 0/2/3/4/5/1; every verb: BaseConfig (never the key), SQLite,
UDS, Heartbeat-precedes-payload, close. First actions: read design §6
in full + the item-10 surfaces (`backtest.report_path_for`,
`ruleset_hashes`, `features.position_views/hip4_pair_views`,
`feeds.fetch_feed/dedupe_items`), get the two operator answers above,
then build `cli.py` + per-verb tests (fake UDS, canned reports,
`--help`-parse no-override assertion, ANTHROPIC_API_KEY-unset
invariant). Items 13 (ai-session.md + scripted subprocess session
test), 14 (Rust: ingress-ai ruleset side-path stub — Rust gates
apply), 15 (docs sweep §9.2 + .env.example) follow in-session if
context allows.

**4. S6 kickoff prompt (ready to paste):**

```
Stage 2 — 8f AI-Ingress, SESSION S6 (checklist items 12–15: operator
verbs + ai-session.md + ruleset side-path stub + docs sweep), MAIN
CHECKOUT /Users/darkcite/trading-engine-multivenue; small commits
directly on main, one per (sub-)item, tests green on the Mac before
the next. NO push, NO rebase, NO history rewrite, NO new branches.
Do NOT touch .env. Do NOT write phase-8-progress.md.
Verify get_project_modules against the main checkout FIRST; if the
MCP won't attach, stop.
OPERATOR ANSWERS REQUIRED AT KICKOFF (S5 open questions): (1)
strategist.py scope (build in S6 vs defer to 8h — no §12 item
exists); (2) market-map + HIP-4-pairs config source (file/env/flag)
for serve, fetch, positions. If unanswered, ASK before item 12
wiring.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED, §3/§13.1 capture
amendment in force) + latest phase-8f-progress.md entries (S5
handoff: harness stdout contract, commander SetBias doctrine,
torn-tail divergence, HIP-4 explicit pairs) supersede the committed
plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S5 handoff (deviations, contracts,
   open questions, push anomaly)
2. docs/phase-8f-design.md §6 (EXACT verb surface + exit codes +
   notes), §5.2 (dual-mode invariant), §7 "8f ruleset side-path
   (stub scope)" + §8 (ai-session.md outline — item 13 deliverable),
   §9.2 (docs-sweep list — item 15), §11 (gate binding, CLI-surface
   no-override test, scripted semi-manual session test, positions
   golden-fills row), §13 decisions 2/5
3. CLAUDE.md (pitfalls #6/#10/#11)
Committed S5 surface to build on: claude_worker.pmlr (Reader v1/v2,
torn flag), features (collect_run, read_fills, position_views,
hip4_pair_views, total_exposure, RestBudget), labeling (prompts +
strict parsers), feeds (fetch_feed, dedupe_items, NewsWatcher),
state (cached_complete, cache_get/put + item-9 surface), backtest
(run_backtest w/ injected run_fn, GateThresholds, ruleset_hashes,
report_path_for, REPORT_SCHEMA_VERSION=1, harness contract in the
S5 log), commander (Commander, Policy, HEARTBEAT_INTERVAL_NS),
llm (make_client/complete/complete_fn_for), daemon (serve with
iterations/stats injection), conftest (FakeUdsServer, FakeClient,
short_sock_dir, TEST_KEY), fixtures pmlr/ + ai_frame_golden.txt.
S6 SCOPE — items 12→13→14→15, one commit each (12 may split
12a verbs-core / 12b gate-binding+positions):
item 12: cli.py Typer verbs per §6 EXACTLY (serve/fetch/backtest/
push/positions/stage-ruleset/commit-ruleset), exit codes 0/2/3/4/5/1,
BaseConfig-only for verbs (ANTHROPIC_API_KEY never read — asserted),
implicit Heartbeat before every verb push (uds.py enforces),
per-kind §3 required-args in push (floats ×1e6 at pack), positions
read-only (tail engine-fills.pmlr + marks from CURRENT run dir,
HIP-4 netting via the answered config source, --json), stage-ruleset
GATE BINDING (sha256 recompute, REP match, gates_passed, schema
version, rulesets row author_mode session/auto, RulesetStage
hash128 frame), commit-ruleset (staged+passed row required);
[project.scripts] entry point added NOW (deferred from item 2);
tests per §11: every verb against fake UDS + canned reports,
gate-refusal exit 3 matrix, no-override --help parse test,
single-client interlock exit 4, key-unset invariant, positions
golden fills incl. torn tail + latest resolution.
item 13: docs/prompts/ai-session.md per §8 outline + scripted
semi-manual session test (§11): subprocess verbs against fake UDS +
canned reports: fetch → backtest pass → stage → commit → push
disable; refusal path: failing report → stage exit 3, commit of
unstaged hash exit 3; zero SDK construction proven.
item 14 (RUST): ingress-ai ruleset stage/commit side-path stub per
§7/§8: hash128 recompute from AI_RULESET_DIR file, table-fill stub
(8g), engine_ai_ruleset_{staged,committed,rejected}_total metrics,
seam already exists (S2 no-op closure). Rust gates apply: targeted
cargo nextest (ingress-ai + touched), cargo check --workspace,
release alloc assertions --test-threads=1 (0 B/op), fuzz cargo
check. Cargo on the Mac ONLY; stale-rmeta playbook incl. S4
post-merge addendum.
item 15: docs sweep per §9.2 exact list (local-setup, architecture,
CLAUDE.md worker lines + model table w/ Fable 5 strategist,
phase-8-architecture.svg 3.12→3.14 text node, PLAN.md two 3.12
mentions, .env.example new keys + RSS_FEEDS worker-only annotation)
+ config.example.toml reference block. Minimal touches; historical
records untouched.
Python: 3.14.7 via uv 0.12.5 (/opt/homebrew/bin/uv); full
`import x` only; ruff format + check + mypy strict + pytest green
before each commit; no live SDK calls, no live feeds, no live
engine; py314 note: ruff format applies PEP 758 unparenthesized
except.
Test hygiene: sockets via tests/conftest.short_sock_dir()
(/tmp/cw-ai-<pid>-*/); METRICS_BIND test-local (NEVER 9191);
MULTIVENUE_LOG_DIR test-local (NEVER ~/multivenue/logs); NEVER run
`multivenue-engine run` or connect live venues (8f live demo stays
operator-gated); no kill/pkill by name (by-PID of own test
processes only, after diagnosis).
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long runs via
nohup > /tmp/8f-build.log & then poll; projectPath =
/Users/darkcite/trading-engine-multivenue. macOS landmines: AF_UNIX
sun_path cap (short_sock_dir), SO_RCVTIMEO EINVAL on peer-closed
UDS, std::thread::scope panic hangs without StopOnDrop, `sample
<pid>` for hang diagnosis. Push anomaly is KNOWN (S4/S5): origin
may advance without session pushes — record, never act. One-line
status after each commit; ask before anything ambiguous.
STOP after item 15: append the §12.2 handoff (status, interim
state, resume point, S7 kickoff for items 16–17) to
docs/phase-8f-progress.md. If context runs short: write interim
state + exact resume point + relaunch prompt there, then tell me.
```

## 2026-08-15 — Session S6 (items 12–15: operator verbs + ai-session.md + ruleset side-path stub + docs sweep)

### Operator answers at kickoff (S5 open questions — RESOLVED)

1. **`strategist.py`: DEFERRED to 8h.** S6 stayed items 12–15; the
   daemon still composes no strategist/backtest cadence (no dead
   code). Fable 5 model const + docs rows landed via item 15.
2. **Market map + HIP-4 pairs: env-keyed JSON file.** New key
   `CLAUDE_WORKER_MARKET_MAP` (BaseConfig, default
   `~/multivenue/worker/market-map.json`), one operator-editable file
   `{"markets": {name: SymbolId}, "hip4_pairs": [[yes,no],…]}`;
   missing file = empty map (triage-only serve, no netting view);
   malformed = exit 2 (strict bool-rejecting parse, `cli.py`
   `load_market_map`). serve reads `markets`; `push --symbol`
   resolves through it; `positions` reads `hip4_pairs`.

### S6 CLOSED — §12.2 handoff

**1. Status.** Items 12–15 of design §12 complete, five commits on
`main` (item 12 split 12a/12b as authorized), gates green on the Mac
before each:

| item | commit | content |
|---|---|---|
| 12a | `3097e81` | `cli.py` verbs core (serve/fetch/backtest/push), exit codes 0/2/3/4/5/1, market-map loader, `[project.scripts]` entry point, `StateError`, `collect_run(syms=)`; 51 tests |
| 12b | `795d99c` | stage-ruleset/commit-ruleset/positions complete the EXACT §6 surface; gate binding in `backtest.py` (the only Stage/Commit path); registry consumers in `state.py`; no-override + exit-3 matrix + positions golden-fills tests |
| 13 | `99b1bb3` | `docs/prompts/ai-session.md` (§8 outline in full) + `test_session_scripted.py` (§11): subprocess verbs vs fake UDS + fake PATH harness; zero SDK construction proven |
| 14 | `70d4dbb` | Rust: `ingress-ai/src/ruleset.rs` side-path stub behind the S2 seam; `engine_ai_ruleset_{staged,committed,rejected}_total`; core-config `ai_ruleset_dir`; `spawn_ai(ruleset_dir=…)` |
| 15 | `495ba13` | docs sweep §9.2: local-setup, architecture AI-plane rewrite, CLAUDE.md worker line + Fable 5 model row, PLAN §10.3 serve invocation, `.env.example` 8f blocks, config.example.toml `[ai]` reference block |

Gates at close (Mac): claude-worker `uv run pytest` **202/202**
(131 → 182 → 200 → 202), `ruff format` + `ruff check` + `mypy
--strict` clean before every Python commit. Rust (item 14): `cargo
check --workspace` clean; targeted `cargo nextest run -p ingress-ai
-p core-config -p cli -p engine` **111/111**; release alloc
assertions `--test-threads=1` **35/35, 0 B/op** (admit-path gate
untouched by the side path); fuzz `cargo check` clean. NOT run
(item-17 gates): full workspace nextest, fuzz time-boxed runs.

**2. Interim state / deviations (all recorded for review):**

- **Verb⇄socket policy (interpretation of the §6 preamble):** only
  frame-SENDING verbs (`push`, `stage-ruleset`, `commit-ruleset`)
  open the UDS and send the implicit Heartbeat; `fetch`/`backtest`/
  `positions` never touch the socket — a data pull must not signal
  AI liveness (§5.4; the §6 positions row and the kickoff's
  "Heartbeat before every verb PUSH" wording support this reading).
- **`push --kind heartbeat` sends exactly ONE frame** — it is its
  own implicit heartbeat; two identical frames would only burn a seq.
- **push strictness:** per-kind required AND allowed option sets
  (§3 table as data); anything outside the kind's set = exit 2
  BEFORE state/socket work (validation precedes transport — pinned
  by running the refusal matrix with no listener; bad args burn no
  seq). `order-intent` pins `strategy_id = 4` and REFUSES an
  explicit `--strategy`; `--venue ai` refused; `--ttl-s` that
  rounds to 0 ns refused (0 = no-expiry inversion).
- **`fetch --no-rest` is a parsed no-op:** RestBudget mechanics are
  S5 surface, but venue REST URL consumers are 8h — with or without
  the flag there is nothing to fetch yet (documented in the verb).
- **`collect_run` gained an additive `syms=` filter** (backward
  compatible) for `fetch --symbols`; marks still cover all symbols.
- **`StateError(RuntimeError)`** introduced in `state.py` for the
  §6 exit-5 mapping (WAL failure, missing seq row, missing registry
  row on commit-mark); `sqlite3.Error` maps to 5 as well.
- **stage-ruleset ordering is §6-literal:** connect → heartbeat →
  bind-check → record row → send Stage. A binding refusal therefore
  leaves a lone heartbeat on the wire (legal per §5.4) and exits 3;
  with the engine DOWN the same invocation is exit 4 (transport
  precedes the gate check — §6 preamble order). Commit is
  check-row → send → mark committed_ts (send-then-record: a failed
  send leaves no phantom commit).
- **Registry semantics:** re-staging a hash refreshes `staged_ts`
  and CLEARS `committed_ts` — a new Stage supersedes an old Commit.
  The Rust stub implements the same machine (its `committed` state
  clears on a successful Stage).
- **NEW convention pinned (S6): ruleset artifact filename** =
  `AI_RULESET_DIR/<hash128-hex>.json`, hash128-hex = FIRST 32 hex
  chars of the full sha256 (= first 16 bytes). Forced by §13-d5:
  the frame carries only hash128, so the engine can only resolve a
  name derivable from it. Taught in `ai-session.md` §4 step 5
  (install cp before staging), implemented by `ruleset.rs`,
  documented in `.env.example`/config.example.toml.
- **Item 14 scope vs design §7:** the JSON bounds-check (≤256 rows,
  symbols exist, caps ≤ risk-policy) and the double-buffered table
  flip are DEFERRED to 8g per the S6 kickoff item-14 scope (kickoff
  supersedes the fuller §7 paragraph). 8f stub = filename resolve +
  full-sha256 recompute + first-16-bytes equality + staged/committed
  state + counters; validated bytes are dropped at the documented
  8g table-fill point. Side-path allocation (`fs::read`,
  `PathBuf::join`) is documented control-plane: ruleset kinds only,
  after capture+push; the 0 B/op admit gate is unaffected.
- **`engine_ai_ruleset_*` counters live in `AiIngressStatus`**
  (writer: ingress thread — the seam runs on it; per-field
  single-writer discipline holds) and mirror through the existing
  ai-family delta machinery in cli.
- **`commit-ruleset --hash` takes the FULL sha256 (64 hex)** —
  "HEX32" read as 32 bytes; matches the registry PK and the
  stage-ruleset stdout. `--by session|auto` exists on stage-ruleset
  per the §6 attribution text (default `session`; refuses others).
- **`market_map_path` joined `BaseConfig`** (all modes) —
  `ServeConfig` construction sites in tests updated; two config
  tests added.
- **core-config gained `ai_ruleset_dir`** (`AI_RULESET_DIR`,
  tilde-expanded, default `~/multivenue/artifacts/rulesets`);
  `spawn_ai` signature grew `ruleset_dir: PathBuf` (boot log prints
  it; `#[allow(clippy::too_many_arguments)]` per the boot-wiring
  pattern).
- **Toolchain note: Typer 0.27 is click-free** — no `click` module
  in the venv; `typer.testing.Result` is the invoke result type.
  The `[project.scripts]` entry point (deferred from item 2) is
  installed by uv and exercised as a real subprocess by the
  scripted session test.
- **Docs sweep delta:** `phase-8-architecture.svg` and the PLAN.md
  `3.12` mentions were ALREADY 3.14 on main (remediation-era
  edits) — no touch. PLAN §10.3 worker line gained the
  `claude-worker serve` launchd invocation instead (§9.2's launchd
  note). Historical records untouched.
- **Push anomaly CONTINUES (operator attention):** local
  `origin/main` ref reads `38e599b` (the S5 handoff commit) — i.e.
  origin advanced past mid-S5 `a45db8a` at some point; this session
  performed NO pushes and NO fetches (none authorized). At close,
  `main` is locally ahead of that ref by the five S6 commits.
  Recorded, not acted on (S4/S5 doctrine).
- `.env` untouched. No push/rebase/branch/history ops. Open
  defects: none known.

**3. Exact resume point.** Design §12 **item 16 — RSS removal**
(S7, sequenced LAST deliberately, ONE commit): the §9.3 blast-radius
sweep — delete `crates/ingress-rss/` + `crates/strategy-news/`
(corpse), `fuzz/fuzz_targets/rss_item.rs` + its `fuzz/Cargo.toml`
entry, the 2 RSS bench alloc-assertions, the ~76 `paper.rs` refs
(`Rings.rss_signal`/`Consumers.rss_signal`/`spawn_rss`/`RssFeed`/
`engine_rss_*`), `core-config` `rss_feeds_csv` + `RSS_FEEDS` engine
key (worker annotation already in `.env.example` from item 15),
CPU core 4 → `ingress-ai` (un-comment the pin in `spawn_ai` — the
"unpinned until item 16" note dies), `.claude/agents/
parser-property-tester.md` scope line, `SignalSource::Rss = 1`
STAYS reserved. Workspace must be green after the single commit.
Then **item 17 — final gates**: full `cargo nextest run
--workspace`, release alloc assertions `--test-threads=1` 0 B/op,
fuzz targets `cargo check` (+ time-boxed runs if the operator
wants the CI default), `uv run pytest`, and the phase-8f closing
progress entry.

**4. S7 kickoff prompt (ready to paste):**

```
Stage 2 — 8f AI-Ingress, SESSION S7 (checklist items 16–17: RSS
removal + final gates), MAIN CHECKOUT
/Users/darkcite/trading-engine-multivenue; item 16 is ONE commit,
item 17 is gates + the closing progress entry. NO push, NO rebase,
NO history rewrite, NO new branches. Do NOT touch .env. Do NOT
write phase-8-progress.md.
Verify get_project_modules against the main checkout FIRST; if the
MCP won't attach, stop.
AUTHORITY: docs/phase-8f-design.md (§13 LOCKED, §3/§13.1 capture
amendment in force) + latest phase-8f-progress.md entries (S6
handoff: verb⇄socket policy, hash128 filename convention, item-14
8g deferrals, push anomaly) supersede the committed plan.
REQUIRED READING, in order:
1. docs/phase-8f-progress.md — S6 handoff (deviations, resume
   point, this prompt)
2. docs/phase-8f-design.md §9.3 (RSS blast radius — the exact
   sweep list), §12 items 16–17, §1 exit criteria
3. CLAUDE.md (pitfalls #10/#11)
S7 SCOPE:
item 16 (ONE commit): delete crates/ingress-rss +
crates/strategy-news + fuzz/fuzz_targets/rss_item.rs (+ its
fuzz/Cargo.toml entry) + the 2 RSS bench alloc-assertions; sweep
paper.rs (~76 refs: Rings.rss_signal, Consumers.rss_signal,
spawn_rss, RssFeed, engine_rss_* metrics, workspace Cargo.toml
members/deps); core-config: drop rss_feeds_csv + the engine
RSS_FEEDS read (worker keeps the env key — .env.example already
annotated in item 15; do NOT touch the worker); cli: pin
ingress-ai to core 4 (the "RSS owns core 4" note dies —
spawn_ai's unpinned comment + log_pin_outcome pattern);
.claude/agents/parser-property-tester.md scope ingress-rss →
ingress-ai; SignalSource::Rss = 1 STAYS reserved (wire-format.md
already marks it retired). Workspace green after the single
commit: cargo check --workspace + targeted nextest (cli, engine,
core-config, bench, strategy-set if touched) before declaring it.
item 17: final gates on the Mac — cargo nextest run --workspace;
release alloc assertions --test-threads=1 (0 B/op; count DROPS by
the 2 removed RSS gates — record the new total); fuzz cargo check
(time-boxed runs only on operator go); claude-worker uv run
pytest + ruff format + ruff check + mypy strict; then append the
8f CLOSING entry to docs/phase-8f-progress.md (§12 exit criteria
status: both modes proven in tests; live demo stays
operator-gated) and STOP — the live AI-cmd demo happens only on
explicit operator go with pgrep clean.
Cargo on the Mac ONLY (pitfall #10; sandbox = greps only);
stale-rmeta playbook incl. the S4 post-merge addendum (after the
RSS tree-ectomy expect stale rmeta — clean ALL workspace-local
crates on impossible errors). Python untouched in item 16 —
worker RSS surface (feeds.py) STAYS.
Test hygiene: sockets via tests/conftest.short_sock_dir()
(/tmp/cw-ai-<pid>-*/); METRICS_BIND test-local (NEVER 9191);
MULTIVENUE_LOG_DIR test-local (NEVER ~/multivenue/logs); NEVER
run `multivenue-engine run` or connect live venues (8f live demo
stays operator-gated); no kill/pkill by name (by-PID of own test
processes only, after diagnosis).
SESSION FACTS: RustRover MCP execute_terminal_command MUST
executeInShell=true, ≤45 s per call — long runs via
nohup > /tmp/8f-build.log & then poll; projectPath =
/Users/darkcite/trading-engine-multivenue. macOS landmines:
AF_UNIX sun_path cap (short_sock_dir), SO_RCVTIMEO EINVAL on
peer-closed UDS, std::thread::scope panic hangs without
StopOnDrop, `sample <pid>` for hang diagnosis. Push anomaly is
KNOWN (S4–S6): origin may advance without session pushes —
record, never act. One-line status after each commit; ask before
anything ambiguous.
STOP after item 17: the closing entry doubles as the §12.2
handoff (status, exit-criteria checklist, anything left for the
operator-gated live demo). If context runs short: write interim
state + exact resume point + relaunch prompt into
docs/phase-8f-progress.md, then tell me.
```
