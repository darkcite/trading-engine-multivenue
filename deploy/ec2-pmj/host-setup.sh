#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# host-setup.sh — ONCE, ON THE EC2 HOST, AS ROOT, after provision.sh's first
# boot (which set UTC, chrony on AWS Time Sync and the engine core's kernel
# arguments, then rebooted once). The operator runs it:
#
#   scp -r deploy/ec2-pmj pmj-ec2:/tmp/ && ssh pmj-ec2 sudo bash /tmp/ec2-pmj/host-setup.sh
#
# Idempotent: re-running re-asserts every item. It never starts the engine and
# never touches a secret (.env, the wallet keypair and s3.env are the
# operator's, written on the host by hand — README).
#
#   1  packages: zsh (the repo's scripts are zsh), build-essential, pkg-config,
#      git, jq, chrony, python3-venv, curl (+ unattended-upgrades, needrestart,
#      logrotate, polkitd); kdump-tools must be ABSENT (purged: a crash dump
#      holds the signer's page); chrony's AWS Time Sync source re-asserted
#   2  the user `mv` (no password, no sudo, NO lingering: every unit here is
#      a system unit); ~mv 0700; ~mv/multivenue 0700 (+ logs/systemd, state,
#      run, .notify-inbox); ~mv/keys 0700 (the wallet keypair's home);
#      ~mv/src, ~mv/build; the unit logs pre-created mv:mv 0600
#   2b ONE polkit rule: `mv` may try-restart multivenue-pmj.service, nothing
#      else — not restart, start or stop (the daily drain; README step 7)
#   3  nofile 8192 for mv (and LimitNOFILE in the units)
#   4  CORE DUMPS OFF (risk-policy J16 lists them as an unchecked key-exposure
#      gap: a dump holds the signer's page): kernel.core_pattern=|/bin/false,
#      fs.suid_dumpable=0, kernel.yama.ptrace_scope=2 (no same-uid ptrace of
#      the engine), DefaultLimitCORE=0 for systemd, `* hard core 0`,
#      apport disabled (it rewrites core_pattern at boot), systemd-coredump
#      Storage=none if present; LimitCORE=0 in every unit
#   5  NO SWAP: the signer's seed sits in an mlock'd page, which never swaps
#      anyway; what is NOT locked — the environment block holding JUP_API_KEY,
#      the heap copies before they are wiped — must never reach a disk either.
#      Any active swap is turned off and its fstab line commented out.
#   6  SSH: keys only, no root, no passwords (sshd_config.d/01-multivenue.conf —
#      01, because sshd keeps the FIRST value of a keyword and cloud-init writes
#      50-cloud-init.conf); local forwarding stays on for the /state tunnel
#   7  unattended-upgrades: SECURITY pockets only, never an automatic reboot;
#      needrestart never restarts multivenue-* (the restart law: a restart is a
#      safe minute's, never a package's)
#   8  Rust: rustup-init 1.28.2 (its sha256 PINNED here) for mv with the
#      repo's pinned toolchain 1.88.0 (rust-toolchain.toml) — no nightly (no
#      fuzzing here); cargo-nextest 0.9.148 (--locked, from crates.io) for
#      host-gates.sh, non-fatal (host-gates falls back to cargo test)
#   9  uv 0.12.5 (its sha256 PINNED here) for the archiver's venv (Python 3.14)
#  10  the core map: /etc/multivenue-pmj/cores.env (first boot wrote it; else
#      computed now, with the kernel arguments, and a reboot is asked for);
#      the engine unit's CPUAffinity drop-in rendered from it
#  11  systemd units installed — the engine neither enabled nor started (README
#      step 6 does both once its files exist); the timers enabled only once a
#      release is installed — and logrotate for the unit logs
#  12  amazon-ssm-agent removed (no instance profile, no remote-command
#      channel on a trading host; --keep-ssm-agent keeps it)
set -euo pipefail

