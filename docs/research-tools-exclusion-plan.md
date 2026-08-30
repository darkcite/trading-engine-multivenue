# Research-tools git exclusion plan (`claude-worker/tools_*.py`)

**Status:** **APPLIED 2026-08-30, UNCOMMITTED** — steps §4.1–§4.5 are in the
working tree and the §5 verification is green (record in §9). The commit is
the operator's; §7 remains deferred and untouched.

**Authority:** subordinate to `CLAUDE.md` (git discipline, Licensing) and
`docs/license-audit-2026-08-27.md` §3.4. It creates two standing laws — the
reserved `tools_` prefix (§4.1/§4.5) and the **reference ban** (§4.6) — and
amends four prose references (§4.3). For the external research corpus it is
subordinate to the license audit, which keeps ownership of that class.

---

## 0. Decision record — operator, 2026-08-30

| Question | Ruling |
|---|---|
| Motive | **Hygiene now, purge later.** Going-forward exclusion is unconditional; the history purge is recorded as a deferred, separately-authorized decision (§7). |
| Location | **In place, glob-ignored.** Files stay at `claude-worker/tools_*.py`; a `.gitignore` glob excludes the class. No moves. |
| `tools_author_v7.py` | **Untrack it too, and amend the docs** so nothing claims a committed source that no longer exists. |

**Follow-up ruling, same day**, after the question *"are we ensuring a
further run will not reference its research artifacts in the permanent
docs?"* — answered honestly as **no**, the §4.4 gate caught tracked files
only:

| Question | Ruling |
|---|---|
| Gate shape | **Hard ban + one allowlisted owner per class.** Only the owning authority doc may name the material; everything else points at it. Chosen over a require-an-annotation-near-the-mention rule, which these aggressively wrapped docs would make brittle in both directions. |
| Scope | **Both excluded classes** — the research one-shots and the external research corpus. The corpus pointer was the sharper risk: it invited a future session into provenance-unconfirmed material with no hint of its status. |

The rulings are recorded here verbatim because §4.3 deletes the evidence
that motivated them.

---

## 1. Scope

### 1.1 The class being excluded

A **repo-side research one-shot**: a Python script that lives in the
`claude-worker/` tree for interpreter and venv convenience, is run by hand
under review, and is **never** imported by the package, never a verb, never
a worker module, and has no tests. It records a moment of research, not a
contract.

Five files match as of application:

| File | Lines | Tracked? | What it is |
|---|---:|---|---|
| `claude-worker/tools_author_v7.py` | 196 | **YES** — `6cc1ba5`, `2a283a5`, `928fc99` | VM2 V7/V8 ruleset authoring one-shot; wrote the sha256-named artifacts incl. the live `xv-v2` |
| `claude-worker/tools_author_bst.py` | 216 | no | M5 binance-stocks candidate ruleset authoring |
| `claude-worker/tools_research_bst.py` | 496 | no | M5 bStock/perp edge mining over `candles.db` |
| `claude-worker/tools_research_bst2.py` | 396 | no | The PMLR ground-truth pass that overturned pass 1 |
| `claude-worker/tools_research_bst3.py` | — | no | Appeared mid-application (2026-08-30) — the class is actively growing, which is the argument for a glob rather than a file list |

They already declare their own status in their module docstrings
("repo-side throwaway tool, not a worker module"). This plan makes git agree
with what the files already say about themselves.

### 1.2 Explicitly NOT in scope

- `claude-worker/src/claude_worker/**` — the package. Anything that earns a
  caller belongs here and stays tracked. This is the escape hatch: promotion
  out of the excluded class is a *move into `src/`*, never an un-ignore.
- `claude-worker/tests/**` — tracked, gated, frozen where marked.
- `scripts/*.sh` — the launchd lane (`carry-cycle`, `xv-cycle`,
  `candles-cycle`, `daily-restart`, `engine-wrapper`, `retention`,
  `seed-push`, `install-launchd`, `recommit-ruleset`). These are *operational
  contracts* the unattended fleet depends on. They stay tracked, and the
  glob in §4.1 cannot reach them.
