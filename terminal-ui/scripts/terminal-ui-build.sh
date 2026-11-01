#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# Build the WASM bundle into terminal-ui/dist (plan v2.1 §4.11.1).
set -euo pipefail
cd "$(dirname "$0")/.."
# The CLI must equal the crate's wasm-bindgen; read it from the lock.
WB=$(awk '/^name = "wasm-bindgen"$/{getline; gsub(/version = |"/,""); print; exit}' Cargo.lock)
if ! wasm-bindgen --version 2>/dev/null | grep -q " $WB\$"; then
  cargo install --locked wasm-bindgen-cli --version "=$WB"
fi
# Ban check on the lock (cargo tree -i is not enough).
if grep -nE '^name = "(tokio|serde_json|reqwest|async-std|ethers|alloy|openssl)"$' Cargo.lock; then
  echo "banned crate in terminal-ui/Cargo.lock" >&2; exit 1
fi
cargo build -p term-ui-web --target wasm32-unknown-unknown --release
TGT="${CARGO_TARGET_DIR:-target}"
rm -rf dist && mkdir -p dist
wasm-bindgen "$TGT/wasm32-unknown-unknown/release/term_ui_web.wasm" \
  --target web --no-typescript --out-name term_ui --out-dir dist
if [[ "${WASM_OPT:-0}" == 1 ]] && command -v wasm-opt >/dev/null; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-reference-types \
    --enable-multivalue --enable-sign-ext --enable-mutable-globals \
    dist/term_ui_bg.wasm -o dist/term_ui_bg.wasm
fi
cp web/index.html web/loader.js web/term.css dist/
test "$(grep -cv '^\s*$' web/loader.js)" -le 15 || { echo "loader.js over 15 lines" >&2; exit 1; }
(cd dist && sha256sum index.html loader.js term.css term_ui.js term_ui_bg.wasm > MANIFEST)
cat dist/MANIFEST
