#!/bin/zsh
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
# M3 restart lane, T2 slot generalization (capture-remediation plan
# 2026-08-28; outage doc 2026-08-27 §7 tier 2). Runs every 60 s from
# com.multivenue.daily-restart.
#
# UTC SLOTS (fire once per UTC day, on the first minute-tick at/after
# the slot time):
#   0010  day boundary — SIGTERM drain; wrapper reboot refreshes the
#         PM universe; retention pass (0010 only).
#         0010, NOT 0000 (operator ruling 2026-09-10): the VRP
#         member's decision instant is E−τ = 00:00Z for the 08:00Z
#         Deribit daily, so a drain on that same second costs the
#         entry a 10–40 s dark window and re-decides a HOLD.
#         (`entry_done` has been PERSISTED since W6 and is written
#         the instant the decision is spent since F20, so a hold no
#         longer leaves nothing to restore — the slot still moves,
#         because a dark window over E−τ is its own cost.)
#         The campaign itself
#         SURVIVES the collision — selection is persisted and the
#         contract re-resolved by (expiry, strike, right) — but
#         nothing in this slot is time-critical to the second, so
#         the slot moves rather than the measured edge.
#   0833  Defect A revival: options settle 08:00Z on Deribit+OKX and
#         the frozen boot-time chain kills both sessions; a restart
#         re-runs discovery onto a live chain. 08:33 — not 08:05 —
#         clears Deribit's 9–19 min post-settlement removal lag.
#         0833, NOT 0830 (BIN15 P5.1, operator ruling 2026-09-13):
#         a 15-minute binary expiring at 08:30 settles on the mean
#         mark over [08:30:00, 08:31:00], which sat EXACTLY inside
#         this slot's drain — so no window ever held that instance's
#         settlement and its P&L was reported nowhere (the defect-6
#         class). 00:10 and 16:05 are already off the 15 m grid;
#         this one was not. Nothing here is time-critical to the
#         second, so the slot moves rather than the measurement.
#         MIGRATION: the 0830 stamp is orphaned and the new 0833
#         stamp SEEDS on its first tick (no fire), by the same
#         deploy-safety law as any new slot.
#   1605  Defect B revival: PM up/down dailies resolve 16:00Z; the
#         wrapper's per-boot refresh subscribes the market that went
#         live at 16:00Z.
#   0020  ACTION slot (no drain): nightly shadow-P&L report for the
#         closed UTC day (M4 D2 — claude_worker.pnl_report module),
#         then the BIN15 accrual — the closed day's HIP-4 instances
#         merged into the calibration ledger and the entry store
#         before retention sweeps the run dirs to object storage.
#
# Restart slots SIGTERM the engine (M1d-proven clean drain — capture
# flushed, run dir sealed); launchd KeepAlive relaunches through
# engine-wrapper.sh -> universe refresh -> fresh boot discovery ->
# fresh run dir + manifests (the per-run manifest law is BUILT for
# restarts). Multiple due slots (deploy day, wake-from-sleep
# catch-up) coalesce into ONE drain. Days now carry 3-4 run dirs —
# Aug-23 carried 3 and scored GAP-FREE; each dark window is ~10-40 s
# against the 300 s catalog tolerance.
#
# MIGRATION SEED (deploy safety): a MISSING slot stamp seeds to
# today WITHOUT firing — deploying this script mid-day never
# triggers a surprise drain. Force a slot NOW (also the sanctioned
# manual mid-day revive) with:
#   echo 19700101 > ~/multivenue/state/last-restart-utc-0010
# (fires within 60 s). The legacy single stamp `last-restart-utc-day`
# is ignored and left in place.
#
# EXECUTION GATE (E3): when ANY ~/multivenue/exec*.toml marks a slot
# LIVE, a fired restart slot must first prove the release binary still
# signs the way the venue expects (scripts/exec-smoke.sh). A failure
# DEFERS the drain — and only the drain: retention and the xsd rotation
# keep running, and the owed restart is taken the minute the gate goes
# green. See the block below for the backoff and why it is mandatory.
#
# PMJ GATE (J16): when slot 11 is armed (strategy.conf's ARM_LIVE names
# it, or an exec*.toml marks it live), the drain that would re-arm it
# waits on scripts/pmj-exec.sh key-check too, the same deferral and
# backoff shape; a review trigger (exit 77) also halts the running slot
# through exec.HALT. A launchd crash relaunch skips E3 and this gate
# alike (their blocks below).
#
# StartInterval (not StartCalendarInterval) because launchd calendar
# fires in LOCAL time — a UTC-day law must not bend to DST.
set -u

STATE="$HOME/multivenue/state"
mkdir -p "$STATE"
REPO_DIR="${0:A:h:h}"
SCRIPTS_DIR="${0:A:h}"

today="$(date -u +%Y%m%d)"
now_hm="$(date -u +%H%M)"

# slot_ready <HHMM>: succeeds when now >= slot and the slot's stamp
# is not today. Side effect: an ABSENT stamp is seeded to today and
# reported not-ready (the migration seed above).
slot_ready() {
  local slot="$1"
  local stamp="$STATE/last-restart-utc-$slot"
  [ "$((10#$now_hm))" -ge "$((10#$slot))" ] || return 1
  if [ ! -f "$stamp" ]; then
    echo "$today" > "$stamp"
    echo "daily-restart: seeded slot $slot (no fire)" >&2
    return 1
  fi
  [ "$(cat "$stamp")" != "$today" ]
}

# slot_mark <HHMM>: stamp the slot as done for today.
slot_mark() {
  echo "$today" > "$STATE/last-restart-utc-$1"
}

