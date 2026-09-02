# Apache-2.0 Compliance Audit — 2026-08-27

Scope: the whole checkout at `main`, tracked files only (`git ls-files`),
plus the resolved dependency graph in `Cargo.lock` (266 packages) and
`claude-worker/uv.lock`.

**Original verdict: the license was correctly *applied* — LICENSE, NOTICE,
README and all 36 workspace crates were consistent and legally sufficient
for source-form distribution. Ten gaps existed, none of them a current
breach. Two (G3, G4) were real defects in artifacts buildable right now.
One (G5) becomes a hard blocker the first time a compiled binary leaves the
Mac, which is on the Stage-3 / PLAN.md Phase-7 path.**

> **STATUS 2026-08-27: ALL TEN GAPS APPLIED, COMMITTED AND VERIFIED.**
> Commits `2dd88d5` (metadata) · `3989d63` (licence gate + this document) ·
> `9780d42` (SPDX headers, 194 files +525/−0) · `a0b9159` (CLAUDE.md rules) ·
> plus the tooling-fix commit carrying `THIRD-PARTY-NOTICES.md`.
> **Baselines re-run green on the Mac: nextest 1240 · alloc 38 · pytest 439**,
> `cargo deny check licenses` ok, notices generated (131 packages).
> Two PRE-EXISTING failures found and left alone, deliberately: `cargo fmt
> --check` (~88 files) and `cargo clippy -D warnings` (~40 lints). See §3.4.
> §2 is retained as written for the reasoning behind each change.

---

## 1. What is already correct

| # | Check | Result |
|---|-------|--------|
| 1 | `LICENSE` present at repo root | ✅ |
| 2 | `LICENSE` is verbatim canonical Apache-2.0 | ✅ 201 lines, all 9 sections + `END OF TERMS` + `APPENDIX`, LF endings, single trailing newline, no CRLF, md5 `86d3f3a95c324c9479bd8986968f4327` — the standard upstream template hash |
| 3 | Appendix placeholders left as `[yyyy] [name of copyright owner]` | ✅ correct — the LICENSE file must stay verbatim; the filled-in form belongs in NOTICE/README/headers, and it is there |
| 4 | `NOTICE` present at repo root | ✅ (content issue — see **G2**) |
| 5 | `[workspace.package] license = "Apache-2.0"` | ✅ valid SPDX identifier |
| 6 | Every workspace crate declares a license | ✅ **36/36** carry `license.workspace = true`. Zero misses |
| 7 | `README.md` license section | ✅ badge (line 3), `## License` §, full boilerplate block, holder line |
| 8 | Inbound contribution terms | ✅ README line 197 states Apache-2.0 §5. §5 is self-executing — no CLA is legally required |
| 9 | `claude-worker` declares Apache-2.0 | ✅ (form is deprecated — see **G4**) |
| 10 | No vendored third-party source in-tree | ✅ **0** tracked `.c/.h/.cpp/.hpp/.cc/.S/.asm`; a provenance grep (`adapted from`, `ported from`, `derived from`, `copied from`, `reference implementation`, `github.com` refs) across 137 `.rs` + 49 `.py` files returned only technical prose — no code-origin attributions. The NOTICE claim that all external deps are consumed as unmodified registry packages is **accurate** |
| 11 | `core-crypto` / `signer-eip712` originality claim | ✅ consistent with (10). The "audited C" behind the signer is upstream `libsecp256k1` inside the `secp256k1-sys` crate, not in-tree |
| 12 | GitHub license auto-detection | ✅ root `LICENSE` + exact template ⇒ `licensee` will classify the repo as Apache-2.0 |
| 13 | No accidental copyleft in the graph | ✅ nothing GPL/AGPL/LGPL/SSPL in the 266 resolved packages |
| 14 | Secrets hygiene adjacent to licensing | ✅ `.env` gitignored, `.env.example` committed, only 2 `Copyright` strings in tracked files (NOTICE, README) and they agree |

---

## 2. Gaps, ordered by severity

### G1 — Zero per-file license headers *(Medium)*

**Finding.** Not one tracked source file carries a license notice:

| Kind | Files | With SPDX or Apache header |
|------|------:|---------------------------:|
| `.rs` (crates + fuzz) | 137 | **0** |
| `.py` (claude-worker) | 49 | **0** |
| `.sh` (scripts + hooks) | 7 | **0** |
| `.plist` (launchd) | 4 | **0** |

