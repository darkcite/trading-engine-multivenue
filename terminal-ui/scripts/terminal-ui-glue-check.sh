#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# The glue gate (TERM plan §4.11.1, T-L14): dist/term_ui.js is the only
# generated JavaScript the terminal runs. It must match the pinned hash in
# glue.sha256 — a wasm-bindgen change is re-pinned on purpose, never by
# accident — and it must not contain eval or new Function (the CSP grants
# 'wasm-unsafe-eval' only). Run after scripts/terminal-ui-build.sh.
#
# What is pinned is the glue with rustc's symbol hashes normalised, every
# other byte as generated. The closure-invoke exports wasm-bindgen names in
# the glue carry rustc's legacy symbol hash (`__h` + 16 hex digits, plus
# wasm-bindgen's `_N` suffix when two exports share a name). That hash comes
# from cargo's -C metadata, which hashes the absolute path of term-proto (a
# path dependency outside this workspace) and the host triple of the
# proc-macro and build-script units, so the raw glue changes with the
# checkout path and the build host while the code does not. Each distinct
# such token is renumbered by order of first appearance (`__h@0`, `__h@1`,
# …): the hash values drop out, which call site refers to which function
# stays pinned, and so does every other byte.
#
# One more host-dependent number lives in comments only: the `shim_idx: N`
# in wasm-bindgen's `// Cast intrinsic for ...` lines is an index into the
# wasm module's function space, which differs between a macOS and a Linux
# build (1942 vs 1936 for the same commit, 2026-11-12). On comment lines
# (first non-blank characters `//`) each distinct `shim_idx: N` is renumbered
# the same way (`shim_idx: @0`, ...); no executable byte is touched.
#
# Re-pin with:            scripts/terminal-ui-glue-check.sh --pin
# Dump the normalised glue for a diff:  scripts/terminal-ui-glue-check.sh --dump FILE
set -euo pipefail
cd "$(dirname "$0")/.."
glue=dist/term_ui.js
test -f "$glue" || { echo "glue-check: $glue missing; build first" >&2; exit 1; }
if grep -nE '(^|[^A-Za-z_.])eval\(|new Function\(' "$glue"; then
  echo "glue-check: eval / new Function in the glue" >&2; exit 1
fi
# POSIX awk (no {n} intervals, no gawk extensions) so that macOS awk agrees.
normalise() {
  awk '
    BEGIN { x = "[0-9a-f]"; re = "__h"
            for (i = 0; i < 16; i++) re = re x
            re = re "(_[0-9]+)?"; n = 0; m = 0 }
    { s = $0; out = ""
      while (match(s, re)) {
        tok = substr(s, RSTART, RLENGTH)
        if (!(tok in id)) id[tok] = n++
        out = out substr(s, 1, RSTART - 1) "__h@" id[tok]
        s = substr(s, RSTART + RLENGTH)
      }
      s = out s
      if (s ~ /^[ \t]*\/\//) {
        out = ""
        while (match(s, /shim_idx: [0-9]+/)) {
          tok = substr(s, RSTART, RLENGTH)
          if (!(tok in sid)) sid[tok] = m++
          out = out substr(s, 1, RSTART - 1) "shim_idx: @" sid[tok]
          s = substr(s, RSTART + RLENGTH)
        }
        s = out s
      }
      print s }
  ' "$glue"
}
if [[ "${1:-}" == "--dump" ]]; then
  test -n "${2:-}" || { echo "glue-check: --dump FILE" >&2; exit 2; }
  normalise > "$2"; echo "glue-check: normalised glue written to $2"; exit 0
fi
raw=$(sha256sum "$glue" | awk '{print $1}')
got=$(normalise | sha256sum | awk '{print $1}')
syms=$(normalise | grep -oE '__h@[0-9]+' | sort -u | wc -l | tr -d ' ')
if [[ "${1:-}" == "--pin" ]]; then
  echo "$got  term_ui.js (normalised: rustc symbol hashes renumbered, see scripts/terminal-ui-glue-check.sh)" > glue.sha256
  echo "glue-check: pinned $got ($syms symbol hashes renumbered; raw $raw)"; exit 0
fi
want=$(awk '{print $1}' glue.sha256)
if [[ "$got" != "$want" ]]; then
  echo "glue-check: normalised term_ui.js is $got, pinned $want (raw $raw) — dump it with --dump, review the diff, then --pin" >&2; exit 1
fi
echo "glue-check: OK ($got normalised, $syms symbol hashes renumbered; raw $raw)"
