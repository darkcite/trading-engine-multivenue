#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# pmj-kill.sh — halt slot 11 on the EC2 standalone, from the Mac, and PROVE it.
#
#   deploy/ec2-pmj/pmj-kill.sh <host>          # halt slot 11, confirm on /state
#   deploy/ec2-pmj/pmj-kill.sh <host> --stop   # halt, then stop the engine
#   deploy/ec2-pmj/pmj-kill.sh <host> --state  # only show slot 11's state
#
# <host> is the ssh alias (provision.sh prints the stanza; `pmj-ec2`). Over
# ssh, as the host's `mv` user, it appends `slot=11 reason=operator` to the
# exec.HALT beside the EXEC_TOML in force (strategy.conf's, else
# ~/multivenue/exec.toml) — the CLAUDE.md immediate halt, the line the daily
# lane's review trigger writes, read by the armed router about once a second
# — unless a line there already halts slot 11. Then it POLLS /state on the
# host's loopback for up to ~5 s:
#
#   exit 0  slot 11 is live (boot.paper 0 — on this engine only slot 11 can
#           be) and `exec.halted[11]` is no longer "none": HALTED, confirmed;
#           or slot 11 is not live (boot.paper 1): nothing can reach a venue
#           now, and the line stands for the next armed boot, which refuses to
#           arm slot 11 while it is there (pmj_live check_halt_file);
#   exit 1  live and NOT seen halted within ~5 s, /state not answering, or
#           the halt line could not be written — use --stop.
#
# --stop then runs `sudo systemctl stop multivenue-pmj.service` (as the ssh
# user): SIGTERM, the drain, 60 s before SIGKILL. A stopped unit stays
# stopped until `sudo systemctl start multivenue-pmj.service`, or a host
# REBOOT (the unit is enabled): `sudo systemctl disable --now
# multivenue-pmj.service` keeps it down across reboots.
#
# WHEN SSH ITSELF IS REFUSED (your IP changed: the security group admits one
# /32): from any shell with your AWS credentials,
#   aws ec2 stop-instances --region eu-west-1 --instance-ids <i>
# — an ACPI shutdown: systemd stops the unit, SIGTERM, the drain;
# termination protection does not block a STOP. Then re-admit SSH:
#   deploy/ec2-pmj/provision.sh --az <az> --key-name <key> --my-ip <new-ip>
# Starting the instance again boots the engine as strategy.conf says (armed,
# if ARM_LIVE names 11): run pmj-kill.sh (or pmj-flip.sh paper) right after
# `start-instances` — slot 11 acts only in a window's last 20 s.
#
# Clearing the halt is the operator's, after review: archive the line by hand,
# or scripts/pmj-flip.sh paper.
set -u -o pipefail

HOST="${1:-}"
MODE="${2:-halt}"
usage() { echo "usage: pmj-kill.sh <host> [--stop|--state]" >&2; exit 64; }
case "$HOST" in '' | -*) usage ;; esac
case "$MODE" in halt | --stop | --state) ;; *) usage ;; esac
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10 "$HOST")
FALLBACK="no ssh? aws ec2 stop-instances --region eu-west-1 --instance-ids <i>, then provision.sh --my-ip (this file's header)"

if [ "$MODE" != "--state" ]; then
  if ! "${SSH[@]}" 'sudo -u mv -H bash -s' <<'REMOTE'; then
set -u
MV="$HOME/multivenue"
conf="$MV/strategy.conf"
exec_toml=""
if [ -f "$conf" ]; then
  exec_toml="$(set +u; unset EXEC_TOML; . "$conf" >/dev/null 2>&1; printf '%s' "${EXEC_TOML:-}")"
fi
exec_toml="${exec_toml:-$MV/exec.toml}"
h="$(dirname -- "$exec_toml")/exec.HALT"
if [ -f "$h" ] && grep -Eq '^[[:space:]]*(slot=)?0*11([[:space:]]|$)' "$h"; then
  echo "pmj-kill: $h already halts slot 11 — left as it is"
else
  if [ -s "$h" ] && [ -n "$(tail -c 1 "$h")" ]; then
    printf '\n' >>"$h"
  fi
  printf 'slot=11 reason=operator\n' >>"$h" || { echo "pmj-kill: could not append to $h" >&2; exit 1; }
  echo "pmj-kill: appended 'slot=11 reason=operator' to $h ($(date -u +%Y-%m-%dT%H:%M:%SZ))"
fi
if [ "$(($(wc -c <"$h")))" -gt 640 ]; then
  echo "pmj-kill: *** $h is over the router's 640 B — it will NOT be read ***" >&2
  exit 1
fi
REMOTE
    echo "pmj-kill: the halt line was NOT confirmed written on $HOST — run: pmj-kill.sh $HOST --stop ($FALLBACK)" >&2
    [ "$MODE" = "--stop" ] || exit 1
  fi
fi

# Poll /state on the host for up to ~5 s (one ssh, the loop on the host).
# shellcheck disable=SC2016 # expands on the host, by design
verdict="$("${SSH[@]}" 'for i in 1 2 3 4 5 6; do
  s="$(curl -fsS --max-time 1 http://127.0.0.1:9191/state 2>/dev/null)"
  if [ -n "$s" ]; then
    paper="$(printf "%s" "$s" | jq -r ".boot.paper")"
    word="$(printf "%s" "$s" | jq -r ".exec.halted[11] // \"none\"")"
    line="$(printf "%s" "$s" | jq -c "{pid: .boot.pid, git_sha: .boot.git_sha, paper: .boot.paper, halt_file_present: .exec.halt_file_present, slot11_halted: .exec.halted[11], slot11_held: .exec.held[11], slot11_enabled: .slots[11].enabled}")"
    if [ "$paper" = 1 ]; then echo "NOTLIVE $line"; exit 0; fi
    if [ "$word" != "none" ]; then echo "HALTED $line"; exit 0; fi
    last="$line"
  fi
  sleep 1
done
echo "UNCONFIRMED ${last:-/state not answering}"' 2>/dev/null)" || verdict="UNCONFIRMED ssh to $HOST failed"
echo "pmj-kill: $verdict"
rc=1
case "$verdict" in
  HALTED*) echo "pmj-kill: slot 11 is LIVE and HALTED (exec.halted[11] above)"; rc=0 ;;
  NOTLIVE*) echo "pmj-kill: slot 11 is not live (paper engine): nothing reaches a venue; the halt line stands for the next armed boot"; rc=0 ;;
  *) echo "pmj-kill: *** slot 11 NOT confirmed halted within ~5 s — run: pmj-kill.sh $HOST --stop ($FALLBACK) ***" >&2 ;;
esac
[ "$MODE" = "--state" ] && exit "$rc"

if [ "$MODE" = "--stop" ]; then
  echo "pmj-kill: stopping multivenue-pmj.service (SIGTERM drain, 60 s before SIGKILL)"
  # shellcheck disable=SC2016 # expands on the host, by design
  if "${SSH[@]}" 'sudo systemctl stop multivenue-pmj.service; s="$(systemctl is-active multivenue-pmj.service)"; echo "pmj-kill: multivenue-pmj.service is $s"; [ "$s" != active ]'; then
    echo "pmj-kill: stopped. It stays stopped until: ssh $HOST sudo systemctl start multivenue-pmj.service (or a reboot: disable it to keep it down)"
    rc=0
  else
    echo "pmj-kill: *** the stop was not confirmed — $FALLBACK ***" >&2
    rc=1
  fi
fi
exit "$rc"