Repo-wide `SPDX` occurrences: **0**.

**Why it matters.** This is not a §4(a) breach — the root LICENSE satisfies
"give any other recipients a copy of this License". The cost is practical
and it compounds:

* §4(c) obliges a downstream redistributor to *retain* all copyright and
  attribution notices found in the Source form. There are none, so the
  moment one file is lifted out — a crate vendored by a counterparty, a
  parser pasted into a gist, an LLM ingesting `strategy-vm/src/lib.rs` —
  provenance is gone and nothing obliges anyone to restore it.
* Automated scanners (FOSSA, ScanCode, `licensee`, `reuse lint`) classify
  header-less files as *unknown license*, which in a diligence or a
  counterparty security review reads as an unlicensed codebase regardless
  of the root file.
* The Apache Appendix explicitly recommends the per-file notice.

**Fix.** SPDX short form — two lines, machine-checkable, REUSE-compliant.

Rust (`//` comments are legal above a `//!` inner-doc block, so this does
not disturb any existing module docs):

```rust
// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # core-ring
//! ...
```

Python (a comment before the module docstring keeps `__doc__` intact):

```python
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Operator verb CLI (design §6) ..."""
```

Shell — insert after the shebang, never before it. `.plist` — XML comment
after the `<?xml ...?>` declaration, or skip (config, not authorship).

One-shot application (review the diff before committing; note the repo rule
that only explicitly-named paths get staged):

```sh
# .rs — prepend above any //! block
for f in $(git ls-files '*.rs'); do
  head -2 "$f" | grep -q 'SPDX-License-Identifier' && continue
  printf '// SPDX-License-Identifier: Apache-2.0\n// Copyright 2026 Anton (darkcite)\n\n%s' "$(cat "$f")" > "$f.tmp" && mv "$f.tmp" "$f"
done

# .py — same, above the docstring
for f in $(git ls-files '*.py'); do
  head -2 "$f" | grep -q 'SPDX-License-Identifier' && continue
  printf '# SPDX-License-Identifier: Apache-2.0\n# Copyright 2026 Anton (darkcite)\n%s' "$(cat "$f")" > "$f.tmp" && mv "$f.tmp" "$f"
done
```

Then `cargo fmt --all && cd claude-worker && uv run ruff check` to confirm
neither formatter objects, and re-run the stay-greens (1240 / 38 / 439) —
the headers are inert, but the alloc gate has been regressed by "trivial"
edits before (CLAUDE.md pitfall 9).

**Keep it enforced.** `.claude/hooks/no-forbidden-crates.sh` is already a
`PreToolUse` Write/Edit gate; the cheapest durable enforcement is a sibling
check in the same hook, plus a Makefile target for humans:

```make
license-check:
	@missing=$$(for f in $$(git ls-files '*.rs' '*.py' '*.sh'); do \
		head -3 "$$f" | grep -q 'SPDX-License-Identifier: Apache-2.0' || echo "$$f"; done); \
	if [ -n "$$missing" ]; then echo "missing SPDX header:"; echo "$$missing"; exit 1; fi
	@echo "license-check: OK"
```

---

### G2 — `NOTICE` carries the license boilerplate *(Medium)*

**Finding.** `NOTICE` lines 4–15 reproduce the "Licensed under the Apache
License, Version 2.0 … See the License for the specific language governing
permissions and limitations" grant.

**Why it matters.** §4(d) makes NOTICE *viral in the attribution sense*:
every downstream redistributor must carry the contents of your NOTICE
forward into theirs. Anything you put in NOTICE, you are asking every future
redistributor to reproduce verbatim, in their own product, where it will sit
next to *their* grant. ASF guidance is explicit that NOTICE holds **only
required attribution** — not the license text, not the boilerplate, not
project description. The boilerplate already lives in README §License, which
is the right home.

**Fix — replacement `NOTICE` (whole file):**

```
Multivenue Trading Engine
Copyright 2026 Anton (darkcite)

This product includes software developed by Anton (darkcite).

Third-party attributions
------------------------
This repository vendors no third-party source. All external dependencies are
consumed as unmodified crates.io / PyPI packages resolved at build time (see
Cargo.toml, Cargo.lock and claude-worker/pyproject.toml); they are not
redistributed in this source tree. Attribution for dependencies linked into
a distributed *binary* is generated at release time into
THIRD-PARTY-NOTICES.md (see docs/license-audit-2026-08-27.md §G5).

Cryptographic primitives that would normally come from a third-party stack
are handwritten in-tree and are original work of this project:
  * crates/core-crypto   — SHA-256, HMAC-SHA256, base64 (RFC 4648)
  * crates/signer-eip712 — EIP-712 typed-data signing over the secp256k1 and
                           tiny-keccak crates (deliberately NOT ethers/alloy)
```

