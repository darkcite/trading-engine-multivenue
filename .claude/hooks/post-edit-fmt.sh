#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# .claude/hooks/post-edit-fmt.sh
#
# Post-tool-use hook for Write/Edit. REPORT-ONLY (operator ruling HK28,
# 2026-10-18): checks whether the edited file is rustfmt / ruff-format clean
# and prints a one-line note to stderr. It never rewrites a file — HEAD was
# never fmt-clean, and reformatting a whole file on first touch would pull
# unrelated lines into the diff. Formatting is the operator's call.
#
# Never blocks: exit 0 always (a PostToolUse hook cannot undo the edit anyway).

set -u

payload="$(cat)"
file_path="$(printf '%s' "$payload" \
    | sed -n 's/.*"file_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    | head -n1)"

if [ -z "$file_path" ]; then
    exit 0
fi

case "$file_path" in
    *.rs)
        if command -v rustfmt >/dev/null 2>&1; then
            if ! rustfmt --edition 2021 --check "$file_path" >/dev/null 2>&1; then
                printf '%s\n' "[fmt] note: $file_path is not rustfmt-clean (not rewritten; HK28)." 1>&2
            fi
        fi
        ;;
    *.py)
        case "$file_path" in
            */claude-worker/*)
                if ! ( cd "$(dirname "$file_path")" \
                        && uv run ruff format --check "$file_path" >/dev/null 2>&1 ); then
                    printf '%s\n' "[fmt] note: $file_path is not ruff-format-clean (not rewritten; HK28)." 1>&2
                fi
                ;;
        esac
        ;;
esac

exit 0
