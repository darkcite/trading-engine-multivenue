#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# The local LLM sidecar for the NEWS cascade's tiers 1-2 (doc 03 §7).
#
# This script parses NOTHING. `news llm-args` reads ~/multivenue/llm.toml,
# verifies the weights against their pinned sha256 and prints the argv; this
# execs it. So the model path, the quantization and every tuning number live in
# the operator's artifact and nowhere else — a weight file that does not match
# its pin never gets served.
#
# Absent llm.toml, absent weights, or a sha256 mismatch: exit 0, silent. The
# lane then runs with no local tiers and counts it. A sidecar is an optimisation,
# never a dependency.
#
# CMDLINE LAW: the resulting argv is `llama-server …`, with no `claude_worker`
# anywhere, so this process is invisible to the global worker guard and may be
# resident forever. `nice` + `taskpolicy -c utility` make it yield to the engine.
set -eu

REPO="$(cd "$(dirname "$0")/.." && pwd)"
VENV="$REPO/claude-worker/.venv"
ALIAS="${CLAUDE_WORKER_VENV_ALIAS:-$HOME/multivenue/venv}"
LLAMA="${LLAMA_SERVER_BIN:-llama-server}"

[ -f "$HOME/multivenue/llm.toml" ] || exit 0
command -v "$LLAMA" >/dev/null 2>&1 || exit 0

# The alias keeps `claude-worker` out of the argv of the process that renders
# the args, the way news-cycle.sh does for the cycle itself.
PY="$ALIAS/bin/python3"
[ -x "$PY" ] || PY="$VENV/bin/python3"
[ -x "$PY" ] || exit 0

ARGS="$("$PY" -m claude_worker.news llm-args --skip-sha 2>/dev/null)" || exit 0
[ -n "$ARGS" ] || exit 0

# shellcheck disable=SC2086 -- the argv is rendered, deliberately word-split
exec nice -n 10 taskpolicy -c utility $ARGS