drain=0
retention=0
rotate=0
fired=""
if slot_ready 0010; then
  slot_mark 0010
  drain=1
  retention=1
  rotate=1
  fired="$fired 0010"
fi
if slot_ready 0833; then
  slot_mark 0833
  drain=1
  fired="$fired 0833"
fi
if slot_ready 1605; then
  slot_mark 1605
  drain=1
  fired="$fired 1605"
fi
# BST3.5 (binance-stocks-plan, operator-approved 2026-08-29): the
# equity-dailies pair — next-day PM equity markets load ≤15 min after
# the US close in BOTH seasons (20:00Z close in EDT, 21:00Z in EST;
# the pre-close slot of the pair resolves idempotently).
if slot_ready 2015; then
  slot_mark 2015
  drain=1
  fired="$fired 2015"
fi
if slot_ready 2115; then
  slot_mark 2115
  drain=1
  fired="$fired 2115"
fi

# ---------------------------------------------------------------------
# E3 PRE-RESTART EXECUTION GATE (real-execution plan §5).
#
#   A relinked binary that cannot sign a TESTNET order must never be
#   allowed to sign a MAINNET one.
#
# A restart is the moment the fleet picks up whatever is in
# target/release. If a rebuild broke the signing path — and LAW E-3
# (msgpack key order is part of the signature) has NO compile-time
# guard, so reordering two struct fields is enough — the engine that
# comes back signs valid signatures over digests the venue never
# computed. Every order rejected, and nothing in the boot tell says why.
#
# So: when exec.toml marks any slot LIVE, prove the binary BEFORE
# handing it the fleet.
#
# TWO HALVES, AND ONLY ONE OF THEM COSTS ANYTHING
#   offline  the binary reproduces the 36 SDK known-answer vectors and
#            rebuilds one action per TYPE from inputs. This is the half
#            that covers LAW E-3 for ORDERS — the network probe sends a
#            cancel, so it structurally cannot. Free, no venue, no key.
#   venue    a good signature is verified and a corrupted one rejected.
#            Two signed POSTs. No balance, no registered agent.
#
# The offline half runs on EVERY attempt. The venue half is rate-limited
# (below), so the offline half is what keeps a red gate diagnosable
# while we are deliberately not talking to the venue.
#
# WHY THE BACKOFF IS NOT OPTIONAL
# Hyperliquid's request budget is ADDRESS-based: 10,000 plus one per
# USDC of lifetime volume, and an exhausted address drops to ONE request
# every 10 seconds. The venue half needs two sequential posts inside a
# 5 s deadline. So a gate that retried every 60 s would spend ~2,880
# requests a day and, within days, exhaust the very budget it needs to
# go green — a failure that repairs itself into a permanent one. Hence
# 1, 5, 15 then 60 minutes.
#
# WHAT A RED GATE DOES AND DOES NOT STOP
# It defers the RESTART, and nothing else. Retention and the xsd table
# rotation still run: they have nothing to do with signing, and a gate
# that silently stopped the disk-pressure sweep would turn a signing
# problem into a full disk on a laptop-hosted engine. The restart is
# recorded as OWED in exec-gate-pending and taken the minute the gate
# goes green — so the slots are deferred, never consumed and never lost.
#
# FAIL-CLOSED, including on an unreachable venue: an unproven binary is
# unproven whatever the reason. The engine already running is untouched
# and keeps trading under the binary that was vetted when IT booted.
# Refusing the restart costs a stale boot chain, which costs a dark
# member, which costs nothing. Handing the fleet an unproven signer
# costs money.
#
# THERE IS NO PAGING LANE. After EXEC_GATE_ALERT_AFTER consecutive
# failures this writes state/exec-gate-RED, which is a file an operator
# has to look at. docs/risk-policy.md records that as a known gap.
# ---------------------------------------------------------------------
EXEC_TOML="${MULTIVENUE_EXEC_TOML:-$HOME/multivenue/exec.toml}"
EXEC_GATE_PENDING="$STATE/exec-gate-pending"
EXEC_GATE_FAILS="$STATE/exec-gate-fails"
EXEC_GATE_NEXT="$STATE/exec-gate-next"
EXEC_GATE_RED="$STATE/exec-gate-RED"
EXEC_GATE_VETTED="$STATE/exec-gate-vetted-sha256"
EXEC_GATE_ALERT_AFTER=3
# How long a passing venue verdict stands for one unchanged artifact.
EXEC_GATE_VET_TTL_MIN=1440

# WHICH FILE ARMS THE FLEET IS NOT KNOWABLE FROM HERE.
#
# Arming is `--exec <path>` plus `--arm-live` on the engine's command
# line, and `--exec` has NO DEFAULT PATH by design (exec.toml.example:
# "an artifact that auto-loads from a well-known path is one filesystem
# accident away from arming a slot nobody meant to arm"). So a gate
# that only ever read ~/multivenue/exec.toml would be SKIPPED ENTIRELY
# by an operator who armed with exec-live.toml — silently, and in the
# one direction that matters.
#
# So this scans EVERY exec*.toml beside it and arms on any of them.
# The asymmetry is deliberate: a false positive costs one 4-second
# probe against testnet, and a false negative costs an unvetted signer
# on the fleet.
#
# For the same reason an EXISTING but UNREADABLE file arms the gate. It
# cannot be shown inert, and "cannot be shown inert" is not "is inert".
#
# An ABSENT file is every slot paper (exec.toml.example, grammar law 2)
# and `enabled = 0` makes the whole file inert, so neither needs a gate.
EXEC_GATE_TRIGGER=""
exec_gate_armed() {
  local f
  # (N) is zsh's null_glob for this glob alone: no matches expands to
  # nothing rather than erroring under the default nomatch.
  for f in "$EXEC_TOML" "$HOME"/multivenue/exec*.toml(N); do
    [ -f "$f" ] || continue
    if [ ! -r "$f" ]; then
      EXEC_GATE_TRIGGER="$f (UNREADABLE — arming the gate rather than assuming it is inert)"
      return 0
    fi
    if grep -Eq '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*0' "$f"; then
      continue
    fi
    if grep -Eq '^[[:space:]]*mode[[:space:]]*=[[:space:]]*"live"' "$f"; then
      EXEC_GATE_TRIGGER="$f"
      return 0
    fi
  done
  EXEC_GATE_TRIGGER=""
  return 1
}

