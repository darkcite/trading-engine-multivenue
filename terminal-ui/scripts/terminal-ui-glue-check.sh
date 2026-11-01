#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# The glue gate (TERM plan §4.11.1, T-L14): dist/term_ui.js is the only
# generated JavaScript the terminal runs. It must match the pinned hash in
# glue.sha256 — a wasm-bindgen change is re-pinned on purpose, never by
# accident — and it must not contain eval or new Function (the CSP grants
# 'wasm-unsafe-eval' only). Run after scripts/terminal-ui-build.sh.
# Re-pin with: scripts/terminal-ui-glue-check.sh --pin
set -euo pipefail
cd "$(dirname "$0")/.."
glue=dist/term_ui.js
test -f "$glue" || { echo "glue-check: $glue missing; build first" >&2; exit 1; }
if grep -nE '(^|[^A-Za-z_.])eval\(|new Function\(' "$glue"; then
  echo "glue-check: eval / new Function in the glue" >&2; exit 1
fi
got=$(sha256sum "$glue" | awk '{print $1}')
if [[ "${1:-}" == "--pin" ]]; then
  echo "$got  term_ui.js" > glue.sha256
  echo "glue-check: pinned $got"; exit 0
fi
want=$(awk '{print $1}' glue.sha256)
if [[ "$got" != "$want" ]]; then
  echo "glue-check: term_ui.js is $got, pinned $want — review the diff, then --pin" >&2; exit 1
fi
echo "glue-check: OK ($got)"