- `EXTERNAL STRATEGIES TO ONBOARD/` — already ignored on a different and
  stronger ground (unconfirmed third-party provenance, license-audit G8).
  Do not merge the two blocks; the reasons must stay separable (§2.3).

---

## 2. Findings — why this is the right call, and where the honest limits are

### 2.1 They are dead weight in git, by construction

Nothing imports them; the wheel does not ship them
(`[tool.hatch.build.targets.wheel] packages = ["src/claude_worker"]`); no
test collects them (`testpaths = ["tests"]`). A tracked file that no build,
test, or runtime path can reach is documentation — and as documentation these
are worse than the docs that already summarize their findings, because they
encode *one* run's parameters as if they were law.

### 2.2 They inflate a gate whose value is per-file scrutiny

`make license-check` walks `git ls-files '*.rs' '*.py' '*.sh'` — **234 files
today**. Every entry is a file whose SPDX header the gate vouches for and
whose content a downstream consumer may lift. Padding that surface with
one-shots that will never be lifted dilutes exactly the signal the license
audit was run to establish.

### 2.3 There is **no** provenance defect here — this matters for §7

All four files carry `# SPDX-License-Identifier: Apache-2.0` /
`# Copyright 2026 Anton (darkcite)`. They are the operator's own work under
the repo's own license. That is a categorically different situation from
`EXTERNAL STRATEGIES TO ONBOARD/`, which is excluded because material of
unconfirmed ownership **must not enter history**.

The consequence: the hygiene motive fully justifies §4, and it does **not**
justify §7. A history purge buys tidiness, not compliance. §7 prices it.

### 2.4 What is genuinely lost

`tools_author_v7.py` authored the artifacts behind the live `xv-v2` row.
Untracking it removes the script that produced them from the repo. §4.3
replaces that reproducibility claim with the record that actually carries the
weight anyway: the **sha256-named artifact** in
`~/multivenue/artifacts/rulesets/` (the stage verb's recompute law makes the
filename a content hash) plus its **stage/commit seqs in the audit-replay
chain**. The script was never the authority; the hash and the chain were.

---

## 3. Coupling audit — measured, not assumed

| Surface | Effect of the change | Verdict |
|---|---|---|
| `make license-check` | Walk set 234 → **233**; the printed count changes, no failure. Untracked files leave the gate's scope entirely. | Cosmetic |
| Wheel / sdist | None — `packages = ["src/claude_worker"]` never included them | None |
| `uv run pytest` | None — `testpaths = ["tests"]`; **553** stays 553 | None |
| `make nextest` / alloc | None — no Rust touched; **1420 / 39** unchanged | None |
| `make py-lint` (`ruff check`) | **REAL LOSS, measured: exactly 41 findings.** Ruff's `respect-gitignore` defaults to true (0.16.3, verified), so ignored files drop out of the traversal: 285 findings → 244. All 41 are style/complexity noise in the one-shots (magic values, `E501`, too-many-args). | Accepted, with a lever — §6.1 |
| Docs / tests referencing them | Exactly **3** tracked references (§4.3) | Amended |
| Git history | `tools_author_v7.py` remains in `6cc1ba5`, `2a283a5`, `928fc99` | Deferred — §7 |
| Files on disk | Untouched. `git rm --cached` unstages; it does not delete. | None |

---

## 4. The change — five steps

Step 2 is a **git operation and is the operator's**. A session prepares the
tree and stops.

> **Preflight (2026-08-30):** a read-only `git status` issued from the Cowork
> Linux sandbox left a stale zero-byte `.git/index.lock` that the sandbox
> cannot unlink (the known mount hazard). Clear it on the Mac before any git
> op here: `rm -f ~/trading-engine-multivenue/.git/index.lock`.

