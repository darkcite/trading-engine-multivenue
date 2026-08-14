.PHONY: help build build-release test test-fast nextest fmt lint \
	check alloc-assert fuzz-quick bench bench-check coverage \
	run-paper clean py-test py-lint

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

clean:
	cargo clean