exec_gate_note_failure() {
  local rc="$1" where="$2" n delay
  n=1
  [ -f "$EXEC_GATE_FAILS" ] && n=$(( $(cat "$EXEC_GATE_FAILS") + 1 ))
  echo "$n" > "$EXEC_GATE_FAILS"
  case "$n" in
    1) delay=60 ;;
    2) delay=300 ;;
    3) delay=900 ;;
    *) delay=3600 ;;
  esac
  echo "$(( $(date -u +%s) + delay ))" > "$EXEC_GATE_NEXT"
  echo "daily-restart: exec gate FAILED at the $where (exit $rc), attempt $n; next venue attempt in $((delay / 60)) min" >&2
  if [ "$n" -ge "$EXEC_GATE_ALERT_AFTER" ]; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) exec gate RED: $where exit $rc, $n consecutive failures" \
      > "$EXEC_GATE_RED"
    echo "daily-restart: *** $EXEC_GATE_RED written — restarts are deferred until this clears ***" >&2
  fi
}

exec_gate_clear() {
  rm -f "$EXEC_GATE_FAILS" "$EXEC_GATE_NEXT" "$EXEC_GATE_RED"
}

# 0 = the binary is proven and the restart may proceed.
# THE GATE VETS AN ARTIFACT, NOT A MOMENT.
#
# "The release binary passed" is a claim about a FILE, so the file is
# what gets recorded — its sha256 — rather than a timestamp attached to
# a name that can come to mean a different file. Two things follow:
#
#   a REBUILD invalidates the pass immediately. The digest no longer
#   matches, so the next fired slot re-proves the new artifact against
#   the venue rather than inheriting a verdict earned by the old one.
#   That is the provenance hole this closes.
#
#   an UNCHANGED binary is not re-probed. Five restart slots a day were
#   spending ten signed POSTs to re-prove the same bytes; now they
#   spend two, once, and the TTL below re-proves it daily anyway. The
#   venue's request budget is address-based and finite, so a gate that
#   asks the same question ten times a day is spending the budget it
#   depends on.
#
# WHAT THIS DOES NOT CLOSE: between the drain and the wrapper's exec
# there are still seconds in which a build could land, and only the
# WRAPPER checking this digest before exec would close that. The
# wrapper is the file that arms the fleet — E4's change, not one to
# make by inference now. The digest is recorded here so E4 has
# something to check against. docs/risk-policy.md records the residue.
exec_gate_release_sha() {
  shasum -a 256 "$REPO_DIR/target/release/multivenue-engine" 2>/dev/null | cut -d" " -f1
}

# 0 = this exact artifact already holds a venue verdict, still in date.
exec_gate_already_vetted() {
  local sha age
  sha="$(exec_gate_release_sha)"
  [ -n "$sha" ] || return 1
  [ -f "$EXEC_GATE_VETTED" ] || return 1
  [ "$(cat "$EXEC_GATE_VETTED")" = "$sha" ] || return 1
  # find -mmin is the portable-enough way to age a file in a script
  # that already assumes macOS + zsh.
  [ -z "$(find "$EXEC_GATE_VETTED" -mmin "+$EXEC_GATE_VET_TTL_MIN" 2>/dev/null)" ]
}

exec_gate_pass() {
  local rc now next
  # Free half, every time. NOTE the explicit `rc=$?` on the line after
  # the command: inside an `if ! cmd; then` branch `$?` is the status of
  # the `if`, which is 0 — so capturing it there reported every failure
  # as "exit 0" and threw away the one number that says WHAT broke.
  "$SCRIPTS_DIR/exec-smoke.sh" --offline >/dev/null
  rc=$?
  if [ "$rc" != 0 ]; then
    exec_gate_note_failure "$rc" "offline self-test"
    return 1
  fi
  # Already proven, and the artifact has not changed since. Do not
  # spend two more venue requests saying so.
  if exec_gate_already_vetted; then
    echo "daily-restart: exec gate — this exact binary already holds a venue verdict (${$(exec_gate_release_sha)[1,16]}…)" >&2
    return 0
  fi

  now="$(date -u +%s)"
  if [ -f "$EXEC_GATE_NEXT" ]; then
    next="$(cat "$EXEC_GATE_NEXT")"
    if [ "$now" -lt "$next" ]; then
      echo "daily-restart: exec gate backing off — $(( (next - now + 59) / 60 )) min until the next venue attempt" >&2
      return 1
    fi
  fi
  "$SCRIPTS_DIR/exec-smoke.sh" >/dev/null
  rc=$?
  if [ "$rc" = 0 ]; then
    exec_gate_clear
    # Record WHICH artifact earned this verdict.
    exec_gate_release_sha > "$EXEC_GATE_VETTED"
    return 0
  fi
  exec_gate_note_failure "$rc" "venue phase"
  return 1
}

