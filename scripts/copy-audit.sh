#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# scripts/copy-audit.sh — the mechanical half of the zero-copy gate.
#
# THE RULE (operator, 2026-09-19): everything that can be done zero-copy
# is done zero-copy; a copy that cannot be avoided carries a
# `// COPY: <what> <bound> — <why unavoidable> — <alternative rejected>`
# line within the EIGHT lines above it (a real justification runs to a
# few lines), the way an `unsafe` block carries `// SAFETY:`.
#
# This script lists every byte-copy verb in the given crate directories
# (default: the exec lane + core-net) that has NO `COPY:` marker within
# the eight preceding lines, and compares the list against the committed
# baseline `scripts/copy-audit-baseline.txt`. It is a RATCHET:
#
#   * a hit that is in the baseline is legacy debt — printed in the
#     summary count, never a failure;
#   * a hit that is NOT in the baseline is NEW and FAILS the gate
#     (exit 1) until it is either made zero-copy or marked `COPY:`;
#   * a baseline entry that no longer matches is debt paid — reported,
#     and `--update-baseline` drops it. The baseline may only shrink
#     by that path; growing it is an operator decision, never the
#     auditor's.
#
#   scripts/copy-audit.sh                       # exec lane + core-net
#   scripts/copy-audit.sh crates/ingress-okx    # any crate dir(s)
#   scripts/copy-audit.sh --update-baseline     # rewrite the baseline
#
# Keys are `<file>:<line text with whitespace collapsed>`, not line
# numbers, so an unrelated edit above a legacy copy does not churn the
# baseline. Comment-only lines are skipped. Scanning stops at the first
# `#[cfg(test)]` line of a file (test modules sit at the end by
# convention here; a copy in a test is cold and not this gate's
# business). Files under any `tests/` directory are skipped.
#
# A whole module may opt out with a doctrine line in its first 60 lines:
#     //! COPY-DOCTRINE: <why every copy in this module is cold>
# — the same shape as the "offline paths MAY allocate" module headers.
# It is for operator tools and boot self-tests (the smoke, the lifecycle
# probe, the selftest), never for anything the engine loop can reach;
# the auditor checks that claim when it reads the header.
#
# The `.claude/agents/zero-copy-auditor` agent runs this first and then
# judges every NEW line it prints; the list is a CANDIDATE list, not a
# verdict — a `[u8; 20]` address by value is fine, a 16 KiB body
# staged through a second buffer is not — but the auditor has to say
# so per line, which is the point.

set -u

baseline="scripts/copy-audit-baseline.txt"
update=0
if [ "${1:-}" = "--update-baseline" ]; then
    update=1
    shift
fi
if [ "$#" -eq 0 ]; then
    set -- crates/exec-router crates/exec-hyperliquid crates/signer-eip712 \
           crates/clob-dispatcher crates/core-net
fi

# Bracket expressions, not backslash escapes: an awk `-v` value has its
# escapes processed once by awk itself, so `\(` reaches the regexp as a
# bare `(` on gawk and the sweep dies (seen 2026-09-19, and it died
# SILENTLY into a green summary — hence the awk exit guard below).
verbs='copy_from_slice|clone_from_slice|copy_within|extend_from_slice|ptr::copy[(]|ptr::copy_nonoverlapping|copy_nonoverlapping[(]|[.]to_vec[(]|[.]to_owned[(]|Vec::from[(]|from_utf8_lossy|[.]concat[(]|read_to_end|read_to_string|io::copy[(]'

tmp_hits="$(mktemp)"
tmp_keys="$(mktemp)"
trap 'rm -f "$tmp_hits" "$tmp_keys"' EXIT

status=0
for dir in "$@"; do
    if [ ! -d "$dir" ]; then
        printf 'copy-audit: no such directory: %s\n' "$dir" 1>&2
        status=2
        continue
    fi
    while IFS= read -r file; do
        awk -v verbs="$verbs" -v file="$file" '
            NR <= 60 && /COPY-DOCTRINE:/ { doctrine = 1 }
            /^[[:space:]]*#\[cfg\(test\)\]/ { intest = 1 }
            {
                line[NR] = $0
                if (intest || doctrine) next
                if ($0 ~ /^[[:space:]]*\/\//) next
                if ($0 ~ verbs) {
                    covered = 0
                    for (k = NR; k >= NR - 8 && k >= 1; k--) {
                        if (line[k] ~ /COPY:/) { covered = 1; break }
                    }
                    if (!covered) {
                        key = $0
                        gsub(/[[:space:]]+/, " ", key)
                        sub(/^ /, "", key); sub(/ $/, "", key)
                        printf "%s\t%d\t%s\n", file, NR, key
                    }
                }
            }
        ' "$file" || { printf 'copy-audit: awk failed on %s — the sweep is INVALID, not green\n' "$file" 1>&2; exit 3; }
    done < <(find "$dir" -name '*.rs' -not -path '*/tests/*' -not -path '*/target/*' | sort)
done >> "$tmp_hits"

# key = file:text
awk -F'\t' '{ printf "%s:%s\n", $1, $3 }' "$tmp_hits" | sort -u > "$tmp_keys"
hits=$(wc -l < "$tmp_keys" | tr -d ' ')

if [ "$update" -eq 1 ]; then
    cp "$tmp_keys" "$baseline"
    printf 'copy-audit: baseline rewritten: %s entries in %s\n' "$hits" "$baseline"
    exit "$status"
fi

if [ ! -f "$baseline" ]; then
    printf 'copy-audit: no baseline at %s — every hit is NEW\n' "$baseline" 1>&2
    : > "$tmp_keys.base"
else
    sort -u "$baseline" > "$tmp_keys.base"
fi
trap 'rm -f "$tmp_hits" "$tmp_keys" "$tmp_keys.base"' EXIT

while IFS=$'\t' read -r file nr key; do
    if ! grep -qxF -- "$file:$key" "$tmp_keys.base"; then
        printf '%s:%s: %s\n' "$file" "$nr" "$key"
    fi
done < "$tmp_hits"

# Counts are over UNIQUE keys, like the baseline itself: one legacy
# message text repeated on ten lines is one debt entry, not ten.
new=$(comm -23 "$tmp_keys" "$tmp_keys.base" | wc -l | tr -d ' ')
paid=$(comm -13 "$tmp_keys" "$tmp_keys.base" | wc -l | tr -d ' ')
baselined=$((hits - new))
printf 'copy-audit: hits=%s baselined=%s new=%s paid=%s (dirs: %s)\n' \
    "$hits" "$baselined" "$new" "$paid" "$*"
if [ "$paid" -gt 0 ]; then
    printf 'copy-audit: %s baseline entries no longer match — run --update-baseline to drop them\n' "$paid"
fi
if [ "$new" -gt 0 ]; then
    printf 'copy-audit: FAIL — %s unmarked copy verb(s) not in the baseline (mark `// COPY:` or make it zero-copy)\n' "$new"
    exit 1
fi
exit "$status"