### 4.1 Step 1 — the `.gitignore` block

Append after the existing external-material block (currently ending line 84),
**before** the `THIRD-PARTY-NOTICES.md` NOTE. Modelled on that block's shape:
a stated reason, a stated scope, a pointer to the authority.

```gitignore
# --- research one-shots (repo-side, never packaged) ---
# Hand-run authoring/mining scripts that record ONE research moment: not
# worker modules, not verbs, never imported by the package, no tests. They
# live here for the venv and the `import claude_worker.*` seam only. Their
# findings belong in docs/; their outputs are the sha256-named artifacts in
# ~/multivenue/artifacts/rulesets, which are the reproducible record.
# Promotion out of this class is a MOVE INTO src/claude_worker/ — never an
# un-ignore. This is hygiene, NOT a provenance rule: these files are the
# operator's own Apache-2.0 work. See docs/research-tools-exclusion-plan.md.
claude-worker/tools_*.py
```

Naming law implied by the glob, and binding from here: **`tools_` is a
reserved prefix inside `claude-worker/`.** A file that must be tracked never
takes it. This is what makes a bare glob safe.

### 4.2 Step 2 — untrack the one tracked file (**operator**)

```sh
cd ~/trading-engine-multivenue
git rm --cached claude-worker/tools_author_v7.py
```

`--cached` is load-bearing: the file survives on disk and stays runnable.
After this, `git status` must show the file as neither staged-modified nor
untracked — the §4.1 glob swallows it. That silence is the done-tell.

### 4.3 Step 3 — amend the references

`git grep -n "tools_author_v7"` returned exactly three hits. Each asserted or
implied that a *committed* script was the record. All three were rewritten to
**point at this document rather than name the file** — the final form, under
the §4.6 ban:

- **`docs/vm2-plan.md`** (the load-bearing one, since it made the explicit
  claim): `committed = the reproducible source` → *"Authored via a
  git-excluded repo-side one-shot (that class, and its worked example:
  `docs/research-tools-exclusion-plan.md` — the only doc that may name one;
  the reproducible record is the sha256-named artifact itself … under the
  stage verb's recompute law, plus its stage/commit seqs in the audit-replay
  chain)"*.
- **`docs/research-universe.md`** (path-to-live): *"author (the worked
  example is a git-excluded local one-shot — see
  `docs/research-tools-exclusion-plan.md`)"*.
- **`claude-worker/tests/test_strategist.py`** (docstring only, no
  assertion): *"authored by a git-excluded local one-shot; see
  docs/research-tools-exclusion-plan.md"*.

A fourth reference, to the **external research corpus**, was folded in under
the same rule (§4.6): `docs/research-universe.md` named that tree as a
research source with no hint of its status. It now states the constraint —
git-excluded, provenance unconfirmed, nothing may enter history until
ownership is recorded in `NOTICE` — and defers to
`docs/license-audit-2026-08-27.md` G8, which owns the rule. That reference
gained information; it did not lose any.

### 4.6 Step 6 — the reference ban (the durable half)

§4.1 and §4.4 stop the *files* entering git. Neither stops a future session
writing `tools_something.py did X` into a permanent doc — which is precisely
the failure this plan was opened to clean up. Prose in `CLAUDE.md` is not a
gate; prose is what let `committed = the reproducible source` exist.

**The law:** git-excluded material may be named in exactly ONE tracked file —
the authority doc that owns its exclusion. Everywhere else points at that doc.

| Class | Owning authority doc |
|---|---|
| Research one-shots (`claude-worker/tools_*.py`) | `docs/research-tools-exclusion-plan.md` (this file) |
| The external research corpus | `docs/license-audit-2026-08-27.md` (G8) |

Enforced in the `license-check` recipe, beside the tracked-file guard:

