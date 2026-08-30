.PHONY: help build build-release test test-fast nextest fmt lint \
	check alloc-assert fuzz-quick bench bench-check coverage \
	run-paper clean py-test py-lint \
	license-check sync-license license-deps

help:
	@echo "targets:"
	@echo "  build           cargo build --workspace"
	@echo "  build-release   cargo build --release --workspace"
	@echo "  test            cargo nextest run --workspace"
	@echo "  test-fast       cargo test --workspace --lib (skip integration/bench compile)"
	@echo "  nextest         alias for test"
	@echo "  fmt             cargo fmt --all"
	@echo "  lint            cargo clippy --workspace --all-targets -- -D warnings"
	@echo "  check           cargo check --workspace --all-targets"
	@echo "  alloc-assert    cargo test --test alloc_assertions --release"
	@echo "  fuzz-quick      cargo fuzz run polymarket_clob_frame -- -max_total_time=60"
	@echo "  bench           cargo bench --workspace"
	@echo "  bench-check     diff criterion output against crates/bench/baselines/*.json"
	@echo "  coverage        cargo llvm-cov --workspace --html (line + branch coverage)"
	@echo "  run-paper       cargo run --release -p cli -- run --paper --env-file ./.env"
	@echo "  py-test         cd claude-worker && uv run pytest"
	@echo "  py-lint         cd claude-worker && uv run ruff check"
	@echo "  license-check   SPDX header + LICENSE/NOTICE sync gate (offline, fast)"
	@echo "  sync-license    refresh claude-worker/{LICENSE,NOTICE} from the root copies"
	@echo "  license-deps    cargo deny check licenses + regenerate THIRD-PARTY-NOTICES.md"
	@echo "  clean           cargo clean"

build:
	cargo build --workspace

build-release:
	cargo build --release --workspace

test: nextest

nextest:
	cargo nextest run --workspace

test-fast:
	cargo test --workspace --lib --bins

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets -- -D warnings

check:
	cargo check --workspace --all-targets

alloc-assert:
	# --test-threads=1 is REQUIRED: the CountingAllocator uses process-
	# global atomic counters, so parallel test threads pollute each
	# other's AllocGuard delta. Serial execution gives us per-test
	# isolation without turning the allocator into a TLS-tracked beast.
	cargo test -p bench --test alloc_assertions --release -- --nocapture --test-threads=1

fuzz-quick:
	cargo fuzz run polymarket_clob_frame -- -max_total_time=60

bench:
	cargo bench --workspace

bench-check:
	# Run the hot_path bench and diff against the checked-in baseline.
	# Exits non-zero if any sample regresses more than `tolerance_pct`.
	cargo bench -p bench --bench hot_path -- \
		--warm-up-time 1 --measurement-time 2 --sample-size 50
	python3 crates/bench/baselines/check_regression.py

coverage:
	# Requires `cargo install cargo-llvm-cov` (one-time). Writes HTML
	# report to target/llvm-cov/html. Excludes the bench crate so the
	# alloc-assertion harness doesn't pollute the report.
	cargo llvm-cov --workspace --html --exclude bench -- --test-threads=1

run-paper:
	cargo run --release -p cli -- run --paper --env-file ./.env

py-test:
	cd claude-worker && uv run pytest

py-lint:
	cd claude-worker && uv run ruff check

# ---- licensing (docs/license-audit-2026-08-27.md) ----

license-check:
	# Offline, no toolchain, ~1 s. Every tracked .rs/.py/.sh must carry the
	# SPDX identifier in its first 3 lines, and the claude-worker copies of
	# LICENSE/NOTICE must be byte-identical to the root originals — without
	# them the built wheel ships with no license file at all (Apache-2.0
	# §4(a)/§4(d)). Two further guards keep git-excluded material excluded:
	# a TRACKED research one-shot fails (the only way one comes back is
	# `git add -f`, which must fail loudly rather than land quietly), and so
	# does NAMING excluded material anywhere but its owning authority doc —
	# a reference outliving the file is how a permanent doc comes to point
	# at nothing. Owners: research one-shots ->
	# docs/research-tools-exclusion-plan.md; external corpus ->
	# docs/license-audit-2026-08-27.md G8. docs/arch is closed history and
	# exempt; this Makefile is exempt because it holds the patterns.
	@fail=0; n=0; \
	for f in $$(git ls-files '*.rs' '*.py' '*.sh'); do \
		n=$$((n+1)); \
		head -3 "$$f" | grep -q 'SPDX-License-Identifier: Apache-2.0' || \
			{ echo "  missing SPDX header: $$f"; fail=1; }; \
	done; \
	cmp -s LICENSE claude-worker/LICENSE || \
		{ echo "  drift: claude-worker/LICENSE != LICENSE  (run: make sync-license)"; fail=1; }; \
	cmp -s NOTICE claude-worker/NOTICE || \
		{ echo "  drift: claude-worker/NOTICE != NOTICE  (run: make sync-license)"; fail=1; }; \
	grep -q '^license' fuzz/Cargo.toml || \
		{ echo "  fuzz/Cargo.toml has no license key (workspace-excluded — it cannot inherit)"; fail=1; }; \
	if git ls-files --error-unmatch 'claude-worker/tools_*.py' >/dev/null 2>&1; then \
		echo "  tracked research one-shot(s) — must stay git-excluded:"; \
		git ls-files 'claude-worker/tools_*.py' | sed 's/^/    /'; \
		echo "    (docs/research-tools-exclusion-plan.md; promote into src/claude_worker/ instead)"; \
		fail=1; \
	fi; \
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
	if [ $$fail -ne 0 ]; then echo "license-check: FAILED"; exit 1; fi; \
	echo "license-check: OK ($$n source files, LICENSE/NOTICE in sync)"

sync-license:
	# claude-worker is its own PEP 621 project root; PEP 639 license-files
	# cannot reference paths outside it, so the root files are copied in.
	cp LICENSE NOTICE claude-worker/
	@echo "sync-license: OK"

license-deps:
	# Requires: cargo install cargo-deny cargo-about (see CLAUDE.md on the
	# 1.88.0 toolchain pin — install +stable from $$HOME if the in-repo
	# install trips it). Run this whenever a dependency is added, changed
	# or removed, and commit the regenerated notices with that change.
	cargo deny check licenses
	# Write via a temp file: a plain `> THIRD-PARTY-NOTICES.md` truncates the
	# committed notices to zero bytes the instant the generate step fails,
	# which is exactly how a binary ships with an empty attribution file.
	cargo about generate about.hbs > THIRD-PARTY-NOTICES.md.tmp
	mv THIRD-PARTY-NOTICES.md.tmp THIRD-PARTY-NOTICES.md
	@echo "license-deps: OK — THIRD-PARTY-NOTICES.md regenerated."
	@echo "  Ship LICENSE + NOTICE + THIRD-PARTY-NOTICES.md beside ANY distributed binary."

clean:
	cargo clean