The substance you wrote is right; it is only the boilerplate block that
should go.

---

### G3 — `fuzz/Cargo.toml` declares no license *(Medium — real defect)*

**Finding.** `fuzz/` is `exclude`d from the workspace (`Cargo.toml` line
~52, cargo-fuzz convention), so it **cannot** inherit
`license.workspace = true`. It is the only package manifest in the repo with
no `license` key. That leaves 29 fuzz targets in `fuzz/fuzz_targets/*.rs`
under a package whose declared license is nothing.

**Fix — `fuzz/Cargo.toml`, add to `[package]`:**

```toml
[package]
name    = "polymarket-fuzz"
version = "0.0.0"
publish = false
edition = "2021"
license = "Apache-2.0"                      # cannot inherit: fuzz/ is workspace-excluded
authors = ["Anton (darkcite) <qqq.darkcite@gmail.com>"]
```

---

### G4 — the built Python wheel ships **no** LICENSE and **no** NOTICE *(Medium — real defect)*

**Finding.** Two compounding problems in `claude-worker/pyproject.toml`:

1. `license = { text = "Apache-2.0" }` is the **deprecated** PEP 621 table
   form. PEP 639 superseded it with a bare SPDX expression; setuptools ≥77
   and hatchling ≥1.27 emit deprecation warnings and will eventually reject
   it.
2. There is no `license-files` key, and `LICENSE`/`NOTICE` live one
   directory up at the repo root — **outside the `claude-worker/` project
   root**. Confirmed: `claude-worker/LICENSE` and `claude-worker/NOTICE` do
   not exist.