RUSTUP_VERSION="1.28.2"
# sha256 of static.rust-lang.org/rustup/archive/1.28.2/x86_64-unknown-linux-gnu/rustup-init
RUSTUP_SHA256="20a06e644b0d9bd2fbdbfd52d42540bdde820ea7df86e92e533c073da0cdd43c"
RUST_TOOLCHAIN="1.88.0"
UV_VERSION="0.12.5"
# sha256 of github.com/astral-sh/uv/releases/download/0.12.5/uv-x86_64-unknown-linux-gnu.tar.gz
UV_SHA256="68a509da24b06b4223a1c0175fb5eb5bc79342b76cbeff0cfe51ac3f5b17b6b2"
NEXTEST_VERSION="0.9.148"
LOGS="engine.out.log engine.err.log daily.log archive.log alert.log"
MVUSER="mv"
KEEP_SSM=0

for a in "$@"; do
  case "$a" in
    --keep-ssm-agent) KEEP_SSM=1 ;;
    -h | --help) sed -n '5,60p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "host-setup: unknown argument $a" >&2; exit 64 ;;
  esac
done

KIT="$(cd "$(dirname "$(readlink -f "$0")")" && pwd -P)"
log() { echo "host-setup: $*" >&2; }
die() { echo "host-setup: $*" >&2; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
# shellcheck disable=SC1091 # the system's own file
. /etc/os-release
[ "${ID:-}" = ubuntu ] && [ "${VERSION_ID:-}" = "24.04" ] || die "Ubuntu 24.04 only (this is ${PRETTY_NAME:-unknown})"
[ "$(uname -m)" = x86_64 ] || die "amd64 only"
[ -d "$KIT/systemd" ] || die "$KIT/systemd is missing (copy the whole deploy/ec2-pmj directory)"
MVHOME="/home/$MVUSER"

# --- 1 packages -------------------------------------------------------------
log "1 packages"
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q -o Dpkg::Options::=--force-confold \
  zsh build-essential pkg-config git jq chrony python3-venv curl ca-certificates \
  unattended-upgrades needrestart logrotate util-linux procps iproute2 tar gzip polkitd
if dpkg-query -W -f='${Status}' kdump-tools 2>/dev/null | grep -q 'install ok installed'; then
  log "  kdump-tools is installed — purging it (a crash dump holds the signer's page)"
  apt-get purge -y -q kdump-tools
fi

# --- 2 the user and its directories -----------------------------------------
log "2 the user $MVUSER"
if ! id "$MVUSER" >/dev/null 2>&1; then
  useradd --create-home --shell /bin/bash "$MVUSER"
fi
passwd -l "$MVUSER" >/dev/null 2>&1 || true
chmod 0700 "$MVHOME"
# No lingering: every unit of this kit is a SYSTEM unit with User=mv.
if [ "$(loginctl show-user "$MVUSER" -p Linger --value 2>/dev/null)" = yes ]; then
  loginctl disable-linger "$MVUSER"
fi
for d in multivenue multivenue/logs multivenue/logs/systemd multivenue/state multivenue/run \
  multivenue/.notify-inbox keys src build; do
  install -d -o "$MVUSER" -g "$MVUSER" -m 0700 "$MVHOME/$d"
done
# systemd opens the units' append: logs itself; pre-created mv:mv 0600 so it
# never creates them root-owned (an existing log is kept, its owner and mode
# asserted).
for f in $LOGS; do
  if [ -e "$MVHOME/multivenue/logs/systemd/$f" ]; then
    chown "$MVUSER:$MVUSER" "$MVHOME/multivenue/logs/systemd/$f"
    chmod 0600 "$MVHOME/multivenue/logs/systemd/$f"
  else
    install -o "$MVUSER" -g "$MVUSER" -m 0600 /dev/null "$MVHOME/multivenue/logs/systemd/$f"
  fi
done

log "2b the polkit rule (mv: try-restart of multivenue-pmj.service only)"
install -d -m 0755 /etc/polkit-1/rules.d
cat >/etc/polkit-1/rules.d/50-multivenue-pmj.rules <<'EOF'
// multivenue pmj (host-setup.sh): the daily lane (pmj-daily.sh, user mv)
// drains the engine with `systemctl try-restart multivenue-pmj.service` — a
// manual job, so systemd's RestartSteps counter is reset and the daily dark
// window stays short. Nothing else: no restart (it would START a stopped
// unit), no start, no stop, no other unit.
polkit.addRule(function(action, subject) {
    if (action.id == "org.freedesktop.systemd1.manage-units" &&
        subject.user == "mv" &&
        action.lookup("unit") == "multivenue-pmj.service" &&
        action.lookup("verb") == "try-restart") {
        return polkit.Result.YES;
    }
});
EOF
chmod 0644 /etc/polkit-1/rules.d/50-multivenue-pmj.rules
systemctl try-restart polkit.service || true
install -d -o root -g root -m 0755 /opt/multivenue /etc/multivenue-pmj
install -d -o root -g root -m 0700 /var/cache/multivenue-uv

# --- 3 limits -----------------------------------------------------------------
log "3 limits"
cat >/etc/security/limits.d/90-multivenue.conf <<EOF
# multivenue pmj (host-setup.sh): the engine's sockets; no core dumps for anyone
$MVUSER soft nofile 8192
$MVUSER hard nofile 8192
*       soft core   0
*       hard core   0
EOF

# --- 4 core dumps off ---------------------------------------------------------
log "4 core dumps off"
cat >/etc/sysctl.d/90-multivenue-pmj.conf <<'EOF'
# multivenue pmj (host-setup.sh): a core dump would hold the signer's page
# (risk-policy "Signing-key handling", J16) — none is written, ever.
kernel.core_pattern = |/bin/false
fs.suid_dumpable = 0
# No ptrace but by CAP_SYS_PTRACE (root): `mv` cannot attach to its own engine
# and read the signer's page out of it.
kernel.yama.ptrace_scope = 2
EOF
if systemctl list-unit-files apport.service >/dev/null 2>&1; then
  systemctl disable --now apport.service >/dev/null 2>&1 || true
fi
[ -f /etc/default/apport ] && sed -i 's/^enabled=1/enabled=0/' /etc/default/apport
install -d /etc/systemd/system.conf.d /etc/systemd/coredump.conf.d
cat >/etc/systemd/system.conf.d/90-multivenue-pmj.conf <<'EOF'
[Manager]
DefaultLimitCORE=0
EOF
cat >/etc/systemd/coredump.conf.d/90-multivenue-pmj.conf <<'EOF'
[Coredump]
Storage=none
ProcessSizeMax=0
EOF
sysctl --system >/dev/null
systemctl daemon-reexec

# --- 5 no swap ----------------------------------------------------------------
log "5 no swap"
if [ -n "$(swapon --show --noheadings 2>/dev/null)" ]; then
  swapoff -a
  cp -p /etc/fstab "/etc/fstab.bak-multivenue-$(date -u +%Y%m%dT%H%M%SZ)"
  sed -i -E 's/^([^#].*[[:space:]]swap[[:space:]].*)$/# multivenue-pmj: swap off (host-setup.sh) # \1/' /etc/fstab
  log "swap turned off; fstab's swap line commented out (backup beside it)"
fi

# --- 6 ssh --------------------------------------------------------------------
log "6 ssh"
cat >/etc/ssh/sshd_config.d/01-multivenue.conf <<'EOF'
# multivenue pmj (host-setup.sh). 01-: sshd keeps the FIRST value it reads.
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PubkeyAuthentication yes
AuthenticationMethods publickey
PermitEmptyPasswords no
X11Forwarding no
AllowAgentForwarding no
# the /state tunnel (ssh -N -L 19191:127.0.0.1:9191) needs local forwarding
AllowTcpForwarding local
PermitTunnel no
MaxAuthTries 3
LoginGraceTime 30
EOF
sshd -t || die "sshd -t refused the configuration — fix /etc/ssh/sshd_config.d/01-multivenue.conf before logging out"
systemctl try-reload-or-restart ssh.service

# --- 7 unattended-upgrades, needrestart ---------------------------------------
log "7 unattended-upgrades (security only, no reboot), needrestart"
cat >/etc/apt/apt.conf.d/52multivenue-unattended <<'EOF'
// multivenue pmj (host-setup.sh): security pockets only, never a reboot.
#clear Unattended-Upgrade::Allowed-Origins;
#clear Unattended-Upgrade::Origins-Pattern;
Unattended-Upgrade::Allowed-Origins {
        "${distro_id}:${distro_codename}-security";
        "${distro_id}ESMApps:${distro_codename}-apps-security";
        "${distro_id}ESM:${distro_codename}-infra-security";
};
Unattended-Upgrade::Automatic-Reboot "false";
Unattended-Upgrade::Automatic-Reboot-WithUsers "false";
EOF
cat >/etc/apt/apt.conf.d/20auto-upgrades <<'EOF'
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF
install -d /etc/needrestart/conf.d
cat >/etc/needrestart/conf.d/90-multivenue.conf <<'EOF'
# multivenue pmj (host-setup.sh): a package never restarts the engine or its
# lanes — a restart is a safe minute's (the daily lane) or the operator's.
$nrconf{override_rc}{qr(^multivenue-)} = 0;
EOF

# --- 8 rust for mv ------------------------------------------------------------
log "8 rustup $RUSTUP_VERSION + toolchain $RUST_TOOLCHAIN for $MVUSER"
if [ ! -x "$MVHOME/.cargo/bin/rustup" ]; then
  tmp="$(mktemp -d)"
  base="https://static.rust-lang.org/rustup/archive/$RUSTUP_VERSION/x86_64-unknown-linux-gnu"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/rustup-init" "$base/rustup-init"
  have="$(sha256sum "$tmp/rustup-init" | awk '{ print $1 }')"
  [ "$have" = "$RUSTUP_SHA256" ] || die "rustup-init sha256 $have is not the pinned $RUSTUP_SHA256"
  chmod 0755 "$tmp/rustup-init"
  chown -R "$MVUSER:$MVUSER" "$tmp"
  sudo -u "$MVUSER" -H "$tmp/rustup-init" -y --no-modify-path --profile minimal \
    --default-toolchain "$RUST_TOOLCHAIN" -c rustfmt,clippy,rust-src -t aarch64-apple-darwin
  rm -rf "$tmp"
fi
# The toolchain file lists both targets and three components; install them so
# a build never downloads a toolchain mid-deploy.
sudo -u "$MVUSER" -H "$MVHOME/.cargo/bin/rustup" toolchain install "$RUST_TOOLCHAIN" --profile minimal \
  -c rustfmt,clippy,rust-src -t x86_64-unknown-linux-gnu,aarch64-apple-darwin >/dev/null
log "  $(sudo -u "$MVUSER" -H "$MVHOME/.cargo/bin/rustc" "+$RUST_TOOLCHAIN" --version)"
if ! sudo -u "$MVUSER" -H "$MVHOME/.cargo/bin/cargo" nextest --version 2>/dev/null | grep -q "^cargo-nextest $NEXTEST_VERSION\b"; then
  log "  cargo-nextest $NEXTEST_VERSION (cargo install --locked, nice 19; a few minutes)"
  sudo -u "$MVUSER" -H nice -n 19 "$MVHOME/.cargo/bin/cargo" "+$RUST_TOOLCHAIN" install --locked --quiet \
    cargo-nextest --version "$NEXTEST_VERSION" ||
    log "  cargo-nextest did not install (non-fatal: host-gates.sh falls back to cargo test)"
fi

# --- 9 uv -----------------------------------------------------------------------
log "9 uv $UV_VERSION"
if ! /usr/local/bin/uv --version 2>/dev/null | grep -q "^uv $UV_VERSION\b"; then
  tmp="$(mktemp -d)"
  tarball="uv-x86_64-unknown-linux-gnu.tar.gz"
  base="https://github.com/astral-sh/uv/releases/download/$UV_VERSION"
  curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/$tarball" "$base/$tarball"
  have="$(sha256sum "$tmp/$tarball" | awk '{ print $1 }')"
  [ "$have" = "$UV_SHA256" ] || die "uv sha256 $have is not the pinned $UV_SHA256"
  tar -xzf "$tmp/$tarball" -C "$tmp"
  install -m 0755 "$tmp/uv-x86_64-unknown-linux-gnu/uv" "$tmp/uv-x86_64-unknown-linux-gnu/uvx" /usr/local/bin/
  rm -rf "$tmp"
fi
log "  $(/usr/local/bin/uv --version)"

# --- chrony on the AWS Time Sync Service (provision.sh's user-data wrote the
# source file; a host provisioned otherwise gets it here) ----------------------
AWS_TS='# AWS Time Sync Service (link-local; the IPv6 one answers on Nitro with IPv6).
server 169.254.169.123 prefer iburst minpoll 4 maxpoll 4
server fd00:ec2::123 iburst minpoll 4 maxpoll 4'
if grep -Eq '^[[:space:]]*sourcedir[[:space:]]+/etc/chrony/sources.d' /etc/chrony/chrony.conf; then
  [ -s /etc/chrony/sources.d/aws-time-sync.sources ] || printf '%s\n' "$AWS_TS" >/etc/chrony/sources.d/aws-time-sync.sources
else
  install -d /etc/chrony/conf.d
  printf '%s\n' "$AWS_TS" >/etc/chrony/conf.d/aws-time-sync.conf
  grep -Eq '^[[:space:]]*confdir[[:space:]]+/etc/chrony/conf.d' /etc/chrony/chrony.conf ||
    echo 'confdir /etc/chrony/conf.d' >>/etc/chrony/chrony.conf
fi
systemctl restart chrony.service

# --- 10 the core map ------------------------------------------------------------
log "10 the core map"
CORES=/etc/multivenue-pmj/cores.env
REBOOT=0
if [ ! -s "$CORES" ]; then
  log "  $CORES absent (not provisioned by provision.sh?) — computing it and the kernel arguments now"
  expand() {
    local IFS=, p
    for p in $1; do
      case "$p" in *-*) seq "${p%-*}" "${p#*-}" ;; *) echo "$p" ;; esac
    done
  }
  n="$(nproc --all)"
  best=-1
  bestcpu=""
  for d in /sys/devices/system/cpu/cpu[0-9]*; do
    [ -r "$d/topology/core_id" ] || continue
    [ "$(cat "$d/topology/physical_package_id")" = 0 ] || continue
    c="$(cat "$d/topology/core_id")"
    if [ "$c" -gt "$best" ]; then best="$c"; bestcpu="${d##*cpu}"; fi
  done
  if [ "$n" -lt 4 ] || [ -z "$bestcpu" ]; then
    printf 'ENGINE_CPU=\nISOLATED_CPUS=\nHOUSEKEEPING_CPUS=0-%d\n' "$((n - 1))" >"$CORES"
  else
    sib="$(cat "/sys/devices/system/cpu/cpu$bestcpu/topology/thread_siblings_list")"
    iso="$(expand "$sib" | sort -n | paste -sd, -)"
    eng="$(expand "$sib" | sort -n | head -1)"
    hk="$(seq 0 $((n - 1)) | grep -vxF -f <(expand "$sib") | paste -sd, -)"
    printf 'ENGINE_CPU=%s\nISOLATED_CPUS=%s\nHOUSEKEEPING_CPUS=%s\n' "$eng" "$iso" "$hk" >"$CORES"
    cat >/etc/default/grub.d/90-multivenue-pmj.cfg <<GRUB
