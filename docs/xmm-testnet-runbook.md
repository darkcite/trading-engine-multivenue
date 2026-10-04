# XMM — the testnet engine-instance runbook (XH5e, R-XH4-8)

Anton runs this. It boots a **second engine on testnet**. That engine has its own
home, state, port, credentials and a minimal universe. It is never the launchd
paper engine and never mainnet. On the binary the mainnet probe will run, it
proves five things:

1. Slot 6 boots on the gateway, and its tells and numbers agree with the venue's.
2. The per-slot halt (XMM XH5a) works with two real arms: slot 6 on the gateway,
   and slot 3 (BIN15's member) on the operator's blocking arm over E3.
3. Top-ups work on an account of slot 6's own (XMM XH5c-2), and a restart restores
   the session (XMM XH5b-2).
4. The venue accepts the ALO priority grouping `{"p": N}`, and we know its answer
   to a priority place when no HYPE is undelegated (XMM XH5d-3). This is required
   before any `ab_mode` 2 on mainnet.
5. The preflight and the probe's artifacts (XMM XH5e-1) work on that binary.

The pass criteria are in §9. Record the results in the vault and tell the
session.

## 1. Before

- **The binary.** The checkout you run must be built from `xmm` (or from `main`
  after the merge): `cargo build --release -p cli`. Check that
  `target/release/multivenue-engine xmm-preflight --help` answers.
- **The time.** The run takes 2–3 h with the top-up wait. It has to fit between
  two scheduled drains (00:10, 08:33, 16:05, 20:15, 21:15Z) and stay outside the
  quiet windows (00:00–00:30, 08:25–08:45, 16:00–16:20Z), so start just after a
  window, e.g. 00:35 or 08:50Z.
  - While a second `multivenue-engine run` runs, the wrapper backs off a launchd
    relaunch.
  - daily-restart's `pkill` would stop the instance too.
- **The accounts (testnet).**
  - Slot 6 needs a master and an API wallet of its own, funded with testnet USDC:
    `HYPERLIQUID_XMM_TESTNET_AGENT_KEY` and `HYPERLIQUID_XMM_TESTNET_MASTER_ADDR`.
    Keep them in `~/multivenue/.env` beside the operator's other testnet keys.
  - Slot 3 runs on E3 (`HYPERLIQUID_TESTNET_AGENT_KEY` and
    `HYPERLIQUID_TESTNET_MASTER_ADDR`), mapped onto the operator arm's names in §2.
  - No other E3 signer may run: the launchd engine's slot 3 is paper
    (`scripts/bin15-flip.sh status`), and no `exec-smoke` is running.
  - For §7: about 2 HYPE undelegated on slot 6's testnet master.
- **Flat.** Neither testnet account holds a position or an open order (see the
  reads in §3).

## 2. The instance

The engine's default paths are `~/multivenue/…`, expanded from `$HOME`. The
instance runs with a home of its own, and you copy into it **by name only** what
`bin15+xmm` needs. Never copy a `*.env` file, and never copy the research
directories.

```sh
TN=$HOME/multivenue-tn
mkdir -p $TN/multivenue/logs $TN/multivenue/run $TN/multivenue/ai-rulesets
for f in bin15.toml fees.toml; do [ -f ~/multivenue/$f ] && cp ~/multivenue/$f $TN/multivenue/; done
cp ~/trading-engine-multivenue/xmm-probe.toml $TN/multivenue/xmm.toml
# exec.toml: [exec], the paper engine's [exec.slot.3] made live, the probe's slot 6.
{ printf '[exec]\nenabled = 1\n\n'
  awk '/^\[/ { s = ($0 ~ /^\[exec\.slot\.3\]/) } s' ~/multivenue/exec.toml | sed 's/^mode = "paper"/mode = "live"/'
  echo; cat ~/trading-engine-multivenue/exec-slot6-probe.toml; } > $TN/multivenue/exec.toml
```

Then write `$TN/multivenue/universe.toml` by hand. It is the minimal universe:
the four perps, their four leads, and bin15's families. That keeps the second
engine's sockets to the few this check reads. Copy any other section the boot asks
for from `~/multivenue/universe.toml`, with its lists emptied.

```toml
[binance]
spot = []
usdm = ["btcusdt", "ethusdt", "solusdt", "xrpusdt"]

[hyperliquid]
coins = ["BTC", "ETH", "SOL", "XRP", "HYPE"]   # HYPE: bin15's underlying
# rolling = …   the paper universe's [hyperliquid] rolling line, verbatim (bin15.toml's `families` must equal it)
```

If the boot names another file it needs, copy that file by name.

**Every engine command runs in one zsh subshell in this environment.**
- The env files are sourced first, silenced. The overrides come after them, so the
  overrides win.
- The operator arm (slot 3) reads `HYPERLIQUID_*`, so E3's testnet pair is mapped
  onto those names.
- Every Hyperliquid host is pointed at testnet.
- Every path is moved under `$TN`.
- Every exported credential is dropped (anything named `*KEY*`, `*SECRET*`, `*SEED*`, `*TOKEN*` or `*HMAC*`) except E3's and slot 6's testnet keys. That drops the mainnet keys and the AI command plane's key.
- `--env-file /dev/null` stops either binary from reloading the repo `.env`.

```zsh
( unsetopt xtrace verbose 2>/dev/null; cd ~/trading-engine-multivenue
  set -a; { [ -f .env ] && . ./.env; . ~/multivenue/.env; } > /dev/null 2>&1; set +a
  for v in ${(k)parameters[(R)*export*]}; do
    case $v in HYPERLIQUID_TESTNET_*|HYPERLIQUID_XMM_TESTNET_*) ;; *KEY*|*SECRET*|*SEED*|*TOKEN*|*HMAC*) unset $v ;; esac
  done
  T=api.hyperliquid-testnet.xyz
  export HOME=$TN METRICS_BIND=127.0.0.1:9192 \
    HYPERLIQUID_WS_HOST=$T HYPERLIQUID_API_HOST=$T HYPERLIQUID_EXCHANGE_HOST=$T HYPERLIQUID_SOURCE=b \
    HYPERLIQUID_TESTNET_EXCHANGE_HOST=$T HYPERLIQUID_TESTNET_SOURCE=b \
    HYPERLIQUID_AGENT_KEY="$HYPERLIQUID_TESTNET_AGENT_KEY" HYPERLIQUID_MASTER_ADDR="$HYPERLIQUID_TESTNET_MASTER_ADDR" \
    MULTIVENUE_LOG_DIR=$TN/multivenue/logs AI_INGRESS_SOCK=$TN/multivenue/run/ai.sock \
    AI_RULESET_DIR=$TN/multivenue/ai-rulesets
  unset HYPERLIQUID_XMM_MASTER_ADDR HYPERLIQUID_HYPARB_MASTER_ADDR   # the mainnet identities' addresses
  ulimit -S -n 8192
  E=./target/release/multivenue-engine
  $E xmm-preflight --env-file /dev/null --exec $TN/multivenue/exec.toml --arm-live 3,6 \
    --xmm $TN/multivenue/xmm.toml --strategy bin15+xmm \
  && $E run --env-file /dev/null --paper --strategy bin15+xmm --exec $TN/multivenue/exec.toml \
    --arm-live 3,6 --xmm $TN/multivenue/xmm.toml 2>&1 | tee -a $TN/engine.log )
```

The preflight must PASS on TESTNET with `own_account=true` before the run
starts. If the boot refuses, its line names what to fix — for example, a key
slot 3's testnet section must state or drop. Fix the instance's files, never
`~/multivenue`'s, then start again. To stop the instance, press Ctrl-C in its
terminal: SIGINT, the shutdown sweep.

## 3. The boot tells and the numbers (first 5 min)

In a second terminal:

```sh
TN=$HOME/multivenue-tn; XMM_MASTER=0x…   # slot 6's testnet master (an address, public)
grep -E 'exec: slot (3|6) |xmm: LIVE ARM ARMED|slot 6 LIVE on the Hyperliquid gateway' $TN/engine.log
curl -s 127.0.0.1:9192/metrics | grep -E '^engine_exec_hlgw_|^engine_xmm_|^engine_exec_slot(3|6)_'
I=https://api.hyperliquid-testnet.xyz/info
q() { curl -s -X POST $I -H 'Content-Type: application/json' -d "{\"type\":\"$1\",\"user\":\"$2\"}"; echo; }
q frontendOpenOrders $XMM_MASTER; q clearinghouseState $XMM_MASTER; q spotClearinghouseState $XMM_MASTER
q userRateLimit $XMM_MASTER; q userFills $XMM_MASTER
```

**The tells:**
- `exec: slot 3 LIVE name=bin15`
- `exec: slot 6 LIVE name=xmm … transport=gateway perp=$150 resting=$300`
- `xmm: LIVE ARM ARMED — the Hyperliquid gateway, TESTNET, slot 6 on its OWN TESTNET account (own_account=true; master <yours>, …)`, with the session loss bound of $50.000000 and `no anchor yet`
- `top-ups of 10000 requests ($5.000000), at most 100000 a UTC day ($50.000000)`
- `running strategy-set with slot 6 LIVE on the Hyperliquid gateway (TESTNET) beside the operator's arm`

**Each number against the venue, within one read period (30 s):**

| Metric | Should match |
|---|---|
| `engine_exec_hlgw_open_orders` | the venue's open orders for slot 6's master (slot 6 alone trades it) |
| `engine_exec_hlgw_worth_usd_1e6` | `accountValue` + spot USDC `total` |
| `engine_exec_hlgw_budget_remaining` | `nRequestsCap − nRequestsUsed` |
| `engine_exec_hlgw_ready`, `engine_exec_hlgw_ws_up`, `engine_exec_hlgw_own_account` | 1 |
| `engine_exec_hlgw_halt_reason`, `engine_exec_hlgw_leverage_dark` | 0 |
| `/state` `exec.arms.hyperliquid_gw.deadman.availability` | `eligible` |
| fills | the venue's `userFills` |

The member quotes:
- `engine_xmm_placed_total` rises;
- orders rest on the testnet book;
- `engine_xmm_rejected_other_total` stays at about 0.

## 4. Top-ups (an own account) — before the halts

Run this before §5: a halted slot spends no address budget and buys nothing more.
The arm buys when its address budget falls toward the floor
(`request_budget_floor` 2 000). At the member's cadence the 10 000 buffer lasts
about an hour. Within the run:

- `engine_exec_hlgw_topup_ok_total` reaches 1 or more;
- `engine_exec_hlgw_topup_day_used` reads 10000;
- `$TN/multivenue/xmm-topup-testnet.state` holds today's line;
- the venue's `nRequestsCap` rises by about 10 000;
- `engine_exec_hlgw_topup_failed_total` and `engine_exec_hlgw_topup_unsent_total`
  stay at 0.

If no purchase comes within 90 min, record it as not reached. The loopback tests
cover the path.

## 5. The per-slot halt (XMM XH5a), and the session across a restart

1. With slot 6's quotes resting, and ideally slot 3's:
   `printf '\nslot=6 reason=operator\n' >> $TN/multivenue/exec.HALT`.
2. Within a few seconds:
   - `engine_exec_slot6_halted` is non-zero — 6, the operator's halt (the gauge holds the halt reason's code);
   - `engine_exec_hlgw_open_orders` is 0;
   - the venue shows none of slot 6's orders;
   - slot 3 is untouched: `engine_exec_slot3_halted` is 0 and its orders at the
     venue are unchanged.
