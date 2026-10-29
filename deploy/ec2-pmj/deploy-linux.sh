#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# deploy-linux.sh — ship a commit of the repo to the EC2 standalone, build it
# THERE and install it as /opt/multivenue/<sha>/ (install-release.sh). Run ON
# THE MAC by the operator.
#
#   deploy/ec2-pmj/deploy-linux.sh <host> [--ref REF] [--with-git]
#                                  [--kit-from-worktree] [--jobs N] [--dry-run]
#   deploy/ec2-pmj/deploy-linux.sh <host> --rollback [NAME]   # NAME: <sha> or <sha>-git
#   deploy/ec2-pmj/deploy-linux.sh <host> --status
#   deploy/ec2-pmj/deploy-linux.sh <host> --prune [--target]
#
# The source is a COMMIT (default HEAD), never the working tree:
#   default      `git archive <sha>` — writes nothing to .git. The binary then
#                reports /state boot.git_sha "unknown" and carries NO PMJ gate
#                line: crates/cli/build.rs reads the gate in the tree AND in
#                `git show HEAD:` and any git failure is 0 (BOOT-AUDIT.md C8).
#                Paper only.
#   --with-git   a one-commit shallow repository of <sha> (`git init` in a temp
#                dir + `git fetch --depth 1 file://<repo> <sha>`: the source
#                repo is only READ, through upload-pack; nothing is written to
#                its .git). The build sees the real HEAD: boot.git_sha is the
#                sha, the gate lines compile in when the COMMITTED risk policy
#                carries them, and scripts/pmj-flip.sh can `git show HEAD:` on
#                the host. Required for any release that may arm slot 11:
#                the wrapper refuses to arm an archive-transport release, and
#                install-release.sh refuses to make one current while slot 11
#                is armed.
# The release is /opt/multivenue/<sha> (archive) or <sha>-git (--with-git):
# one directory per transport. After a deploy, run the Linux gate battery on
# the host (host-gates.sh, README step 4b) — a prerequisite before any dust
# line.
# The kit itself (deploy/ec2-pmj) comes from <sha>; while it is not committed
# yet, --kit-from-worktree ships this working tree's copy instead (recorded in
# the release's BUILD_INFO as kit_source=worktree).
#
# It never copies .env (git-ignored; refused if a tarball carries one), never
# restarts the engine (it boots the new release at its next drain — README
# "Taking a release"), and writes nothing on the Mac but a temp dir it removes.
set -u -o pipefail

HOST="${1:-}"
case "$HOST" in
  '' | -*) echo "usage: deploy-linux.sh <host> [--ref REF] [--with-git] [--kit-from-worktree] [--jobs N] [--dry-run] | --rollback [NAME] | --status | --prune [--target]" >&2; exit 64 ;;
esac
shift
REF="HEAD"
WITH_GIT=0
KIT_WT=0
JOBS=4
DRY=0
MODE=deploy
RB_SHA=""
PRUNE_ARG=""

say() { echo "deploy-linux: $*" >&2; }
die() { echo "deploy-linux: $*" >&2; exit 1; }
need() { [ $# -ge 2 ] && [ -n "$2" ] || die "$1 needs a value"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --ref) need "$@"; REF="$2"; shift 2 ;;
    --with-git) WITH_GIT=1; shift ;;
    --kit-from-worktree) KIT_WT=1; shift ;;
    --jobs) need "$@"; JOBS="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    --status) MODE=status; shift ;;
    --prune)
      MODE=prune
      shift
      if [ $# -gt 0 ] && [ "$1" = --target ]; then PRUNE_ARG="--target"; shift; fi
      ;;
    --rollback)
      MODE=rollback
      shift
      if [ $# -gt 0 ] && [[ "$1" =~ ^[0-9a-f]{40}(-git)?$ ]]; then RB_SHA="$1"; shift; fi
      ;;
    *) die "unknown argument: $1" ;;
  esac
done
[[ "$JOBS" =~ ^[1-9][0-9]?$ ]] || die "--jobs must be 1..99"

KIT="$(cd "$(dirname "$0")" && pwd -P)"
REPO="$(cd "$KIT/../.." && pwd -P)"
SSH=(ssh -o ServerAliveInterval=30 -o ServerAliveCountMax=6 "$HOST")
run() {
  if [ "$DRY" = 1 ]; then
    printf '[dry-run]' >&2
    printf ' %q' "$@" >&2
    printf '\n' >&2
    return 0
  fi
  "$@"
}

if [ "$MODE" = status ]; then
  run "${SSH[@]}" 'sudo bash -s status' <"$KIT/install-release.sh"
  exit $?
fi
if [ "$MODE" = prune ]; then
  say "prune on $HOST: releases other than current, previous and the running one${PRUNE_ARG:+, and the build caches}"
  run "${SSH[@]}" "sudo bash -s prune $PRUNE_ARG" <"$KIT/install-release.sh"
  exit $?
