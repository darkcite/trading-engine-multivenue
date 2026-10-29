#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# host-gates.sh — the Linux gate battery for ONE installed release, ON THE EC2
# HOST, as root (the cargo work runs as `mv`):
#
#   sudo bash /opt/multivenue/current/deploy/ec2-pmj/host-gates.sh [NAME]
#
# NAME is a release (<sha> or <sha>-git; default: the one `current` points at).
# The macOS gates (CLAUDE.md "Gates") say nothing about this binary: another
# OS, another CPU, another codegen (-C target-cpu=native on Intel). So, on the
# release's own source tree (~mv/src/NAME) at its sha, with its own RUSTFLAGS
# (read from its BUILD_INFO), in a SEPARATE target dir (~mv/build/gates-target:
# the release's build cache is never touched), with HOME a SCRATCH directory
# (~mv/build/gates-home, emptied each run: no test can read or write the live
# ~mv/multivenue — its halts, ledgers, .env) and CARGO_HOME / RUSTUP_HOME
# pinned to mv's toolchain, at nice 19 / idle IO inside a MemoryMax scope
# (the engine keeps its CPUs and its memory). It takes the deploy lock
# (~mv/build/.deploy.lock, install-release.sh's): no deploy, rollback or
# prune runs under it, and it waits for none.
#
#   1  tests: `cargo nextest run` (cargo test when nextest is absent) on
#      -p cli -p exec-jupiter -p signer-ed25519 -p core-config -p core-net
#      -p exec-router — every crate slot 11's boot, arm, guard, signer,
#      config and transport run through;
#   2  alloc: `cargo test -p bench --test alloc_assertions --release --
#      --nocapture --test-threads=1` after `cargo clean -p bench --release` —
#      green only with a FRESH `Compiling bench` in the log (the CLAUDE.md
#      false-green guard) and every assertion at 0 B;
#   3  `multivenue-engine pmj-exec --self-test` with the INSTALLED binary
#      (RFC 8032, the guard's known answers, the keypair parser);
#   4  fuzz: NOT RUN — the repo's fuzz targets need nightly, which this host
#      does not carry (recorded as such).
#
# Each verdict line is written into the release's BUILD_INFO (gate_* lines,
# replaced on a re-run) with gates_verdict=GREEN only when 1-3 are green.
# The full output: ~mv/build/gates-NAME-<UTC>.log. Exit 0 = GREEN.
# A green battery is a README prerequisite before any PMJ-DUST-GATE line. It
# reads no secret, dials no venue (crates.io only, for a cold dependency
# fetch) and never touches the running engine — but its test binaries bind
# loopback ports: run it outside the safe-minute drain, and expect the
# wall-clock tests to be slower beside a live engine (a red there is re-run
# alone before it is believed, as on the Mac).
set -uo pipefail

MVUSER="mv"
MVHOME=/home/mv
ROOT=/opt/multivenue
GATES_TARGET="$MVHOME/build/gates-target"
GATES_HOME="$MVHOME/build/gates-home"
MEMORY_MAX="${PMJ_BUILD_MEMORY_MAX:-8G}"
JOBS="${PMJ_GATES_JOBS:-4}"

log() { echo "host-gates: $*" >&2; }
die() { echo "host-gates: $*" >&2; exit 2; }
[ "$(id -u)" = 0 ] || die "run as root (sudo bash …)"

NAME="${1:-}"
if [ -z "$NAME" ]; then
  NAME="$(basename "$(readlink "$ROOT/current" 2>/dev/null)")"
fi
[[ "$NAME" =~ ^[0-9a-f]{40}(-git)?$ ]] || die "no release named '$NAME'"
REL="$ROOT/$NAME"
SRC="$MVHOME/src/$NAME"
INFO="$REL/BUILD_INFO"
[ -f "$INFO" ] || die "$REL is not an installed release"
[ -d "$SRC" ] || die "$SRC (the release's source tree) is missing — redeploy it"
flags="$(sed -n 's/^rustflags=RUSTFLAGS=//p' "$INFO")"
[ -n "$flags" ] || flags="-C target-cpu=native"
exec 9>"$MVHOME/build/.deploy.lock"
flock -n 9 || die "a deploy, rollback, prune or another host-gates.sh holds $MVHOME/build/.deploy.lock"
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
GLOG="$MVHOME/build/gates-$NAME-$STAMP.log"
install -o "$MVUSER" -g "$MVUSER" -m 0600 /dev/null "$GLOG"
rm -rf "$GATES_HOME"
install -d -o "$MVUSER" -g "$MVUSER" -m 0700 "$GATES_HOME"

# The clean environment every gate step runs in, as mv: a scratch HOME, mv's
# toolchain by CARGO_HOME / RUSTUP_HOME, nothing else inherited.
as_mv_clean() {
  sudo -u "$MVUSER" -H env -i HOME="$GATES_HOME" USER="$MVUSER" \
    CARGO_HOME="$MVHOME/.cargo" RUSTUP_HOME="$MVHOME/.rustup" \
    PATH="$MVHOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" "$@"
}

