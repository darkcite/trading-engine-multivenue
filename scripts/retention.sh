#!/bin/zsh
# M3 retention (mvp-plan §4-M3 item 3): age/size-based ARCHIVAL of
# capture run dirs. Policy: KEEP-ALL until disk pressure — only when
# the log volume's free space drops under MIN_FREE_GIB does archival
# start, compressing the OLDEST run dirs (tar -cz — bsdtar's INTERNAL
# gzip: macOS `--zstd` shells out to an uninstalled external binary,
# observed live 2026-08-22; zero new deps is doctrine — then remove
# the original only after a verified archive) until free space
# reaches TARGET_FREE_GIB or only protected dirs remain.
#
# Never touched: the newest run dir (the engine's live capture) and
# anything younger than PROTECT_DAYS (the standing backtest window).
# Same-volume `mv` frees nothing, hence compression; an operator who
# prefers moving to another volume sets ARCHIVE_MODE=move with
# ARCHIVE_DIR on that volume. Archives are never deleted here —
# pruning ~/multivenue/archive is a deliberate operator act.
#
# Config: ~/multivenue/retention.conf (KEY=VALUE, sourced; see
# retention.conf.example) < CLI flags. Invoked once per UTC day by
# scripts/daily-restart.sh; safe to run by hand any time (idempotent,
# exit 0 = policy satisfied).
set -u

LOG_ROOT="${MULTIVENUE_LOG_DIR:-$HOME/multivenue/logs}"
CONF="$HOME/multivenue/retention.conf"
MIN_FREE_GIB=25
TARGET_FREE_GIB=40
PROTECT_DAYS=7
ARCHIVE_DIR="$HOME/multivenue/archive"
ARCHIVE_MODE="compress" # compress | move

while [ $# -gt 0 ]; do
  case "$1" in
    --root) LOG_ROOT="$2"; shift 2 ;;
    --conf) CONF="$2"; shift 2 ;;
    *) echo "retention: unknown arg $1" >&2; exit 64 ;;
  esac
done
[ -f "$CONF" ] && . "$CONF"

free_gib() { df -g "$LOG_ROOT" | awk 'NR==2 {print $4}'; }

[ -d "$LOG_ROOT" ] || { echo "retention: no log root $LOG_ROOT" >&2; exit 0; }
free="$(free_gib)"
if [ "$free" -ge "$MIN_FREE_GIB" ]; then
  exit 0 # keep-all: no pressure
fi
echo "retention: free ${free}GiB < ${MIN_FREE_GIB}GiB — archiving oldest run dirs" >&2
mkdir -p "$ARCHIVE_DIR"
now_s=$(date +%s)
# Equal-width ns epochs ⇒ name sort = age sort; drop the newest (live).
candidates=$(ls -d "$LOG_ROOT"/run-* 2>/dev/null | sort | sed '$d')
for d in ${(f)candidates}; do
  free="$(free_gib)"
  [ "$free" -ge "$TARGET_FREE_GIB" ] && break
  name="${d:t}"
  epoch_ns="${name#run-}"
  case "$epoch_ns" in (*[!0-9]*|'') continue ;; esac
  age_days=$(( (now_s - epoch_ns / 1000000000) / 86400 ))
  if [ "$age_days" -le "$PROTECT_DAYS" ]; then
    echo "retention: $name is ${age_days}d old (<= protect ${PROTECT_DAYS}d) — stopping" >&2
    break # older→newer order: everything after is younger
  fi
  if [ "$ARCHIVE_MODE" = "move" ]; then
    echo "retention: move $name -> $ARCHIVE_DIR/" >&2
    mv "$d" "$ARCHIVE_DIR/" || { echo "retention: move failed — stopping" >&2; break; }
  else
    out="$ARCHIVE_DIR/$name.tar.gz"
    echo "retention: compress $name -> $out" >&2
    if tar -czf "$out" -C "$LOG_ROOT" "$name" && [ -s "$out" ]; then
      rm -r "$d"
    else
      echo "retention: compression failed — original kept, stopping" >&2
      rm -f "$out"
      break
    fi
  fi
done
exit 0