```make
	refs=$$(git grep -nE 'tools_[a-z0-9_]*\.py|EXTERNAL STRATEGIES TO ONBOARD' -- \
		':!:Makefile' ':!:.gitignore' ':!:docs/arch' \
		':!:docs/research-tools-exclusion-plan.md' \
		':!:docs/license-audit-2026-08-27.md'); \
	if [ -n "$$refs" ]; then \
		echo "  git-excluded material named outside its owning authority doc:"; \
		echo "$$refs" | sed 's/^/    /'; \
		echo "    owners: research one-shots -> docs/research-tools-exclusion-plan.md;"; \
		echo "            external corpus     -> docs/license-audit-2026-08-27.md (G8)"; \
		echo "    Point at the owner doc instead of naming the file or tree."; \
		fail=1; \
	fi; \
```

Four exemptions, each for a reason:

- **`.gitignore`** must name the pattern in order to ignore it.
- **`Makefile`** holds the patterns themselves; scanning it would make the
  gate fail on its own source.
- **`docs/arch/`** is closed history — "never write there, read only for
  archaeology". A gate that fails on an archived doc would force an edit to
  a doc nobody may edit. (It is clean today regardless.)
- **The two owner docs**, which is the whole point of the allowlist.

**The regex bans concrete filenames, not the class glob** — by design, and
worth stating so nobody "fixes" it. `tools_[a-z0-9_]*\.py` matches
`tools_author_v7.py` but NOT `tools_*.py`, because `*` is outside the
character class. Stating the law requires naming the *pattern*; that stays
legal everywhere, including `CLAUDE.md`. Naming a *file the repo does not
ship* is what rots.

**What this deliberately does not do:** it does not stop a session creating a
new one-shot, running it, or recording its *findings* in `docs/`. Findings
are the durable product and belong in the docs. Only the pointer to the
ephemeral artifact is banned.

### 4.4 Step 4 — the regression gate

Without a gate the rule decays the first time someone runs `git add -f` or
names a file `tools_something.py` under a different directory. Add to the
`Makefile`, inside the `license-check` recipe, immediately before the
`if [ $$fail -ne 0 ]` line, so the existing gate carries it at zero extra
cost (`make license-check` is already run before every commit):

```make
	if git ls-files --error-unmatch 'claude-worker/tools_*.py' >/dev/null 2>&1; then \
		echo "  tracked research one-shot(s) — must stay git-excluded:"; \
		git ls-files 'claude-worker/tools_*.py' | sed 's/^/    /'; \
		echo "    (docs/research-tools-exclusion-plan.md; promote into src/claude_worker/ instead)"; \
		fail=1; \
	fi; \
```

Offline, no toolchain, sub-millisecond. It fires on the forced-add path,
which is the only way back in.

### 4.5 Step 5 — the standing law in `CLAUDE.md`

Two edits so no future session re-adds one and no future session is confused
by the absence:

1. In the **Directory guide**, under the `claude-worker/` entry, append:
   *"Research one-shots (`tools_*.py`) are deliberately git-excluded — see
   `docs/research-tools-exclusion-plan.md`. Findings go to `docs/`; anything
   that earns a caller moves into `src/claude_worker/`."*
2. In **Common pitfalls**, add item 16:
   *"**Committing a `claude-worker/tools_*.py` research one-shot** (or
   `git add -f`-ing one). They are ignored by policy; `make license-check`
   fails on a tracked one. If it needs to be tracked, it needs to be a
   module in `src/claude_worker/` with tests — not a forced add."*

---

## 5. Verification — the done-tells

Run in order on the Mac. Sandbox `cargo`/`git` write-ops are not trusted
(CLAUDE.md pitfall 10 + the sandbox git-unlink hazard).

