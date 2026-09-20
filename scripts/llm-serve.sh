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

[ -f "$HOME/multivenue/llm.toml" ] || exit 0

# launchd gives a job the minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin), which
# does NOT contain a Homebrew prefix — so resolving the binary by name works in
# an interactive shell and silently exits 0 under launchd. Resolve it here, and
# say where it was looked for if it is genuinely absent.
LLAMA="${LLAMA_SERVER_BIN:-}"
if [ -z "$LLAMA" ]; then
  for candidate in /opt/homebrew/bin/llama-server /usr/local/bin/llama-server; do
    [ -x "$candidate" ] && LLAMA="$candidate" && break
  done
fi
[ -z "$LLAMA" ] && LLAMA="$(command -v llama-server 2>/dev/null || true)"
if [ -z "$LLAMA" ]; then
  echo "llm-serve: no llama-server (looked in /opt/homebrew/bin, /usr/local/bin, PATH)" >&2
  exit 0
fi

# The alias keeps `claude-worker` out of the argv of the process that renders
# the args, the way news-cycle.sh does for the cycle itself.
PY="$ALIAS/bin/python3"
[ -x "$PY" ] || PY="$VENV/bin/python3"
[ -x "$PY" ] || exit 0

ARGS="$("$PY" -m claude_worker.news llm-args --skip-sha 2>&1)" || {
  echo "llm-serve: llm-args refused: $ARGS" >&2
  exit 0
}
[ -n "$ARGS" ] || exit 0

# `llm-args` prints the argv starting with the bare name; swap in the resolved
# path so the exec does not depend on PATH either.
ARGS="$LLAMA${ARGS#llama-server}"

# shellcheck disable=SC2086 -- the argv is rendered, deliberately word-split
exec nice -n 10 taskpolicy -c utility $ARGS
