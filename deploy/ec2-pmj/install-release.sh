#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# install-release.sh — the HOST half of deploy-linux.sh (which copies this
# file and the source tarball to the host and runs it as root):
#
#   sudo install-release.sh deploy --sha SHA --transport git|archive --src TAR [--kit TAR] [--jobs N]
#   sudo install-release.sh rollback [NAME]   # current <- previous (or NAME)
#   sudo install-release.sh status
#   sudo install-release.sh prune [--target]  # old releases; --target: the build caches too
#
# A RELEASE is /opt/multivenue/<name>, name = <sha> (git archive) or
# <sha>-git (a one-commit shallow repository): one directory per transport, so
# a sha deployed both ways never shares (or overwrites) a release.
#
# deploy: the tarball is extracted as `mv` into ~mv/src/<name> and BUILT there
# as `mv` — `cargo build --release --locked -p cli` with the pinned toolchain,
# CARGO_TARGET_DIR ~mv/build/target, RUSTFLAGS="-C target-cpu=native" (it
# REPLACES the repo's .cargo/config.toml x86_64 flags, `znver3`: an AMD target,
# and this host is Intel — BOOT-AUDIT C7), nice 19 / idle IO, inside a
# `systemd-run --scope` with MemoryMax (the build can never starve the engine
# of memory; the engine also has OOMScoreAdjust -900). The flags rustc ACTUALLY
# ran with are read back from the build's verbose log into BUILD_INFO. The
# binary is checked (`strategy-mask pmj` = 2048<TAB>pmj, the offline
# `pmj-exec --self-test`), the tree plus the binary installed root-owned and
# immutable, the archiver's venv built in it (uv, Python 3.14, wheels only:
# --no-build; `scripts/archive-run.py` is the archiver, module
# claude_worker.archive — the `multivenue-archive` console script's), `current`
# moved to it and `previous` kept. Units refreshed, systemd reloaded. THE
# ENGINE IS NEVER RESTARTED here: it boots the release at its next drain.
#
# REFUSED while slot 11 is armed (strategy.conf's ARM_LIVE names 11, or an
# exec*.toml marks [exec.slot.11] live): making an ARCHIVE-transport release
# current, by deploy or rollback (no sha, no PMJ gate line in it).
#
# .env: ONE file, /home/mv/multivenue/.env (the operator's; mv, 0600), named
# to every reader by MULTIVENUE_ENV_FILE (the engine and daily units set it;
# scripts/pmj-exec.sh, pmj-flip.sh and pmj-dust.sh read it before their repo
# .env). A release carries NO .env of any kind: a tarball or kit holding one
# is refused, and every deploy and rollback deletes any .env name it finds in
# any release tree (an earlier kit's symlink or hard link).
#
# host-gates.sh records the Linux gate battery's verdicts in BUILD_INFO.
set -euo pipefail

MVUSER="mv"
MVHOME=/home/mv
ROOT=/opt/multivenue
TARGET_DIR="$MVHOME/build/target"
UV=/usr/local/bin/uv
BUILD_MEMORY_MAX="${PMJ_BUILD_MEMORY_MAX:-8G}"
LOGS="engine.out.log engine.err.log daily.log archive.log alert.log"

log() { echo "install-release: $*" >&2; }
die() { echo "install-release: $*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "run as root"
id "$MVUSER" >/dev/null 2>&1 || die "no user $MVUSER — run host-setup.sh first"
[ -d "$ROOT" ] || die "$ROOT is missing — run host-setup.sh first"

as_mv() { sudo -u "$MVUSER" -H "$@"; }

lock() {
  exec 9>"$MVHOME/build/.deploy.lock"
  flock -n 9 || die "another deploy, rollback or prune holds $MVHOME/build/.deploy.lock"
}

# 0 = slot 11 is (or may be) armed on this host: strategy.conf's ARM_LIVE
# names 11 (sourced AS mv, never as root), or an exec*.toml marks
# [exec.slot.11] live (unreadable = armed; enabled = 0 = inert).
armed_11() {
  local spec part f
  # shellcheck disable=SC2016 # expanded by mv's bash, never by root's
  spec="$(as_mv bash -c 'f="$HOME/multivenue/strategy.conf"; [ -f "$f" ] || exit 0; set +u; unset ARM_LIVE; . "$f" >/dev/null 2>&1; printf "%s" "${ARM_LIVE:-}"' 2>/dev/null || true)"
  IFS=, read -r -a parts <<<"$spec"
  for part in "${parts[@]}"; do
    part="${part//[[:space:]]/}"
    part="${part#+}"
    [[ "$part" =~ ^[0-9]+$ ]] && [ "$((10#$part))" = 11 ] && return 0
  done
  for f in "$MVHOME"/multivenue/exec*.toml; do
    [ -e "$f" ] || continue
    [ -r "$f" ] || return 0
    grep -Eq '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*0' "$f" && continue
    awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) } s && /^[ \t]*mode[ \t]*=[ \t]*"live"/ { found = 1 } END { exit !found }' "$f" && return 0
  done
  return 1
}