3. Stop the instance (Ctrl-C) and flatten slot 6's testnet position by hand — a position held at boot holds the arm until flat (H5). Archive the halt line:
   `mv $TN/multivenue/exec.HALT $TN/multivenue/logs/exec.HALT.6`.
   Start the instance again (§2). Its slot-6 tell must now say `anchor RESTORED`:
   the same session, restored (XMM XH5b-2).
4. Repeat with `slot=3` (the same `printf`): slot 3's orders cancel, and slot 6's stay and keep
   quoting.

If slot 3 rests nothing on testnet (its member places nothing there), judge its
half on the counters instead: halting 3 leaves `engine_exec_slot6_halted` at 0
and slot 6's open orders unchanged. Record that slot 3 had nothing resting. The
XH5a composition loopback test is the authority for the other direction.

## 6. The A/B arms (optional on testnet)

The probe's first mainnet run is `ab_mode` 0. To look at modes 1 and 3:

1. Set `ab_mode` in the instance's `xmm.toml`.
2. Stop the instance, flatten slot 6 by hand, rerun the preflight (a new hash), then start it again.
3. Read `ab_arms=` in the member tell, and the arm tags in `engine-orders.pmlr`.

## 7. The ALO priority proof (before any `ab_mode` 2 on mainnet)

1. Stop the instance and flatten slot 6 by hand (before every restart). In its `xmm.toml`, set `ab_mode = 2` and
   `alo_priority_1e8 = 10000` (1 bp).