fi
if [ "$MODE" = rollback ]; then
  say "rollback on $HOST: current <- ${RB_SHA:-previous} (no restart; the engine boots it at its next drain)"
  run "${SSH[@]}" "sudo bash -s rollback $RB_SHA" <"$KIT/install-release.sh"
  exit $?
fi

# --- the commit -------------------------------------------------------------
G=(git --no-optional-locks -C "$REPO")
SHA="$("${G[@]}" rev-parse --verify --quiet "$REF^{commit}")" || die "'$REF' is not a commit in $REPO"
SHORT="${SHA:0:12}"
say "ref $REF = $SHA"
if "${G[@]}" cat-file -e "$SHA:deploy/ec2-pmj/pmj-wrapper.sh" 2>/dev/null; then
  if [ "$KIT_WT" = 1 ] && ! "${G[@]}" diff --quiet "$SHA" -- deploy/ec2-pmj; then
    say "NOTE: the working tree's deploy/ec2-pmj differs from $SHORT's — shipping the working tree's (--kit-from-worktree)"
  fi
elif [ "$KIT_WT" = 0 ]; then
  die "$SHORT has no deploy/ec2-pmj kit (not committed yet?) — commit it, or pass --kit-from-worktree"
fi
TRANSPORT=archive
[ "$WITH_GIT" = 1 ] && TRANSPORT=git
if [ "$WITH_GIT" = 0 ]; then
  say "transport: git archive (paper only: boot.git_sha 'unknown', no PMJ gate line can compile in) — --with-git for a release that may arm"
else
  say "transport: a one-commit shallow repository (--with-git)"
fi

TMP="$(mktemp -d "${TMPDIR:-/tmp}/pmj-deploy.XXXXXX")" || die "mktemp failed"
trap 'rm -rf "$TMP"' EXIT
export COPYFILE_DISABLE=1
if [ "$WITH_GIT" = 1 ]; then
  run git init -q "$TMP/src" || die "git init failed"
  run git -C "$TMP/src" fetch -q --depth 1 "file://$REPO" "$SHA" || die "git fetch of $SHA failed"
  run git -C "$TMP/src" -c advice.detachedHead=false checkout -q --detach FETCH_HEAD || die "checkout failed"
  if [ "$DRY" = 0 ]; then
    [ "$(git -C "$TMP/src" rev-parse HEAD)" = "$SHA" ] || die "the shallow repository's HEAD is not $SHA"
  fi
  run tar -C "$TMP" -czf "$TMP/src.tar.gz" src || die "tar failed"
else
  run "${G[@]}" archive --format=tar.gz --prefix=src/ -o "$TMP/src.tar.gz" "$SHA" || die "git archive failed"
fi
KIT_ARGS=()
if [ "$KIT_WT" = 1 ]; then
  bad="$(find "$KIT" -name '.env*' ! -name 'env.template' -print | head -1)"
  [ -z "$bad" ] || die "refusing: $bad looks like an env file inside the kit"
  run tar -C "$REPO/deploy" -czf "$TMP/kit.tar.gz" ec2-pmj || die "kit tar failed"
  KIT_ARGS=(--kit "/tmp/mvdeploy-$SHORT/kit.tar.gz")
fi
if [ "$DRY" = 0 ]; then
  n_env="$(tar -tzf "$TMP/src.tar.gz" | grep -Ec '(^|/)\.env$' || true)"
  [ "${n_env:-0}" = 0 ] || die "the source tarball carries a .env — refusing"
  say "source tarball: $(du -h "$TMP/src.tar.gz" | awk '{ print $1 }')"
fi

# --- ship, build, install ---------------------------------------------------
run "${SSH[@]}" "rm -rf /tmp/mvdeploy-$SHORT && mkdir -m 700 /tmp/mvdeploy-$SHORT" || die "ssh $HOST failed"
FILES=("$TMP/src.tar.gz" "$KIT/install-release.sh")
[ "$KIT_WT" = 1 ] && FILES+=("$TMP/kit.tar.gz")
run scp -q "${FILES[@]}" "$HOST:/tmp/mvdeploy-$SHORT/" || die "scp failed"
say "building on $HOST (~15-30 min cold; the running engine keeps its CPUs: nice 19, idle IO, -j $JOBS)"
# ${KIT_ARGS[@]+...}: bash 3.2 (macOS) treats an empty array as unset under -u.
run "${SSH[@]}" "sudo bash /tmp/mvdeploy-$SHORT/install-release.sh deploy --sha $SHA --transport $TRANSPORT --src /tmp/mvdeploy-$SHORT/src.tar.gz --jobs $JOBS ${KIT_ARGS[*]+${KIT_ARGS[*]}}"
rc=$?
run "${SSH[@]}" "rm -rf /tmp/mvdeploy-$SHORT" || true
[ "$rc" = 0 ] || die "the install failed on $HOST (exit $rc) — current is unchanged unless it said otherwise"
say "installed $SHORT ($TRANSPORT) on $HOST. Next: the gate battery on the host (README step 4b); the engine takes the release at its next drain (README \"Taking a release\")"