# >>> pmj-gate (definitions)
# ---------------------------------------------------------------------
# PMJ J16 PRE-RESTART GATE (slot 11; risk review J16 M2, round 2).
#
#   Every arming of slot 11 re-proves its wallet and its venue's code.
#
# A restart is an ARMING: the engine that comes back boots slot 11 live
# when strategy.conf's ARM_LIVE names 11. So the review trigger the flip
# runs — scripts/pmj-exec.sh key-check: the keypair file; the wallet at
# most the day cap (requirement (a)) and its SOL floor; the API key and
# the region; the Predict program's deploy slot against its pin (a new
# deploy is a review trigger); the vault's lookup-table entry; the deposit
# mint's decimals — runs again before every drain that would re-arm it,
# at the [exec.slot.11] cap_day_usd_1e6 of the EXEC_TOML in force.
#
# WHEN IT ARMS (E3's asymmetry: a false positive costs a key check, a
# false negative an unproven arming): strategy.conf's ARM_LIVE names 11,
# read as the wrapper and the engine read it — the file SOURCED in a
# subshell (quotes, `export `, $HOME and ~, the last assignment wins),
# each comma-separated part trimmed and compared as a number (`011`,
# `3, 11` count) — OR the EXEC_TOML in force or any ~/multivenue/exec*.toml
# marks [exec.slot.11] live (an existing but unreadable one arms too; an
# `enabled = 0` file is inert, as E3 reads it).
#
# THE SAME SHAPE AS E3: the offline half (pmj-exec.sh self-test) on every
# attempt; the online half backs off 1, 5, 15, then 60 minutes (Jupiter's
# API key and public Solana RPC have request budgets too); a failure
# DEFERS the drain and only the drain — the restart is owed in
# pmj-gate-pending and taken the minute the key check passes, retention
# and the xsd rotation still run; after PMJ_GATE_ALERT_AFTER consecutive
# failures it writes state/pmj-gate-RED. It decides only a restart E3 did
# not defer (a restart E3 owes stays E3's). FAIL-CLOSED: an unreadable
# cap, a missing key check, an unreachable venue all defer.
#
# A REVIEW TRIGGER STOPS THE RUNNING SLOT: key-check exit 77 (the Predict
# program's deploy slot is not the pinned one, the vault's lookup-table
# entry is not the pinned vault, or the deposit mint is not the reviewed
# 6-decimal one) also appends `slot=11 reason=operator` to the exec.HALT
# beside the EXEC_TOML in force — the running router reads that file every
# second and halts slot 11 (`operator` is the router's own word; any word
# it does not know reads as `operator` too) — once per halt, and pages the
# operator (scripts/notify-send.sh -l -s PMJ). Every other failure (78:
# transport, budget, balance, the key) only defers. The key check reads the
# chain FIRST (public RPC, no secret), so no 78 can mask a 77.
#
# A HALTED SLOT 11 DEFERS, NOT LOOPS: the live boot refuses while the
# exec.HALT beside the EXEC_TOML in force names slot 11 (pmj_live::
# check_halt_file), so a drain then would leave launchd relaunching into
# that refusal — every lane of the engine down. So when the gate is armed
# and that exec.HALT names slot 11 as the router reads it (a line whose
# `slot=` — or, with none, first bare number — is 11), or exists but cannot
# be read whole (not a regular file, over 640 B, unreadable, not UTF-8, as
# check_halt_file refuses it), the restart is deferred and kept owed in
# pmj-gate-pending, no key check runs, it counts as no failure (no backoff,
# no RED), and the operator is paged once per exec.HALT mtime (silently:
# notify-send.sh -s PMJ). Archive the halt line after review, or
# pmj-flip.sh paper.
#
# WHAT THIS GATE DOES NOT COVER: a launchd CRASH RELAUNCH (KeepAlive) boots
# the engine without this script — it skips E3 and this gate alike; and a
# slot-11 boot refusal (a wallet over the cap, Jupiter down, the keypair
# file) then keeps that engine down in a relaunch loop until the operator
# flips slot 11 to paper (scripts/pmj-flip.sh paper) or sweeps the wallet.
# Slot 11 is meant to run alone, on the standalone (R-PM-23), where that
# loop takes no other slot down.
#
# With slot 11 armed nowhere, nothing here runs a check and nothing changes.
# ---------------------------------------------------------------------
STRATEGY_CONF="$HOME/multivenue/strategy.conf"
PMJ_GATE_PENDING="$STATE/pmj-gate-pending"
PMJ_GATE_FAILS="$STATE/pmj-gate-fails"
PMJ_GATE_NEXT="$STATE/pmj-gate-next"
PMJ_GATE_RED="$STATE/pmj-gate-RED"
PMJ_GATE_HALT_PAGED="$STATE/pmj-gate-halt-paged"
PMJ_GATE_ALERT_AFTER=3
PMJ_GATE_TRIGGER=""
PMJ_GATE_FILE=""
exec_gate_deferred=0

# The value strategy.conf gives KEY as the wrapper sees it: the file
# SOURCED in a subshell (the wrapper sources it), nothing it prints kept.
pmj_conf_get() {
  [ -f "$STRATEGY_CONF" ] || return 0
  (
    unset "$1"
    set +u
    . "$STRATEGY_CONF" >/dev/null 2>&1
    print -r -- "${(P)1:-}"
  )
}