```sh
cd ~/trading-engine-multivenue

# 1. no research one-shot is tracked
git ls-files 'claude-worker/tools_*.py'          # expect: EMPTY

# 2. and none shows as untracked either — the ignore is doing its job
git status --short | grep tools_                 # expect: no output

# 3. the ignore rule attributes to the block we just wrote
git check-ignore -v claude-worker/tools_research_bst2.py
#   expect: .gitignore:<line>:claude-worker/tools_*.py  claude-worker/...

# 4. the files still exist and still run
ls -1 claude-worker/tools_*.py                   # expect: 4 files
cd claude-worker && uv run python -c "import ast,pathlib,sys; [ast.parse(p.read_text()) for p in pathlib.Path('.').glob('tools_*.py')]; print('parse OK')"; cd ..

# 5. the gates
make license-check                               # expect: OK (233 source files)
cd claude-worker && uv run pytest -q; cd ..      # expect: no regression (observed 598 passed)
cargo nextest run --workspace                    # expect: 1420 (unchanged; not rerun — no Rust changed)

# 6a. the tracked-file gate bites (prove it red, then leave it green)
git add -f claude-worker/tools_research_bst.py
make license-check                               # expect: FAILED, names the file
git reset HEAD claude-worker/tools_research_bst.py
make license-check                               # expect: OK again

# 6b. the §4.6 reference gate bites — plant a reference in a permanent doc
printf '\nSee claude-worker/tools_author_v7.py\n' >> docs/mvp-completion-plan.md
make license-check                               # expect: FAILED, names doc:line
git checkout -- docs/mvp-completion-plan.md
make license-check                               # expect: OK again

# 7. no stale reference survives
git grep -n "committed = the reproducible"       # expect: no output
git grep -n "tools_author_v7"                    # expect: 3 hits, all amended
```

Note on check 7 once **this document** is committed: it quotes the old
wording inside the §4.3 diff, so `git grep "committed = the reproducible"`
will then match this file. That single hit is the plan's own record, not a
stale reference.

Alloc gate (**38/39**) needs no rerun — no Rust changed. Fuzz likewise.

Suggested commit shape (operator's to make): one commit,
`chore: exclude claude-worker research one-shots from git`, carrying
`.gitignore`, the `git rm --cached`, the three amendments, the Makefile gate,
the `CLAUDE.md` law, and this document. Staging is explicit-path only —
never `git add -A`/`-u`.

---

## 6. Accepted losses and their levers

### 6.1 Ruff no longer sees them

Real, and the plan does not pretend otherwise: `respect-gitignore` defaults
to true (ruff 0.16.3, verified), so the *traversal* `make py-lint` performs
(`ruff check` with no path ⇒ `.`) stops descending into them.

The lever, run by hand when a research tool is being actively iterated:

```sh
cd claude-worker && uv run ruff check --no-respect-gitignore tools_*.py
```

Two independent reasons this works: ruff analyzes **explicitly passed paths
regardless of ignore rules** unless `force-exclude` is enabled (it is not set
in `pyproject.toml`), and `--no-respect-gitignore` turns the walk filter off
outright. The flag is belt-and-braces — keep it, so the lever survives
someone later enabling `force-exclude`.

Deliberately **not** wired into `make py-lint`. A gate that fails on a
throwaway is a gate that gets bypassed, and bypass habits do not stay local
to throwaways. Optional if churn justifies it later: a separate
`py-lint-research` target that is advisory and never blocks a commit.

### 6.2 No backup — these now exist in exactly one place

Untracked, un-ignored files at least showed up in `git status` as a nag.
Ignored ones are invisible, and a disk loss takes them silently. Two honest
positions:

- **Accept the loss** (consistent with "throwaway", and what their own
  docstrings claim) — recommended; or
- **Archive out of band**, e.g. a line in `scripts/retention.sh` copying
  `claude-worker/tools_*.py` into `~/multivenue/tools-archive/` alongside the
  artifacts they authored. Same tree, same non-git status, survives a
  checkout wipe. This is the cheaper insurance and pairs naturally with the
  artifacts already living there.

### 6.3 Glob breadth

`claude-worker/tools_*.py` is anchored to that one directory (a leading path
component makes the pattern non-recursive relative to the repo root), so it
cannot reach `src/`, `tests/`, or `scripts/`. The reserved-prefix law (§4.1)
plus the §4.4 gate close the remaining hole, which is a *deliberate* forced
add — and that now fails loudly instead of landing quietly.

---

## 7. Deferred — the history purge (operator-gated; **recommendation: do not**)

Recorded per the "purge later" ruling so the decision is not lost.

### 7.1 What it would cost

`tools_author_v7.py` first landed in `6cc1ba5` (VM2 V7). A purge rewrites
every commit from `6cc1ba5` to `HEAD`, which means:

- **Every sha from `6cc1ba5` forward changes** — including `2a283a5`,
  `928fc99`, `d0be3a7`, and everything after. `CLAUDE.md`'s CURRENT STATE,
  `docs/vm2-plan.md` §8, `docs/arch/m3-progress.md` and the M4/M5 logs cite
  those shas as the record of what landed when. A purge silently invalidates
  that entire citation graph, or forces a mechanical rewrite of it.
- **A force-push to `origin/main`**, which the operator pushes by hand — plus
  a fresh clone anywhere the repo is checked out.
- **The launchd fleet** runs from this checkout. A rewrite means a stop,
  re-clone, `cargo build --release -p cli` relink (G0 law), and a restart —
  a capture gap on a live continuity streak, for zero functional gain.

### 7.2 Why not

§2.3: there is no provenance defect. The file is the operator's own
Apache-2.0-headered work. A purge trades a live capture gap and the integrity
of every sha citation in the doc set for tidiness in blobs nobody fetches.

### 7.3 What would flip it

Only new information: if a research one-shot is found to contain material
that is **not** the operator's to license (lifted third-party strategy code,
a pasted vendor payload, an embedded credential). That is the
`EXTERNAL STRATEGIES TO ONBOARD/` category, and it is a different plan —
purge-first, then this document's §4 as the follow-on.

