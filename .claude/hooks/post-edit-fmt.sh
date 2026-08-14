#!/usr/bin/env bash
# .claude/hooks/post-edit-fmt.sh
#
# Post-tool-use hook for Write/Edit. If the edited file is a Rust file,
# runs `rustfmt` on it (best-effort, non-blocking). If Python, runs
# `uv run ruff format` inside claude-worker/.
#
# Never blocks — formatting failures should not abort Claude's turn.

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
            rustfmt --edition 2021 "$file_path" >/dev/null 2>&1 || true
        fi
        ;;
    *.py)
        case "$file_path" in
            */claude-worker/*)
                ( cd "$(dirname "$file_path")" \
                    && uv run ruff format "$file_path" >/dev/null 2>&1 ) || true
                ;;
        esac
        ;;
esac

exit 0