# 0 = the ARM_LIVE spec $1 names slot 11 as the engine's parse_arm_live
# reads it: comma-separated parts, each trimmed, a decimal slot number.
pmj_spec_names_11() {
  local part
  for part in "${(@s:,:)1}"; do
    part="${part//[[:space:]]/}"
    part="${part#+}"
    [[ "$part" == <-> ]] || continue
    (( 10#$part == 11 )) && return 0
  done
  return 1
}

# The EXEC_TOML in force: strategy.conf's, else the default.
pmj_exec_toml() {
  local f
  f="$(pmj_conf_get EXEC_TOML)"
  print -r -- "${f:-$HOME/multivenue/exec.toml}"
}

# 0 = file $1 has an [exec.slot.11] section whose mode is live.
pmj_file_arms_11() {
  awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) } s && /^[ \t]*mode[ \t]*=[ \t]*"live"/ { found = 1 } END { exit !found }' "$1"
}

# 0 = slot 11 is (or may be) armed (the header's WHEN IT ARMS);
# PMJ_GATE_TRIGGER says why, PMJ_GATE_FILE the exec file that armed it.
pmj_gate_armed() {
  local f
  PMJ_GATE_FILE=""
  if pmj_spec_names_11 "$(pmj_conf_get ARM_LIVE)"; then
    PMJ_GATE_TRIGGER="strategy.conf's ARM_LIVE names slot 11"
    return 0
  fi
  for f in "$(pmj_exec_toml)" "$HOME"/multivenue/exec*.toml(N); do
    [ -f "$f" ] || continue
    if [ ! -r "$f" ]; then
      PMJ_GATE_TRIGGER="$f (UNREADABLE — arming the pmj gate rather than assuming slot 11 is not live)"
      return 0
    fi
    if grep -Eq '^[[:space:]]*enabled[[:space:]]*=[[:space:]]*0' "$f"; then
      continue
    fi
    if pmj_file_arms_11 "$f"; then
      PMJ_GATE_TRIGGER="$f marks [exec.slot.11] live"
      PMJ_GATE_FILE="$f"
      return 0
    fi
  done
  PMJ_GATE_TRIGGER=""
  return 1
}

# The [exec.slot.11] cap_day_usd_1e6 of file $1, digits only (TOML's
# underscores dropped); empty when there is none.
pmj_cap_of() {
  local v
  [ -r "$1" ] || return 0
  v="$(awk '/^[ \t]*\[/ { s = ($0 ~ /^[ \t]*\[exec\.slot\.11\][ \t]*(#.*)?$/) } s && /^[ \t]*cap_day_usd_1e6[ \t]*=/ { v = $0; sub(/^[^=]*=[ \t]*/, "", v); sub(/[ \t]*(#.*)?$/, "", v); print v; exit }' "$1")"
  print -r -- "${v//_/}"
}

# The cap the key check judges the wallet at: the EXEC_TOML in force's,
# else that of the file that armed the gate; empty when there is none.
pmj_gate_cap_day() {
  local v
  v="$(pmj_cap_of "$(pmj_exec_toml)")"
  if [ -z "$v" ] && [ -n "$PMJ_GATE_FILE" ]; then
    v="$(pmj_cap_of "$PMJ_GATE_FILE")"
  fi
  print -r -- "$v"
}

pmj_gate_note_failure() {
  local rc="$1" where="$2" n delay
  n=1
  [ -f "$PMJ_GATE_FAILS" ] && n=$(( $(cat "$PMJ_GATE_FAILS") + 1 ))
  echo "$n" > "$PMJ_GATE_FAILS"
  case "$n" in
    1) delay=60 ;;
    2) delay=300 ;;
    3) delay=900 ;;
    *) delay=3600 ;;
  esac
  echo "$(( $(date -u +%s) + delay ))" > "$PMJ_GATE_NEXT"
  echo "daily-restart: pmj gate FAILED at the $where (exit $rc), attempt $n; next pmj key-check in $((delay / 60)) min" >&2
  if [ "$n" -ge "$PMJ_GATE_ALERT_AFTER" ]; then
    echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) pmj gate RED: $where exit $rc, $n consecutive failures" \
      > "$PMJ_GATE_RED"
    echo "daily-restart: *** $PMJ_GATE_RED written — restarts are deferred until the pmj key-check passes ***" >&2
  fi
}

pmj_gate_clear() {
  rm -f "$PMJ_GATE_FAILS" "$PMJ_GATE_NEXT" "$PMJ_GATE_RED" "$PMJ_GATE_HALT_PAGED"
}

# The exec.HALT beside the EXEC_TOML in force (the router's and the live
# boot's halt file).
pmj_halt_path() {
  local h
  h="$(pmj_exec_toml)"
  print -r -- "${h:h}/exec.HALT"
}

# The mtime of $1 (zsh's own stat: no platform flags), or `unknown`.
pmj_halt_mtime() {
  zmodload -F zsh/stat b:zstat 2>/dev/null
  zstat +mtime -- "$1" 2>/dev/null || print -r -- unknown
}

