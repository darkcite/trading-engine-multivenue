#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# .claude/hooks/guard-bash.py
#
# PreToolUse hook for Bash. Backs the CLAUDE.md standing laws with a check on
# the full command text, because settings.json Bash rules match only the
# leading words of each subcommand (`git -C . push` slips past `Bash(git push*)`).
#
# Blocks (exit 2, reason on stderr, shown to Claude):
#   - git push / rebase / reset --hard / filter-branch / update-ref
#   - branch creation, deletion or rename (checkout -b, switch -c, branch <name|-d|-D|-m>)
#   - bulk staging (add -A / --all / -u / .) and commit -a / --all
#   - cargo fmt over the workspace without --check, and `make fmt` (HEAD was never fmt-clean)
#   - pkill -f / pgrep-style patterns with no bracketed letter (they match the caller's own shell)
#   - reading a .env file with a pager or text tool (source .env stays allowed)
#
# Anything it cannot parse is allowed: the settings.json rules and the
# operator's prompt are the first line, this is the second.

import json
import re
import sys

GIT = r"\bgit\b(?:\s+-[cC]\s+\S+|\s+--[a-z-]+(?:=\S+)?)*\s+"

RULES = (
    (GIT + r"push\b", "git push is the operator's, by hand (CLAUDE.md: Git law)", True),
    (GIT + r"rebase\b", "no rebase / history rewrite (CLAUDE.md: Git law)", True),
    (GIT + r"reset\s+--hard\b", "no reset --hard (CLAUDE.md: Git law)", True),
    (GIT + r"(?:filter-branch|update-ref|replace)\b", "no history rewrite (CLAUDE.md: Git law)", True),
    (GIT + r"checkout\s+(?:-b|-B|--orphan)\b", "no branches without the operator's ask (CLAUDE.md: Git law)", True),
    (GIT + r"switch\s+(?:-c|-C|--create)\b", "no branches without the operator's ask (CLAUDE.md: Git law)", True),
    (GIT + r"branch\s+(?:-[dDmMcC]\b|--delete\b|--move\b|--copy\b|[A-Za-z0-9_][^\s;&|]*)",
     "no branch create/delete/rename without the operator's ask (CLAUDE.md: Git law)", True),
    (GIT + r"add\s+(?:-A\b|--all\b|-u\b|--update\b|\.(?:\s|$|;|&|\|))",
     "staging is explicit-path only, never -A / -u / . (CLAUDE.md: Git law)", True),
    (GIT + r"commit\s+(?:[^;&|]*\s)?(?:-a\b|--all\b|-[a-zA-Z]*a[a-zA-Z]*\b)",
     "commit -a stages every dirty file; stage explicit paths (CLAUDE.md: Git law)", True),
    (r"\bmake\s+(?:[^;&|]*\s)?fmt\b", "make fmt is cargo fmt --all (CLAUDE.md: never cargo fmt --all; HK28 is the operator's call)", True),
    (r"\bpkill\s+(?:-\w+\s+)*-f\s+(?:'[^'\[]*'|\"[^\"\[]*\"|[^\s'\"\[]+)(?:\s|$|;|&|\|)",
     "pkill -f without a bracketed letter matches your own shell (CLAUDE.md pitfall 15): use e.g. multivenue-engin[e]", False),
    (r"\b(?:cat|head|tail|less|more|bat|strings|xxd|od|hexdump|nl|grep|rg|awk|sed)\b[^;&|]*?"
     r"(?:^|[\s/'\"])\.env(?!\.example)(?:\.[A-Za-z0-9_-]+)?(?=$|[\s'\";&|)])",
     ".env is never read or printed by a session (CLAUDE.md: Secrets law); `set -a; source .env; set +a` is fine", False),
    (r"\b(?:cat|head|tail|less|more|bat|strings|xxd|od|hexdump|nl|grep|rg|awk|sed)\b[^;&|]*?"
     r"multivenue/[A-Za-z0-9_-]*\.env\b",
     "~/multivenue/*.env is never read or printed by a session (CLAUDE.md: Secrets law)", False),
)

FMT = re.compile(r"\bcargo\s+fmt\b(?P<rest>[^;&|]*)")
QUOTED = re.compile(r"'[^']*'|\"[^\"]*\"")


def main() -> int:
    raw = sys.stdin.read()
    try:
        payload = json.loads(raw)
    except ValueError:
        return 0
    tool_input = payload.get("tool_input") or {}
    command = tool_input.get("command") or ""
    if not command:
        return 0
    # git and cargo flags are judged with quoted text blanked, so a commit
    # message such as "fix the -a path" is not read as a flag.
    unquoted = QUOTED.sub("''", command)
    for pattern, reason, on_unquoted in RULES:
        if re.search(pattern, unquoted if on_unquoted else command):
            sys.stderr.write("[guard-bash] BLOCKED: " + reason + "\n")
            return 2
    for match in FMT.finditer(unquoted):
        rest = match.group("rest")
        if "--check" in rest:
            continue
        if "--all" in rest or rest.strip() == "" or rest.strip().startswith("--"):
            sys.stderr.write(
                "[guard-bash] BLOCKED: cargo fmt over the workspace rewrites ~200 files "
                "(CLAUDE.md: never cargo fmt --all; HK28 is the operator's call). "
                "Use rustfmt on the file you changed, or add --check.\n")
            return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