Consequence: `uv build` in `claude-worker/` produces a wheel and sdist with
**no license file inside**. The instant that wheel is copied anywhere — a
second machine, an EC2 host at Phase 7, a colleague — §4(a) ("give any other
recipients a copy of this License") and §4(d) (carry the NOTICE) are both
breached. Today the worker is only ever run from the checkout, so nothing is
broken *yet*; it breaks silently at first packaging.

**Fix.**

```sh
# physical copies keep every build backend and every archive format happy;
# symlinks work with hatchling but not with every sdist consumer.
cp LICENSE NOTICE claude-worker/
```

```toml
# claude-worker/pyproject.toml — [project] table
license       = "Apache-2.0"            # PEP 639 SPDX expression (was: { text = ... })
license-files = ["LICENSE", "NOTICE"]   # lands in *.dist-info/licenses/
authors       = [{ name = "Anton (darkcite)", email = "qqq.darkcite@gmail.com" }]
```

Add `claude-worker/LICENSE` and `claude-worker/NOTICE` to a `make sync-license`
target (or a one-line check in `license-check` above) so the copies cannot
drift from the originals:

```make
sync-license:
	cp LICENSE NOTICE claude-worker/
	@cmp -s LICENSE claude-worker/LICENSE && cmp -s NOTICE claude-worker/NOTICE \
		&& echo "sync-license: OK"
```

Verify after the change:

```sh
cd claude-worker && uv build && unzip -l dist/*.whl | grep -i licens
```

---

### G5 — no third-party license inventory; blocks the first binary release *(Medium now, High at Stage 3 / Phase 7)*

**Finding.** 266 packages resolve in `Cargo.lock`. There is no `deny.toml`,
no `about.toml`, no `THIRD-PARTY-NOTICES` file, and no license gate in the
`Makefile` or the `.claude` permission set. Nothing in the graph is
copyleft, so there is no contamination risk — but **the attribution
obligations of those licenses attach on binary distribution**, which is
exactly where `docs/mvp-completion-plan.md` §7 → Stage 3 → PLAN.md Phase 7
(EC2) is heading.

Three resolved packages need a deliberate decision rather than a rubber
stamp:

| Package | Version | License | Why it needs attention |
|---|---|---|---|
| **`ring`** | 0.17.14 | **`Apache-2.0 AND ISC`** | **`AND`, not `OR`** — both must be satisfied simultaneously. Reached via `rustls`, so it is linked into **every** release binary. It carries 3,837 lines of C, 22,680 lines of C headers and 31,829 lines of assembly derived from BoringSSL/OpenSSL; those upstream notices must travel with any binary you ship. |
| **`webpki-roots`** | 0.26.11 **and** 1.0.7 (two copies) | **`CDLA-Permissive-2.0`** | Community Data License Agreement — a *data* license, not an OSI software license. It covers the Mozilla CA trust store compiled into the binary by the workspace's "compiled-in Mozilla CA roots" decision. It is permissive and Apache-compatible, but it is **absent from `cargo-deny`'s default allowlist and from most corporate allowlists**, so it will trip the first scan that runs. Needs an explicit allow entry plus its own attribution line. |
| **`unicode-ident`** | 1.0.24 | `(MIT OR Apache-2.0) AND Unicode-3.0` | Build-time only (`proc-macro2`/`syn` path) ⇒ **no binary attribution obligation**, but scanners flag `Unicode-3.0`. Decide once, record that it is build-only, move on. |

Adjacent hygiene surfaced by the same pass — each duplicate is another entry
in the eventual notices file: `webpki-roots` ×2 (0.26.11, 1.0.7),
`getrandom` ×3, `rustix` ×2, `linux-raw-sys` ×2, `hashbrown` ×2,
`once_cell` ×2, `itertools` ×2, `wit-bindgen` ×2, `windows-sys` ×4,
`windows-targets` ×2.

**Fix — `deny.toml` at repo root:**

```toml
# cargo-deny — license gate. Run: cargo deny check licenses
[licenses]
version = 2
# Allowlist: every license present in the resolved graph as of 2026-08-27.
allow = [
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "MIT",
    "MIT-0",
    "ISC",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "Zlib",
    "CC0-1.0",              # secp256k1-sys
    "Unicode-3.0",          # unicode-ident — BUILD-TIME ONLY, not linked into the binary
    "CDLA-Permissive-2.0",  # webpki-roots (Mozilla CA trust data)
]
confidence-threshold = 0.93

# ring is `Apache-2.0 AND ISC`: both are allowed above, so the AND resolves.
# Its LICENSE file is bespoke and scores below threshold on some scanners —
# clarify explicitly if `cargo deny` reports it as unlicensed.
[[licenses.clarify]]
crate      = "ring"
expression = "Apache-2.0 AND ISC"
license-files = [{ path = "LICENSE", hash = 0xbd0eed23 }]

[bans]
multiple-versions = "warn"   # see the duplicate list in the audit; not license-blocking

[advisories]
version = 2
```

**Fix — `THIRD-PARTY-NOTICES.md` as a release artifact:**

```sh
cargo install cargo-about cargo-deny   # one-time, +stable from $HOME (toolchain pin, see CLAUDE.md)
cargo about init                       # writes about.toml + about.hbs
cargo about generate about.hbs > THIRD-PARTY-NOTICES.md
```

```make
license-deps:
	cargo deny check licenses
	cargo about generate about.hbs > THIRD-PARTY-NOTICES.md
	@echo "license-deps: OK — ship THIRD-PARTY-NOTICES.md beside any binary"
```

**Gate it explicitly**: no compiled `multivenue-engine` binary leaves the
Mac without `LICENSE`, `NOTICE` and `THIRD-PARTY-NOTICES.md` alongside it.
That belongs as a line item in the Stage-3 entry gate rather than being
discovered at deploy time.

---

### G6 — copyright holder ≠ `authors` metadata *(Low)*

**Finding.** `NOTICE` and `README` both say `Copyright 2026 Anton
(darkcite)`. `Cargo.toml` says
`authors = ["Multivenue Trading Engine Team"]` — an entity that does not
exist and owns nothing. All 36 crates inherit it, so every crate's metadata
attributes the work to a name that does not match the copyright holder.

Harmless while `publish = false`, but it is the first thing a diligence pass
or a future `cargo publish` surfaces, and mismatched attribution is
precisely the kind of detail that costs time to unwind later.

**Fix — `Cargo.toml` `[workspace.package]`:**

```toml
authors = ["Anton (darkcite) <qqq.darkcite@gmail.com>"]
```

(Or drop `authors` entirely — Cargo has not required it since 1.53.)

---

### G7 — no `CONTRIBUTING.md` *(Low)*

README line 197 correctly states that contributions arrive under §5, and §5
is self-executing — no CLA or DCO is legally necessary. What is missing is a
statement at the point a contributor actually looks. If the repo is or
becomes public, add a short `CONTRIBUTING.md`:

```markdown
# Contributing

By submitting a contribution you agree it is licensed under the Apache
License 2.0, per §5 of that license, with no additional terms.

Every source file carries `SPDX-License-Identifier: Apache-2.0`. Keep it.
Read CLAUDE.md before touching hot-path code: zero allocations, no `dyn` in
hot paths, no tokio/serde_json/reqwest/ethers/alloy, property test + fuzz
target for every ingress parser.
```

---

### G8 — `EXTERNAL STRATEGIES TO ONBOARD/` is untracked *and* un-ignored *(Low, but sharp)*

**Finding.** The directory holds two documents —
`Trading-Strategy-Consolidated-2026-08-05.md` and
`Trading-Strategy-UpliftResearch-2026-08-05.md`. `git ls-files` returns
nothing for it, so it is not committed. It is **also not in `.gitignore`**,
so it sits one `git add -A` away from entering history.

Both documents originate from a separate engagement, reference source
documents (`docs/.Trading-Strategy-ForthOpinion-Final-2026-08-02.md`,
`docs/.Trading-AI-Analyst-and-AIDLC-Plan-2026-08-01.md`) that are **not in
this repo**, and carry no authorship, copyright or license statement of
their own. Their content is substantive and proprietary-looking — measured
backtests, sleeve specs, fold protocols, named strategy parameters.

The repo already forbids `git add -A` (CLAUDE.md, parallel-session
protocol), which is the main protection. Two things to add:

1. Ignore it now, so the rule is enforced by tooling rather than discipline:

```gitignore
# --- external material, provenance unconfirmed (see docs/license-audit-2026-08-27.md G8) ---
EXTERNAL STRATEGIES TO ONBOARD/
```

2. Before *any* of that material — text, numbers, or derived
   implementations — lands in-tree, confirm you own it or are licensed to
   relicense it under Apache-2.0, and record the provenance in `NOTICE`. A
   strategy implemented from a spec you own is fine; a spec copied from a
   document you do not own is not, and the distinction disappears once it is
   in git history.

*(Unrelated but noticed in the same sweep: `_wtest` is a tracked 0-byte
file, and `.DS_Store` is present in the working tree though correctly
ignored. Neither is a license issue.)*

---

### G9 — docs and diagrams licensing not stated *(Low — informational)*

`docs/*.md`, `docs/*.svg`, `PLAN.md`, `CLAUDE.md`, `AGENTS.md` all fall under
the repo's Apache-2.0 by default, which is valid — Apache-2.0's "Source"
definition explicitly covers documentation source. No action required. If
you would prefer docs under CC-BY-4.0 (common for architecture write-ups you
want quoted freely), that has to be stated explicitly in README; silence
means Apache-2.0.

---

### G10 — no trademark disclaimer *(Low — worth adding for a public repo)*

Apache-2.0 §6 grants no trademark rights, and the repo names Polymarket,
Binance, OKX, Deribit and Hyperliquid throughout — legitimate nominative use
to describe interoperability, not a legal problem. But a public trading repo
that names five venues benefits from one explicit line, both to signal
non-affiliation and because venues have historically been sensitive about
third-party tooling implying endorsement. Append to `README.md` §License:

```markdown
Polymarket, Binance, OKX, Deribit and Hyperliquid are trademarks of their
respective owners. This project is not affiliated with, endorsed by, or
sponsored by any of them, and names them only to describe interoperability.
Per Apache-2.0 §6, this license grants no trademark rights.
```

---

## 3. Application record — 2026-08-27

All ten gaps applied to the working tree in one pass. **Nothing staged,
nothing committed, no branch created** — per the standing git discipline.
Tree was clean before the pass (only untracked docs), so no parallel lane's
work was at risk.

### 3.1 What changed

| Gap | Files | Change |
|---|---|---|
| **G1** | 194 (`137 .rs`, `50 .py`, `7 .sh`) | SPDX + copyright header prepended; shebang-aware for shell |
| **G2** | `NOTICE` | boilerplate removed; attribution kept; binary-notices pointer added |
| **G3** | `fuzz/Cargo.toml` | literal `license = "Apache-2.0"` + `authors` (cannot inherit — workspace-excluded) |
| **G4** | `claude-worker/pyproject.toml`, `claude-worker/LICENSE`, `claude-worker/NOTICE` | PEP 639 SPDX string + `license-files` + `authors`; root LICENSE/NOTICE copied into the project root so the wheel actually ships them |
| **G5** | `deny.toml`, `about.toml`, `about.hbs`, `Makefile` | license allowlist (12, each verified against crates.io for the version in `Cargo.lock`), cargo-about template, `make license-deps` |
| **G6** | `Cargo.toml` | `authors = ["Anton (darkcite) <qqq.darkcite@gmail.com>"]` (was the non-existent "Multivenue Trading Engine Team") |
| **G7** | `CONTRIBUTING.md` | new — §5 inbound terms, provenance rule, header rule, the CLAUDE.md hard rules, the green baselines |
| **G8** | `.gitignore` | `EXTERNAL STRATEGIES TO ONBOARD/` ignored; explicit note that `THIRD-PARTY-NOTICES.md` is deliberately **not** ignored |
| **G9** | `README.md` | documentation and diagrams explicitly covered on the same terms |
| **G10** | `README.md` | trademark disclaimer (five venues) + third-party-dependency section |

Also added: `make license-check` (offline, ~1 s — header coverage +
LICENSE/NOTICE drift + the fuzz manifest key) and `make sync-license`.

Totals: **201 modified, 6 added, 635 insertions, 20 deletions.** All 20
deletions are the five intentional metadata edits (`Cargo.toml` −1,
`Makefile` −1, `NOTICE` −16, `README.md` −1, `pyproject.toml` −1). Every
one of the 194 header files is a **pure insertion, zero deletions** — that
is the proof no content was lost.

### 3.2 Verified in-tree

* Header coverage **137/137 `.rs`, 50/50 `.py`, 7/7 `.sh`**; `make
  license-check` passes at 194 files.
* `git diff --numstat` reports **0 deletions** across all 194 header files;
  insertions total exactly 525 = 137×3 + 50×2 + 7×2.
* Every touched file's byte count grew by **exactly** the header length
  (75 for `//`, 72 for `#`); the script refused to write any file that did
  not, and none did.
* Shebangs remain line 1 in all 7 shell scripts; `bash -n` clean on both
  bash scripts (the 5 zsh scripts have no zsh in the audit sandbox).
* Rust headers sit **above** the `//!` inner-doc block and above `#![no_main]`
  in the fuzz targets — legal placement; comments may precede inner
  attributes.
* Python headers sit above the module docstring: `ast.get_docstring` still
  resolves for **all 48** files parseable by the sandbox's Python 3.10.
  The 2 that do not parse (`feeds.py:96`, `labeling.py:102`) use PEP 758
  unparenthesised `except ValueError, TypeError:` — **3.14-only syntax,
  confirmed pre-existing** by parsing the `HEAD` version with the same
  interpreter and getting the identical failure.
* All **42** TOML manifests parse, with the new/changed keys read back and
  confirmed.

### 3.3 One regression, caught and fixed

The header script wrote each file via `> tmp && mv`, which creates a new
inode at the umask and **destroyed the executable bit** on the five
`scripts/*.sh` files — `100755 → 100644` on `engine-wrapper.sh`,
`daily-restart.sh`, `candles-cycle.sh`, `retention.sh`,
`install-launchd.sh`. Committed like that, the **live unattended M3 launchd
fleet would have failed at its next 00:00Z restart.**

Caught by `git diff --summary`, fixed with `chmod +x`, re-verified:
`git diff --summary` is now empty and `git ls-files -s` shows `100755` for
all five. `.claude/hooks/*.sh` were already `100644` in the index before
this session and were deliberately left as found.

**Rule for any future repo-wide rewrite: verify `git diff --summary` is
empty of mode changes, or use `chmod --reference` / in-place editing.**
`--numstat` does not show mode changes and will not catch this.

### 3.4 Verification run — 2026-08-27 08:38–08:47Z, on the Mac

All of it run on the Mac, none in a sandbox (CLAUDE.md pitfall 10).

| Check | Result |
|---|---|
| `cargo nextest run --workspace` | ✅ **1240 passed**, 1 skipped — baseline exactly |
| `cargo test -p bench --test alloc_assertions --release -- --test-threads=1` | ✅ **38 passed**, 0 B/op. False-green guard satisfied: `target/release/deps/alloc_assertions-*` re-linked at 15:40 local during this run |
| `uv run pytest` | ✅ **439 passed** in 12.62 s — baseline exactly; frozen surfaces intact |
| `uv sync` | ✅ re-resolved cleanly; hatchling accepted PEP 639 `license-files` |
| `uv build` + wheel inspection | ✅ **`claude_worker-0.2.0.dist-info/licenses/LICENSE` (11357 B) and `.../NOTICE` (1295 B)** — G4 proven fixed; the wheel previously shipped neither |
| `make license-check` | ✅ 194 source files, LICENSE/NOTICE in sync |
| `cargo deny check licenses` | ✅ **licenses ok** over the full graph, zero warnings after trimming two over-listed entries |
| `make license-deps` | ✅ **`THIRD-PARTY-NOTICES.md` generated — 200,795 bytes, 131 packages, 60 license sections** |
| `cargo fmt --all -- --check` | ⚠️ FAILS — **pre-existing**, see below |
| `cargo clippy --workspace --all-targets -- -D warnings` | ⚠️ FAILS (~40 lints) — **pre-existing**, see below |
| `uv run ruff check` | ⚠️ FAILS (RUF002 en-dash in docstrings, PLR0913) — **pre-existing** |

**The two pre-existing failures are not caused by this pass, and that is
proven, not assumed.** The header commit is `+525 −0` with every `.rs` file
gaining exactly 3 lines and deleting 0 — no Rust source line changed. Spot
checks across five crates confirm line *N* today is byte-identical to line
*N−3* pre-licence (`core-config/universe.rs:954`,
`ingress-deribit/lib.rs:1065`, `ingress-okx/discovery.rs:273`,
`strategy-set/lib.rs:755`, `ingress-polymarket/run_loop.rs:409`). Running
`rustfmt --check` on the pre-licence blob of `alloc_assertions.rs` returns
47 diffs of its own.

* **`cargo fmt --check`: ~88 files drifted.** Import ordering and
  `assert_eq!` wrapping, `max_width = 100`. The repo has evidently never
  been fmt-clean, and nothing gates it. Fixing means a large mechanical
  diff — worth its own commit, deliberately **not** folded into the licence
  work.
* **`cargo clippy -D warnings`: ~40 lints** across core-config, ingress-ai,
  ingress-binance, ingress-deribit, ingress-hyperliquid, ingress-okx,
  ingress-polymarket, ingress-rpc, strategy-set. Mostly `clippy::ptr_arg`
  (`&PathBuf` where `&Path` would do) and `assert!(true)` in test helpers.
  `make lint` therefore does not currently pass. Also its own task.

**Findings from the first real `license-deps` run**

* The graph attributes **131 packages** into a distributed binary — Apache-2.0
  77, MIT 28, ISC 18, CC0-1.0 3, **CDLA-Permissive-2.0 2**, BSD-3-Clause 1,
  Unicode-3.0 1, Zlib 1. The two CDLA entries are exactly the predicted
  `webpki-roots` 0.26.11 and 1.0.7.
* **`ring` alone contributes 18 separate license/notice documents** to the
  file — its BoringSSL/OpenSSL heritage, reproduced in full. This is the
  concrete form of the §G5 concern: shipping the binary without this file
  would drop 18 required attributions on that one dependency.
* Two allowlist entries (`MIT-0`, `BSD-2-Clause`) matched nothing and were
  removed from `deny.toml`/`about.toml`. A pre-approved licence that nothing
  uses is a licence approved for no reason.
* `unicode-ident` still appears despite `ignore-build-dependencies` —
  harmless over-attribution; left as is rather than configured away.

**Two tooling defects found and fixed during the run**

1. `about.toml`'s `filter-noassertion = false` — cargo-about 0.9 expects a
   table, not a bool. Removed; the default already surfaces unresolvable
   licences.
2. The Makefile's `cargo about generate > THIRD-PARTY-NOTICES.md`
   **truncated the committed notices file to zero bytes the moment generate
   failed** — observed live. Now writes `.tmp` and `mv`s on success only.
   Left unfixed, this is precisely how a binary ships with an empty
   attribution file.
3. The handlebars template HTML-escaped the licence texts (`&quot;`
   throughout a legal document). Switched to triple-stache; verified **0
   HTML entities** in the output.

### 3.4b Commands, for repeat runs

```sh
cargo nextest run --workspace
cargo build --release -p cli && \
  cargo test -p bench --test alloc_assertions --release -- --test-threads=1
cd claude-worker && uv sync && uv run pytest
make license-check
make license-deps        # needs: cargo +stable install cargo-deny; \
                         #        cargo +stable install cargo-about --features cli
```

Note `cargo-about` needs `--features cli` or it installs no binary at all
and still reports success for the other package.

<details>
<summary>Original pre-run checklist (kept for the record)</summary>

```sh
cargo fmt --all -- --check          # headers are inert, but confirm
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace                                            # expect 1240
cargo build --release -p cli && \
  cargo test -p bench --test alloc_assertions --release -- --test-threads=1   # expect 38, 0 B/op
cd claude-worker && uv sync && uv run pytest                             # expect 439
cd claude-worker && uv run ruff check
make license-check
```

The pytest run is the one with a real chance of movement: `pyproject.toml`
changed, so `uv sync` re-resolves. If hatchling rejects `license-files`,
its version is older than 1.27 — `uv lock --upgrade-package hatchling`.
Confirm the wheel now carries the files:

```sh
cd claude-worker && uv build && unzip -l dist/*.whl | grep -i licens
```

Then, once (needs network + `$HOME` toolchain per CLAUDE.md):

```sh
cargo install cargo-deny cargo-about
make license-deps        # writes THIRD-PARTY-NOTICES.md — commit it
```

`make license-deps` is the only step that can still surface a surprise: it
is the first time the full 266-package graph is machine-checked rather than
spot-checked. If it rejects a license not in the `deny.toml` allowlist,
that is the gate doing its job — read what the license obliges before
adding the line.

</details>

### 3.5 Commit shape (operator's call)

Three commits are cleaner than one, and the third is the one worth reading:

1. **metadata** — `Cargo.toml`, `fuzz/Cargo.toml`, `NOTICE`, `README.md`,
   `.gitignore`, `CONTRIBUTING.md`, `claude-worker/{pyproject.toml,LICENSE,NOTICE}`
2. **license tooling** — `deny.toml`, `about.toml`, `about.hbs`, `Makefile`,
   `THIRD-PARTY-NOTICES.md`, this document
3. **SPDX headers** — the 194-file mechanical pass, alone, so it stays
   reviewable as "+525 −0" rather than hiding a real change inside noise

**Ownership note:** the header pass necessarily touched M3-owned paths
(`claude-worker/**`, `scripts/*.sh`) and `.claude/hooks/*.sh`. M3 is
complete but calendar-waiting C6, so the additive-files-only coordination
rule was still nominally in force. The tree was clean, so nothing was
clobbered — but sequence commit 3 with M3's lane, and stage by explicit
path, never `git add -A` (which would now also be the thing that sweeps in
`EXTERNAL STRATEGIES TO ONBOARD/`, were it not ignored as of this pass).

No frozen surface changed semantically: `backtest.py` and `cli.py` received
the two-line header and nothing else, and the verb surface is untouched.
The 202 frozen worker tests should be unaffected — confirm, do not assume.

## 4. Method

* Tracked-file enumeration: `git ls-files` (untracked and ignored paths
  reported separately where relevant).
* LICENSE verified structurally (line count, all 9 section headers,
  `END OF TERMS`, `APPENDIX`, encoding, terminator) and by md5 against the
  canonical upstream template.
* Manifest coverage: every `crates/*/Cargo.toml` and `fuzz/Cargo.toml`
  grepped for a `license` key.
* Header coverage: `grep -rl "Apache License\|SPDX-License-Identifier"` over
  all `.rs`, `.py`, `.sh`.
* Provenance: keyword sweep for code-origin attributions plus an extension
  sweep for vendored native source.
* Dependency licenses: `Cargo.lock` package set cross-checked against the
  crates.io API for the versions actually resolved — `ring` 0.17.14,
  `webpki-roots` 0.26.11, `unicode-ident` 1.0.24, `zmij` 1.0.21,
  `secp256k1-sys` 0.10.1. The remaining 261 packages were **not**
  individually verified; that is what **G5**'s `cargo-deny` /
  `cargo-about` pass is for, and no conclusion in this document depends on
  them.

**Not legal advice** — this is an engineering compliance review. If the
repo goes public or ships binaries commercially, have counsel confirm the
Stage-3 release checklist.