# 0 = the halt file $1 would refuse slot 11's live boot (the header's A
# HALTED SLOT 11): it names slot 11 as the router's parse_halt_file reads
# it, or it exists but cannot be read whole. Absent = 1.
pmj_halt_blocks() {
  local h="$1"
  [ -e "$h" ] || return 1
  [ -f "$h" ] && [ -r "$h" ] || return 0
  [ "$(( $(wc -c < "$h") ))" -le 640 ] || return 0
  if command -v iconv >/dev/null 2>&1 && ! iconv -f UTF-8 -t UTF-8 "$h" >/dev/null 2>&1; then
    return 0
  fi
  awk '
    {
      line = $0
      sub(/\r$/, "", line); sub(/^[ \t]+/, "", line); sub(/[ \t]+$/, "", line)
      if (line == "" || substr(line, 1, 1) == "#") next
      n = split(line, w, /[ \t]+/)
      slot = ""
      for (i = 1; i <= n; i++) {
        f = w[i]
        if (f ~ /^slot=/) {
          v = substr(f, 6)
          slot = (v ~ /^[+]?[0-9]+$/) ? v : ""
        } else if (f ~ /^reason=/) {
          continue
        } else if (slot == "" && f ~ /^[+]?[0-9]+$/) {
          slot = f
        }
      }
      if (slot != "") { sub(/^[+]/, "", slot); if (slot + 0 == 11) found = 1 }
    }
    END { exit !found }' "$h"
}

# Page once per halt-file mtime (silently: the halt is the operator's own,
# or the review trigger already rang).
pmj_halt_page() {
  local m
  m="$(pmj_halt_mtime "$1")"
  if [ -f "$PMJ_GATE_HALT_PAGED" ] && [ "$(cat "$PMJ_GATE_HALT_PAGED")" = "$m" ]; then
    return 0
  fi
  echo "$m" > "$PMJ_GATE_HALT_PAGED"
  "$SCRIPTS_DIR/notify-send.sh" -s PMJ "slot 11: exec.HALT names slot 11 — restarts deferred until the halt line is archived (after review) or slot 11 is flipped to paper (pmj-flip.sh paper)." >/dev/null 2>&1 \
    || echo "daily-restart: pmj: notify-send.sh failed" >&2
}

# The review trigger fired (key-check exit 77): halt the RUNNING slot 11
# through the exec.HALT beside the EXEC_TOML in force (once per halt: a
# file that already halts slot 11 is left alone) and page the operator.
pmj_gate_review_halt() {
  local h when
  h="$(pmj_halt_path)"
  when="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ -f "$h" ] && grep -Eq '^[[:space:]]*(slot=)?0*11([[:space:]]|$)' "$h"; then
    echo "daily-restart: pmj REVIEW TRIGGER — $h already halts slot 11" >&2
    return 0
  fi
  if [ -s "$h" ] && [ -n "$(tail -c 1 "$h")" ]; then
    printf '\n' >> "$h"
  fi
  if printf 'slot=11 reason=operator\n' >> "$h"; then
    echo "daily-restart: *** pmj REVIEW TRIGGER — slot 11 HALTED: slot=11 appended to $h ***" >&2
    if [ "$(( $(wc -c < "$h") ))" -gt 640 ]; then
      echo "daily-restart: *** $h is over the router's 640 B — it will NOT be read: HALT SLOT 11 BY HAND ***" >&2
    fi
  else
    echo "daily-restart: *** pmj REVIEW TRIGGER — could NOT append to $h: HALT SLOT 11 BY HAND ***" >&2
  fi
  "$SCRIPTS_DIR/notify-send.sh" -l -s PMJ "slot 11 REVIEW TRIGGER at $when: pmj key-check exit 77 (the Predict deploy, the vault entry or the deposit mint is not what was reviewed, or a pinned account closed). slot=11 written to exec.HALT; restarts deferred. Review before clearing." >/dev/null 2>&1 \
    || echo "daily-restart: pmj review trigger — notify-send.sh failed (page by hand)" >&2
  # This page rang for this halt: the halt deferral below does not re-page
  # until the file changes.
  pmj_halt_mtime "$h" > "$PMJ_GATE_HALT_PAGED"
}

# 0 = slot 11's wallet and venue code are proven for this arming.
pmj_gate_pass() {
  local rc now next cap pmj
  # Free half, every time (the explicit rc=$?, as E3's).
  "$SCRIPTS_DIR/pmj-exec.sh" self-test >/dev/null
  rc=$?
  if [ "$rc" != 0 ]; then
    pmj_gate_note_failure "$rc" "pmj self-test"
    return 1
  fi
  now="$(date -u +%s)"
  if [ -f "$PMJ_GATE_NEXT" ]; then
    next="$(cat "$PMJ_GATE_NEXT")"
    if [ "$now" -lt "$next" ]; then
      echo "daily-restart: pmj key-check backing off — $(( (next - now + 59) / 60 )) min until the next attempt" >&2
      return 1
    fi
  fi
  cap="$(pmj_gate_cap_day)"
  case "$cap" in
    ''|*[!0-9]*)
      pmj_gate_note_failure 78 "[exec.slot.11] cap_day_usd_1e6 read (none in the EXEC_TOML in force)"
      return 1
      ;;
  esac
  pmj="$(pmj_conf_get PMJ_TOML)"
  if [ -n "$pmj" ]; then
    "$SCRIPTS_DIR/pmj-exec.sh" key-check --pmj "$pmj" --cap-day-usd-1e6 "$cap" >/dev/null
  else
    "$SCRIPTS_DIR/pmj-exec.sh" key-check --cap-day-usd-1e6 "$cap" >/dev/null
  fi
  rc=$?
  if [ "$rc" = 0 ]; then
    pmj_gate_clear
    return 0
  fi
  if [ "$rc" = 77 ]; then
    pmj_gate_review_halt
    pmj_gate_note_failure "$rc" "pmj key-check REVIEW TRIGGER"
    return 1
  fi
  pmj_gate_note_failure "$rc" "pmj key-check"
  return 1
}
# <<< pmj-gate

