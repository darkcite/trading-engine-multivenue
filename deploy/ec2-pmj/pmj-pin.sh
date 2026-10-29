#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# pmj-pin.sh — multivenue-pmj.service's ExecStartPost: once the engine's loop
# is running, move the engine's MAIN thread (the busy-polling loop, TID = PID)
# alone onto the isolated engine CPU (/etc/multivenue-pmj/cores.env
# ENGINE_CPU). Non-fatal by design (the unit runs it with a `-` prefix): a
# failure leaves the loop on the unit's housekeeping CPUAffinity, which is
# slower, never wrong.
#
# Why from outside: the engine pins its ingress threads to fixed core ids
# (pm 1, bn 2, hl 7, pm5 13 — the last absent on an 8-vCPU host, so that pin
# fails and the thread keeps the inherited mask) and pins NOT its main loop
# (BOOT-AUDIT.md C6). And why not a CPUAffinity on the isolated CPUs for the
# whole unit: isolcpus disables load balancing there, so every unpinned
# thread — the loop, ingress-pm5 (slot 11's own feed), pm5-discovery,
# metrics-http, jup-gw — would stack on ONE CPU.
#
# The wait: /state is published by the engine loop alone (paper.rs, the 1 s
# snapshot), so `boot.pid == MAINPID` means every boot-time thread has been
# spawned and inherited the housekeeping mask; only then is the main thread
# moved (`taskset -p` without -a moves that one task).
set -u

CORES="${PMJ_CORES_ENV:-/etc/multivenue-pmj/cores.env}"
URL="${PMJ_STATE_URL:-http://127.0.0.1:9191/state}"
say() { echo "pmj-pin: $*" >&2; }

pid="${MAINPID:-}"
[[ "$pid" =~ ^[1-9][0-9]*$ ]] || { say "no MAINPID — nothing pinned"; exit 0; }
[ -r "$CORES" ] || { say "$CORES absent — the loop stays on the unit's CPUAffinity"; exit 0; }
eng="$(sed -n 's/^ENGINE_CPU=\([0-9]\{1,4\}\)$/\1/p' "$CORES")"
[ -n "$eng" ] || { say "$CORES names no ENGINE_CPU — nothing pinned"; exit 0; }
grep -qw "$eng" <(tr ',' '\n' </sys/devices/system/cpu/isolated 2>/dev/null) ||
  say "CPU $eng is not in /sys/devices/system/cpu/isolated ($(cat /sys/devices/system/cpu/isolated 2>/dev/null)) — pinning anyway; reboot after host-setup if the kernel arguments are new"

deadline=$((SECONDS + 240))
while :; do
  kill -0 "$pid" 2>/dev/null || { say "pid $pid exited before its loop ran"; exit 0; }
  seen="$(curl -fsS --max-time 2 "$URL" 2>/dev/null | jq -r '.boot.pid // empty' 2>/dev/null)"
  [ "$seen" = "$pid" ] && break
  [ "$SECONDS" -lt "$deadline" ] || { say "no /state from pid $pid within 240 s — not pinned"; exit 0; }
  sleep 2
done

if taskset -p -c "$eng" "$pid" >/dev/null 2>&1; then
  say "engine loop (pid $pid) pinned to CPU $eng"
else
  say "taskset -p -c $eng $pid failed — the loop stays on the housekeeping CPUs"
fi
for t in /proc/"$pid"/task/*; do
  tid="${t##*/}"
  printf 'pmj-pin:   tid %-8s %-16s cpus %s\n' "$tid" "$(cat "$t/comm" 2>/dev/null)" \
    "$(awk '/^Cpus_allowed_list/ { print $2 }' "$t/status" 2>/dev/null)" >&2
done
exit 0