2. Rerun the preflight: `ab_arms=alo_priority:B=…,cap_usd_1e6=25000000`.
3. **With HYPE undelegated** (§1), run for 20 min, then read:
   - `engine_xmm_priority_places_total` > 0;
   - each priority place RESTING at the venue;
   - the testnet HYPE balance falls by the fees;
   - `engine_xmm_priority_fee_usd_1e6_total` ≈ that fall.
4. Stop. Delegate the HYPE or move it, so none is undelegated. Run again until
   three priority places have been answered.
5. **Record the venue's answer word for word** (the log's REJECTED reason).
   - If it was accepted, the fee was charged somewhere else; record where.
   - If it was refused, the latch trips: `engine_xmm_priority_places_total` stays
     frozen, `engine_xmm_priority_refused_total` ≥ 3, and
     `engine_xmm_priority_fee_usd_1e6_total` falls short of 25 000 000.
6. Set `ab_mode` back to 0.

## 8. Stop and clean up

1. Press Ctrl-C on the instance. Its shutdown sweep cancels each live slot's
   orders.
2. `$TN/multivenue/xmm-gateway-shutdown.tell` should be absent. If it is present,
   read it: it names an unconfirmed sweep.
3. Both testnet accounts show none of our orders. Flatten any position by hand.
4. End the session:
   `mv $TN/multivenue/xmm-pnl-anchor-testnet.state $TN/multivenue/logs/`.
5. `find $TN -name '*env*'` comes back empty: the instance holds no credential files.
6. The paper engine still answers (`curl -sf 127.0.0.1:9191/healthz`), and its PID
   in `launchctl list | grep com.multivenue.engine` is unchanged.

## 9. Pass criteria (all must hold)

1. The preflight passed on TESTNET with `own_account=true`. The boot's tells read
   as in §3.
2. Every number in §3 agreed with the venue within one read period.
   `dm_availability` was `eligible`.
3. Halting 6 left slot 3's orders resting, and halting 3 left slot 6's, or §5's
   fallback is recorded. The restart's tell said `anchor RESTORED`.
4. A top-up was bought and confirmed, or recorded as not reached within 90 min.
5. With HYPE undelegated, the priority grouping was accepted (RESTING), and the
   no-HYPE answer is recorded (§7). This is required before `ab_mode` 2 on
   mainnet. It is not required for the first run at `ab_mode` 0.
6. The stop left nothing resting. The instance held no credential files and no
   key but E3's and slot 6's testnet ones, and the paper engine never restarted
   into a backed-off wrapper.