if [ "$drain" = 1 ] || [ -f "$EXEC_GATE_PENDING" ] || [ -f "$PMJ_GATE_PENDING" ]; then
  if exec_gate_armed; then
    echo "daily-restart: $EXEC_GATE_TRIGGER marks a slot LIVE — the execution gate decides this restart" >&2
    if exec_gate_pass; then
      echo "daily-restart: exec gate PASSED — restart may proceed" >&2
      if [ -f "$EXEC_GATE_PENDING" ]; then
        echo "daily-restart: taking the restart owed from$(cat "$EXEC_GATE_PENDING")" >&2
        fired="$fired$(cat "$EXEC_GATE_PENDING")"
        rm -f "$EXEC_GATE_PENDING"
        drain=1
      fi
    else
      if [ "$drain" = 1 ]; then
        # Record the restart as OWED. The slots stay stamped, so
        # retention and the xsd rotation keep their once-a-day law.
        echo "$fired" >> "$EXEC_GATE_PENDING"
        echo "daily-restart: restart DEFERRED (slots$fired owed); retention and rotation still run" >&2
      fi
      drain=0
      exec_gate_deferred=1
    fi
  else
    # Not armed. Any pending restart is moot — the binary it was
    # waiting on cannot reach a venue.
    if [ -f "$EXEC_GATE_PENDING" ]; then
      echo "daily-restart: no live slot any more — dropping the deferred restart and clearing the gate" >&2
    fi
    rm -f "$EXEC_GATE_PENDING"
    exec_gate_clear
  fi
fi

# >>> pmj-gate (decision)
# The PMJ gate (its definitions above): a restart E3 did not defer that
# would re-arm slot 11 waits on its key check; a failure defers it as E3's
# does, owed in pmj-gate-pending (exit 77 also halts the running slot); an
# exec.HALT naming slot 11 defers it without a check (no boot loop).
if [ "$exec_gate_deferred" = 0 ] && { [ "$drain" = 1 ] || [ -f "$PMJ_GATE_PENDING" ]; }; then
  if pmj_gate_armed && pmj_halt_blocks "$(pmj_halt_path)"; then
    echo "daily-restart: pmj: exec.HALT names slot 11 — restart deferred; archive the halt line or pmj-flip.sh paper" >&2
    if [ "$drain" = 1 ]; then
      echo "$fired" >> "$PMJ_GATE_PENDING"
    fi
    drain=0
    pmj_halt_page "$(pmj_halt_path)"
  elif pmj_gate_armed; then
    echo "daily-restart: $PMJ_GATE_TRIGGER — the pmj key-check decides this restart" >&2
    if pmj_gate_pass; then
      echo "daily-restart: pmj key-check PASSED — restart may proceed" >&2
      if [ -f "$PMJ_GATE_PENDING" ]; then
        echo "daily-restart: taking the restart the pmj key-check owed, from$(cat "$PMJ_GATE_PENDING")" >&2
        fired="$fired$(cat "$PMJ_GATE_PENDING")"
        rm -f "$PMJ_GATE_PENDING"
        drain=1
      fi
    else
      if [ "$drain" = 1 ]; then
        echo "$fired" >> "$PMJ_GATE_PENDING"
        echo "daily-restart: restart DEFERRED by the pmj key-check (slots$fired owed); retention and rotation still run" >&2
      fi
      drain=0
    fi
  else
    # Slot 11 armed nowhere any more: a restart its key check deferred is moot.
    if [ -f "$PMJ_GATE_PENDING" ]; then
      echo "daily-restart: slot 11 is armed nowhere any more — dropping the restart the pmj key-check deferred" >&2
    fi
    rm -f "$PMJ_GATE_PENDING"
    pmj_gate_clear
  fi
fi
# <<< pmj-gate

# XSD-4 (statarb doc 08 §3.7; operator ruling 2026-09-12 "automatic
# monthly at the 00:10Z restart"): the xsd member's TABLE ROTATION.
# Once a UTC day, at the 0010 slot and BEFORE the drain, re-run the
# research screen when the standing table is >= 30 days old (the
# research's 30-day trading window) or absent — the wrapper's boot
# that follows reads the new table, its seed-out covers the new
# descriptors, and the engine flattens every position held under the
# old hash (`xsd: state discarded (table hash changed)`), which is the
# research's fold-end behaviour. The screen writes NOTHING when it
# finds no rows (exit 3 — the old table stays, the boot restores under
# the old hash), so a data hole can never brick the member. Runs only
# when the member is configured (xsd.toml + xsd-universe.tsv present)
# and no worker verb is live; the release dir on PATH is not needed
# (candles.db in, TSV out — no engine spawn). Seconds of numpy work;
# the 60 s StartInterval never overlaps itself.
if [ "$rotate" = 1 ] && [ -f "$HOME/multivenue/xsd.toml" ] && [ -f "$HOME/multivenue/xsd-universe.tsv" ]; then
  if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
    echo "daily-restart: xsd rotation skipped — worker busy (next 0010 retries)" >&2
  else
    (
      cd "$REPO_DIR/claude-worker" || exit 0
      export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
      # xsd_status, NOT status: zsh's $status is read-only (it is $?), and
      # assigning it killed this subshell at every 0010 slot until
      # 2026-09-27 (restart.log: "read-only variable: status").
      xsd_status="$(uv run python -m claude_worker.xsd_author status --table "$HOME/multivenue/xsd-table.tsv" 2>/dev/null)"
      echo "daily-restart: $xsd_status" >&2
      case "$xsd_status" in
        *rotation_due=yes*|*"table absent"*)
          echo "daily-restart: xsd table rotation — re-running the screen" >&2
          uv run python -m claude_worker.xsd_author author \
              --universe "$HOME/multivenue/xsd-universe.tsv" \
              --out "$HOME/multivenue/xsd-table.tsv" >&2 ||
            echo "daily-restart: xsd rotation failed (non-fatal; the old table boots)" >&2
          ;;
      esac
    )
  fi