refuse_archive_if_armed() {
  local rel="$1" t
  t="$(sed -n 's/^transport=//p' "$rel/BUILD_INFO" 2>/dev/null)"
  if [ "$t" != git ] && armed_11; then
    die "slot 11 is ARMED on this host and $rel is transport='${t:-unknown}' (no sha, no PMJ gate line) — refusing to make it current. Flip to paper first (pmj-flip.sh paper), or deploy --with-git."
  fi
}

running_release() {
  local pid exe
  pid="$(systemctl show -p MainPID --value multivenue-pmj.service 2>/dev/null || echo 0)"
  [[ "${pid:-0}" =~ ^[1-9][0-9]*$ ]] || return 0
  exe="$(readlink "/proc/$pid/exe" 2>/dev/null || true)"
  exe="${exe% (deleted)}"
  case "$exe" in "$ROOT"/*/target/release/multivenue-engine) printf '%s\n' "${exe%/target/release/multivenue-engine}" ;; esac
}

logs_precreate() {
  local f
  install -d -o "$MVUSER" -g "$MVUSER" -m 0700 "$MVHOME/multivenue/logs/systemd"
  for f in $LOGS; do
    if [ -e "$MVHOME/multivenue/logs/systemd/$f" ]; then
      chown "$MVUSER:$MVUSER" "$MVHOME/multivenue/logs/systemd/$f"
      chmod 0600 "$MVHOME/multivenue/logs/systemd/$f"
    else
      install -o "$MVUSER" -g "$MVUSER" -m 0600 /dev/null "$MVHOME/multivenue/logs/systemd/$f"
    fi
  done
}

units_refresh() {
  local rel="$1" u changed=0 CORES=/etc/multivenue-pmj/cores.env hk
  logs_precreate
  for u in "$rel"/deploy/ec2-pmj/systemd/*.service "$rel"/deploy/ec2-pmj/systemd/*.timer; do
    if ! cmp -s "$u" "/etc/systemd/system/${u##*/}"; then
      install -m 0644 "$u" "/etc/systemd/system/${u##*/}"
      log "unit ${u##*/} updated"
      changed=1
    fi
  done
  if [ -s "$CORES" ]; then
    hk="$(sed -n 's/^HOUSEKEEPING_CPUS=\([0-9,-]*\)$/\1/p' "$CORES")"
    if [ -n "$hk" ]; then
      install -d /etc/systemd/system/multivenue-pmj.service.d
      printf '%s\n' "# rendered by host-setup.sh / install-release.sh from $CORES" \
        "[Service]" "CPUAffinity=" "CPUAffinity=${hk//,/ }" >/etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf.new
      cmp -s /etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf.new \
        /etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf || changed=1
      mv -f /etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf.new \
        /etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf
    fi
  fi
  systemctl daemon-reload
  [ "$changed" = 0 ] || log "systemd reloaded — unit changes apply at each unit's next start"
}

# No release tree carries a .env (MULTIVENUE_ENV_FILE names the one file):
# delete any name an earlier kit left (a symlink, a hard link) — the name
# only; the operator's file is untouched.
env_names_purge() {
  local d
  for d in "$ROOT"/*/; do
    d="${d%/}"
    [ -L "$d" ] && continue # current / previous: links, not releases
    [ -d "$d" ] || continue
    if [ -e "$d/.env" ] || [ -L "$d/.env" ]; then
      rm -f "$d/.env"
      log "removed a .env name from ${d##*/} (releases carry none: MULTIVENUE_ENV_FILE names /home/mv/multivenue/.env)"
    fi
  done
}

switch_to() {
  local dest="$1" cur
  cur="$(readlink "$ROOT/current" 2>/dev/null || true)"
  if [ "$cur" = "$dest" ]; then
    log "current already is $dest"
    return 0
  fi
  if [ -n "$cur" ]; then
    ln -sfn "$cur" "$ROOT/previous.new" && mv -Tf "$ROOT/previous.new" "$ROOT/previous"
  fi
  ln -sfn "$dest" "$ROOT/current.new" && mv -Tf "$ROOT/current.new" "$ROOT/current"
  log "current -> $dest (previous -> ${cur:-none})"
}