### 7.4 The procedure, if ordered

Non-normative; recorded so it does not have to be re-derived under pressure.

```sh
# 1. full mirror backup FIRST, kept until the rewrite is blessed
git clone --mirror . ~/multivenue/backups/repo-premirror-$(date -u +%Y%m%dT%H%M%SZ).git

# 2. stop the launchd fleet (no capture during a rewrite)
#    launchctl bootout the T2 restart + timers per docs/local-setup.md

# 3. git-filter-repo (NOT filter-branch)
git filter-repo --invert-paths --path claude-worker/tools_author_v7.py

# 4. re-verify EVERY gate from a clean build, then rewrite the sha citations
#    in CLAUDE.md + docs/*.md, then force-push, then re-clone and relink.
```

Steps 3 and 4 are a history rewrite and a force-push: **both forbidden
without an explicit, specific operator order**, per CLAUDE.md git discipline.

---

## 8. Rollback

Cheap and total, because nothing was deleted:

```sh
# undo the exclusion entirely
git revert <commit>            # restores tracking, docs, gate, .gitignore
# or, selectively:
#   drop the .gitignore block, then: git add claude-worker/tools_author_v7.py
```

The files never left disk, so no rollback path depends on a backup. The only
irreversible step in this whole plan is §7 — which is exactly why it is
deferred.

---

## 9. Application record — 2026-08-30

Applied on the Mac through the RustRover terminal lane (the Cowork sandbox
cannot perform git write-ops on this mount). Preflight: the stale
`.git/index.lock` was cleared first.

**Working tree after application** (uncommitted; `docs/research-tools-exclusion-plan.md` untracked):

```
 M .gitignore                              step 1 — the ignore block (line 96)
 M CLAUDE.md                               step 5 — directory-guide line + pitfall 16
 M Makefile                                step 4 — the license-check guard
 M claude-worker/tests/test_strategist.py  step 3(c) — docstring only
D  claude-worker/tools_author_v7.py        step 2 — git rm --cached (ON DISK, index only)
 M docs/research-universe.md               step 3(b)
 M docs/vm2-plan.md                        step 3(a)
?? docs/research-tools-exclusion-plan.md   this document
```