fi

if [ "$drain" = 1 ]; then
  if pgrep -f "multivenue-engine run" >/dev/null 2>&1; then
    echo "daily-restart: UTC $today $now_hm slots$fired — SIGTERM drain" >&2
    pkill -TERM -f "multivenue-engine run" 2>/dev/null || true
  else
    echo "daily-restart: UTC $today $now_hm slots$fired — engine not running (KeepAlive will boot)" >&2
  fi
fi

if [ "$retention" = 1 ]; then
  # Once per UTC day: the M3 retention pass (keep-all until disk
  # pressure; scripts/retention.sh documents the policy).
  "$SCRIPTS_DIR/retention.sh" >&2 || true
fi

# 0020 ACTION slot — nightly shadow-P&L for the closed UTC day (the
# M4 D2 deferral landing here per the remediation plan). Worker-verb
# serialization law: any live claude-worker invocation defers this to
# the next minute (the stamp stays unmarked until it actually runs).
if slot_ready 0020; then
  if pgrep -f 'claude[-_]worke[r]' >/dev/null 2>&1; then
    echo "daily-restart: 0020 pnl deferred — worker busy (retry next minute)" >&2
  else
    slot_mark 0020
    (
      cd "$REPO_DIR" || exit 0
      # ICDP I6 (2026-09-03): the release dir on PATH — the module spawns
      # `multivenue-engine` by name (the §14 spawn contract) and launchd's
      # PATH never carried it: every 0020 run since Aug-23 died with
      # FileNotFoundError before audit-pnl started (restart.log).
      export PATH="$REPO_DIR/target/release:$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
      if [ -f .env ]; then
        set -a
        . ./.env
        set +a
      fi
      cd claude-worker || exit 0
      echo "daily-restart: 0020 pnl_report (closed UTC day, per-run bounded, fee tier)" >&2
      # Day mode: one bounded audit-pnl per run of the closed UTC day
      # (never the whole root — ops debt c), merged into the day pair,
      # with the operator's tier from ~/multivenue/fees.toml when present.
      uv run python -m claude_worker.pnl_report --closed-day >&2 ||
        echo "daily-restart: pnl_report failed (non-fatal; tomorrow retries)" >&2
      # BIN15 accrual (operator 2026-09-13, "keep it working to collect
      # data"): turn the closed day's HIP-4 capture into ledger rows and
      # entry rows before retention can sweep the run dirs.
      #
      # It has to live HERE, in the 0020 ACTION slot: retention runs at
      # 0010 and protects one day, so a run from yesterday is still on
      # disk ten minutes later with ~24 h of margin — but only just, and
      # once it is archived the evidence costs an S3 pull to recover.
      # The lane is a SAMPLE SIZE problem (32 settled entries gave a
      # 95 % interval of 51-82 % on the hit rate), so the whole point is
      # that every day's evidence lands in one small permanent file.
      #
      # Idempotent by construction: both merges dedupe, so a re-run adds
      # nothing and can never un-settle a row. Non-fatal like the report
      # above — a failed night is retried by tomorrow's, because the
      # capture is what is precious, not the derivation.
      #
      # S7-L1: once slot 3 trades live, ~/multivenue/bin15.toml is the
      # LIVE-sized artifact, and the replay must keep scoring the PAPER
      # law — the arming step copies the paper artifact to
      # bin15.paper-s7.toml first, and the replay reads that copy when
      # it exists.
      if [ -f "$HOME/multivenue/bin15.toml" ]; then
        b15="$HOME/multivenue/bin15.toml"
        [ -f "$HOME/multivenue/bin15.paper-s7.toml" ] && b15="$HOME/multivenue/bin15.paper-s7.toml"
        echo "daily-restart: 0020 bin15 accrual (closed UTC day, $b15)" >&2
        uv run python -m claude_worker.bin15_accrue accrue --bin15 "$b15" >&2 ||
          echo "daily-restart: bin15 accrual failed (non-fatal; tomorrow retries)" >&2
      fi
      # HYPARB G1 (operator 2026-10-15): the gate's tally. Fold the closed
      # day's slot-0 sessions under the member's own P&L law into
      # ~/multivenue/worker/hyparb/g1-sessions.tsv (each run dir is one
      # session; /state's level dies with it), then replay the day's lean
      # <= 2 h windows at r3 and at the judge's 2-block delta into
      # g1-replay.tsv. Here for the same reason as the bin15 accrual: the
      # run dirs are still local. The replay is cheap enough to ride the
      # slot -- one lean 2 h unit measured 0.4 s to cut and 0.4 s per rung
      # (2026-10-15) -- and BOUNDED, because a live worker process holds
      # off the engine's boot recommit: 120 s a rung, 300 s for the whole
      # replay; units past the budget stay pending (no row) for the next
      # night or a manual `backfill --replay`. Idempotent and non-fatal like
      # the two above.
      if [ -f "$HOME/multivenue/hyparb.toml" ]; then
        echo "daily-restart: 0020 hyparb G1 accrual (closed UTC day)" >&2
        uv run python -m claude_worker.hyparb_g1 accrue >&2 ||
          echo "daily-restart: hyparb G1 accrual failed (non-fatal; tomorrow retries)" >&2
      fi
    )
  fi
fi