# multivenue pmj: the engine core (host-setup.sh)
GRUB_CMDLINE_LINUX="\${GRUB_CMDLINE_LINUX} isolcpus=managed_irq,domain,$iso nohz_full=$iso rcu_nocbs=$iso irqaffinity=$hk"
GRUB
    update-grub
    REBOOT=1
  fi
fi
# shellcheck disable=SC1090 # written above or by the first boot
. "$CORES"
log "  engine CPU ${ENGINE_CPU:-none}, isolated ${ISOLATED_CPUS:-none}, housekeeping ${HOUSEKEEPING_CPUS:-all}"
log "  kernel now: isolated=$(cat /sys/devices/system/cpu/isolated) nohz_full=$(cat /sys/devices/system/cpu/nohz_full 2>/dev/null)"
if [ -n "${ISOLATED_CPUS:-}" ] && [ "$(cat /sys/devices/system/cpu/isolated)" != "$ISOLATED_CPUS" ]; then
  # /sys prints ranges ("3,7" stays "3,7"; "6-7" for adjacent ones)
  log "  the running kernel does not isolate $ISOLATED_CPUS yet — REBOOT before starting the engine"
  REBOOT=1
fi
if [ -n "${ISOLATED_CPUS:-}" ] && [ -f /etc/default/irqbalance ]; then
  sed -i '/^IRQBALANCE_BANNED_CPULIST=/d' /etc/default/irqbalance
  echo "IRQBALANCE_BANNED_CPULIST=$ISOLATED_CPUS" >>/etc/default/irqbalance
  systemctl try-restart irqbalance.service || true
