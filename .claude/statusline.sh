#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# .claude/statusline.sh — Claude Code status line: model and context use.
# Reads the session JSON on stdin; falls back to a fixed label without jq.
input="$(cat)"
if command -v jq >/dev/null 2>&1; then
    printf '%s' "$input" | jq -r '"multivenue • \(.model.display_name // "?") • \(.context_window.used_percentage // 0)% ctx"'
else
    printf 'multivenue\n'
fi
