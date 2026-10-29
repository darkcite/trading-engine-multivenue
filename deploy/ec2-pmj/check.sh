#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# check.sh — the kit's static checks; no AWS, no ssh, no network:
#   bash -n on every script; shellcheck when installed; `systemd-analyze
#   verify` on the units (their /opt paths mapped onto this directory in a temp
#   copy) when installed; provision.sh and az-rtt-probe.sh --dry-run (no
#   credentials needed); the SPDX header on every script.
# Runs on the Mac (shellcheck) and on Linux (systemd-analyze). Exit 1 on any red.
set -u
KIT="$(cd "$(dirname "$0")" && pwd -P)"
cd "$KIT" || exit 1
red=0
ok() { echo "check: OK   $*"; }
bad() { echo "check: RED  $*"; red=1; }

for f in *.sh; do
  if bash -n "$f"; then ok "bash -n $f"; else bad "bash -n $f"; fi
  if sed -n 2p "$f" | grep -q '^# SPDX-License-Identifier: Apache-2.0$' &&
    sed -n 3p "$f" | grep -q '^# Copyright 2026 Anton (darkcite)$'; then
    ok "SPDX $f"
  else
    bad "SPDX $f"
  fi
  if [ -x "$f" ]; then ok "exec bit $f"; else bad "exec bit $f"; fi
done

if command -v shellcheck >/dev/null 2>&1; then
  if shellcheck -S style ./*.sh; then ok "shellcheck (style and up)"; else bad "shellcheck"; fi
else
  echo "check: SKIP shellcheck (not installed)"
fi

if command -v systemd-analyze >/dev/null 2>&1; then
  tmp="$(mktemp -d)"
  repo="$(cd "$KIT/../.." && pwd -P)"
  for u in systemd/*.service systemd/*.timer; do
    sed -e "s#/opt/multivenue/current#$repo#g" \
      -e "s#/home/mv/multivenue/logs/systemd#$tmp#g" \
      -e "s#ReadWritePaths=/home/mv/multivenue#ReadWritePaths=$tmp#" "$u" >"$tmp/${u##*/}"
  done
  out="$(cd "$tmp" && SYSTEMD_LOG_LEVEL=notice systemd-analyze verify ./*.service ./*.timer 2>&1)"
  # Only findings about these units count; the build host has no user `mv`;
  # RestartSteps= / RestartMaxDelaySec= need systemd >= 254 (Ubuntu 24.04: 255),
  # so an older verifier's "Unknown key" for those two alone is tolerated.
  sdv="$(systemd-analyze --version | awk 'NR == 1 { print $2 }')"
  old_keys=""
  [ "${sdv:-0}" -lt 254 ] && old_keys="Unknown key name 'Restart\(Steps\|MaxDelaySec\)'"
  rest="$(printf '%s\n' "$out" | grep -e 'multivenue-pmj' -e "$tmp" |
    grep -v -e "User=mv\|Group=mv\|user mv\|group mv\|Failed to resolve user\|Failed to resolve group" |
    { if [ -n "$old_keys" ]; then grep -v -e "$old_keys"; else cat; fi; } || true)"
  set -- systemd/*.service systemd/*.timer
  n_units=$#
  if [ -z "$rest" ]; then ok "systemd-analyze verify ($n_units units; systemd $sdv${old_keys:+, the two >= 254 keys tolerated})"; else bad "systemd-analyze verify:"; printf '%s\n' "$rest"; fi
  rm -rf "$tmp"
else
  echo "check: SKIP systemd-analyze (not installed)"
fi

if out="$(./provision.sh --az eu-west-1b --create-key pmj-check-dummy --my-ip 192.0.2.10 --dry-run 2>&1)"; then
  missing=""
  for want in run-instances HttpTokens=required HttpPutResponseHopLimit=1 disable-api-termination \
    Encrypted CidrIp=192.0.2.10/32 isolcpus=managed_irq allocate-address associate-address \
    ssh-keygen import-key-pair describe-security-group-rules NetworkInterfaces; do
    printf '%s\n' "$out" | grep -q -- "$want" || missing="$missing $want"
  done
  if [ -z "$missing" ]; then
    ok "provision.sh --dry-run (no credentials)"
  else
    bad "provision.sh --dry-run lacks:$missing"
  fi
else
  bad "provision.sh --dry-run exited non-zero"
  printf '%s\n' "$out"
fi
if out="$(./az-rtt-probe.sh --dry-run 2>&1)"; then
  if [ "$(printf '%s\n' "$out" | grep -c 'run-instances')" = 3 ] && printf '%s\n' "$out" | grep -q 'shutdown -h +15' &&
    printf '%s\n' "$out" | grep -q 'create-security-group' && printf '%s\n' "$out" | grep -q 'delete-security-group' &&
    printf '%s\n' "$out" | grep -q 'ws-live-data.polymarket.com\|base64'; then
    ok "az-rtt-probe.sh --dry-run"
  else
    bad "az-rtt-probe.sh --dry-run"
  fi
else
  bad "az-rtt-probe.sh --dry-run exited non-zero"
fi
exit "$red"