fi

# --- 11 units and log rotation ----------------------------------------------------
log "11 systemd units"
for u in "$KIT"/systemd/*.service "$KIT"/systemd/*.timer; do
  install -m 0644 "$u" "/etc/systemd/system/${u##*/}"
done
install -d /etc/systemd/system/multivenue-pmj.service.d
if [ -n "${HOUSEKEEPING_CPUS:-}" ]; then
  cat >/etc/systemd/system/multivenue-pmj.service.d/10-cpus.conf <<EOF
# rendered by host-setup.sh / install-release.sh from $CORES — the engine's
# threads load-balance on the housekeeping CPUs; pmj-pin.sh then moves the
# engine loop alone onto CPU ${ENGINE_CPU:-none} (README "Core map").
[Service]
CPUAffinity=
CPUAffinity=${HOUSEKEEPING_CPUS//,/ }
EOF
fi
systemctl daemon-reload
if [ -e /opt/multivenue/current ]; then
  systemctl enable --now multivenue-pmj-daily.timer multivenue-pmj-archive.timer >/dev/null
else
  log "  no release installed yet: the timers stay disabled (README step 7 enables them)"
fi
cat >/etc/logrotate.d/multivenue-pmj <<EOF
# copytruncate: systemd holds each log open (append:), so the file is copied
# and truncated in place — it keeps its owner mv and mode 0600; the copies are
# written as mv (su) with the original's mode.
$MVHOME/multivenue/logs/systemd/*.log {
  su $MVUSER $MVUSER
  daily
  rotate 14
  maxsize 512M
  compress
  delaycompress
  missingok
  notifempty
  copytruncate
  nomail
}
EOF

# --- 12 the SSM agent ---------------------------------------------------------------
if [ "$KEEP_SSM" = 0 ] && command -v snap >/dev/null 2>&1 && snap list amazon-ssm-agent >/dev/null 2>&1; then
  log "12 removing the amazon-ssm-agent snap (--keep-ssm-agent keeps it)"
  snap remove --purge amazon-ssm-agent >/dev/null || log "  snap remove failed (non-fatal)"
fi

# --- the checks ---------------------------------------------------------------------
log "checks:"
log "  timezone $(timedatectl show -p Timezone --value)"
log "  chrony: $(chronyc -n sources 2>/dev/null | awk '/^\^\*/ { print "selected " $2 }')"
log "  core_pattern '$(cat /proc/sys/kernel/core_pattern)', suid_dumpable $(cat /proc/sys/fs/suid_dumpable), ptrace_scope $(cat /proc/sys/kernel/yama/ptrace_scope), kdump-tools $(dpkg-query -W -f='${Status}' kdump-tools 2>/dev/null | grep -q 'install ok installed' && echo PRESENT || echo absent)"
log "  polkit: $(systemctl is-active polkit.service 2>/dev/null) (the daily drain's try-restart)"
log "  swap: $(swapon --show --noheadings | wc -l) active"
log "  sshd: $(sshd -T 2>/dev/null | grep -E '^(passwordauthentication|permitrootlogin) ' | paste -sd' ' -)"
log "  units: engine $(systemctl is-enabled multivenue-pmj.service 2>/dev/null) / daily $(systemctl is-enabled multivenue-pmj-daily.timer 2>/dev/null) / archive $(systemctl is-enabled multivenue-pmj-archive.timer 2>/dev/null)"
if [ "$REBOOT" = 1 ]; then
  log "*** REBOOT REQUIRED (kernel arguments) before the engine starts: sudo systemctl reboot ***"
fi
log "done. Next: deploy-linux.sh from the Mac (README step 4)."
