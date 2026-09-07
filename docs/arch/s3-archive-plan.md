# S3 Market-Data Archive Plan (Hetzner Object Storage) — S0–S7

Status: **DRAFT v2.1 — awaiting the operator rulings in §2 and the decisions
in §14.** Author: session 2026-09-03; **reviewed, fact-checked against the tree
and amended 2026-09-05** (every `file:line` below was read on that date; an
independent audit pass re-read every anchor — Appendix B lists what v1 got
wrong). Scope: cold storage + reuse of captured market data. Nothing in this
plan touches the Rust engine, the hot path, or Stage-3 work. Implementer: a
fresh Claude session (Opus-class). **§0 is the implementer contract — read it
first, then §13 (non-interference), then the phase you are on.**

---

## 0. Implementer contract (READ FIRST)

This section exists so the implementing session cannot mis-step on facts that
were verified for it. Every rule here is checkable in the tree.

### 0.1 What is verified true (use without re-deriving)

| Fact | Where |
|---|---|
| Python is **3.14.7**; `compression.zstd` is in the stdlib of the worker venv (libzstd 1.5.7); public names `ZstdCompressor`, `ZstdDecompressor`, `ZstdFile`, `open`, `compress`, `decompress`, `COMPRESSION_LEVEL_DEFAULT` (= 3) | `cd claude-worker && uv run python -c "import compression.zstd as z; print(z.zstd_version_info)"` |
| Worker deps are exactly 4: `anthropic`, `httpx`, `typer`, `structlog`; console script today = ONE (`claude-worker = "claude_worker.cli:main"`); `uv.lock` IS tracked | `claude-worker/pyproject.toml:18-32` |
| Installed: httpx **0.28.1**, httpcore 1.0.9, h11 **0.16.0** | worker venv |
| `hmac`/`hashlib` precedent: `frames.tag16(key, cmd_bytes)` = `hmac.new(key, cmd_bytes, hashlib.sha256).digest()[:TAG_LEN]`, `TAG_LEN = 16` | `claude-worker/src/claude_worker/frames.py:41, 198-201` |
| Secret-redaction precedent: `ai_ingress_hmac_key: bytes = dataclasses.field(repr=False)` / `anthropic_api_key: str = dataclasses.field(repr=False)`; config loads from `os.environ` ONLY (no dotenv anywhere in the worker); `load_base_from_env(env=None)` takes an injectable mapping | `config.py:35-40, 66-70, 113-123` |
| HTTP-client test seam: `def _make_http_client() -> httpx.Client:` returning `httpx.Client()`; tests monkeypatch it to `httpx.Client(transport=httpx.MockTransport(handler))` | `cli.py:219-221`; `tests/test_cli.py:442-448` |
| There is NO autouse network blocker in the tests; network hygiene = injection seams + the `network` marker | `tests/conftest.py` (one fixture, `fake_uds`, `:189`); `pyproject.toml:83` |
| Run-dir discovery law (Python): name starts `run-`, suffix `isdigit()`, sorted oldest-first | `features.py:45, 62-75` (`run_dirs`), `78-81` (`latest_run_dir`) |
| Run-dir discovery law (Rust): `parse_run_dir_name` = `strip_prefix("run-")` + all ASCII digits; `discover_runs` keeps `is_dir()` entries, sorted by `(epoch_ns, path)`; a `--dir` that is itself `run-<ns>` is used directly | `crates/cli/src/backtest.rs:394-400, 407-451` |
| `pnl_report --closed-day` enumerates runs with ITS OWN filter `pnl_report.select_runs(replay_dir, day)` (UTC-day match), then cuts ≤ 2 h units via `window_root.windows_of` + `window_root.cut_run` in `_audit_units`; spawn argv `[claude_worker.backtest.ENGINE_BINARY, "audit-pnl", "--dir", str(unit_dir)] + flags` | `pnl_report.py:200-217, 424-446, 470-481` |
| pnl report JSON top-level keys: `audit_pnl_version, day, runs, window, paper, strategies, vm_by_ruleset, regime, runs_detail, failed_runs, fee_flags`; readers check ONLY `audit_pnl_version == 1` (additive keys are safe) | `pnl_report.py:142, 354-367, 493, 503-504` |
| `window_root` public surface: `first_last_ts(path)`, `run_span(run_dir)`, `windows_of(run_dir, window_s)`, `complete_windows(run_dir)`, `run_pmlr_version(run_dir)`, `cut_run(run_dir, dst_root, from_s, to_s, report=None, seed=None)`, `pool_dir_for`, `pool_candidates`, `pool_windows`, `pool_ensure`, `symlink_root`; constants `HEADER_SIZE = 64`, `WINDOW_MAX_S = 7200.0`, `MANIFESTS = ("instrument-manifest.tsv", "options-manifest.tsv")`, `POOL_MIN_PMLR_VERSION = 3`; the header struct literal `"<4sHBxQ"` | `window_root.py:44-52, 46, 102-149, 208-215, 272-305, 317-378` |
| `cut_run` copies every `*.pmlr` (sorted glob) + the two TSV manifests; the `ai-cmds` special case keys on header `slot_kind == 4`, NOT on the file name | `window_root.py:196, 241-244` |
| PMLR header = 64 B: `magic b"PMLR"` @0, `version u16 LE` @4, `slot_kind u8` @6, pad @7, `epoch_ns u64 LE` @8 → `struct.Struct("<4sHBxQ")`; slot size = 64 B for kinds 0–6, 192 B for kind 7; `ts_ns` = first 8 B (`<Q`) of every slot | `docs/wire-format.md:539-546, 555-557`; `window_root.py:70-87` |
| A run dir holds, per venue label in `pm bn okx rpc deribit hl bybit`: `<v>-ticks.pmlr` (kind 0), `<v>-events.pmlr` (5), `<v>-signals.pmlr` (1), `<v>-opt-summary.pmlr` (6), `<v>-depth.pmlr` (7), optional `<v>-raw.tap`; engine-side `engine-fills.pmlr` (2), `engine-orders.pmlr` (3), `ai-cmds.pmlr` (4); sidecars `instrument-manifest.tsv`, `options-manifest.tsv`. Header-only files are exactly 64 B (never 0 B) | `docs/wire-format.md:450-475`; `crates/core-io/src/capture.rs:176-210`; live listing 2026-09-05 (37 files, 20 of them 64 B) |
| Capture flush interval = 1 s (`CAPTURE_FLUSH_INTERVAL_NS = 1_000_000_000`) | `crates/core-io/src/capture.rs:134-137` |
| `capture-catalog` CLI = `multivenue-engine capture-catalog --dir <root-or-run> [--gap-tolerance-ns N]`; JSON ALWAYS on stdout, summary on stderr; **there is no `--json` flag** | `crates/cli/src/bin/multivenue-engine.rs:76, 91-102, 454-457` |
| Catalog defect: the per-day array emits `venue_ticks[0..=5]` of a 7-wide array (`VENUE_LABELS = ["pm","bn","okx","rpc","deribit","hl","bybit"]`) so `bybit` is missing there; the `venue_totals` block iterates all 7 | `crates/cli/src/capture_catalog.rs:869-887, 797-819`; `backtest.rs:104` |
| `audit-pnl` stdout JSON carries NO filesystem path (its `runs` field is a count) — byte-identical stdout across two locations of the same run is achievable | `crates/cli/src/audit_pnl.rs:1072-1075` |
| `retention.sh`: zsh, `set -u`, defaults `MIN_FREE_GIB=25 TARGET_FREE_GIB=40 PROTECT_DAYS=7 ARCHIVE_DIR=~/multivenue/archive ARCHIVE_MODE=compress`; args `--root`, `--conf` ONLY (no `--dry-run`); sources `~/multivenue/retention.conf`; keep-all early `exit 0` when free ≥ MIN; candidates = `ls -d run-* \| sort \| sed '$d'` (newest dropped); per dir: TARGET check → epoch digit check → `age_days <= PROTECT_DAYS` ⇒ `break`; modes `move` / else compress (`tar -czf` then `rm -r`, failure ⇒ `rm -f` archive + `break`); always `exit 0` | `scripts/retention.sh:24-81` |
| `daily-restart.sh` runs every 60 s; 0000Z slot = SIGTERM (`:108`) then `retention.sh` **synchronously** (`:117`) with launchd's minimal PATH and WITHOUT `.env`; the 0020Z pnl subshell (`:129-148`) is the ONLY place that exports PATH (`:135`) and sources `.env` (`:136-140`); it defers while any `claude[-_]worke[r]` process is alive (`:125`) | `scripts/daily-restart.sh:105-150` |
| **CMDLINE LAW (RG6, standing):** every worker lane's overlap guard is `pgrep -f 'claude[-_]worke[r]'`, which matches ANY cmdline carrying `claude_worker`/`claude-worker` — a `-m claude_worker.x`, the package dir, **the venv path `claude-worker/.venv/bin/…`**. The boot-time recommit waits on it for at most 5 min then GIVES UP until the next boot (`vm_rows_active 0` for hours). A long-running process must therefore run as `~/multivenue/venv/bin/python3 <repo>/scripts/<launcher>.py` (venv dir-symlink alias + repo-root launcher) | `scripts/recommit-ruleset.sh:38-47`; `scripts/dashboard.sh:10-23, 30-40, 48`; `scripts/dashboard-serve.py:6-14` |
| `engine-wrapper.sh` sources `.env` with `set -a` ⇒ everything in `.env` is in the ENGINE process environment (file dirty from RG7 on 2026-09-05 — read, never edit) | `scripts/engine-wrapper.sh` (`set -a; . ./.env; set +a` block) |
| launchd agents are TEMPLATES in `launchd/*.plist` (`@REPO@`/`@HOME@`), rendered + bootstrapped by `scripts/install-launchd.sh` over an EXPLICIT label list — **re-running that installer restarts the engine** (bootout/bootstrap of `com.multivenue.engine`) | `scripts/install-launchd.sh:27-32, 36`; `launchd/com.multivenue.dashboard.plist` |
| Live operator state 2026-09-05 (15:30 local): `~/multivenue/retention.conf` = `PROTECT_DAYS=5` only; LaunchAgents = `caffeinate candles daily-restart dashboard engine regime`; engine up (`--strategy ai+icdp`); host `LocalHostName` = `Antons-MacBook-Pro` (mutable — never derive the host id from it); macOS 26.5.2; `/usr/sbin/taskpolicy`, `/usr/bin/nice` present; the Data volume is **460 GB, 392 used, 22 GiB free (95 %) — already UNDER `MIN_FREE_GIB=25`**, so `retention.sh` fires at the next 0000Z in `compress` mode | measured 2026-09-05 |
| Capture volume: `~/multivenue/logs` = **71 GB across 48 run dirs**; a 4.9 h run = 1.0 GB (`bn-ticks.pmlr` 636 MB of it) ⇒ **≈ 5 GB/day raw**; `~/multivenue/archive` = 47 `run-*.tar.gz` (3.6 GB) that ALREADY left the log root | measured 2026-09-05 |
| Hetzner Object Storage (docs read 2026-09-05): **SigV4 only**; TLS **1.3** (1.2 being retired); locations `fsn1`/`nbg1`/`hel1`; boto3 example uses `region_name="fsn1"`, `endpoint_url="https://fsn1.your-objectstorage.com"`, `addressing_style: virtual`; multipart "strongly recommended" > 100 MB, moderate parallelism preferred; ≥ ~1 MB files preferred (tiny-file churn discouraged); **503 under load** (retry logic required); versioning + lifecycle supported; **Object Lock only at bucket creation**; credentials are created in the Console, everything else via the S3 API | `docs.hetzner.com/storage/object-storage/{faq/general,faq/buckets-objects,getting-started/using-libraries}` |
| httpx 0.28.1 honours an explicit `Content-Length` and drops its own `Transfer-Encoding: chunked` (`Request._prepare`); h11 0.16.0 `Connection.send` does `b"".join(data_list)` (one copy per chunk) | verified in the worker venv 2026-09-05 |
| SigV4 golden vectors for THIS bucket's request shapes already exist as an untracked fixture (11 vectors, botocore-derived, fake credentials) | `claude-worker/tests/fixtures/sigv4/vectors.json` (Appendix A) |

### 0.2 What does NOT exist (do not reference, import, or "fix")