**§5 results — all green:**

| Check | Expected | Observed |
|---|---|---|
| `git ls-files 'claude-worker/tools_*.py'` | empty | **empty** |
| `git status` tools_ lines | none | **none** (all five ignored) |
| `git check-ignore -v` attribution | the new block | **`.gitignore:96:claude-worker/tools_*.py`** on v7 and bst3 |
| Negative control (`src/`, `tests/`, `scripts/`) | not ignored | **not ignored** — glob confirmed non-recursive |
| All one-shots still on disk + parse | 5 files, parse OK | **5 files, `parse OK`** |
| `make license-check` | OK, 233 | **OK (233 source files, LICENSE/NOTICE in sync)** — the predicted 234→233 |
| Gate bites on `git add -f` | FAILED, names the file | **FAILED**, named `tools_research_bst.py`, then **OK** after unstage |
| Stale-reference sweep | none | **`committed = the reproducible` gone; 3 `tools_author_v7` hits, all amended** |
| `uv run pytest` | no regression | **598 passed, 0 failed** (11.82 s, release binary on PATH) |

Rust gates not rerun — no Rust changed (`nextest 1420 / alloc 39` stand).

### 9.1 Second wave — the §4.6 reference ban (same day, after `278a34c`)

`278a34c` shipped §4.1–§4.5. The operator then asked whether a further run
would be *prevented* from referencing research artifacts in the permanent
docs. It would not have been: §4.4 gated tracked **files**, never prose. §4.6
closes that, and the audit that came with it found a fourth reference —
the external-corpus pointer — of a class nobody had looked for.

**Gate proven on both classes, red → green, with actionable `doc:line`:**

| Probe | Result |
|---|---|
| Plant `claude-worker/tools_author_v7.py` in `docs/mvp-completion-plan.md` | **FAILED**, `docs/mvp-completion-plan.md:455` |
| Plant the external-corpus tree name in the same doc | **FAILED**, same line, same actionable output |
| Revert both | **OK (233 source files)** |
| Plant the **class glob** `claude-worker/tools_*.py` (stating the law) | **OK** — the by-design distinction of §4.6 holds |
| Owner docs + `CLAUDE.md`'s pattern mentions | not flagged — exemptions correct |
| Probe residue | none; `git status` clean on the probed doc |

`pytest` after the second wave: **598 passed** (11.0 s), twice.

**One flake, characterized, not a regression.** The first full run of the
second wave reported `1 failed, 597 passed` —
`test_recommit.py::test_recommit_restages_and_recommits_active_row`, failing
as `len(fake_uds.frames) == 0`: no frame reached the `FakeUdsServer`, i.e.
the UDS fixture rather than the assertion under test. Nothing in either wave
touches UDS, recommit or config (the only Python edit in the whole change is
a docstring in an unrelated test file). Re-ran 6/6 green in isolation and
598/598 green on two consecutive full runs. Recorded in `CLAUDE.md`'s macOS
session facts, beside the AF_UNIX hazards it resembles.

### 9.2 Findings carried forward

1. **The pytest stay-green has drifted: 598, not the 553 in `CLAUDE.md`'s
   CURRENT STATE.** Nothing here caused it (the only Python edit is a
   docstring, and the run is 0-failure) — the documented baseline is simply
   behind the tree. Reconcile it at the next phase boundary.
2. **`make py-lint` is RED and was already red**: 244 findings, 154 of them
   in `src/claude_worker/`, none at the lines this change touched (the three
   `test_strategist.py` E501s are at lines 52/180/607, pre-existing). This is
   the Python sibling of the recorded `make lint` / `cargo fmt` red state and
   belongs to its own commit — it is *not* a consequence of the exclusion,
   and it further weakens any argument for gating on the one-shots' 41.