# Run "$@" as mv in the release's tree: the clean environment, the release's
# RUSTFLAGS, the gates' own target dir, nice 19 / idle IO, MemoryMax.
in_tree() {
  # shellcheck disable=SC2016 # "$1" / "$@" are the inner bash's
  systemd-run --quiet --scope --collect -p "MemoryMax=$MEMORY_MAX" -p MemorySwapMax=0 \
    sudo -u "$MVUSER" -H env -i HOME="$GATES_HOME" USER="$MVUSER" \
    CARGO_HOME="$MVHOME/.cargo" RUSTUP_HOME="$MVHOME/.rustup" \
    PATH="$MVHOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
    CARGO_TARGET_DIR="$GATES_TARGET" RUSTFLAGS="$flags" CARGO_TERM_COLOR=never \
    nice -n 19 ionice -c 3 bash -c 'cd "$1" && shift && exec "$@"' _ "$SRC" "$@"
}

log "release $NAME ($(sed -n 's/^transport=//p' "$INFO")), RUSTFLAGS='$flags', log $GLOG"
PKGS=(-p cli -p exec-jupiter -p signer-ed25519 -p core-config -p core-net -p exec-router)

# 1 tests
if (cd "$SRC" && as_mv_clean cargo nextest --version) >/dev/null 2>&1; then
  runner="cargo nextest run"
  echo "=== 1 $runner ${PKGS[*]}" >>"$GLOG"
  in_tree cargo nextest run "${PKGS[@]}" -j "$JOBS" --no-fail-fast >>"$GLOG" 2>&1
  rc=$?
  summary="$(grep -E '^[[:space:]]*Summary \[' "$GLOG" | tail -n 1 | sed 's/^[[:space:]]*//')"
else
  runner="cargo test (nextest absent)"
  echo "=== 1 $runner ${PKGS[*]}" >>"$GLOG"
  in_tree cargo test "${PKGS[@]}" -j "$JOBS" --no-fail-fast >>"$GLOG" 2>&1
  rc=$?
  summary="$(awk '/^test result:/ { p += $4; f += $6; i += $8 } END { printf "test result: %d passed, %d failed, %d ignored (summed)", p, f, i }' "$GLOG")"
fi
if [ "$rc" = 0 ]; then t_verdict="GREEN"; else t_verdict="RED (exit $rc)"; fi
gate_tests="$t_verdict — $runner ${PKGS[*]}: ${summary:-no summary line}"
log "1 $gate_tests"

# 2 alloc (a fresh `Compiling bench` or it is no reading)
echo "=== 2 cargo clean -p bench --release; cargo test -p bench --test alloc_assertions --release -- --nocapture --test-threads=1" >>"$GLOG"
in_tree cargo clean -p bench --release >>"$GLOG" 2>&1
alog="$(mktemp)"
in_tree cargo test -p bench --test alloc_assertions --release -- --nocapture --test-threads=1 >"$alog" 2>&1
rc=$?
cat "$alog" >>"$GLOG"
fresh=0
grep -qE '^[[:space:]]*Compiling bench v' "$alog" && fresh=1
result="$(grep -E '^test result:' "$alog" | tail -n 1)"
if [ "$rc" = 0 ] && [ "$fresh" = 1 ] && printf '%s' "$result" | grep -q 'ok\. [0-9]* passed; 0 failed'; then
  gate_alloc="GREEN — $result (every assertion at 0 B; fresh Compiling bench)"
elif [ "$fresh" = 0 ]; then
  gate_alloc="RED — no fresh 'Compiling bench' in the log: no reading (exit $rc; $result)"
else
  gate_alloc="RED (exit $rc) — ${result:-no test result line}"
fi
rm -f "$alog"
log "2 $gate_alloc"

# 3 the installed binary's offline self-test
echo "=== 3 $REL/target/release/multivenue-engine pmj-exec --self-test" >>"$GLOG"
# shellcheck disable=SC2024 # the log is root's on purpose; mv only writes into the fd
if as_mv_clean "$REL/target/release/multivenue-engine" pmj-exec --self-test >>"$GLOG" 2>&1; then
  gate_selftest="GREEN — pmj-exec --self-test (the installed binary)"
else
  gate_selftest="RED — pmj-exec --self-test failed (the installed binary)"
fi
log "3 $gate_selftest"
gate_fuzz="not run (needs nightly; this host carries the pinned stable toolchain only)"
log "4 fuzz: $gate_fuzz"

verdict=GREEN
case "$gate_tests$gate_alloc$gate_selftest" in *RED*) verdict=RED ;; esac
tmp="$(mktemp)"
grep -vE '^(gates_utc|gates_rustflags|gates_log|gate_tests|gate_alloc|gate_selftest|gate_fuzz|gates_verdict)=' "$INFO" >"$tmp"
{
  echo "gates_utc=$STAMP"
  echo "gates_rustflags=$flags"
  echo "gates_log=$GLOG"
  echo "gate_tests=$gate_tests"
  echo "gate_alloc=$gate_alloc"
  echo "gate_selftest=$gate_selftest"
  echo "gate_fuzz=$gate_fuzz"
  echo "gates_verdict=$verdict"
} >>"$tmp"
install -o root -g root -m 0644 "$tmp" "$INFO"
rm -f "$tmp"
log "gates_verdict=$verdict — written to $INFO"
rm -rf "$GATES_HOME"
[ "$verdict" = GREEN ]
