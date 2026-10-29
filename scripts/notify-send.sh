#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# notify-send.sh — queue a Telegram message from an AI session (NOTIFY lane).
#
# POSIX sh, no dependencies: it runs on the Mac (Claude Code, the RustRover
# terminal, a worker job) and inside a Cowork session's Linux shell, which
# sees only this repo folder. It writes one `*.msg` file atomically into an
# inbox; the running notifier that owns the inbox (main's job) relays it and
# removes it. Only that notifier holds the bot token — this script never
# reads a .env and never talks to the network.
#
#   scripts/notify-send.sh [-l] [-s SOURCE] [-d DIR] TEXT...
#   echo "long text" | scripts/notify-send.sh -s aires
#
#   -l         ring (default: delivered silently)
#   -s SOURCE  shown as [AI:SOURCE] (letters, digits, - _ .)
#   -d DIR     the inbox (default: <this repo>/.notify-inbox, git-ignored)
#
# Messages are capped at about 3 000 bytes and 60 an hour (notify.toml
# inbox_max_per_hour) by the notifier. Exit 0 = queued.
set -eu

loud=0
source=""
dir=""
while getopts "ls:d:h" opt; do
  case "$opt" in
    l) loud=1 ;;
    s) source="$OPTARG" ;;
    d) dir="$OPTARG" ;;
    h) sed -n '4,22p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "usage: notify-send.sh [-l] [-s SOURCE] [-d DIR] TEXT..." >&2; exit 64 ;;
  esac
done
shift $((OPTIND - 1))

if [ -z "$dir" ]; then
  here=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
  dir="$here/../.notify-inbox"
fi
mkdir -p "$dir"
chmod 700 "$dir" 2>/dev/null || true

if [ "$#" -gt 0 ]; then
  body="$*"
else
  body=$(cat)
fi
if [ -z "$(printf '%s' "$body" | tr -d '[:space:]')" ]; then
  echo "notify-send.sh: empty message" >&2
  exit 64
fi

source=$(printf '%s' "$source" | tr -cd 'A-Za-z0-9._-' | cut -c1-24)
stamp="$(date -u +%Y%m%dT%H%M%S)-$$-$(od -An -N4 -tx4 /dev/urandom 2>/dev/null | tr -d ' \n' || echo 0)"
tmp="$dir/.$stamp.msg.tmp"
{
  if [ "$loud" = 1 ]; then echo "level=loud"; else echo "level=silent"; fi
  [ -n "$source" ] && echo "source=$source"
  echo
  printf '%s\n' "$body"
} > "$tmp"
mv "$tmp" "$dir/$stamp.msg"
echo "queued $dir/$stamp.msg"