`claude_worker.objstore`, `claude_worker.archive`, `claude_worker.archive_config`,
`claude_worker.data_source`, `claude_worker.capture_catalog`, a `--json` flag
on `capture-catalog`, a `--dry-run` flag on `retention.sh`, any `MULTIVENUE_*`
key read by Python (Python reads `CLAUDE_WORKER_REPLAY_DIR`, never
`MULTIVENUE_LOG_DIR`), any `zstandard`/`zstd` import, a `mypy` Makefile target
(`make lint` = clippy only; `make py-lint` = ruff only), a `docs/` session-notes
file for this lane (this plan's §16 IS the log).

### 0.3 Absolute rules for the implementing session

1. **Zero Rust.** No file under `crates/`, no `Cargo.toml`/`Cargo.lock`, no
   `cargo build`, no `cargo nextest`, no relink, no engine restart, no launchd
   `bootstrap`/`bootout`/`kickstart`, and NEVER `scripts/install-launchd.sh`
   (it restarts the engine). S-LAW 1 is proven by `git diff` (§7 S7), not by a
   compile.
2. **Never touch the parallel lanes' files** — the verbatim per-lane list in
   §13.1 and the do-not-touch list in §11.3. If a phase here says "modify X"
   and X is dirty from another lane, STOP and ask the operator.
3. **Git:** explicit-path `git add` of THIS lane's paths only (§11); never
   `git add -A`/`-u`; commit only on the operator's ask; never push, rebase,
   branch, stash, or touch `.git` from the sandbox (sandbox git ops leave stale
   `index.lock`s — use the Mac/RustRover lane with `--no-optional-locks` for
   every git READ too).
4. **Secrets:** never read, print, `cat`, or `grep` `.env` or
   `~/multivenue/s3.env`; never put a credential in argv, a log line, a
   manifest, a report, a URL, an exception message, or this document. Test
   credentials are the fake ones in the fixture.
5. **No network in tests.** Every test goes through `httpx.MockTransport`. The
   only real-endpoint traffic is S0 (a git-excluded `tools_` one-shot) and the
   operator-run gates G-S4(c)/G-S5.
6. **Compile/test on the Mac only** (`uv run pytest` in `claude-worker/`);
   `pgrep -f pytes[t]` first — pytest runs across sessions COLLIDE; the
   RustRover MCP terminal window is ~45 s — `nohup … > /tmp/s3-<phase>.log
   2>&1 &` then poll for anything longer.
7. **Every new `.py`/`.sh` starts with the two SPDX lines** (`# SPDX-License-
   Identifier: Apache-2.0` / `# Copyright 2026 Anton (darkcite)`; shell: after
   the shebang). `make license-check` is a gate.
8. **Python: full `import x` only, never `from x import y`.** ruff enforces it
   (`force-single-line`, `ban-relative-imports`).
9. **The ≤ 2 h law binds every harness run in this plan** (G-S5 uses a
   `cut_run` window, never a whole run), every soak/test time, and every
   backfill invocation (≤ 2 h wall per run of the cycle).
10. **The CMDLINE LAW (§0.1) binds every archiver invocation longer than a few
    seconds:** launchd cycle, retention's verify, and the backfill run through
    `scripts/archive-run.py` under `~/multivenue/venv/bin/python3`. The
    console script `multivenue-archive` is for short interactive use only.
11. **When a fact is needed that is not in §0.1, read the file** — do not
    recall it. When context runs short: write §16 (state + exact resume point
    + relaunch prompt) and tell the operator.

---

## 1. Purpose

Today every byte of captured market data lives on one Mac. `scripts/retention.sh`
deletes the oldest `run-*` dirs when the Data volume drops below `MIN_FREE_GIB`
(25 GiB), after a local `tar -czf` into `~/multivenue/archive`, which nothing
ever reads. The volume already hit 100 % once (2026-09-02, ENOSPC-wedged every
capture lane) and sits at 22 GiB free today. Meanwhile the ≤ 2 h window law
(`docs/venue-time-capture-plan.md` §6.1, lines 133-158) means research quality
is a function of **how many disjoint windows already exist** — i.e. of how much
history we still hold. Deleting capture is deleting the only input the ICDP G1 /
VT5 / RG4-pool / RG7-soak gates have.

This plan gives the deployment a durable, S3-compatible second tier:

- **Write:** closed capture run dirs (and, later, the small derived stores and
  the 47 legacy tarballs) are pushed to `market-data-qu` on Hetzner Object
  Storage by a daily cycle, **ahead of** deletion; retention may delete a run
  only after it is verified in the bucket.
- **Read:** any consumer that today takes a run-dir path keeps taking a run-dir
  path; a resolver materialises the run from the bucket into a local cache first
  when it is not on disk.
- **Awareness:** the Python AI agent can ask, for every run, *where is this* —
  and every report it writes stamps the provenance it actually used.

### Non-goals

- No Rust change of any kind. The engine does not link, call, or know about S3.
- No object storage in any hot path, and no read path that can block capture.
- Not a backup of `.env`, `s3.env`, keys, `~/multivenue/state`, `market-map`.
- Not a Stage-3 activity (paper mode only, no dispatcher/signer/RiskGate).
- Not the dashboard (RG6 closed; an "archive" panel is a follow-up, §14 Q10).

---

## 2. Operator rulings required before S1 starts

CLAUDE.md, *Hard architectural rules → Deployment*, reads:

> **No cloud services, at any phase.** … no KMS, no SSM, no CloudWatch …

and `AGENTS.md:49` makes a cloud SDK a hard stop (`- **A cloud SDK**
(`aws-sdk-*`, `google-cloud-*`, etc.) → **stop**.`). Hetzner Object Storage is a
cloud service. **This plan cannot be implemented under the doctrine as
written.** The requested amendment is narrow and should be recorded verbatim in
CLAUDE.md at S7:

> **S-DOCTRINE (operator ruling, 2026-09-__):** the no-cloud rule binds the
> *trading path* — the engine binary, its dependency graph, and anything the
> engine loads, links, or calls at runtime. A **cold data archive that lives
> entirely outside the engine**, is reachable only from offline Python and two
> ops shell scripts, is optional at every layer, and whose absence changes no
> engine behaviour, is permitted. No cloud SDK enters the Cargo graph or
> `pyproject.toml` — the ban list in `.claude/hooks/no-forbidden-crates.sh:53`
> stands unchanged. No cloud service may ever be on a path the engine can
> block on.

Two secondary rulings are needed with it:

- **R-1 (credential rotation).** The access key pair for `market-data-qu` was
  transmitted in plaintext chat on 2026-09-03. Treat it as disclosed: rotate it
  in the Hetzner Console **before** S1, and put only the new pair in the
  credential file (§6). This plan contains no credential values and never will.
- **R-2 (bucket ACL).** `market-data-qu` must be private. S0 proves it with an
  unauthenticated `GET` that must return 403/401.

Until §2 is ruled, S0 (a read-only probe with a throwaway credential) is the only
step that may proceed. §14 lists the further decisions with recommended
defaults; the implementer proceeds on the recommendation ONLY where §14 says
"default unless overridden".

---

## 3. Standing laws for this subsystem

- **S-LAW 1 — engine isolation.** Zero lines of Rust change. No new crate, no
  new Cargo dependency, no new engine flag, no new engine env key. The engine
  writes PMLR to local disk exactly as it does today. Proof at close = the
  S3-lane commits touch nothing under `crates/`, `Cargo.toml`, `Cargo.lock`.
- **S-LAW 2 — optional at every layer.** Subsystem disabled (§6 enablement law)
  ⇒ every new code path is a no-op and every existing behaviour is
  byte-identical to today. Tested (G-S2, G-S4b), not claimed.
- **S-LAW 3 — archive before delete, verify before delete.** `retention.sh` may
  delete a run dir only after `verify <run>` exits 0: the run's index object
  exists, it is byte-identical to the run-prefix `_manifest.json`, and every
  listed object `HEAD`s with the manifest's `stored_bytes` and ETag. Any
  failure ⇒ keep the local copy and `break` (the script's existing
  stop-on-failure shape).
- **S-LAW 4 — never upload an open run.** Only run dirs that are not the newest
  `run-*` under the log root are eligible. The engine writes only into the
  newest dir; a non-newest dir is closed by construction. Belt-and-braces:
  `--force` on the newest dir is refused outright while any `*.pmlr` mtime is
  younger than 60 s or a `multivenue-engine run` process is alive.
- **S-LAW 5 — manifest-last, index-last commit.** S3 has no directory
  transaction. Commit order per run: data objects → `_manifest.json` under the
  run prefix → the index object `v1/index/<host>/run-<ns>.json` (same bytes).
  **A run is complete in the bucket iff its index object exists.** A partial
  upload is invisible to every reader and safely resumable (§7 S3 resume law).
- **S-LAW 6 — secrets never leave the credential file.** Credentials are read
  by the Python loader from the process environment or `~/multivenue/s3.env`
  (§6) only — no shell script sources the credential file. Never in argv
  (visible in `ps`), never in a log line, manifest, report, URL query string
  (no presigned URLs are ever generated), never echoed by an error message.
  `dataclasses.field(repr=False)` on the secret; a test asserts the secret
  bytes appear in no `repr`/`str`/exception/log record.
- **S-LAW 7 — the ≤ 2 h window law is unchanged.** The archive stores whole run
  dirs. Windows are still cut locally by `window_root.cut_run` after a pull.
  Pulling more data never authorises a wider window, a longer soak, or a gate
  in hours; a backfill invocation is bounded to ≤ 2 h of wall time.
- **S-LAW 8 — provenance is recorded, not assumed.** Any report produced from
  archived data carries the run's location and, if pulled, the manifest
  checksum it was verified against.
- **S-LAW 9 — the cache is disposable.** Anything under the pull cache can be
  deleted at any moment without loss, because the bucket holds it. The cache is
  never the only copy of anything; `gc` touches only the cache dir.
- **S-LAW 10 — no new runtime dependency.** The S3 client is handwritten SigV4
  over `hmac` + `hashlib` + `httpx` (+ stdlib `xml.etree.ElementTree`,
  `compression.zstd`, `mmap`, `urllib.parse`). `pyproject.toml` `dependencies`
  stays the 4-item list. `botocore` was used ONCE, in a throwaway sandbox, to
  generate the golden vectors — it never enters the repo, the venv, or a dev
  group.
- **S-LAW 11 — the bucket is append-only and permanent (operator intent,
  2026-09-03).** Nothing in this subsystem deletes, expires, or overwrites a
  completed bucket object. Structural: `ObjectStore` exposes **no
  `delete_object`**; the ONLY `DELETE`-verb request it can send is
  `abort_multipart(key, upload_id)`, which can affect nothing but the parts of
  an upload that never completed. `gc` operates on the local cache only. The
  only bucket lifecycle rule permitted is `AbortIncompleteMultipartUpload`
  (Hetzner-recommended; it expires no data) — S0 verifies nothing else is set.
  Overwrites: `put` is only ever called for a key that `HEAD`ed absent or
  size-mismatched (the resume law); with versioning ON (§14 Q9) an accidental
  overwrite is recoverable. The only deletion authority is the operator, by
  hand, in the Hetzner Console.
- **S-LAW 12 — non-interference.** This lane owns exactly the paths in §11 and
  never edits a file that is dirty from another lane (§13). One change at a
  time in the restart lane.
- **S-LAW 13 — never wait on data.** No gate in this plan is stated in hours or
  days; S4's contention check (§12.5) compares two ≤ 2 h windows that already
  exist.
- **S-LAW 14 — the frozen worker contract stands.** `backtest.py` argv /
  schema-1, the verb surface of `claude-worker`, and the 202 frozen pytest pin
  are untouched. `multivenue-archive` is a SECOND console script, not a verb.
- **S-LAW 15 — the cmdline law.** No archiver process that may run longer than
  a few seconds carries `claude_worker`/`claude-worker` in its cmdline (§0.1).
  The archiver does NOT participate in the worker-serialization guard: it
  touches no `state.db`, no `ai.sock`, no seq namespace — it reads run dirs and
  the bucket. Its own single-instance guard is `pgrep -f 'archive-ru[n].py'`.

---

## 4. Architecture

```
                        ┌──────────────────────────────┐
   HOT / LOCAL          │  multivenue-engine (Rust)    │   NEVER touches S3
   ───────────          │  PmlrCapture → local disk    │   (S-LAW 1)
                        └───────────────┬──────────────┘
                                        │ writes (newest run-* only)
                            ~/multivenue/logs/run-<epoch_ns>/
                                        │
        ┌───────────────────────────────┼────────────────────────────────────┐
        │ OFFLINE                       │                                    │
        │  com.multivenue.archive ──► scripts/archive-cycle.sh   STAGE A:    │
        │  (launchd, once a day,        │ push-pending, oldest first,        │
        │   quiet slot, budget 600 s)   │ budgeted, never the newest run     │
        │                               │                                    │
        │  com.multivenue.daily-restart ► scripts/retention.sh   STAGE B     │
        │  (0000Z, unchanged shape)     │ (pressure only): verify → delete   │
        │                               │                                    │
        │   ~/multivenue/s3.env ──(read by Python only)──┐                   │
        │                               ▼                │                   │
        │              ~/multivenue/venv/bin/python3 scripts/archive-run.py   │
        │                     (cmdline law, S-LAW 15)     │                   │
        │                               │                                    │
        │                     ┌─────────▼──────────────────────────┐         │
        │                     │ claude_worker.archive              │         │
        │                     │  push-run · push-pending · verify  │         │
        │                     │  list · pull · gc · status ·       │         │
        │                     │  push-tarball · push/pull-derived  │         │
        │                     └─────────┬──────────────────────────┘         │
        │                               │ uses                               │
        │        ┌──────────────────────▼──────────────┐                     │
        │        │ claude_worker.objstore                │  SigV4 + httpx     │
        │        │  head/get/put/list/multipart/abort    │  (no new deps)     │
        │        └──────────────────────┬────────────────┘                   │
        │   ┌───────────────────────────▼──────────────────────────┐         │
        │   │ claude_worker.data_source  (the resolver)            │         │
        │   │  list_runs() · where(run) · ensure_local(run)        │         │
        │   └───────────────┬──────────────────────────────────────┘         │
        │                   │ returns a REAL local path                      │
        │   ┌───────────────▼──────────────────────────────────────┐         │
        │   │ consumers, unchanged in shape: pnl_report (day mode) │         │
        │   │ cli positions (run id) · window_root.cut_run ·       │         │
        │   │ multivenue-engine {backtest,audit-pnl,audit-replay,  │         │
        │   │ capture-catalog}  ← all still take a directory path  │         │
        │   └──────────────────────────────────────────────────────┘         │
        └────────────────────────────────────────────────────────────────────┘
                                        │
                     https://market-data-qu.fsn1.your-objectstorage.com
```

The resolver is the only new concept the rest of the codebase sees, and it hands
back a `pathlib.Path` — which is what every consumer already accepts (Rust
`discover_runs` and Python `run_dirs`/`select_runs` gate on `is_dir()` + the
`run-<digits>` name and are indifferent to origin). That is why cache-on-demand
has a blast radius of zero.

---

## 5. Bucket layout

```
market-data-qu/
  v1/runs/<host_id>/run-<epoch_ns>/<original file name>.zst     one object per file (zstd frame)
  v1/runs/<host_id>/run-<epoch_ns>/_manifest.json               written second-to-last
  v1/index/<host_id>/run-<epoch_ns>.json                        SAME bytes, written LAST (completeness truth)
  v1/tarballs/<host_id>/run-<epoch_ns>.tar.gz                   S7: the legacy ~/multivenue/archive tarballs, verbatim
  v1/tarballs/<host_id>/run-<epoch_ns>.json                     their manifest (sha256 + size); index-last law applies
  v1/derived/<host_id>/candles/candles-<YYYYMMDD>.db.zst        S7
  v1/derived/<host_id>/reports/pnl-<YYYY-MM-DD>.json            S7
  v1/derived/<host_id>/features/<YYYYMMDD>/…                    S7
```

Design notes:

- **`epoch_ns` is the partition key.** Every epoch since 2001-09-09 is a
  19-digit ns timestamp until 2286-11; lexicographic `ListObjectsV2` order *is*
  chronological order. A range query is `start-after` on a synthesised
  `run-<ns>` key. Do not add `YYYY/MM/DD` levels — it would break this.
- **`v1/index/<host>/`** exists so that ONE `LIST` (1000 keys/page) enumerates
  every complete run with no `HEAD` per run; the run-prefix `_manifest.json`
  is the copy that travels with the data. `list` reads the index; manifests are
  cached locally under `<cache>/manifests/<run>.json` so repeat listings cost
  one `LIST` and only the `GET`s for manifests not yet cached.
- **`<host_id>` is REQUIRED and explicit** (`MULTIVENUE_S3_HOST_ID`, regex
  `^[a-z0-9][a-z0-9-]{0,31}$`). It is never derived from `LocalHostName`
  (`Antons-MacBook-Pro` — macOS renames it on conflict, which would silently
  split the archive). It preserves the "latency is measured per deployment"
  law: data from a different host is a different population.
- **`v1/`** is the layout version. A layout change is a new prefix, never an
  in-place rewrite.
- **File names are preserved verbatim** (`<name>.zst` is only the storage
  encoding). A pulled run dir is byte-identical to the original, so
  `discover_runs`, `VENUE_LABELS` joins, and `window_root.run_span`'s
  `glob("*.pmlr")` all behave identically.
- **Tiny objects.** 20 of a run's 37 files are 64-byte header-only PMLRs.
  Hetzner discourages *high-frequency* tiny-file churn; ~80 tiny objects/day is
  not that. They are stored verbatim (bundling would break the
  one-object-per-file resume law). Not negotiable without a layout version bump.

### `_manifest.json` (schema `archive_manifest_version: 1`)

```json
{
  "archive_manifest_version": 1,
  "run": "run-1788417289611943000",
  "epoch_ns": 1788417289611943000,
  "host_id": "mbp-m4",
  "uploaded_at_ns": 1788503689000000000,
  "tool_version": "archive/0.1.0",
  "stored_encoding": "zstd",
  "zstd_level": 3,
  "files": [
    {
      "name": "pm-ticks.pmlr",
      "key": "v1/runs/mbp-m4/run-1788417289611943000/pm-ticks.pmlr.zst",
      "size_bytes": 12220224,
      "sha256": "<hex of the ORIGINAL bytes>",
      "stored_bytes": 1187341,
      "stored_sha256": "<hex of the OBJECT bytes (the zstd frame)>",
      "etag": "<as returned by S3, quotes stripped>",
      "parts": 1,
      "pmlr_version": 3,
      "slot_kind": 0,
      "slots": 190940,
      "first_ts_ns": 1788417289700000000,
      "last_ts_ns": 1788424489100000000
    }
  ],
  "totals": { "files": 37, "size_bytes": 1049873664, "stored_bytes": 96331776 },
  "span_ns": [1788417289700000000, 1788424489100000000],
  "pmlr_version": 3,
  "windows_2h_complete": 2,
  "catalog": { "…verbatim stdout of `multivenue-engine capture-catalog --dir <run>`, or null…" },
  "catalog_note": "venue_totals is authoritative for per-venue ticks; the per-day venue_ticks array omits bybit (capture_catalog.rs:869-887)"
}
```

Laws embedded in the manifest:

- `span_ns` and `windows_2h_complete` mirror `window_root.run_span` +
  `complete_windows` EXACTLY: `first = min(first_ts_ns over files named
  *-ticks.pmlr that have ≥ 1 slot)`, `last = max(last_ts_ns over ALL files with
  ≥ 1 slot)`, `windows_2h_complete = floor((last − first) / 1e9 / 7200)`. A
  test asserts equality with `len(window_root.complete_windows(run_dir))` on
  the fixture run. `pmlr_version` = `window_root.run_pmlr_version(run_dir)`
  (min over `*-ticks.pmlr`; the pool admits ≥ 3).
- `first_ts_ns`/`last_ts_ns`/`slots` come from `window_root.first_last_ts` +
  the file size (`(size − 64) // slot_size`); `null` for non-PMLR files and
  header-only PMLRs.
- `catalog` is `null` — never a failed upload — when `multivenue-engine` is not
  on PATH or exits non-zero; the manifest records `"catalog_error": "<exit
  code or 'absent'>"` in that case. The embedded catalog is what lets the agent
  answer coverage questions without pulling a byte of PMLR.

---

## 6. Configuration surface

**Canonical location (recommended, §14 Q7): `~/multivenue/s3.env`** — a
`KEY=VALUE` file, `chmod 600`, never in git, mirrored by a committed
`s3.env.example` at the repo root (the `retention.conf.example` /
`fees.toml.example` precedent). **Only the Python loader reads it** (S-LAW 6):
process environment first, then the file named by `MULTIVENUE_S3_ENV_FILE`
(default `~/multivenue/s3.env`) fills any key the environment lacks. If the
operator rules "keep it in `.env`", the same keys go in `.env` and
`~/multivenue/retention.conf` exports `MULTIVENUE_S3_ENV_FILE=<repo>/.env`;
nothing else changes.

```sh
# ---- Object-storage archive (OPTIONAL — absent/incomplete = subsystem disabled) ----
MULTIVENUE_S3_ENABLED=1
MULTIVENUE_S3_ENDPOINT=fsn1.your-objectstorage.com   # host only, no scheme; S0 pins it
MULTIVENUE_S3_REGION=fsn1                            # SigV4 credential-scope region; S0 pins it
MULTIVENUE_S3_BUCKET=market-data-qu
MULTIVENUE_S3_ACCESS_KEY_ID=REPLACE_ME
MULTIVENUE_S3_SECRET_ACCESS_KEY=REPLACE_ME
MULTIVENUE_S3_HOST_ID=mbp-m4                         # REQUIRED; ^[a-z0-9][a-z0-9-]{0,31}$
MULTIVENUE_S3_ADDRESSING=virtual                     # virtual | path — S0 pins it
MULTIVENUE_S3_PREFIX=v1
MULTIVENUE_S3_CACHE_DIR=~/multivenue/s3-cache
MULTIVENUE_S3_CACHE_MAX_GIB=20                       # hard cap on <cache>/run-* + staging
MULTIVENUE_S3_CACHE_MIN_FREE_GIB=25                  # never pull/stage below this free space (= retention MIN_FREE_GIB)
MULTIVENUE_S3_ZSTD_LEVEL=3                           # S0-pinned: L3 beats L9 end-to-end on this link
MULTIVENUE_S3_PART_SIZE_MIB=8                        # S0-pinned (was 64): 5 MiB floor confirmed; 64 MiB parts blew the timeout
MULTIVENUE_S3_CONCURRENCY=1                          # parts in flight; keep 1 while the engine is live (§12.5)
MULTIVENUE_S3_TIMEOUT_S=300                          # S0-pinned (was 120): a 16 MiB GET took 28 s at background QoS
MULTIVENUE_S3_SKIP_GLOBS=                            # comma-separated, default EMPTY = push everything (§14 Q3)
# Environment-only (never in this file): MULTIVENUE_S3_ENV_FILE — path of this file when it is not the default.
```

**Naming decision.** The keys take the shared `MULTIVENUE_*` prefix, exactly as
`MULTIVENUE_LOG_DIR` does for the engine/worker/retention triple, because the
retention/cycle shell scripts NAME them (never their values) in
`retention.conf`. The engine never reads them (`core_config::Config::load`
reads named keys via `dotenvy`; unknown keys are ignored). `.env.example`'s
header (lines 15-18, "EVERY key in this file is consumed by …") gains ONE
sentence pointing at `s3.env.example`; the S3 keys themselves are NOT added to
`.env.example` unless Q7 is ruled "`.env`".

**Enablement law** (`archive_config.is_enabled()`): true iff
`MULTIVENUE_S3_ENABLED == "1"` AND `BUCKET`, `ENDPOINT`, `REGION`, `HOST_ID`,
`ACCESS_KEY_ID`, `SECRET_ACCESS_KEY` are all non-empty AND `HOST_ID` matches
its regex. Otherwise disabled with a one-word reason (`disabled: flag` /
`disabled: missing host_id` — never a value), the resolver reports every run as
`local` or `absent`, `retention.sh` falls back to `compress`, and no HTTP
client is ever constructed.

`~/multivenue/retention.conf` gains (documented in `retention.conf.example`):

```sh
ARCHIVE_MODE="s3"                       # compress (default) | move | s3   — retention stage B
S3_CYCLE_BUDGET_S=600                   # wall budget per archive-cycle run (stage A)
# MULTIVENUE_S3_ENV_FILE="/abs/path"    # only when Q7 = ".env"; exported to the archiver by both scripts
```

---

## 7. Phases

Each phase is independently committable and leaves the tree green. Commits are
operator-authorised; **no git operations without an explicit ask.** Every phase
has a PREFLIGHT (run before writing code) and a POSTFLIGHT (the gate). Record
both in §16.

### S0 — Endpoint probe and contract pin *(git-excluded one-shot; do first)*

PREFLIGHT: §2 R-1 rotated or a throwaway key issued.

The probe is a `claude-worker/tools_<name>.py` one-shot (the reserved,
git-excluded prefix — `docs/arch/research-tools-exclusion-plan.md:165-167`,
`.gitignore:104`; its concrete file name is deliberately NOT written in this
document — CLAUDE.md pitfall 16 and `make license-check` refuse a tracked doc
that names one). It reads the credentials through `archive_config`'s loader
shape (environment, then `~/multivenue/s3.env`) and answers, against the real
endpoint, with observed evidence:

1. **Addressing:** virtual-hosted `market-data-qu.fsn1.your-objectstorage.com`
   (Hetzner's documented default) vs path-style `fsn1.your-objectstorage.com/
   market-data-qu/…` — which does the endpoint accept; use the fixture's V2
   (virtual) and V10 (path) request shapes with the real key.
2. **Region token** in the credential scope: `fsn1` (documented) — confirm a
   signed `GET /?list-type=2&max-keys=1` returns 200; record whether
   `us-east-1` also validates (informational only; we use `fsn1`).
3. **Signature version:** SigV4 only per the docs — confirm a SigV2-style
   `Authorization: AWS …` header is rejected (informational).
4. **ListObjectsV2:** `continuation-token` pagination + `delimiter` +
   `start-after`; note whether the XML carries the
   `http://s3.amazonaws.com/doc/2006-03-01/` namespace (parse with `{*}Key`
   wildcards either way).
5. **Multipart:** initiate / upload-part / complete / abort on a 130 MiB
   throwaway object under `v1/_probe/`; min part size (expect 5 MiB), the
   `ETag` format of the completed object (expect `"<md5-of-part-md5s>-N"`).
6. **Integrity headers:** does `x-amz-content-sha256: <real digest>` get
   verified (send a WRONG digest for a 1 KiB object and expect
   `XAmzContentSHA256Mismatch`/400)? Is `x-amz-checksum-sha256` accepted and
   returned by `HEAD` with `x-amz-checksum-mode: ENABLED`? (Optional
   feature — the design does not depend on it.)
7. **Conditional PUT:** `If-None-Match: *` — honoured (412 on existing) or
   ignored? (We `HEAD` first regardless; informational.)
8. **R-2:** an unauthenticated `GET` of a known key returns 403/401, not 200;
   an unauthenticated `LIST` likewise.
9. **S-LAW 11 support:** `GET /?versioning` (fixture V11 shape) and
   `GET /?lifecycle` — record the current state. Recommend to the operator
   (§14 Q9): enable versioning (`PUT /?versioning` with
   `<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>`
   — the probe does it ONLY with `--enable-versioning`, an explicit operator
   action), and set the single lifecycle rule
   `AbortIncompleteMultipartUpload DaysAfterInitiation=7`. Note that Object
   Lock cannot be added to an existing bucket.
10. **Throughput + compression:** upload/download MB/s for the 130 MiB object;
    zstd ratio + MB/s at levels 3 and 9 on one real `bn-ticks.pmlr` and one
    `okx-depth.pmlr` (read-only; nothing under `~/multivenue/logs` is written).
11. **Cleanup:** the probe deletes ONLY the `v1/_probe/` keys it created (the
    shipped client has no delete; the probe is not the shipped client).

**Gate G-S0:** items 1, 2, 4, 5, 6, 8, 9 answered with observed evidence.
Findings (raw responses, timings, ratios) go to the git-excluded research vault
(`docs/research/`); ONLY the decisions (endpoint, region, addressing, part
size, whether checksum headers are used, zstd level) are copied into §6 and
§16. Nothing else in this plan is written in stone until G-S0 passes.

### S1 — `claude_worker.objstore` — the S3 client

PREFLIGHT: G-S0 passed; `git status` shows none of §11's paths dirty from
another lane; fixture `tests/fixtures/sigv4/vectors.json` present.

New module, ~300 lines, no new dependency (S-LAW 10). Exact surface:

```python
KEY_RE: re.Pattern[str] = re.compile(r"^[A-Za-z0-9._/-]+$")   # keys never need URI-encoding

@dataclasses.dataclass(frozen=True, slots=True)
class Credentials:
    access_key_id: str
    secret_access_key: str = dataclasses.field(repr=False)   # never logged (config.py:40 precedent)

@dataclasses.dataclass(frozen=True, slots=True)
class Endpoint:
    host: str            # "fsn1.your-objectstorage.com"
    region: str          # "fsn1"
    bucket: str
    addressing: str      # "virtual" | "path"
    def base_url(self) -> str: ...        # https://<bucket>.<host> or https://<host>/<bucket>
    def host_header(self) -> str: ...

class ObjectMeta(typing.NamedTuple):
    key: str; size: int; etag: str; last_modified: str

class ObjStoreError(Exception):
    status: int; code: str                # code = the S3 error XML <Code>; message never carries headers/query

class ObjectStore:
    def __init__(self, endpoint: Endpoint, creds: Credentials, http: httpx.Client,
                 *, timeout_s: float, clock: typing.Callable[[], float] = time.time) -> None: ...
    def head(self, key: str) -> ObjectMeta | None                                   # 404 → None
    def get(self, key: str, dst: pathlib.Path, *, expect_size: int | None = None) -> ObjectMeta   # streamed via resp.iter_raw()
    def get_bytes(self, key: str, *, max_bytes: int = 4 << 20) -> bytes           # manifests only
    def put(self, key: str, src: pathlib.Path, *, sha256_hex: str, md5_hex: str) -> ObjectMeta     # single PUT below the multipart threshold
    def put_bytes(self, key: str, body: bytes) -> ObjectMeta                       # manifests / index only
    def list(self, prefix: str, *, delimiter: str | None = None, start_after: str | None = None) -> collections.abc.Iterator[ObjectMeta]
    def list_prefixes(self, prefix: str, delimiter: str = "/") -> list[str]        # CommonPrefixes
    def multipart_put(self, key: str, src: pathlib.Path, *, part_size: int, sha256_hex: str,
                      part_md5_hex: list[str], budget: "Budget | None" = None) -> ObjectMeta
    def abort_multipart(self, key: str, upload_id: str) -> None                    # the ONLY DELETE-verb request (S-LAW 11)
    def get_versioning(self) -> str | None                                         # "Enabled" | "Suspended" | None
    # NO delete_object(). NO put_lifecycle(). NO presign(). Deliberate — S-LAW 11 / S-LAW 6.
```

**Signing — the exact algorithm** (also in the fixture's `canonical_rules`;
implement `sign(method, path, query_pairs, headers, payload_sha256_hex,
amz_date, creds, region) -> str` as a pure function and test IT against the
vectors, not the HTTP layer):

1. `canonical_uri` = the request path as sent (for `virtual`: `/` + key; for
   `path`: `/<bucket>/` + key). Keys are validated against `KEY_RE`, so no
   percent-encoding is ever needed; S3 never double-encodes. Bucket-level
   requests use `/`.
2. `canonical_query` = each `(name, value)` RFC-3986 percent-encoded with
   `urllib.parse.quote(s, safe="-_.~")` (space → `%20`, never `+`; uppercase
   hex), pairs sorted by encoded name, joined with `&`; a valueless subresource
   is `name=` (`uploads=`, `versioning=`, `lifecycle=`). The URL SENT carries
   the same encoded string — never build the URL from unencoded values.
3. `canonical_headers` = `name:value\n` for each signed header, names
   lowercased, values trimmed, sorted by name; `signed_headers` = the same
   names joined with `;`. Always sign `host`, `x-amz-content-sha256`,
   `x-amz-date`; also `range` when sent; also EVERY `x-amz-*` header you add
   (S3 rejects unsigned `x-amz-*`). Never sign `authorization`, `content-length`,
   `user-agent`, `accept-encoding`.
4. `canonical_request` = `METHOD\ncanonical_uri\ncanonical_query\n
   canonical_headers\n\nsigned_headers\npayload_sha256_hex` (the last line has
   no trailing newline; `canonical_headers` ends with `\n`, hence the blank
   line).
5. `string_to_sign` = `AWS4-HMAC-SHA256\n<amz_date>\n<yyyymmdd>/<region>/s3/
   aws4_request\n<hex sha256(canonical_request)>`.
6. `kSigning = HMAC(HMAC(HMAC(HMAC("AWS4"+secret, yyyymmdd), region), "s3"),
   "aws4_request")`; `signature = hex(HMAC(kSigning, string_to_sign))`.
7. `Authorization: AWS4-HMAC-SHA256 Credential=<akid>/<scope>,
   SignedHeaders=<signed>, Signature=<sig>`.
8. `x-amz-date` = `time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(clock()))`;
   payload digest always real (`e3b0c442…b855` for empty bodies) — the server
   verifies it, which is a free integrity check on every upload. **Re-sign on
   every retry attempt** (the date must be within 15 min of server time).
9. Send `Content-Length` explicitly on every PUT/POST with a body. httpx
   honours an explicit `Content-Length` and does NOT chunk (§0.1); S3 rejects
   chunked bodies without `aws-chunked` signing, which we do not implement.

**Multipart, the exact requests** (part size `PART_SIZE_MIB`, threshold = the
same; ≥ 5 MiB per non-final part; ≤ 10 000 parts):
`POST <key>?uploads` → parse `{*}UploadId`; `PUT <key>?partNumber=N&uploadId=ID`
with the part body and that part's real sha256 → keep the response `ETag`;
`POST <key>?uploadId=ID` with body `<CompleteMultipartUpload><Part>
<PartNumber>1</PartNumber><ETag>"…"</ETag></Part>…</CompleteMultipartUpload>`
→ the completed object's ETag is `md5(concat(binary part md5s))-N` — compute
it locally from `part_md5_hex` and compare; any failure after initiate ⇒
`abort_multipart` (best effort) and raise.

**ListObjectsV2 parsing:** `xml.etree.ElementTree.fromstring`, tags matched
with the namespace wildcard — **`root.findall("{*}Contents")`**, then `{*}Key`,
`{*}Size`, `{*}ETag`, `{*}LastModified`; `{*}IsTruncated`,
`{*}NextContinuationToken`, `{*}CommonPrefixes/{*}Prefix`. Loop until
`IsTruncated` is `false`.

> **S0 CORRECTION (2026-09-05).** This paragraph said `root.iter("{*}Contents")`
> through v2.1. **`Element.iter()` does not honour the `{*}` namespace
> wildcard** — only `find`/`findall`/`iterfind` do. Hetzner's ListObjectsV2 XML
> IS namespaced (`http://s3.amazonaws.com/doc/2006-03-01/`), so `iter("{*}…")`
> returns nothing, silently: the first probe run reported `page1_keys: 0` over
> five real objects while `IsTruncated`, read with `find`, was `true`. Use
> `findall`. The same correction applies to `{*}CommonPrefixes`.

**Retries:** bounded (6 attempts, backoff 1·2ⁿ s capped at 32 s, ±20 % jitter)
on 500/502/503/504/429 and `httpx.TransportError` only. Never on other 4xx.
Never unbounded — a stuck upload must fail loudly so S-LAW 3 keeps the local
copy.

**Copy ledger** (offline Python; the standing zero-copy preference asks that
every unavoidable copy be named):

| Step | Mechanism | Copy? |
|---|---|---|
| Hash a file | `mmap.mmap(fd, 0, access=ACCESS_READ)` + `hashlib.sha256().update(memoryview(mm)[a:b])` in 8 MiB slices | none — hashes the page-cache mapping in place (`pmlr.py:197` / `window_root.py:111` precedent) |
| Compress to staging | `ZstdCompressor.compress(memoryview(mm)[a:b])` → `f.write(chunk)` | one write per compressed chunk (the compressed stream must exist on disk before signing — its digest and length are needed up front) |
| Upload body | generator yielding `memoryview(mm_staged)[a:b]` as `content=` | **one copy per chunk inside httpx/h11** (`h11.Connection.send` joins into a fresh `bytes`, §0.1) + the TLS record copy in OpenSSL — both unavoidable without a handwritten HTTP/TLS client, which this plan rejects as surface |
| Download to disk | `resp.iter_raw()` → `f.write(chunk)`; sha256 updated on the same chunk | one copy per chunk (`iter_raw` skips the decode layer) |
| Manifest / index JSON | `json.dumps` | irrelevant (kilobytes) |

If httpx rejects `memoryview` chunks (verify in `test_put_streams_memoryview`),
yield `bytes(view)` per chunk and add that copy to the ledger — do not switch
transports.

**Gate G-S1** (`tests/test_objstore.py`): `test_sigv4_vectors_match_fixture`
passes all 11 vectors (canonical request, string-to-sign, signature,
Authorization header — each asserted separately so a failure names the stage);
MockTransport-backed tests for `head`/`get`/`put`/`put_bytes`/`list`
(paginated, namespaced and un-namespaced XML)/`multipart_put`/
`abort_multipart`/retry-on-503/no-retry-on-403/re-sign-on-retry/
Content-Length-not-chunked/`test_put_streams_memoryview`; pytest
additive-green over the frozen 202; `make py-lint` green; `uv run mypy
src/claude_worker/objstore.py` clean under the pyproject's strict config;
`make license-check` green.

### S2 — `claude_worker.archive_config` — config, enablement, redaction

```python
@dataclasses.dataclass(frozen=True, slots=True)
class ArchiveConfig:
    enabled: bool; reason: str            # reason is one of a fixed set of words, never a value
    endpoint: objstore.Endpoint | None
    creds: objstore.Credentials | None    # repr=False inside
    host_id: str; prefix: str
    cache_dir: pathlib.Path; cache_max_gib: int; cache_min_free_gib: int
    zstd_level: int; part_size_mib: int; concurrency: int; timeout_s: float
    skip_globs: tuple[str, ...]

ENV_FILE_KEY: str = "MULTIVENUE_S3_ENV_FILE"
DEFAULT_ENV_FILE: str = "~/multivenue/s3.env"

def load(env: collections.abc.Mapping[str, str] | None = None,
         env_file: pathlib.Path | None = None) -> ArchiveConfig
    # env=None → os.environ; env_file=None → env.get(ENV_FILE_KEY, DEFAULT_ENV_FILE) tilde-expanded;
    # env wins, the file fills gaps; a missing file is NOT an error (subsystem simply disabled)
def is_enabled(cfg: ArchiveConfig) -> bool
def read_env_file(path: pathlib.Path) -> dict[str, str]           # KEY=VALUE, '#' comments, optional quotes, no expansion
```

`load(env=…)` mirrors `config.load_base_from_env(env)` (`config.py:113-123`)
so tests inject a mapping and never touch the operator's files. Repo additions:
`s3.env.example` (names + placeholders), `retention.conf.example` (+ the §6
keys), one sentence in `.env.example:15-18`.

**Gate G-S2 (the S-LAW 2 proof, `tests/test_archive_config.py`):**
`test_disabled_when_keys_absent` (each of the six required keys missing in
turn ⇒ `enabled=False` with the right `reason`), `test_env_wins_over_file`,
`test_missing_env_file_is_disabled_not_error`, `test_host_id_regex`,
`test_secret_never_in_repr_str_log_or_exception` (the secret is a sentinel
string; `repr(cfg)`, `str(cfg)`, every `logging` record captured by `caplog`,
and the text of a forced `ObjStoreError` must not contain it).

### S3 — `claude_worker.archive` — the uploader, the console script, the launcher

`multivenue-archive` is added as a **second console script**
(`[project.scripts] multivenue-archive = "claude_worker.archive:main"`) —
**that one line and its `uv sync` land at S7, not here, while RG7 is open**
(§13.2 rule 9); `archive.py` ends with `if __name__ == "__main__": raise
SystemExit(main())` so `uv run python -m claude_worker.archive …` is the
interim short-command entry (the `cli.py` no-op pitfall does not recur). **`scripts/archive-run.py`** (repo root, the
`scripts/dashboard-serve.py:19-24` shape: `import sys; import
claude_worker.archive; sys.exit(claude_worker.archive.main())`) is the entry
point for every long run (S-LAW 15). `argparse` with sub-parsers — the
`regime.py:1718-1764` / `pnl_report.py:524-532` precedent, not typer. The
frozen `claude-worker` verb surface is untouched (S-LAW 14).

```
multivenue-archive push-run     <run-dir> [--dry-run] [--force] [--budget-s N] [--no-catalog]
multivenue-archive push-pending [--root DIR] [--budget-s N] [--dry-run]      # every closed run not yet complete in the bucket, oldest first
multivenue-archive verify       <run-id|run-dir> [--deep]                    # --deep = GET + sha256 every object (slow)
multivenue-archive list         [--since <ns|YYYY-MM-DD>] [--json]
multivenue-archive pull         <run-id> [--into DIR]
multivenue-archive gc           [--max-gib N]                                # local cache only
multivenue-archive status                                                    # the one-line tell (§7 S6)
# S7:
multivenue-archive push-tarball <path.tar.gz> | --all
multivenue-archive push-derived candles|reports|features [--day YYYY-MM-DD]
multivenue-archive pull-derived candles|reports|features --day YYYY-MM-DD [--force]
```

**Exit codes (one table, used by every lane and by the shell scripts):**

| Code | Meaning | Who relies on it |
|---|---|---|
| 0 | success / idempotent no-op / disabled for a non-gating lane (`list`, `status`, `gc`, `pull`) | — |
| 1 | `verify` failed (missing index, manifest mismatch, size/ETag mismatch, deep hash mismatch) or an upload error (ETag mismatch, exhausted retries) | retention stage B `break`s; the cycle stops |
| 2 | `push-run` refused: newest run without `--force`, or `--force` refused by the mtime/pgrep guard | — |
| 3 | subsystem disabled on a GATING lane (`push-run`, `push-pending`, `verify`) | retention stage B never deletes on 3 |
| 4 | free-space floor would be breached (§12.4) — nothing touched | the cycle stops |
| 5 | wall budget spent mid-run — resumable, no manifest written | the cycle exits 0 after logging; tomorrow resumes |

**`push_run(run_dir)` — exact algorithm:**

1. Resolve `run_dir`; require the `run-<digits>` name (`features.py:69-72`
   law). Refuse (exit 2) if it is the newest `run-*` under its parent unless
   `--force`; with `--force`, refuse if any `*.pmlr` mtime < 60 s old or
   `pgrep -f 'multivenue-engine ru[n]'` finds a process (S-LAW 4).
2. Check the index: `HEAD v1/index/<host>/<run>.json`. Present ⇒ print
   `archive: run=<run> already complete` and exit 0 (idempotent). Absent but
   `HEAD v1/runs/<host>/<run>/_manifest.json` present ⇒ the previous attempt
   died between the two writes: `GET` the manifest, run the `verify` checks
   against the bucket, write the index, exit 0.
3. Free-space guard: `shutil.disk_usage(cache_dir)` free − (largest file ÷ 4,
   a conservative compressed-size bound) must stay ≥ `CACHE_MIN_FREE_GIB`;
   otherwise exit 4 without touching anything (§12.4).
4. Enumerate files: `sorted(p for p in run_dir.iterdir() if p.is_file())`,
   minus `skip_globs`. For each file, in that order:
   a. sha256 + PMLR facts from the mmap (§5 laws; `struct.Struct("<4sHBxQ")`
      on the first 64 B when the name ends `.pmlr`).
   b. Compress into `<cache>/staging/<run>/<name>.zst` with
      `compression.zstd.ZstdCompressor(level)` in 8 MiB slices, then sha256 +
      md5 (single) or per-part md5 (multipart) of the staged bytes.
   c. `HEAD` the key. If present with `size == stored_bytes` AND the recorded
      progress entry (`<cache>/staging/<run>/progress.json`) says verified ⇒
      skip. If present with a different size ⇒ overwrite is allowed ONLY here
      (the object is a torn previous attempt; S-LAW 11's overwrite clause).
   d. `put` or `multipart_put`; compare the returned ETag with the local md5
      (single) or composite (multipart); on mismatch exit 1 (no manifest, no
      index ⇒ the run stays incomplete and resumable).
   e. Append the file's record to `progress.json` (atomic write:
      `tmp` + `os.replace`), delete the staged `.zst`.
   f. Budget check between files and between parts: if the wall budget is
      spent, print `archive: run=<run> budget spent after <k>/<n> files` and
      exit **5** (resumable; no manifest written).
5. Catalog: `subprocess.run(["multivenue-engine", "capture-catalog", "--dir",
   str(run_dir)], capture_output=True, text=True, check=False)`; stdout JSON
   on exit 0 else `null` + `catalog_error`. Never a failure of the push.
6. Build the manifest (§5); `put_bytes` it under the run prefix; `get_bytes`
   it back and compare; then `put_bytes` the SAME bytes as the index object
   (S-LAW 5). Remove `<cache>/staging/<run>/`.
7. Emit one line: `archive: run=<run> files=37 bytes=1.05G stored=96M
   ratio=10.9x elapsed=214s parts=<n>`.

**`push_pending`** = the closed runs under `--root` (default
`~/multivenue/logs`; `CLAUDE_WORKER_REPLAY_DIR` is honoured when set — the
worker's key, `config.py:123`), oldest first, newest excluded, each through
`push_run` with the shared budget; stops at the first exit ≠ 0 and propagates
it. This is stage A (§7 S4).

**`verify`** = index object exists → `get_bytes` it → `get_bytes` the
run-prefix `_manifest.json` and require byte equality → for every `files[]`
entry `HEAD` the key and compare `size == stored_bytes` and `etag`; `--deep`
also `GET`s and re-hashes (`stored_sha256`). Exit 0 only when everything
matches, else 1.

**Gate G-S3 (round-trip + resume, `tests/test_archive.py`):** a fake S3
(`tests/fake_s3.py`: an `httpx.MockTransport` handler over `dict[str, bytes]`
with `PUT`/`GET`(+`Range`)/`HEAD`/`LIST`/multipart/abort/`?versioning`
semantics, a request counter, and fault hooks) receives a fixture run dir
assembled in `tmp_path` from `tests/fixtures/pmlr/ticks_v3.pmlr` (+ copies
renamed per §0.1's file inventory, + a 64 B header-only file, + one file
larger than a 1 MiB test part size to force multipart); `pull` into a second
tmp dir; assert **byte-identical** files and identical manifest bytes in run
prefix and index. Named tests: `test_round_trip_byte_identical`,
`test_resume_after_kill_skips_verified_files` (fault: `httpx.ReadError` on the
5th PUT; rerun; the first 4 keys are not re-PUT and NO manifest/index existed
in between), `test_manifest_before_index_and_index_last`,
`test_windows_2h_complete_matches_window_root`,
`test_newest_run_refused_without_force`, `test_budget_exit_5_is_resumable`,
`test_catalog_absent_is_null_not_failure`, `test_etag_mismatch_exit_1_no_manifest`,
`test_verify_exit_codes`, `test_disabled_gating_lanes_exit_3_and_do_zero_http`
(request counter stays 0), `test_key_order_property` (10 000 seeded random
epochs: lexicographic == numeric).

### S4 — Stage A cycle (new launchd agent) + stage B in `scripts/retention.sh`

PREFLIGHT (§13): `scripts/retention.sh`, `retention.conf.example`,
`scripts/install-launchd.sh` clean; the RG7 lane's `scripts/engine-wrapper.sh`
edit is theirs. G2's 0020Z slot must keep firing on time.

**S4a — `scripts/archive-cycle.sh` + `launchd/com.multivenue.archive.plist`
(stage A).** The `dashboard.sh` + `regime-cycle.sh` shapes combined:

```sh
#!/bin/zsh
# SPDX … (two lines)
# Archive cycle (launchd com.multivenue.archive, once a day at a quiet
# local hour): push every CLOSED run not yet complete in the bucket,
# oldest first, under a wall budget. Never the newest run (S-LAW 4);
# never sources the credential file (the archiver reads it — S-LAW 6);
# cmdline law: runs as ~/multivenue/venv/bin/python3 scripts/archive-run.py.
set -u
REPO="${MULTIVENUE_REPO:-$HOME/trading-engine-multivenue}"
cd "$REPO" || exit 78
export PATH="$REPO/target/release:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"   # capture-catalog
CONF="$HOME/multivenue/retention.conf"; S3_CYCLE_BUDGET_S=600; ARCHIVE_MODE="compress"
[ -f "$CONF" ] && . "$CONF"
[ "$ARCHIVE_MODE" = "s3" ] || exit 0                                   # honest no-op
VENV="$REPO/claude-worker/.venv"; ALIAS="$HOME/multivenue/venv"       # dashboard.sh:30-40 alias law
[ -x "$VENV/bin/python3" ] || { echo "archive-cycle: venv missing" >&2; exit 78; }
[ "$(readlink "$ALIAS" 2>/dev/null)" = "$VENV" ] || { rm -f "$ALIAS"; ln -s "$VENV" "$ALIAS"; }
if pgrep -f 'archive-ru[n].py' >/dev/null 2>&1; then echo "archive-cycle: already running — skipping" >&2; exit 0; fi
if pgrep -f 'recommit-rulese[t].sh' >/dev/null 2>&1; then echo "archive-cycle: boot recommit live — skipping" >&2; exit 0; fi
[ -n "${MULTIVENUE_S3_ENV_FILE:-}" ] && export MULTIVENUE_S3_ENV_FILE
exec taskpolicy -b nice -n 19 "$ALIAS/bin/python3" "$REPO/scripts/archive-run.py" push-pending --budget-s "$S3_CYCLE_BUDGET_S"
```

The plist is a template like `launchd/com.multivenue.dashboard.plist`
(`@REPO@`/`@HOME@`, `StandardOutPath`/`StandardErrorPath` →
`@HOME@/multivenue/logs/launchd/archive.log`) with `StartCalendarInterval`
`{Hour: 4, Minute: 30}` (local time is acceptable here — this is not a
UTC-day law; the point is distance from every restart slot: 0000Z/0020Z/
0830Z/1605Z/2015Z/2115Z) and NO `KeepAlive`. Installation is the
OPERATOR's: render + `launchctl bootstrap gui/$UID` of this ONE label
(documented in `local-setup.md` at S7); `scripts/install-launchd.sh:27, 36`
gain the label in both lists so a future reinstall includes it — a 2-line
edit made only while that file is clean. The lane NEVER runs the installer
(it restarts the engine).

**Backfill (operator-run, before enabling stage B):** `taskpolicy -b nice -n
19 ~/multivenue/venv/bin/python3 scripts/archive-run.py push-pending
--budget-s 7000` repeated until it prints nothing pending — each invocation
≤ 2 h (S-LAW 7), from a terminal under `nohup`, or via `launchctl submit`
under the self-removing-label law. 71 GB + 3.6 GB of tarballs.

**S4b — `scripts/retention.sh` (stage B).** The existing shape is preserved
line-for-line; additions only:

```sh
ARCHIVE_MODE="compress" # compress | move | s3
…
[ -f "$CONF" ] && . "$CONF"
if [ "$ARCHIVE_MODE" = "s3" ]; then
  REPO_DIR="${0:A:h:h}"; ALIAS="$HOME/multivenue/venv"
  if [ -x "$ALIAS/bin/python3" ] && [ -f "$REPO_DIR/scripts/archive-run.py" ]; then
    [ -n "${MULTIVENUE_S3_ENV_FILE:-}" ] && export MULTIVENUE_S3_ENV_FILE
    archive_verify() { "$ALIAS/bin/python3" "$REPO_DIR/scripts/archive-run.py" verify "$1"; }
  else
    echo "retention: ARCHIVE_MODE=s3 but the archiver is not installed — falling back to compress" >&2
    ARCHIVE_MODE="compress"
  fi
fi
… (existing free-space early exit, candidate loop, PROTECT_DAYS break — unchanged) …
  if [ "$ARCHIVE_MODE" = "s3" ]; then
    if archive_verify "$d" >&2; then rm -r "$d"                      # S-LAW 3: only what the bucket verifiably holds
    else echo "retention: $name not verified in bucket — stopping" >&2; break; fi
  elif [ "$ARCHIVE_MODE" = "move" ]; then … unchanged …
  else … the existing compress branch, unchanged … fi
```

- Stage B never uploads: `verify` is a handful of `HEAD`s per run (seconds),
  so the 0000Z tick loop is not stalled and §12.3's budget concern is
  confined to stage A, which does not live in the restart lane at all.
- `break`, not `continue`, on an unverified run: oldest-first is the script's
  invariant; an unverified oldest means stage A has not reached it — the
  operator's backfill or tomorrow's cycle will. Disk pressure is never a
  reason to lose data.
- `verify` under `retention.sh` runs without `.env`: that is fine — the
  archiver reads `~/multivenue/s3.env` itself (or `MULTIVENUE_S3_ENV_FILE`
  exported from `retention.conf` when Q7 = `.env`).

**Gate G-S4:** (a) `tests/test_retention_sh.py` (skipped when `zsh` is
absent) runs the script on a seeded `tmp_path` root with `HOME` pointed at a
tmp dir whose `multivenue/venv/bin/python3` is a stub that exits 1 / 3 / 0
per an env var — assert the run dir survives on 1 and 3 with the reason
logged, and is removed on 0; (b) with `ARCHIVE_MODE=compress` the new
script's stderr and filesystem effect on the seeded root are identical to the
OLD script's (`git show HEAD:scripts/retention.sh > tmp/old.sh`, same conf
with `MIN_FREE_GIB=999999 TARGET_FREE_GIB=999999 PROTECT_DAYS=0`) — the
S-LAW 2 proof for the restart lane; (c) OPERATOR-RUN live check: one
archive-cycle run (`S3_CYCLE_BUDGET_S=300`) on a day the backlog is already
pushed — `pgrep -f 'claude[-_]worke[r]'` prints nothing while it runs, the
previous day's runs appear in `list --json`, and the next 0000Z/0020Z slots
fire on time (`restart.log`); (d) the §12.5 contention comparison recorded in
§16. Run `git diff --summary -- scripts/` and confirm NO mode change (the
2026-08-27 exec-bit incident); new `.sh` files are `chmod +x`.

### S5 — `claude_worker.data_source` — the resolver and the pull cache

```python
Location = typing.Literal["local", "local+s3", "cache+s3", "s3", "absent"]

class RunRef(typing.NamedTuple):
    run_id: str; epoch_ns: int; location: Location
    local_path: pathlib.Path | None       # under the log root or the cache
    span_ns: tuple[int, int] | None       # manifest (no pull) or window_root.run_span (local)
    size_bytes: int | None; stored_bytes: int | None
    pmlr_version: int | None; windows_2h_complete: int | None
    venues: tuple[str, ...]               # catalog venue_totals with ticks > 0; () when unknown
    verified_sha256: str | None           # sha256 of the manifest bytes the pull was verified against

class DataSource:
    def __init__(self, cfg: archive_config.ArchiveConfig, replay_dir: pathlib.Path,
                 store: objstore.ObjectStore | None) -> None: ...   # store None when disabled
    def list_runs(self, since_ns: int | None = None, until_ns: int | None = None) -> list[RunRef]
    def where(self, run_id: str) -> Location
    def ensure_local(self, run_id: str) -> pathlib.Path          # pulls if needed; raises DataSourceError
    def release(self, run_id: str) -> None                        # LRU hint only
```

- `list_runs` merges the local scan (reuse `features.run_dirs(replay_dir)` —
  read-only call, the file is not modified) with the cache dir's complete
  `run-*` entries and ONE `list("v1/index/<host>/")` (manifests cached under
  `<cache>/manifests/`).
- `ensure_local`: log-root path if present; else cache path if
  `<cache>/run-<ns>/.complete` exists (touch it — the LRU clock; APFS atime is
  not trusted); else pull into `<cache>/run-<ns>.partial/` (invisible to every
  discovery law — the suffix breaks the `run-<digits>` name), verify every
  file's `sha256` after decompression, `os.rename` to `<cache>/run-<ns>/`, then
  write `.complete` with the manifest sha256 inside. Interrupted pulls leave
  only `.partial` dirs; `gc` removes them.
- Cache policy, enforced BEFORE every pull: `sum(sizes) + manifest.size_bytes ≤
  CACHE_MAX_GIB` (evict LRU by `.complete` mtime until true) AND
  `disk free − manifest.size_bytes ≥ CACHE_MIN_FREE_GIB` (else raise
  `DataSourceError("cache: free space floor")` — never fill the volume
  retention is trying to free, §12.4).

**Gate G-S5 (the acceptance test — parity, OPERATOR-RUN on the Mac):** pick one
CLOSED run (≥ 2 h of ticks, v3, ~1 GB). (1) `window_root.cut_run(run, tmp,
0, 7200)` → `multivenue-engine audit-pnl --dir <cut>` → keep stdout. (2)
`push-run` it; `mv` the local dir aside (NOT delete); `ensure_local` it back
from the bucket; repeat the cut + audit-pnl. **Byte-identical stdout** (stderr
carries timings and may differ). (3) `pnl_report --day <that day>` with the
pulled dir in place produces the same `pnl-<day>.json` modulo the additive
`data_source` block (S6). Restore the original dir afterwards. Record the
release-binary mtime vs the last `crates/cli` commit before running (pitfall
18). Unit-level (`tests/test_data_source.py`):
`test_partial_dir_invisible_to_discovery`,
`test_ensure_local_verifies_every_sha256`, `test_lru_evicts_by_complete_mtime`,
`test_free_space_floor_refuses_pull`, `test_list_runs_merges_three_sources`,
`test_disabled_resolver_does_zero_http`.

### S6 — Agent awareness

1. **A catalog the agent can read without pulling** — `multivenue-archive list
   --json`, one JSON object per line:
   ```json
   {"run":"run-1788417289611943000","epoch_ns":1788417289611943000,"host_id":"mbp-m4",
    "location":"s3","span_ns":[…,…],"size_bytes":1049873664,"stored_bytes":96331776,
    "pmlr_version":3,"windows_2h_complete":2,"venues":["pm","bn","okx","deribit","hl","bybit"],
    "local_path":null,"uploaded_at_ns":…}
   ```
   `windows_2h_complete` lets the agent plan a G1-style N ≥ 4 disjoint-window
   pool BEFORE deciding what to pull.
2. **Provenance in the consumers that resolve old runs.** `pnl_report.select_
   runs(replay_dir, day)` (`pnl_report.py:200`) gains an optional
   `source: DataSource | None = None`; when given AND the day has index
   entries not on disk, it `ensure_local`s them (bounded by the cache policy)
   and returns their paths — day mode for the CLOSED day is unaffected (those
   runs are local by PROTECT_DAYS). `run_day` writes an **additive** block:
   ```json
   "data_source": {"archive_enabled": true,
                   "runs": [{"run": "run-…", "location": "local+s3", "verified_sha256": null}]}
   ```
   `cli positions --run-dir` (`cli.py:848-856`, `_resolve_run_dir` at `:761`)
   additionally accepts a bare run id and resolves it through `DataSource` —
   **no new verb, no changed argv, no changed output schema** (S-LAW 14).
   `features.run_dirs`/`latest_run_dir` are NOT changed (15 callers; the
   newest run is always local).
3. **A one-line tell**, house style (`icdp: artifact configured hash=…`):
   `archive: enabled bucket=market-data-qu host=mbp-m4 runs_local=12
   runs_remote=87 cache=6.2/20G` or `archive: disabled (flag)` — printed by
   `multivenue-archive status` and by `pnl_report` day mode on stderr.

**Gate G-S6** (in the EXISTING `tests/test_pnl_report.py` and
`tests/test_cli.py` — additive tests only): `test_report_data_source_block_
matches_where`, `test_disabled_report_is_golden_identical` (with the subsystem
disabled the block is `{"archive_enabled": false, "runs": []}` and every other
byte of a report built from the `backtest-real` fixture equals the pre-S6
golden), `test_positions_accepts_run_id`.

### S7 — Legacy tarballs, derived stores, docs, close

- `push-tarball --all`: every `~/multivenue/archive/run-*.tar.gz` (47 today,
  3.6 GB) verbatim to `v1/tarballs/<host>/` with a `{sha256,size,run}`
  manifest, index-last; `pull` of such a run = download + verify + `tar -xzf`
  into the cache (the dir inside the tarball is `run-<ns>/`, the same shape).
  Local tarballs are NOT deleted by this lane (retention's own law: pruning
  `~/multivenue/archive` is a deliberate operator act).
- `push-derived candles` copies `candles.db` via `sqlite3 .backup` into staging
  (never a live-file copy), then zstd; `reports` = `~/multivenue/worker/
  reports/pnl-*.json`; `features` = the features dir by day. `pull-derived`
  refuses to overwrite an existing local store without `--force`.
- Docs: this plan moves to `docs/arch/` with a close entry; `CLAUDE.md` gains
  the §2 S-DOCTRINE ruling, ONE CURRENT-STATE bullet, and directory-guide
  entries for the four new modules + the two scripts (**only after `git
  status` shows CLAUDE.md clean** — §13); `docs/local-setup.md` gains ONE new
  H2 appended at the END of the file (setup, `s3.env`, the one-label launchd
  bootstrap, backfill, rollback); `docs/migration.md` gains the
  `archive_manifest_version: 1` entry in its dated-H2 format (`:12` template;
  no H3s exist in that file).
- **Stay-greens at close:** worker pytest additive-green (frozen 202 inside);
  `make py-lint`; `make license-check`; `git diff --stat <base>..HEAD --
  crates Cargo.toml Cargo.lock` EMPTY for the S3-lane commits (the S-LAW 1
  proof — no cargo invocation by this lane).

---

## 8. Test strategy

- **No network in tests, ever.** `tests/fake_s3.py` = an `httpx.MockTransport`
  handler over `dict[str, bytes]` implementing `PUT`/`GET` (with `Range`)/
  `HEAD`/`LIST` (paginated, `delimiter`, `start-after`, both namespaced and
  bare XML)/multipart (`?uploads`, `?partNumber`, `?uploadId`, abort)/
  `?versioning`, with fault hooks (`fail_after_n_puts`, `status_once=503`,
  `truncate_get`, `corrupt_etag`). It counts requests so "zero HTTP" gates are
  assertable.
- **Property test on the key schema:** for random `epoch_ns` sets,
  lexicographic key order == numeric order (hypothesis is NOT a dependency —
  use `random` with a fixed seed and 10 000 draws).
- **Fault injection:** 503 on the Nth part; connection reset mid-body; a
  truncated download; a checksum mismatch; a manifest listing a missing object;
  a torn previous upload (size mismatch on `HEAD`). Each must fail closed — no
  delete, no `.complete`, no `run-<ns>` dir visible to `discover_runs`.
- **The frozen 202 pytest baseline stays untouched-green**; all new tests are
  additive; `pgrep -f pytes[t]` before every run.

---

## 9. Sizing (measured 2026-09-05)

- **Rate:** ≈ 5 GB/day raw (a 4.9 h run = 1.0 GB; `bn-ticks.pmlr` is ~60 %
  of it). v1's "tens of GB/day" was a guess and is retracted.
- **Holdings:** 71 GB / 48 runs in the log root + 3.6 GB / 47 gzip tarballs.
- **Compression:** zstd ratio on PMLR is measured in S0 (64 B slots with
  monotonic `ts_ns` and repeated symbol ids should compress well; the
  manifest example's ~10× is a placeholder until S0 replaces it).
- **Levers if needed:** zstd level (S0 measures 3 vs 9); `SKIP_GLOBS`
  (`*-raw.tap` is a 64 MiB-budgeted debugging tap) — default EMPTY per the
  operator's "keep everything" intent (§14 Q3); egress is bounded by the
  cache and by `windows_2h_complete`-first planning.
- **Retention interplay:** the volume is under `MIN_FREE_GIB` TODAY, so the
  compress lane will start deleting (after tarring) at the next 0000Z. Switching
  `ARCHIVE_MODE=s3` before the backlog is pushed loses nothing (stage B is
  verify-gated) but frees nothing until the backlog is in the bucket; the
  operator-run backfill (§7 S4a) should precede the switch.

---

## 10. Risks

| Risk | Mitigation |
|---|---|
| Disclosed credentials (chat plaintext) | R-1: rotate before S1; `s3.env` chmod 600 (or `.env`, Q7); S-LAW 6 tests |
| Bucket accidentally public | R-2: S0 proves 403 unauthenticated; `status` re-checks once per invocation |
| Doctrine violation ("no cloud, any phase") | §2 operator ruling, narrowly scoped, engine untouched |
| Silent data loss (delete after a bad upload) | S-LAW 3 + S-LAW 5 + G-S3/G-S4 fault injection; server-verified payload digests on every upload |
| Torn tail archived as truth | S-LAW 4: newest run never eligible; mtime + pgrep guard on `--force` |
| Endpoint behaviour differs from AWS | S0 probes it; the fixture pins OUR request shapes; nothing is assumed |
| Cache/staging fills the disk that retention is trying to free | `CACHE_MAX_GIB` + `CACHE_MIN_FREE_GIB` floor enforced BEFORE every pull/stage; `gc` before the sweep |
| SigV4 subtly wrong (query encoding, header set, chunked body) | 11 botocore-derived vectors covering every request shape used; `Content-Length` law; re-sign per attempt |
| Hetzner 503 under load | bounded retries with jitter; `CONCURRENCY=1` while the engine is live |
| **The archiver blocks the boot recommit** (cmdline law) | S-LAW 15: launcher + venv alias; G-S4(c) asserts `pgrep -f 'claude[-_]worke[r]'` is empty while it runs |
| The restart lane stalls (§12.3) | stage A is NOT in the restart lane (own agent, quiet slot, budget); stage B = seconds of `HEAD`s |
| Feed degradation while uploading (§12.5) | `taskpolicy -b nice -n 19`; measured against a quiet ≤ 2 h window; byte-rate cap if it moves |
| Parallel-lane collision (RG7 regime soak, RG4 worker set, funding seed) | §13: ownership list + dirty-set exclusion + one change at a time in the restart lane |
| "Forever" quietly ends — a lifecycle rule, a console delete, a buggy sweep | S-LAW 11: no delete in the client; S0 records the lifecycle/versioning state; versioning recommended ON |
| Storage cost grows without bound (that is the point) | Accepted; levers are zstd and `SKIP_GLOBS`; revisit as a cost decision, never a code default |
| Single bucket, single location = durability, not backup | Hetzner: one data centre per bucket, erasure-coded; a second bucket in `hel1`/`nbg1` mirrored by the same client is a separate decision (§14 Q11) |

---

## 11. Files touched — the complete list

### 11.1 New files (all S3-lane owned)

| Phase | Path | Notes |
|---|---|---|
| S0 | `claude-worker/tools_<name>.py` | The probe. Git-excluded by the reserved prefix; its name is never written in a tracked doc. Findings → vault; decisions → §6/§16 |
| S1 | `claude-worker/src/claude_worker/objstore.py` | SigV4 + minimal S3 verbs; no `delete_object` |
| S1 | `claude-worker/tests/test_objstore.py` | Vectors + MockTransport tests |
| S1 | `claude-worker/tests/fixtures/sigv4/vectors.json` | **Already present (untracked)** — stage explicitly at S1; do not regenerate by hand |
| S1 | `claude-worker/tests/fake_s3.py` | The in-process fake (imported by S3/S5/S6 tests) |
| S2 | `claude-worker/src/claude_worker/archive_config.py`, `tests/test_archive_config.py` | Loader + redaction proofs |
| S2 | `s3.env.example` (repo root) | Names + placeholders only |
| S3 | `claude-worker/src/claude_worker/archive.py`, `tests/test_archive.py` | Uploader + console script |
| S3 | `scripts/archive-run.py` | The cmdline-law launcher (`dashboard-serve.py` shape) |
| S4 | `scripts/archive-cycle.sh`, `launchd/com.multivenue.archive.plist` | Stage A agent (`chmod +x` the script) |
| S4 | `claude-worker/tests/test_retention_sh.py` | zsh-gated script test |
| S5 | `claude-worker/src/claude_worker/data_source.py`, `tests/test_data_source.py` | Resolver + cache |
| — | `docs/s3-archive-plan.md` | This file (untracked until §14 Q6 is ruled; if tracked, it lives at `docs/arch/` from the start — see Q6) |

### 11.2 Modified files

| Phase | Path | Change | Clean on 2026-09-05 15:30? |
|---|---|---|---|
| S2 | `.env.example` | ONE pointer sentence in the `:15-18` header | yes |
| S2 | `retention.conf.example` | + `ARCHIVE_MODE=s3`, `S3_CYCLE_BUDGET_S`, the commented `MULTIVENUE_S3_ENV_FILE` | yes |
| S3 | `claude-worker/pyproject.toml` + `claude-worker/uv.lock` | + one `[project.scripts]` line; `dependencies` unchanged; `uv sync` between ticks (§12.6) | yes |
| S4 | `scripts/retention.sh` | s3 preflight + `archive_verify` + the `s3` branch; everything else byte-identical | yes |
| S4 | `scripts/install-launchd.sh` | `com.multivenue.archive` appended to the two label lists (`:27`, `:36`) — never executed by this lane | yes |
| S6 | `claude-worker/src/claude_worker/pnl_report.py` | `select_runs(…, source=None)`; additive `data_source` block; stderr tell | yes |
| S6 | `claude-worker/src/claude_worker/cli.py` | `_resolve_run_dir` accepts a run id via `DataSource`; nothing else | yes |
| S6 | `claude-worker/tests/test_pnl_report.py`, `tests/test_cli.py` | additive tests only | yes |
| S7 | `docs/local-setup.md` | ONE H2 appended at the end | yes |
| S7 | `docs/migration.md` | dated H2 entry | yes |
| S7 | `CLAUDE.md` | S-DOCTRINE + one bullet + directory-guide lines | **DIRTY (RG4/RG7) — only after the operator commits** |
| S7 | `docs/arch/README.md` | index entry at archive time | yes |

### 11.3 Deliberately NOT touched — the S-LAW 1 / S-LAW 12 evidence

`crates/**`, `Cargo.toml`, `Cargo.lock`, `deny.toml`, `about.toml`,
`THIRD-PARTY-NOTICES.md` (no dependency change on either side ⇒ `make
license-deps` is not required); `claude-worker/src/claude_worker/backtest.py`
(frozen + dirty), `window_root.py` (dirty from RG4; `cut_run` is CALLED, never
edited), `features.py` (15 callers), `state.py`, `strategist.py`,
`regime.py` (dirty from RG7), `library.py`, `compose.py`, `dashboard.py`;
`scripts/engine-wrapper.sh` (dirty from RG7), `scripts/daily-restart.sh`,
`scripts/recommit-ruleset.sh`, `scripts/dashboard*.{sh,py}`,
`scripts/regime-cycle.sh`, the existing `launchd/*.plist`,
`~/Library/LaunchAgents/*` (the ONE new agent is bootstrapped by the operator);
`~/multivenue/{universe,strategy,icdp,regime,fees}.{toml,conf}`;
`docs/prompts/ai-session.md`; `docs/regime-and-dashboard-plan.md`;
`claude-worker/tests/test_regime_soak.py` (RG7, untracked).

---

## 12. Impact on the currently running engine

Live shape 2026-09-05 15:30 local: `com.multivenue.engine` (paper, `--strategy
ai+icdp` = mask 112; restarted 15:30 by the RG7 lane) + caffeinate +
daily-restart + hourly candles/iv + retention (`PROTECT_DAYS 5`) +
`com.multivenue.regime` + `com.multivenue.dashboard` (9292). RG6 is CLOSED
and committed (`ef75c91`, `634f740`); RG7 (regime soak) is ACTIVE in another
session. This lane never restarts anything.

### 12.1 The engine binary: zero impact, no restart required

No Rust file changes ⇒ `target/release/multivenue-engine` is never relinked by
this lane ⇒ no phase requires an engine restart, rebuild, or mask change.
`vm_rows_active`, `composed mask=112`, `icdp: artifact configured`, the
`/metrics` and `/state` surfaces on 9191, `ai.sock`, and the one-engine law
are untouched.

### 12.2 Credentials and the engine process

`engine-wrapper.sh` sources `.env` with `set -a`, so anything in `.env` is in
the ENGINE process's environment. With the recommended `s3.env` the S3 secret
is read only by the archiver's Python loader — no shell script sources it — and
never enters the engine, the worker's `serve`, or any `uv run` shell that
sourced `.env`. If Q7 rules "`.env`", the secret also sits, unused, in the
engine's environment — same uid, no privilege boundary crossed, but a wider
blast radius for any future log-the-environment mistake.

### 12.3 The restart lane — and why stage A is not in it

`com.multivenue.daily-restart` runs `scripts/daily-restart.sh` **every 60 s**.
At the 0000Z slot it SIGTERMs the engine (`:108`) and runs `retention.sh`
**synchronously** (`:117`); launchd will not start a second copy while one is
running, so anything slow inside `retention.sh` delays every later slot
(0020Z pnl — G2's accumulator; 0830Z; 1605Z; 2015Z/2115Z). Worse, the boot
recommit that follows the SIGTERM (`recommit-ruleset.sh:38-47`) gives up after
5 min if a `claude[-_]worke[r]` cmdline is alive, leaving `vm_rows_active 0`
until the next boot. Hence: stage A (uploads) lives in its OWN agent at a quiet
hour under the cmdline law; stage B inside `retention.sh` is `verify` only
(seconds). The engine restart itself never waits: `:108` only sends SIGTERM;
KeepAlive + `engine-wrapper.sh` relaunch independently (10–40 s gap against
the 300 s catalog tolerance).

### 12.4 Disk — a new writer on the volume that already wedged once

ENOSPC does not retry (2026-09-02): capture stops on every lane and only an
engine restart recovers it. The pull cache and the staging dir are new writers
on that volume; the volume has 22 GiB free today. Therefore: `CACHE_MAX_GIB`
(20) and the `CACHE_MIN_FREE_GIB` floor (25) are enforced BEFORE every stage/
pull; staging holds at most ONE file's compressed bytes at a time; `gc` runs
before the sweep. Steady state the archive *reduces* pressure — retention can
finally delete with a copy elsewhere. The dangerous interval is the backfill,
which reads 71 GB and frees nothing until the first verified delete.

### 12.5 CPU and network contention with live capture — measured, not assumed

sha256 + zstd over GBs plus a busy uplink, on the box whose ingress threads
measure venue latency. Binance delivery already carries a p90 ≈ 1.3 s tail
(`docs/venue-latency.md`); the VT staleness gate judges ticks from venue time,
so an upload that adds delivery delay **degrades the data being captured while
it runs** and can move `engine_ingress_<venue>_stale_ticks_total` and the VM's
ABSENT-on-stale rule. Requirements: `taskpolicy -b nice -n 19` on the archiver
(macOS background QoS = CPU + I/O + network traffic class); `CONCURRENCY=1`
whenever the engine is live; the cycle's slot is far from every restart slot.
**Close gate for S4:** compare `engine_ingress_<venue>_feed_delay_ema_ms` and
the stale counters (scraped from `127.0.0.1:9191/metrics` once a minute)
across ONE ≤ 2 h upload window against ONE quiet ≤ 2 h window of the same UTC
hours from an existing run (S-LAW 13 — never schedule a wait). If the archiver
moves them, add a byte-rate cap (`--max-mib-s`, then, not now).

### 12.6 Worker venv reinstall

`pyproject.toml` gains one console script ⇒ `uv sync` in `claude-worker/`
(also rewrites `uv.lock`). The hourly candles/iv jobs, the regime cycle, the
dashboard server and the 0020Z pnl slot all run from that venv, so run the
sync between ticks (`pgrep -f claude-worke[r]` empty) and expect the dashboard
agent (KeepAlive) to relaunch itself if its interpreter is replaced under it.
No engine involvement.

### 12.7 launchd PATH — the known bug class

`retention.sh` runs with launchd's minimal PATH and WITHOUT `.env`
(`daily-restart.sh` exports PATH and sources `.env` only inside the 0020Z
subshell, `:135-140`) — the bug class that left the nightly pnl dead from
2026-08-23 to 2026-09-03. S4 therefore resolves the interpreter by the
absolute alias path, needs no `.env` (the archiver loads its own credential
file), exports `target/release` in the cycle for `capture-catalog`, and
preflights the launcher — falling back to `compress`, never to deleting.

### 12.8 Rollback

`ARCHIVE_MODE=compress` in `~/multivenue/retention.conf` (stops both stages:
the cycle exits 0 as an honest no-op, retention returns to today's branch), or
`MULTIVENUE_S3_ENABLED=0` in the credential file. Effective at the next tick:
no engine restart, no rebuild, no data movement, no cache invalidation. Bucket
objects remain (S-LAW 11). `launchctl bootout gui/$UID/com.multivenue.archive`
removes the agent entirely (operator).

---

## 13. Parallel work — the non-interference protocol (S-LAW 12)

### 13.1 Lanes in flight on 2026-09-05 15:30 local (verified by `git status`, `git log`, mtimes)

| Lane | State | Its files |
|---|---|---|
| **RG6 engine + TUI + worker page** | **COMMITTED + CLOSED** (`ef75c91`, `634f740`; operator ruling 2026-09-05); `com.multivenue.dashboard` bootstrapped 08:19Z | none dirty — `crates/engine-snapshot/**`, `claude-worker/src/claude_worker/dashboard.py`, `claude-worker/src/claude_worker/dashboard/dashboard.html`, `tests/test_dashboard.py`, `launchd/com.multivenue.dashboard.plist`, `scripts/dashboard{.sh,-serve.py}`, `scripts/install-launchd.sh` are all clean (still: read-only for this lane except the 2-line installer edit at S4) |
| **RG7 regime soak** | **ACTIVE in another session right now** (engine restarted 15:30; `test_regime_soak.py` appearing) | `claude-worker/src/claude_worker/regime.py` (M) `claude-worker/tests/test_regime_soak.py` (??) `scripts/engine-wrapper.sh` (M) + expected: `docs/regime-and-dashboard-plan.md`, `CLAUDE.md` |
| **RG4 library/compose** | code-complete, UNCOMMITTED (operator stages separately) | `claude-worker/src/claude_worker/{backtest.py,state.py,strategist.py,window_root.py}` (M) `{library.py,compose.py}` (??) `tests/{craft.py,test_session_scripted.py}` (M) `tests/{test_compose.py,test_library.py}` (??) `docs/prompts/ai-session.md` (M) `fees.toml.example` (M) `CLAUDE.md` (M) `docs/regime-and-dashboard-plan.md` (M) |
| **G2 / nightly pnl** | accumulating; the 0020Z slot must fire on time nightly | `~/multivenue/worker/reports/pnl-<day>.json` (runtime, not repo) |
| **Harness funding seed** (alternative the operator may order) | not started | would touch `crates/cli` + `window_root.py` |
| **Kronos forecast lane** | plan in the vault, awaiting rulings | vault only |

The only untracked files that belong to THIS lane today: `docs/s3-archive-plan.md`,
`claude-worker/tests/fixtures/sigv4/vectors.json`.

### 13.2 Rules

1. **Before every phase:** `git --no-optional-locks status --porcelain` on the
   Mac and `git --no-optional-locks log --oneline -3`. Any path this lane is
   about to edit that is already dirty ⇒ STOP, ask the operator; never "merge
   by hand", never stash. Re-read §13.1 against reality — it is a snapshot,
   and the tree moved twice during the review that wrote it.
2. **Never edit** (regardless of dirty state): everything in §11.3 and every
   path in the table above. `window_root.py` and `features.py` are CALLED, not
   edited. `CLAUDE.md` and `docs/local-setup.md` are edited only at S7 and only
   when clean; if still dirty at S7, put the text in §16 and hand it to the
   operator.
3. **`pyproject.toml`** is clean today; re-read it at S3 and add the
   `multivenue-archive` line as a separate one-line diff.
4. **Staging is explicit-path only:** `git add docs/… claude-worker/src/
   claude_worker/objstore.py …` — the RG4 and RG7 sets are the operator's to
   stage. Never `-A`/`-u`.
5. **One change at a time in the restart lane:** S4b (`retention.sh`) is
   enabled live (`ARCHIVE_MODE=s3`) only after the backfill is complete and on
   a day with no other restart-lane change; the cycle agent (S4a) is
   bootstrapped by the operator on a different day than S4b's switch.
6. **Serialize the worker lanes:** `pgrep -f 'claude[-_]worke[r]'` and
   `pgrep -f pytes[t]` before any pytest / `uv sync`; the archiver itself is
   OUTSIDE the worker guard by design (S-LAW 15) — it must never be started
   through `uv run`/`python -m` for anything but a seconds-long command.
7. **No engine restart, no relink, no launchd bootstrap/bootout, no
   `install-launchd.sh` by this lane** — the operator bootstraps the one new
   agent by hand from the rendered template.
8. **Sandbox git hazard:** never run git from the Cowork/Linux sandbox
   (stale `index.lock`, observed 2026-09-05); read-only git goes through the
   Mac terminal with `--no-optional-locks`.
9. **Running in parallel with RG7 (operator question 2026-09-05 — the
   answer is yes, with this split).** While RG7 is open and the RG4/RG7 sets
   are uncommitted:
   - **GO now:** S0 (no repo code), S1, S2, S3, S5 — new files only, plus the
     one-line `retention.conf.example`/`.env.example`/`s3.env.example` doc
     edits. The `pyproject.toml` console-script line and its `uv sync` are
     DEFERRED to S7: `uv sync` rewrites the shared venv under the running
     dashboard agent and under any pytest the RG7 session may be running;
     until S7 the entry points are `uv run python -m claude_worker.archive …`
     (short commands) and `scripts/archive-run.py` (long runs) — a new module
     inside `src/claude_worker/` is importable without a sync.
   - **HOLD until RG7 is CLOSED and the RG4/RG7 sets are committed:** S4
     (the restart lane + the new agent — the soak measures the engine across
     its restarts and must not gain a second variable), S6 (`pnl_report.py`
     is the nightly per-regime P&L accumulator the soak reads — keep it
     byte-stable), S7 (`CLAUDE.md`, `local-setup.md`, `migration.md`).
   - **Two sessions, one checkout:** `pgrep -f pytes[t]` and `pgrep -f
     'claude[-_]worke[r]'` before every pytest/harness run; the S3 session
     never runs `uv sync`, never restarts anything, never touches `CLAUDE.md`
     or the dirty set; the operator tells the RG7 session that the S3 lane
     exists (its paths = §11) so its own explicit-path staging excludes them.
   - G-S5's parity run (a 2 h `cut_run` + two `audit-pnl`s) is CPU-heavy but
     engine-neutral; run it outside the 0000Z–0040Z window and never while
     the RG7 session is running a harness of its own.

---

## 14. Open questions for the operator (with the implementer's default)

| # | Question | Default unless overridden |
|---|---|---|
| Q1 | §2 S-DOCTRINE — approve the narrow amendment? | **Blocking.** No S1 without it |
| Q2 | R-1 — rotate the disclosed key pair before S1? | **Blocking** |
| Q3 | Push everything (`*-raw.tap`, `*-depth.pmlr` included) or keep a skip list? | Push everything (`SKIP_GLOBS` empty) — "keep everything" intent |
| Q4 | `host_id` value | `mbp-m4`, explicit in the credential file; REQUIRED |
| Q5 | Backfill: operator-run `push-pending` in ≤ 2 h invocations before switching `ARCHIVE_MODE=s3`? | Yes — backlog never from the daily cycle |
| Q6 | Does this plan doc enter git? (no research substance, no credentials) | Untracked until ruled. **If tracked, it must live under `docs/arch/`** — `make license-check` (`Makefile:129-138`) refuses a tracked file outside `docs/arch/` that names a `tools_*.py`; this doc names the class pattern only, but `docs/arch/` is the safe home either way |
| Q7 | Credential location: `~/multivenue/s3.env` (recommended, §12.2) or `.env`? | `s3.env`; the loader supports both |
| Q8 | Cycle slot and budget | 04:30 local, `S3_CYCLE_BUDGET_S=600` (300 on the first live run) |
| Q9 | Enable bucket versioning + the `AbortIncompleteMultipartUpload` lifecycle rule (operator-run one-shot in S0)? Object Lock needs a NEW bucket — wanted? | Versioning ON + the one rule; no new bucket |
| Q10 | Archive panel on the RG6 dashboard (`/api/worker` reads `list --json`)? | Follow-up, not this plan |
| Q11 | Second-location mirror (`hel1`/`nbg1`) | Out of scope; revisit as a cost decision |
| Q12 | Legacy tarballs: push verbatim (S7) — and may the operator then prune `~/multivenue/archive` by hand? | Push verbatim; pruning stays manual |
| Q13 | A new launchd agent (`com.multivenue.archive`) is now part of the design (v1 promised none) — accepted? | Yes — it is what keeps uploads out of the restart lane and off the worker guard |

---

## 15. Relaunch prompt (for the implementing session)

> Continue the S3 archive lane in trading-engine-multivenue. Read
> `docs/s3-archive-plan.md` §0 (implementer contract), §13 (non-interference —
> run `git --no-optional-locks status --porcelain` and `git log --oneline -3`
> on the Mac FIRST and compare with §13.1), then §16 (progress log — the last
> entry is the resume point), then the phase you are on. Laws: zero Rust, no
> cargo, no engine restart, no launchd ops, never `install-launchd.sh`; never
> edit a file dirty from another lane; explicit-path `git add`, commit only on
> my ask, never push; never read or print `.env` / `s3.env`; no network in
> tests (MockTransport); the archiver's long runs go through
> `scripts/archive-run.py` under `~/multivenue/venv/bin/python3` (cmdline
> law); every harness run on a ≤ 2 h `cut_run` window; SPDX header on every
> new `.py`/`.sh`; Python full `import x` only; compile/test on the Mac via
> RustRover (`nohup … &` + poll for > 45 s; `pgrep -f pytes[t]` first);
> research findings to the vault, never git; tell me when short on context
> with a §16 entry + this prompt.

---

## 16. Progress log

Entry format: `- **<date> — <phase> <state>:** what landed (paths) · gates
(numbers) · decisions pinned into §6 · deviations from the plan and why ·
RESUME POINT.`

- **2026-09-05 — review v2.1 (no code):** the plan was fact-checked against
  the tree (Appendix B), restructured for the implementer (§0), re-sequenced
  against RG4/RG6/RG7/G2 (§13), and the SigV4 golden-vector fixture was
  generated (`claude-worker/tests/fixtures/sigv4/vectors.json`, untracked).
  Decisions taken by the reviewer and open for the operator's veto: stage A
  moves out of the restart lane into `com.multivenue.archive` (Q13); the
  cmdline law applies to the archiver (S-LAW 15); `v1/index/` completeness
  truth; explicit `host_id`; `s3.env` read by Python only; cache 20 GiB +
  25 GiB floor; push everything by default; exit-code table. RESUME POINT: S0
  (needs Q2's throwaway/rotated key).

- **2026-09-05 — S0 CLOSED, gate G-S0 PASSED.** Operator rulings taken at the
  session start: §2 S-DOCTRINE **APPROVED**; Q7 = **`~/multivenue/s3.env`**;
  §14 otherwise on defaults; and the §13.2-rule-9 **hold on S4/S6/S7 was
  LIFTED**.
  - **Tree re-verified against §13.1 (rule 1) and it has MOVED AGAIN:**
    `git status --porcelain` = only `?? docs/s3-archive-plan.md` and
    `?? claude-worker/tests/fixtures/sigv4/`; `git log --oneline -3` =
    `4e8e8f3` / `b246069` / `e9687f1`. The RG4 and RG7 sets are **committed**;
    `regime.py`, `window_root.py`, `backtest.py`, `pnl_report.py`, `cli.py`,
    `retention.sh`, `engine-wrapper.sh`, `CLAUDE.md` are all **clean**. §13.1's
    dirty-file table is historical from here on; §11.2's "clean on 2026-09-05"
    column is now true for every row, `CLAUDE.md` included.
  - **Landed:** `~/multivenue/s3.env` (mode 600, 17 keys, never read back);
    `s3.env.example` at the repo root; the git-excluded `tools_`-prefixed probe;
    findings + both raw JSON runs under `docs/research/s3-probe/`.
  - **Gate G-S0: items 1,2,4,5,6,8,9 answered with observed evidence** (full
    table in the vault). Headlines: addressing — virtual **and** path both 200
    (virtual pinned); region token **not validated** (`us-east-1` also 200 — a
    wrong region fails silently, so pin it); SigV2 → 403; ListObjectsV2
    pagination + `delimiter` (5 CommonPrefixes) + exclusive `start-after`, XML
    **namespaced**; multipart initiate/part/complete 200 with the server ETag
    **equal to the locally computed `md5(concat(part md5s))-N`**, and two 1 MiB
    parts → **400 EntityTooSmall** (5 MiB floor confirmed); a deliberately wrong
    `x-amz-content-sha256` → **400 XAmzContentSHA256Mismatch** (the payload
    digest is server-verified — free integrity on every request) while
    `x-amz-checksum-sha256` is accepted on PUT but **not returned** by HEAD;
    `If-None-Match: *` → 412; **R-2 SATISFIED** — anonymous GET and LIST both
    **403 AccessDenied**; versioning **not enabled**, lifecycle **absent**.
    Cleanup: 24 probe objects created, 24 deleted, 0 remaining.
  - **Extra gate item, not in the plan's list, run FIRST and offline: the signer
    matched all 11 golden vectors at all four stages plus the kDate→kSigning
    chain** before a byte crossed the network. S1's signer is that function.
  - **Decisions pinned into §6:** `PART_SIZE_MIB` **64 → 8** (the 130 MiB /
    64 MiB-part attempt died on httpx's `WriteTimeout`; measured uplink is
    1.6–2.3 MiB/s at background QoS, so a 64 MiB part cannot finish inside a
    sane timeout); `TIMEOUT_S` **120 → 300**; `ZSTD_LEVEL` **stays 3, now on
    evidence** (L3 4.86× @211 MiB/s vs L9 5.35× @49.5 MiB/s on real
    `bn-ticks.pmlr` — for a 1 GB run L3 is ≈96 s end to end vs L9 ≈104 s, so L3
    wins *even though the link is the bottleneck*, at a quarter of the CPU next
    to the live ingress threads); **no `x-amz-checksum-*` headers**;
    `ADDRESSING=virtual`; `REGION=fsn1`.
  - **PLAN BUG FOUND AND CORRECTED (§7 S1):** the spec said
    `root.iter("{*}Contents")`. `Element.iter()` does **not** honour the `{*}`
    namespace wildcard — only `find`/`findall`/`iterfind` do — so against
    Hetzner's namespaced XML it returns nothing, silently. The first probe run
    reported `page1_keys: 0` over five real objects while `IsTruncated`, read
    with `find`, was `true`. §7 S1 now says `findall`, with the correction
    recorded inline. Shipping v2.1 verbatim would have produced a listing that
    always looked empty.
  - **Sizing corrected (§5/§9):** the manifest example's "~10×" is optimistic
    for tick data — expect **≈5–6× blended ⇒ ≈0.9 GB/day stored** from ≈5 GB/day
    raw. At the measured uplink: steady state ≈7 min/day (inside the 600 s
    budget, without much headroom); backfill ≈1.6 h of pure transfer, i.e. one
    to three ≤ 2 h invocations (S-LAW 7 holds).
  - **httpx confirmed empirically over 37 requests:** the raw query string is
    preserved byte-for-byte as signed, and an explicit `Content-Length`
    suppresses chunking in every case. Both §0.1 claims stand; S1 keeps them as
    assertions.
  - **CMDLINE LAW (S-LAW 15) proven early:** the long probe ran as
    `~/multivenue/venv/bin/python3 /tmp/<copy>.py --repo <repo>` and
    `pgrep -f 'claude[-_]worke[r]'` was **empty while it ran**. Note the trap:
    the one-shot's own path contains `claude-worker`, so running it in place
    would trip the guard — hence the `--repo` flag and the /tmp copy.
  - **DEVIATIONS (declared, not silent):** (1) multipart was probed at
    **3 × 8 MiB = 24 MiB**, not the planned 130 MiB — the 130 MiB run timed out,
    and 24 MiB proves every semantic the gate asks for (initiate/part/complete,
    composite ETag, minimum part size) while being kinder to the live engine;
    throughput came from a separate 1/4/16 MiB ladder instead. (2)
    `s3.env.example` was created at S0 rather than S2, on the operator's
    explicit ask. (3) A throughput-ladder stage and `--only` / `--timeout-s` /
    `--repo` flags were added to the probe beyond the plan's item list.
  - **OPEN (both are the operator's):** (a) **Q9 not applied** — the bucket
    still has no versioning and no lifecycle rule; the mutating call was refused
    by this session's permission classifier as a bucket-configuration change,
    which the plan already designates an operator action. Until versioning is
    on, S-LAW 11 has no backstop against an accidental overwrite. (b) **Rotate
    the credential pair** — it was transmitted in plaintext chat on 2026-09-05,
    the same condition that made R-1 necessary for the previous pair.
  - **RESUME POINT: S1** (`claude_worker.objstore`) — signer proven, endpoint
    contract pinned, fixture in place. Then S2, S3, S5 and — hold lifted — S4,
    S6, S7.

- **2026-09-05 — S1–S7 LANDED. THE LANE IS CODE-COMPLETE AND LIVE-PROVEN, BUT
  NOT ARMED.** Operator rulings for the run: one commit per phase, explicit
  paths; all live system actions authorised; G-S5 parity on a ≤ 2 h cut window;
  Q6 = track this plan at `docs/arch/`.

  **Commits (explicit-path, never `-A`/`-u`, nothing pushed):**
  `8196280` S1 · `efd3055` S2 · then S3/S5/S4/S6/S7 as recorded in the log.

  **S-LAW 1 PROVEN, not asserted:** `git diff` over `crates/`, `Cargo.toml` and
  `Cargo.lock` is EMPTY for every commit of this lane. No `cargo` command was
  run, the release binary was never relinked, and the engine was never
  restarted. `pyproject.toml` `dependencies` is still the same 4-item list —
  the S3 client is handwritten SigV4 over `hmac`/`hashlib`/`httpx`.

  **Gates.** G-S1: 71 tests, all 11 golden vectors at four stages each plus the
  key chain. G-S2: 34 tests (six required keys dropped in turn, env-over-file,
  missing/unreadable file, host_id regex, closed reason vocabulary, sentinel
  redaction across repr/str/caplog/exception). G-S3: 25 tests (byte-identical
  round trip, resume-after-kill, index-last, `windows_2h_complete` equals
  `window_root.complete_windows`, torn-object overwrite, ETag mismatch leaves
  no manifest, budget exit 5, catalog-null, verify exit codes, 10 000-draw key
  order). G-S4: 8 zsh tests including **the compress-mode diff against
  `git show HEAD:scripts/retention.sh`** — same stderr, same filesystem effect.
  G-S5: 19 tests. G-S6: 9 tests. **Worker suite 723 → 889 passed / 3 skipped,
  additive-green, frozen 202 untouched.** `mypy --strict` clean on all four new
  modules; ruff clean on every file this lane owns; `make license-check` OK.

  **LIVE PROOF (real bucket, engine running):**
  - `push-run` on a real 475 MB closed run: `files=25 bytes=474.55M
    stored=87.38M ratio=5.4x elapsed=908s parts=33`, then `verify` exit 0.
  - `multivenue-archive status` → `archive: enabled bucket=market-data-qu
    host=mbp-m4 runs_remote=1`.
  - **CMDLINE LAW held throughout:** `pgrep -f 'claude[-_]worke[r]'` was empty
    for the whole 908 s upload, while `pgrep -f 'archive-ru[n].py'` matched.
  - Pull observed creating `run-<ns>.partial`, invisible to `features.run_dirs`
    exactly as designed.
  - Ratio 5.4× confirms the S0 prediction (5–6× blended) and retires the §5
    manifest example's optimistic "~10×".
  - **G-S5 PARITY PASSED ON REAL DATA (operator-ruled variant: a ≤ 2 h cut
    window, not the whole run).** The pushed run was pulled back out of the
    bucket into a separate directory; `diff -rq` reports the pulled tree
    **byte-for-byte identical** to the original. `window_root.cut_run` was then
    run on BOTH copies over the same first complete 2 h window (the run holds
    3), and `multivenue-engine audit-pnl --dir <cut>` produced **byte-identical
    stdout** — 2 482 bytes, sha256 `9298bc16cdb0a1b20f5078b17d4168903fa877313a49d0d6ec76010162d5b415`
    from each side, over a non-trivial workload (orders 35 054, fills 108).
    Pitfall 18 was checked first: the release binary (16:51:41) is newer than
    the last `crates/cli` commit (`b246069`, 16:50:56). The local run was never
    moved or deleted — the pull went to a scratch directory, so nothing in
    `~/multivenue/logs` was at risk. The one-off needed
    `MULTIVENUE_S3_CACHE_MIN_FREE_GIB=0` in its environment because the volume
    (22 GiB free) is under the production floor; that is the floor working, not
    failing.

  **DEVIATIONS FROM THE PLAN (declared, with reasons):**
  1. **§7 S3 step 3's free-space rule is WRONG and was not implemented as
     written.** It gates uploads on `cache_min_free_gib`, which equals
     retention's `MIN_FREE_GIB` — and retention only fires BELOW that number.
     The subsystem would therefore refuse to upload at exactly the moment
     retention needs a verified copy: a deadlock. On this deployment it is not
     hypothetical (22 GiB free against a 25 GiB floor), so every push would have
     exited 4 forever and the backlog could never have started. Uploads now
     require a small absolute reserve (`STAGING_RESERVE_GIB`, 2 GiB) against the
     ONE staged file they hold at a time; the full `cache_min_free_gib` floor
     still governs PULLS, which do add a whole run to the cache.
  2. **§7 S1's `root.iter("{*}Contents")` is wrong** (recorded at S0, fixed in
     the text): `Element.iter()` ignores the `{*}` wildcard. `findall`
     throughout, with a regression test whose fake serves namespaced XML.
  3. `objstore.list` needed `builtins.list[...]` annotations inside the class —
     the method named `list` shadows the builtin for annotation resolution.
  4. G-S6's tests live in a NEW `tests/test_archive_agent.py` rather than being
     appended to `tests/test_pnl_report.py` / `tests/test_cli.py`, so that this
     lane adds no diff to files the RG7 soak's own suite touches.
  5. `s3.env.example` landed at S0 (operator ask) rather than S2.

  **INCIDENT, disclosed:** a `ruff --fix` was run with too broad a path scope
  and mechanically modified 20 files belonging to other lanes. Caught by
  `git status` before any commit and reverted with `git checkout --` on exactly
  those 20 paths; the working tree was verified to hold only this lane's four
  modified files afterwards, and nothing of it reached a commit. Lesson for the
  next session: `ruff --fix` takes explicit paths, always.

  **THROUGHPUT — THE OPEN RISK, and the reason stage A must not be armed
  blind.** The real 87.38 MiB upload sustained **0.096 MiB/s**, against
  1.6–2.3 MiB/s measured by the burst probe an hour earlier on the same box,
  same QoS. At the sustained figure one day of capture (≈0.9 GB stored) needs
  **≈2.7 h — 16× the 600 s cycle budget** — and the 71 GB backfill becomes
  days, which no ≤ 2 h invocation can absorb. A follow-up A/B of
  `taskpolicy -b` on vs off did not complete: a 21 MiB ladder was still running
  after 20 minutes, which is itself evidence that the link, not the QoS class,
  was the binding constraint at that moment. **Before arming stage A, re-measure
  and pick a lever:** raise `S3_CYCLE_BUDGET_S`, drop `taskpolicy -b` (at the
  cost of §12.5's feed protection), or add the `--max-mib-s` cap the plan
  already contemplates. Nothing about correctness depends on this; the archive
  simply will not keep up until it is settled.

  **NOT ARMED — deliberately.** `ARCHIVE_MODE` is still `compress`;
  `com.multivenue.archive` is NOT bootstrapped; `install-launchd.sh` was never
  run (it restarts the engine). The plist and the two-line installer edit exist
  so the operator can bootstrap the ONE label by hand — the runbook is in
  `docs/local-setup.md`.

  **STANDING OPEN ITEMS (operator-only):**
  (a) **Bucket versioning and the `AbortIncompleteMultipartUpload` lifecycle
      rule are still unset.** Until versioning is on, S-LAW 11 has no backstop
      against an accidental overwrite. The one-shot is in §16's S0 entry.
  (b) **Rotate the credential pair** — it was transmitted in plaintext chat on
      2026-09-05, the same condition that made R-1 necessary for its
      predecessor. Replace only the two values in `~/multivenue/s3.env`.
  (c) The backfill (71 GB local + 3.6 GB of legacy tarballs) has not been run;
      it must precede `ARCHIVE_MODE=s3`, and (c) above gates how long it takes.

  **FINDING, unrelated but material: `make py-lint` is RED at HEAD and was
  before this lane touched anything** — 696 ruff errors across committed files
  (`test_compose.py` 75, `test_library.py` 63, `compose.py` 62, `library.py` 43,
  …). CLAUDE.md's stay-green list claims it is green. Not this lane's to fix
  (those files belong to other lanes), but the claim should be corrected or the
  files cleaned.

  **RESUME POINT: the lane is closed. What remains is operator action** — (a)
  and (b) above, then a throughput decision, then the backfill, then arm stage A
  and finally stage B on a separate day (§13.2 rule 5: one change at a time in
  the restart lane).

- **2026-09-05 — GO-LIVE. Operator requirement changed the target policy: "keep
  the last 24 h locally, older on demand."** Rulings taken: PROTECT_DAYS 5 → 1
  (**this SUPERSEDES standing ruling D3**); retention becomes a fixed-window
  sweep rather than a pressure-driven one; build tarball pull now; use the
  credentials as supplied (no rotation this session).

  **THE THROUGHPUT MYSTERY, SOLVED — and an earlier §16 conclusion RETRACTED.**
  This entry corrects the previous one. The story in order, because the wrong
  answer was convincing twice:
  1. First real push: 87.38 MiB in 908 s = **0.096 MiB/s**. Recorded as "the
     link is slow and variable".
  2. A 4 MiB A/B of `taskpolicy -b`: 5.21 vs 5.47 MiB/s → recorded as "background
     QoS is free". **Both readings were true and both conclusions were wrong.**
  3. Disk was ruled out (2.2 s of digest+compress for the largest file, even at
     LowPriorityIO).
  4. The isolating test: a 32 MiB unthrottled probe hit **5.76 MiB/s while the
     throttled backfill was crawling on the same link at the same moment**, and
     `sample` showed the backfill parked in
     `_ssl__SSLSocket_write → PySSL_select → poll` — blocked on the socket, not
     on CPU or disk.
  **Conclusion: macOS's background traffic class does not measurably affect a
  short burst but throttles a SUSTAINED transfer by ~13×.** A 4 MiB test cannot
  see it; that is why it looked free. Unthrottled, real runs upload at
  **4.7–5.6 MiB/s** (1.79 GiB raw → 349 MB stored in 69 s; 3.85 GiB → 733 MB in
  130 s), consistently ~5.3× compression.

  **The cost of dropping it is real and was measured, not waved away:** during
  the unthrottled backfill venue feed-delivery EMAs rose transiently (binance
  2 → 210 ms, bybit 9 → 209 ms) before settling back to 21–40 ms, and okx's
  stale-tick counter ran at ~525/min against a ~28/min lifetime average. The
  venue-time gate judges ticks while the upload runs, so that hour of capture is
  measurably worse. The default is nonetheless OFF, justified by DURATION: a
  day's capture is ~1.8 GiB stored ≈ 6 minutes at 5 MiB/s, at 04:30 local.
  `S3_NICE_NETWORK=1` restores the background class for anyone who prefers hours
  of invisible upload.

  **Sizing corrected again:** capture is **9.6 GiB/day raw** measured across 50
  runs over 7.4 days — roughly double the plan's §9 estimate of ~5 GB/day.
  Stored, that is ~1.8 GiB/day.

  **Landed this session:** `pull_tarball` (download → verify sha256 → extract,
  all under the `.partial` invisibility law, sha checked BEFORE `tar` runs so an
  unverified archive is never scattered across the cache) + resolver/CLI
  auto-detection of run-vs-tarball shape; `PART_SIZE_MIB` 8 → **32** and
  `TIMEOUT_S` 300 → **900** (multipart costs ~3× a single PUT *per part*, so
  bigger parts recover most of it, while 32 MiB against 900 s still survives
  ~0.04 MiB/s on a bad night); per-run backfill progress reporting;
  `S3_NICE_NETWORK`; `S3_CYCLE_BUDGET_S` 600 → 3600.

  **`com.multivenue.archive` BOOTSTRAPPED** (one label, rendered from the
  template; `install-launchd.sh` never run). Verified inert: `state = not
  running`, and the engine label kept **pid 85953 before and after**. Both cycle
  guards proven live — `ARCHIVE_MODE=compress` exits 0 silently, and the
  single-instance `pgrep -f 'archive-ru[n].py'` guard correctly skips while a
  backfill is running.

  **STILL BLOCKED, and genuinely operator-only: bucket versioning + the
  `AbortIncompleteMultipartUpload` lifecycle rule.** Two attempts to set them
  were refused by this session's permission classifier as bucket-configuration
  changes. I did not work around it. Until versioning is on, S-LAW 11 has no
  backstop against an accidental overwrite. Set them in the Hetzner Console, or
  re-run the S0 probe's `--enable-versioning --set-lifecycle` in a session where
  that is permitted.

  **THE LANE IS LIVE. Backfill and first sweep, both complete:**
  - **Backfill:** 49/50 runs pushed (the 50th is the live capture, excluded by
    S-LAW 4) + all 40 legacy tarballs. **16.99 GiB stored** for 71 GiB of runs
    plus 3.6 GiB of tarballs. Sustained ~5 MiB/s; compression consistently
    5.0–5.7× on run directories, 1.0× on tarballs (already gzip, stored
    verbatim on purpose — re-packing would destroy the evidence that they are
    what retention actually wrote).
  - **Sample verify: 3/3 PASS** across the oldest, middle and newest archived
    runs, including a v2 and a v3 root.
  - **Tarball pull proven on real data:** a 9.6 MiB legacy archive pulled from
    the bucket, extracted, and compared against a local `tar -xzf` of the same
    file — **17/17 files byte-identical**, the result visible to
    `features.run_dirs` as a real run, and no `.partial` left behind.
  - **`retention.conf` REWRITTEN to the fixed-window policy** (backup kept as
    `retention.conf.bak-*`): `PROTECT_DAYS=1`, `MIN/TARGET_FREE_GIB=999999` so
    the sweep runs nightly rather than only under pressure, `ARCHIVE_MODE=s3`,
    `S3_CYCLE_BUDGET_S=3600`, `S3_NICE_NETWORK=0`.
  - **Preflight before the first sweep:** of 50 local runs — 1 newest (never a
    candidate), 7 protected under 24 h, **42 eligible, and 0 of those missing
    from the bucket**.
  - **First sweep: 37 runs deleted, 0 refused**, stopping cleanly on
    `run-… is 1d old (<= protect 1d) — stopping`. Free space **22 → 75 GiB**;
    log root 72 GiB → 18 GiB; 13 runs local. (37 rather than 42 because the
    sweep re-evaluates age as it goes and the boundary moved.)
  - **Engine untouched throughout:** pid **85953** before and after,
    `engine_vm_rows_active 2`, `engine_regime_configured 1`.
    `multivenue-archive status` → `archive: enabled bucket=market-data-qu
    host=mbp-m4 runs_remote=49`.

- **2026-09-07 — first unattended cycles reviewed; two live defects found and
  fixed.** The nightly lane had been running on its own for two days.

  **What worked unattended, with no help:** the 0000Z sweep verified against the
  bucket and pruned to the window, stopping cleanly at the protect boundary; the
  **0020Z pnl report fired on time** (`2026-09-06: strategies=1 runs=15
  failed=0`) — the thing stage A was kept out of the restart lane to protect;
  the RG4/RG7 window pool is intact at 8 windows; the engine survived its own
  daily restarts.

  **DEFECT 1 — the plist was re-imposing the QoS the script had been told to
  skip.** The 04:30 cycle moved 306 MB in 3185 s (0.096 MiB/s) and then spent
  its budget mid-run, despite `S3_NICE_NETWORK=0`. Cause: launchd's
  `ProcessType=Background` applies macOS background QoS — network traffic class
  included — to the whole job, overriding the script's own decision.
  `LowPriorityIO` compounded it. **Consequence, which is the part that matters:**
  a cycle that cannot keep up means retention can never verify-and-delete, so
  the 24 h policy quietly stops holding. It was already happening — 7 runs
  pending, three of them past 24 h and correctly refused for deletion. Fixed by
  removing both keys (`Nice 19` stays; CPU politeness does not touch the
  socket), so network QoS has ONE lever. Verified: the same work then ran at
  **3.4–5.0 MiB/s** (302 MB in 12 s; 2.81 GiB in 130 s). The run that had exited
  on budget at 27/35 files finished in 12 s — **production proof that resume
  skips what was already uploaded.** Backlog cleared to 0 pending.

  **DEFECT 2 — wiring the resolver into the nightly lane made a TEST RUN reach
  the live bucket** and pull 24 runs / 1.2 GB into the operator's cache. Two
  causes, both fixed: `select_runs` consulted the archive even when the day was
  local (it is now a FALLBACK — consulted only when the day has no local run,
  which is correct because PROTECT_DAYS keeps a whole UTC day on disk), and
  three tests passed `env={}`, which makes the loader fall back to the
  operator's REAL cache dir; they now pin `tmp_path`. The spurious cache was
  emptied (S-LAW 9 — disposable by construction). A regression test asserts the
  locally-present day costs **zero HTTP requests**.

  Two smaller bugs found while proving it: `list_runs` raised `KeyError` on
  `totals` for a run that is local AND held as a legacy tarball (tarball
  manifests carry only `{size_bytes, sha256}`); and `gc --max-gib 0` silently
  did nothing, because the CLI read it as `args.max_gib or cfg.cache_max_gib`
  and swallowed an explicit zero as falsy.

  **Operator rulings 2026-09-07:** keep the 24–48 h window as-is (integer
  `age_days` in `retention.sh` means anything under 48 h survives — a useful
  margin that guarantees the 0020Z report always has its full closed day);
  accept the ~6 min/day of full-priority upload rather than build a rate cap;
  wire the resolver in; versioning to be set in the Hetzner Console.

  **State at close: 58 runs + 40 tarballs, 19.41 GiB in the bucket, 0 pending;
  14 runs / 17 GiB local; 79 GiB free; engine pid 51165 with
  `vm_rows_active 2`; suite 898 passed / 3 skipped; `git diff` over `crates/`
  still 0 lines.**

- **2026-09-07 — docs actualized; legacy pruned (operator ruling).**
  - **Docs moved to this archive:** `venue-time-capture-plan.md` (VT0–VT6,
    closed 2026-09-03 — its §6.1 ≤ 2 h law REMAINS a standing absolute, and is
    cited from nine `crates/` files at the pre-move path, left as written per
    this archive's own convention) and `phase-8-architecture.svg`.
  - **Docs actualized:** `local-setup.md` (the retention section described only
    the pressure-driven policy; the fixed-window one is what runs here, and the
    window is **24–72 h** not 24 h — `age_days` is integer so anything under
    48 h is protected, and the sweep runs once a day); `research-universe.md`
    (it said research reads `~/multivenue/logs/run-*/`, which is no longer the
    whole history — added how to plan a pool from `list --json`'s
    `windows_2h_complete` BEFORE pulling a byte); `migration.md` (the measured
    defaults, `S3_NICE_NETWORK`, the plist trap, `gc --max-gib` semantics, the
    fallback resolver, the retention change); `CLAUDE.md` stay-greens.
    **Corrected a false stay-green while there: `make py-lint` is NOT green and
    was not before this lane** — 696 ruff errors across committed files at HEAD;
    the old "make lint green" covered clippy only.
  - **Legacy tarballs pruned.** All 40 were verified against the bucket first
    (index object present, manifest agreeing, and the on-disk size matching the
    manifest's) with an explicit refuse-all-if-any-fail guard; 40/40 passed and
    **3.60 GiB** was freed. Seven empty leftover directories removed.
    `~/multivenue/archive` now holds nothing but a `.DS_Store`. Re-proven after
    deletion: a tarball with no local copy left still pulls back from the bucket
    and appears to `features.run_dirs` as a real run.
  - **State: 83 GiB free** (from 22 at the start of this lane), 17 GiB of logs,
    1.1 MB of cache, bucket 58 runs + 40 tarballs = 19.41 GiB, engine pid 51165,
    worker pytest 898, git tree clean.

  **RESUME POINT: nothing outstanding but the two bucket-configuration
  settings.** The nightly shape from here: `com.multivenue.archive` uploads at
  04:30 local; `retention.sh` at 0000Z verifies and prunes to 24 h. Watch the
  first unattended cycle in `~/multivenue/logs/launchd/archive.log`, and watch
  that the 0020Z pnl slot still fires on time (it is the reason stage A is not
  in the restart lane).

---

## Appendix A — SigV4 golden vectors

`claude-worker/tests/fixtures/sigv4/vectors.json` holds 11 vectors for the
exact request shapes this client sends (`V1_list_index`, `V2_head_manifest`,
`V3_put_signed`, `V4_initiate_mpu`, `V5_upload_part`, `V6_complete_mpu`,
`V7_get_range`, `V8_put_small_json`, `V9_list_delimiter_token` — a
`continuation-token` containing `/ + =`, `V10_path_style_head`,
`V11_get_versioning`). Each carries the request (`method`, `host`, `path`,
`query` as sent, `body`, `extra_headers`, `x_amz_content_sha256`), the
`canonical_request`, `string_to_sign`, `signature`, and full `authorization`
header; the top level carries the signing-key chain (`kDate`…`kSigning`) for
the fake credentials `AKIAEXAMPLEARCHIVE00` / `archive-golden-secret-…` at
`x-amz-date 20260905T120000Z`, region `fsn1`, plus the `canonical_rules`.
Provenance: generated by the reviewing session with botocore 1.43.89's
`S3SigV4Auth` in a throwaway sandbox; every signature was asserted equal to
botocore's own `Authorization` tail. The test must assert the four stages
separately and MUST NOT be "fixed" by editing the fixture — if the signer
disagrees with a vector, the signer is wrong (the one exception: if S0 shows
Hetzner needs a different signed-headers set, add NEW vectors by the same
method in a throwaway venv, never in the repo's venv, and record the
regeneration in §16).

## Appendix B — What the 2026-09-05 review corrected in v1 (and in v2)

| v1 / v2 claim | Reality | Effect on v2.1 |
|---|---|---|
| `capture-catalog … --json` | no `--json` flag; JSON is always stdout | §0.1, S3 step 5 |
| `retention.sh --dry-run` used by G-S2(c) | no such flag | G-S4(b) compares old vs new scripts behaviourally |
| `make lint` runs ruff + mypy | clippy only; `make py-lint` = ruff; mypy has no target | gates say `make py-lint` + `uv run mypy <new files>` |
| `cut_run` special-cases the file named `ai-cmds.pmlr` | keys on header `slot_kind == 4` | §0.1 |
| `pnl_report --closed-day` uses `features.run_dirs` | uses its own `select_runs` + `window_root` | S6 targets `select_runs` |
| `features.run_dirs`/`latest_run_dir` to be modified | 15 callers; the newest run is always local | NOT modified |
| host id defaults to `scutil --get LocalHostName` | `Antons-MacBook-Pro`, mutable | REQUIRED explicit `host_id` |
| "daily capture is plausibly tens of GB" | ≈ 5 GB/day measured | §9 |
| `windows_2h` = `len(windows_of(...))` from `span_ns` | `windows_of` includes the partial tail; `complete_windows` is the pool law | `windows_2h_complete = floor(span_s/7200)` |
| stay-greens "nextest unchanged count", "alloc 40/40" | counts are 1593 / 42 and belong to RG6; a nextest run competes for the cargo lock | S-LAW 1 proof = `git diff`, no cargo |
| §12.9 "RG2 live smoke awaiting the operator" (v1); "RG6 uncommitted + dashboard ACTIVE" (v2) | RG6 committed `ef75c91` + closed; RG7 is the active lane now | §13 rewritten twice; rule 1 says re-verify |
| `UPLOAD_AHEAD=0` default (v1); stage A inside `retention.sh` at 0000Z (v2) | a `claude-worker/.venv` cmdline inside the restart lane would trip the boot recommit's 5-min guard ⇒ `vm_rows_active 0` for hours; the tick loop would stall | stage A = own agent at a quiet slot + cmdline law (S-LAW 15); stage B = verify only |
| AWS-doc SigV4 vectors "licence-checked in S1" | the AWS example pages have moved (redirect to the API welcome page); values from memory did not reproduce | botocore-derived fixture, present now |
| the existing `~/multivenue/archive/*.tar.gz` | 47 tarballs / 3.6 GB not mentioned | S7 `push-tarball` |
| v2 named the S0 one-shot file | `make license-check` refuses a tracked doc naming a `tools_*.py` outside `docs/arch/` (CLAUDE.md pitfall 16) | class pattern only; Q6 home = `docs/arch/` |
| v2 shell scripts sourced `s3.env` | S-LAW 6 tightened: only the Python loader reads it | §6, S4 |
| v2 `verify` "manifest reads back byte-identical to the local copy" | no local copy exists after staging is removed | index bytes == run-prefix manifest bytes |
| v2 G-S2 "resolver does zero HTTP" | the resolver is S5's file | moved to G-S3 (`archive`) and G-S5 (`data_source`) |
| v2 G-S6 tests had no home in §11 | | `tests/test_pnl_report.py`, `tests/test_cli.py` added to §11.2 |
| v2 `window_root.py` anchor ranges | `POOL_MIN_PMLR_VERSION` at `:276`, `pool_*`/`symlink_root` at `:279-378`, the struct literal at `:46` | §0.1 corrected |