running_report() {
  local run
  run="$(running_release)"
  if [ -n "$run" ]; then
    log "the engine runs $run — untouched; it boots $(readlink "$ROOT/current") at its next drain"
  else
    log "the engine is not running (multivenue-pmj.service $(systemctl is-active multivenue-pmj.service 2>/dev/null || true))"
  fi
}

cmd="${1:-}"
shift || true
case "$cmd" in
  status)
    for d in "$ROOT"/*/; do
      d="${d%/}"
      [ -L "$d" ] && continue # current / previous: links, not releases
      [ -f "$d/BUILD_INFO" ] || continue
      printf '%s  %s\n' "${d##*/}" "$(grep -E '^(transport|built_utc|kit_source|rustflags_observed|gates_verdict)=' "$d/BUILD_INFO" | paste -sd' ' -)"
    done
    log "current  -> $(readlink "$ROOT/current" 2>/dev/null || echo none)"
    log "previous -> $(readlink "$ROOT/previous" 2>/dev/null || echo none)"
    running_report
    curl -fsS --max-time 3 http://127.0.0.1:9191/state 2>/dev/null |
      jq -c '{pid: .boot.pid, git_sha: .boot.git_sha, paper: .boot.paper, strategy: .boot.strategy, uptime_s: .uptime_s, slot11_enabled: .slots[11].enabled, slot11_halt: .exec.halted[11]}' ||
      log "/state is not answering"
    ;;
  rollback)
    lock
    want="${1:-}"
    if [ -n "$want" ]; then
      [[ "$want" =~ ^[0-9a-f]{40}(-git)?$ ]] || die "rollback takes a release name: a full 40-hex sha, optionally -git"
      target="$ROOT/$want"
    else
      target="$(readlink "$ROOT/previous" 2>/dev/null || true)"
      [ -n "$target" ] || die "no previous release to roll back to"
    fi
    [ -f "$target/BUILD_INFO" ] || die "$target is not an installed release"
    refuse_archive_if_armed "$target"
    switch_to "$target"
    env_names_purge
    units_refresh "$target"
    running_report
    ;;
  prune)
    lock
    keep="$(readlink "$ROOT/current" 2>/dev/null || true) $(readlink "$ROOT/previous" 2>/dev/null || true) $(running_release)"
    for d in "$ROOT"/*/; do
      d="${d%/}"
      [ -L "$d" ] && continue # current / previous: links, not releases
      [ -f "$d/BUILD_INFO" ] || continue
      case " $keep " in *" $d "*) log "keeping ${d##*/} (current, previous or running)"; continue ;; esac
      log "pruning ${d##*/} (/opt and ~mv/src)"
      rm -rf "$d" "$MVHOME/src/${d##*/}"
    done
    "$UV" cache prune --cache-dir /var/cache/multivenue-uv >/dev/null 2>&1 || true
    if [ "${1:-}" = "--target" ]; then
      log "removing the build caches (~mv/build/target, ~mv/build/gates-target): the next build and gates run cold"
      rm -rf "$TARGET_DIR" "$MVHOME/build/gates-target"
    fi
    df -h "$ROOT" "$MVHOME" | sed 's/^/install-release: /' >&2
    ;;
  deploy)
    SHA=""
    TRANSPORT=""
    SRC_TAR=""
    KIT_TAR=""
    JOBS=4
    while [ $# -gt 0 ]; do
      case "$1" in
        --sha) SHA="${2:-}"; shift 2 ;;
        --transport) TRANSPORT="${2:-}"; shift 2 ;;
        --src) SRC_TAR="${2:-}"; shift 2 ;;
        --kit) KIT_TAR="${2:-}"; shift 2 ;;
        --jobs) JOBS="${2:-}"; shift 2 ;;
        *) die "deploy: unknown argument $1" ;;
      esac
    done
    [[ "$SHA" =~ ^[0-9a-f]{40}$ ]] || die "deploy needs --sha <40 hex>"
    case "$TRANSPORT" in git | archive) ;; *) die "deploy needs --transport git|archive" ;; esac
    [ -f "$SRC_TAR" ] || die "no source tarball $SRC_TAR"
    [ -z "$KIT_TAR" ] || [ -f "$KIT_TAR" ] || die "no kit tarball $KIT_TAR"
    [[ "$JOBS" =~ ^[1-9][0-9]?$ ]] || die "--jobs must be 1..99"
    [ -x "$MVHOME/.cargo/bin/cargo" ] || die "no cargo for $MVUSER — run host-setup.sh"
    [ -x "$UV" ] || die "no $UV — run host-setup.sh"
    NAME="$SHA"
    [ "$TRANSPORT" = git ] && NAME="$SHA-git"
    SRC="$MVHOME/src/$NAME"
    DEST="$ROOT/$NAME"
    lock

    if [ -f "$DEST/BUILD_INFO" ]; then
      [ "$(sed -n 's/^transport=//p' "$DEST/BUILD_INFO")" = "$TRANSPORT" ] ||
        die "$DEST exists with another transport in its BUILD_INFO — refusing"
      log "$DEST is already installed — switching only"
    else
      # --- the source, as mv ---
      n_env="$(tar -tzf "$SRC_TAR" | grep -Ec '(^|/)\.env$' || true)"
      [ "${n_env:-0}" = 0 ] || die "the source tarball carries a .env — refusing (deploy never copies one)"
      # The upload directory is the ssh user's (0700); mv reads private copies.
      install -o "$MVUSER" -g "$MVUSER" -m 0600 "$SRC_TAR" "$MVHOME/build/src-$NAME.tar.gz"
      rm -rf "$SRC.partial"
      install -d -o "$MVUSER" -g "$MVUSER" -m 0700 "$SRC.partial"
      as_mv tar -xzf "$MVHOME/build/src-$NAME.tar.gz" -C "$SRC.partial" --strip-components=1 \
        --no-same-owner --warning=no-unknown-keyword
      rm -f "$MVHOME/build/src-$NAME.tar.gz"
      if [ -n "$KIT_TAR" ]; then
        n_env="$(tar -tzf "$KIT_TAR" | grep -Ec '(^|/)\.env$' || true)"
        [ "${n_env:-0}" = 0 ] || die "the kit tarball carries a .env — refusing (deploy never copies one)"
        install -o "$MVUSER" -g "$MVUSER" -m 0600 "$KIT_TAR" "$MVHOME/build/kit-$NAME.tar.gz"
        rm -rf "$SRC.partial/deploy/ec2-pmj"
        as_mv mkdir -p "$SRC.partial/deploy"
        as_mv tar -xzf "$MVHOME/build/kit-$NAME.tar.gz" -C "$SRC.partial/deploy" --no-same-owner --warning=no-unknown-keyword
        rm -f "$MVHOME/build/kit-$NAME.tar.gz"
        log "the kit comes from the Mac's working tree (--kit-from-worktree)"
      fi
      [ -f "$SRC.partial/deploy/ec2-pmj/pmj-wrapper.sh" ] || die "the release carries no deploy/ec2-pmj kit"
      if [ "$TRANSPORT" = git ]; then
        [ -d "$SRC.partial/.git" ] || die "--transport git but the tarball carries no .git"
        head="$(as_mv git -C "$SRC.partial" rev-parse HEAD)"
        [ "$head" = "$SHA" ] || die "the shipped .git's HEAD is $head, not $SHA"
      else
        [ ! -e "$SRC.partial/.git" ] || die "--transport archive but the tarball carries a .git"
      fi
      rm -rf "$SRC"
      mv "$SRC.partial" "$SRC"

      # --- the build, as mv, gently, in a memory-bounded scope ---
      BUILD_LOG="$MVHOME/build/build-$NAME.log"
      log "building $NAME (cargo build --release --locked -p cli -v, -j $JOBS, nice 19, idle IO, MemoryMax $BUILD_MEMORY_MAX; ~15-30 min cold) — log: $BUILD_LOG"
      # shellcheck disable=SC2016 # $1/$2/$3 expand in the inner bash, by design
      if ! systemd-run --quiet --scope --collect -p "MemoryMax=$BUILD_MEMORY_MAX" -p MemorySwapMax=0 \
        sudo -u "$MVUSER" -H env -i HOME="$MVHOME" USER="$MVUSER" PATH="$MVHOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin" \
        CARGO_TARGET_DIR="$TARGET_DIR" RUSTFLAGS="-C target-cpu=native" \
        bash -c 'cd "$1" && nice -n 19 ionice -c 3 cargo build --release --locked -p cli -v -j "$2" >"$3" 2>&1' _ "$SRC" "$JOBS" "$BUILD_LOG"; then
        tail -n 30 "$BUILD_LOG" >&2 || true
        die "the build failed (log: $BUILD_LOG)"
      fi
      BIN="$TARGET_DIR/release/multivenue-engine"
      [ -x "$BIN" ] || die "the build left no $BIN"
      # The flags rustc ran with, read back from the verbose log (the bin crate).
      run_line="$(grep -E 'Running `.*rustc --crate-name multivenue_engine ' "$BUILD_LOG" | tail -n 1 || true)"
      observed="$(printf '%s' "$run_line" | grep -oE -- '-C target-cpu=[^ `]+' | sort -u | paste -sd' ' - || true)"
      [ -n "$observed" ] || observed="not observed (the bin was not recompiled?)"
      case "$observed" in *znver*) die "rustc ran with '$observed' — an AMD target on this Intel host; refusing" ;; esac
      [ "$(as_mv "$BIN" strategy-mask pmj)" = $'2048\tpmj' ] || die "strategy-mask pmj did not answer 2048<TAB>pmj"
      as_mv "$BIN" pmj-exec --self-test >/dev/null || die "pmj-exec --self-test failed on the new binary"
      log "built and checked: rustc flags '$observed', strategy-mask pmj = 2048, pmj-exec --self-test passed"

      # --- the release, root-owned and immutable ---
      rm -rf "$DEST.partial" "$DEST"
      install -d -m 0755 "$DEST.partial"
      cp -a "$SRC/." "$DEST.partial/"
      install -d -m 0755 "$DEST.partial/target/release"
      install -m 0755 "$BIN" "$DEST.partial/target/release/multivenue-engine"
      chown -R root:root "$DEST.partial"
      chmod -R go-w,o+rX "$DEST.partial"
      ln -sfn "$MVHOME/multivenue/.notify-inbox" "$DEST.partial/.notify-inbox"
      mv "$DEST.partial" "$DEST"
      # The archiver's venv (uv; Python 3.14 per .python-version), dependencies
      # only and from wheels only (--no-build: no sdist runs code here); the
      # archiver itself runs from the release's claude-worker/src through
      # scripts/archive-run.py. Built at the FINAL path (a venv records it).
      if ! (cd "$DEST/claude-worker" &&
        UV_PYTHON_INSTALL_DIR="$ROOT/uv-python" UV_CACHE_DIR=/var/cache/multivenue-uv \
          UV_PYTHON_PREFERENCE=only-managed "$UV" sync --frozen --no-dev --no-build --no-install-project --quiet); then
        rm -rf "$DEST"
        die "uv sync failed — the release was removed; current is unchanged"
      fi
      chmod -R go-w,o+rX "$DEST/claude-worker/.venv"
      [ ! -d "$ROOT/uv-python" ] || chmod -R go-w,o+rX "$ROOT/uv-python"
      if ! as_mv "$DEST/claude-worker/.venv/bin/python3" "$DEST/scripts/archive-run.py" --help >/dev/null 2>&1; then
        rm -rf "$DEST"
        die "the archiver (scripts/archive-run.py) does not start in the new venv — the release was removed"
      fi
      {
        echo "sha=$SHA"
        echo "name=$NAME"
        echo "transport=$TRANSPORT"
        echo "kit_source=$([ -n "$KIT_TAR" ] && echo worktree || echo ref)"
        echo "built_utc=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "rustc=$(as_mv "$MVHOME/.cargo/bin/rustc" --version 2>/dev/null || true)"
        echo "rustflags=RUSTFLAGS=-C target-cpu=native"
        echo "rustflags_observed=$observed"
        echo "build_cpu=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)"
        echo "binary_sha256=$(sha256sum "$DEST/target/release/multivenue-engine" | awk '{ print $1 }')"
        echo "install_release_sha256=$(sha256sum "$0" | awk '{ print $1 }')"
        echo "archiver=scripts/archive-run.py (claude_worker.archive; venv deps only, wheels only)"
        echo "gates_verdict=NOT RUN (sudo bash $DEST/deploy/ec2-pmj/host-gates.sh $NAME)"
      } >"$DEST/BUILD_INFO"
      chmod 0644 "$DEST/BUILD_INFO"
      if [ "$TRANSPORT" = git ]; then
        as_mv git config --global --get-all safe.directory 2>/dev/null | grep -qxF "$DEST" ||
          as_mv git config --global --add safe.directory "$DEST"
        [ "$(as_mv git -C "$DEST" rev-parse HEAD)" = "$SHA" ] || die "git in $DEST does not answer $SHA"
      else
        log "transport=archive: no .git in the release — /state boot.git_sha reads 'unknown', the binary carries NO PMJ gate line (build.rs needs git), pmj-flip.sh live refuses and the wrapper refuses to arm. Paper only."
      fi
      log "installed $DEST — next: sudo bash $DEST/deploy/ec2-pmj/host-gates.sh $NAME"
    fi
    refuse_archive_if_armed "$DEST"
    switch_to "$DEST"
    env_names_purge
    units_refresh "$DEST"
    running_report
    ;;
  *)
    sed -n '8,11p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 64
    ;;
esac
