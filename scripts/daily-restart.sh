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
#   offline  the binary reproduces the 25 SDK known-answer vectors and
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
EXEC_GATE_ALERT_AFTER=3

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
    return 0
  fi
  exec_gate_note_failure "$rc" "venue phase"
  return 1
}

if [ "$drain" = 1 ] || [ -f "$EXEC_GATE_PENDING" ]; then
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
      status="$(uv run python -m claude_worker.xsd_author status --table "$HOME/multivenue/xsd-table.tsv" 2>/dev/null)"
      echo "daily-restart: $status" >&2
      case "$status" in
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
      if [ -f "$HOME/multivenue/bin15.toml" ]; then
        echo "daily-restart: 0020 bin15 accrual (closed UTC day)" >&2
        uv run python -m claude_worker.bin15_accrue accrue >&2 ||
          echo "daily-restart: bin15 accrual failed (non-fatal; tomorrow retries)" >&2
      fi
    )
  fi
fi
