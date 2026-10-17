#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# .claude/hooks/no-forbidden-crates.sh
#
# Pre-tool-use hook for Write/Edit. Reads the tool-call JSON from stdin and
# blocks edits that introduce crates banned by CLAUDE.md's hard rules:
#
#   - tokio, async-std            (no async runtime on hot path)
#   - serde_json                  (handwritten byte scanners only)
#   - reqwest                     (hyper + rustls only)
#   - ethers, alloy               (direct secp256k1 + tiny-keccak only)
#   - any aws-sdk-*, azure-*, google-cloud-*  (no cloud services, any phase)
#
# Also blocks `from x import y` in any .py file (codebase rule).
#
# Exit codes (per Claude Code hook protocol):
#   0  -> allow
#   2  -> block, write reason to stderr (shown to Claude)
#   *  -> other errors fall through as "allow" for robustness

set -u

payload="$(cat)"

# Extract file_path and the new-content-bearing field (Write -> "content",
# Edit -> "new_string") from the hook JSON. The payload is ONE line with the
# file's newlines escaped as \n, so it must be decoded before any per-line
# grep: a sed extraction left `^` anchored at the payload start and missed
# every `from x import y` below line 1 (found 2026-10-17).
field() {
    printf '%s' "$payload" | python3 -c '
import json
import sys
try:
    tool_input = json.load(sys.stdin).get("tool_input") or {}
except ValueError:
    sys.exit(0)
sys.stdout.write(str(tool_input.get(sys.argv[1]) or ""))
' "$1"
}

if command -v python3 >/dev/null 2>&1; then
    file_path="$(field file_path)"
    new_content="$(field new_string)"
    if [ -z "$new_content" ]; then
        new_content="$(field content)"
    fi
else
    # Fallback without python3: single-line view, first-line-only Python check.
    file_path="$(printf '%s' "$payload" \
        | sed -n 's/.*"file_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
        | head -n1)"
    new_content="$(printf '%s' "$payload" \
        | sed -n 's/.*"new_string"[[:space:]]*:[[:space:]]*"\(.*\)".*/\1/p')"
    if [ -z "$new_content" ]; then
        new_content="$(printf '%s' "$payload" \
            | sed -n 's/.*"content"[[:space:]]*:[[:space:]]*"\(.*\)".*/\1/p')"
    fi
fi

# If we couldn't parse anything, allow and let downstream validation catch it.
if [ -z "$file_path" ]; then
    exit 0
fi

reject() {
    printf '%s\n' "[hook] BLOCKED: $1" 1>&2
    exit 2
}

case "$file_path" in
    */Cargo.toml|*/Cargo.toml.in)
        for crate in tokio async-std serde_json reqwest ethers alloy aws-sdk- azure_ google-cloud-; do
            if printf '%s' "$new_content" | grep -q "$crate"; then
                reject "Cargo.toml would introduce forbidden crate pattern: '$crate' — see CLAUDE.md."
            fi
        done
        ;;
    *.rs)
        for banned in "use tokio" "use async_std" "use serde_json" "use reqwest" "use ethers" "use alloy"; do
            if printf '%s' "$new_content" | grep -q "$banned"; then
                reject "Rust code would introduce forbidden import: '$banned' — see CLAUDE.md."
            fi
        done
        ;;
    *.py)
        # A real import line: `from <module> import`, excluding `from __future__`.
        # Prose that merely starts with "from" (docstrings, comments) passes.
        bad_from=$(printf '%s\n' "$new_content" \
            | grep -E '^[[:space:]]*from[[:space:]]+[A-Za-z_.][A-Za-z0-9_.]*[[:space:]]+import([[:space:]]|\(|$)' \
            | grep -vE '^[[:space:]]*from[[:space:]]+__future__[[:space:]]' || true)
        if [ -n "$bad_from" ]; then
            reject "Python file uses 'from x import y' — codebase rule is full 'import x' only."
        fi
        ;;
esac

exit 0
