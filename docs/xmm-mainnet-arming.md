# XMM — the mainnet probe's arming checklist (XH5e)

**Only Anton arms.** Work the boxes in order; any "no" stops the arming.

**What the probe is.** Slot 6 on the Hyperliquid gateway, on an account of its
own: SOL, XRP, BTC and ETH, $15 clips, with the artifacts `xmm-probe.toml` and
`exec-slot6-probe.toml` (XMM XH5e-1). `scripts/xmm-flip.sh` (XMM XH5e-2)
installs them and restarts nothing.

**Open questions.** This checklist runs under the defaults below:

- **Q-XH5-3: no third switch.**
  - The mainnet interlock has four parts: the artifact's slot 6 live,
    `--arm-live` naming 6, slot 6's own mainnet variables, and the venue's
    mainnet hosts.
  - Putting `HYPERLIQUID_XMM_*` into `.env` **is** the mainnet switch. Keep those
    two variables out of `.env` except during an armed session (§B7, §F22).
- **Q-XH5-4: the top-up cap.**
  - The cap is $50 a UTC day (100 000 requests, O-XH9).
  - The measured need is about $104–117 a day at the paper cadence. The probe's
    `lead_stale_ms` 500 lowers it.
  - When the day's cap is spent and the address budget reaches its floor
    (2 000), the slot halts itself (fail-closed). That is a stop, not a fault
    (§E).

**Known limits** (read before choosing the hour):

- **The drains.** The five scheduled drains (00:10, 08:33, 16:05, 20:15,
  21:15Z) restart the engine.
  - If slot 6 holds a position at a drain, the boot holds its arm until flat (H5).
    It quotes nothing, and once its reads go stale for 180 s it halts.
  - So each drain needs a hand flatten (§F21), or the session ends before the
    drain.
  - Arm just after a drain's quiet window for the longest run: 00:35–08:15,
    08:50–15:45, or 16:25–19:55Z.
- **`dm_availability`.** XH5b-3 asks for `dm_availability` before the first
  quote. The checklist can only read it within the first read period after the
  boot, so the member's first quotes may go out before it is read.
  - If it is not `eligible`, halt at once (§C12).
  - This is a recorded deviation (A-S7-43).

## A. Before the day

1. **The merge.** Anton has said the word and `xmm` is merged into `main`. The
   engine's launchd job runs the checkout named in
   `~/Library/LaunchAgents/com.multivenue.engine.plist` (its `WorkingDirectory`,
   normally `~/trading-engine-multivenue`).
   - Build that checkout after its HEAD: `cargo build --release -p cli`.
   - Then check that `target/release/multivenue-engine xmm-preflight --help`
     answers.
2. **The testnet runbook passed** on this code (`docs/xmm-testnet-runbook.md`
   §9). Its §7, the priority proof, comes before any `ab_mode` 2.
3. **The account:**
   - A master of slot 6's own, never used by slot 0, slot 3 or HYPARB.
   - Its API wallet, approved on that master, with the approval's expiry
     (`extraAgents` → `validUntil`) after the session's end.
   - **Funded in full before the first boot**: $1 000 USDC. Spot USDC margins the
     perps (XH4g), and the first positive worth anchors the session, so funding
     in steps would anchor at the first step.
   - No HYPE is needed at `ab_mode` 0. About $50 undelegated is needed only for
     `ab_mode` 2, whose priority spend is capped at $25 a boot.
4. **The venue's reads.** These are public `/info` calls; the addresses are typed
   by hand and no key is used:

   ```sh
   MASTER=0x…; AGENT=0x…; I=https://api.hyperliquid.xyz/info
   q() { curl -s -X POST $I -H 'Content-Type: application/json' -d "{\"type\":\"$1\",\"user\":\"$2\"}"; echo; }
   q clearinghouseState $MASTER; q spotClearinghouseState $MASTER; q frontendOpenOrders $MASTER
   q userRateLimit $MASTER; q extraAgents $MASTER; q userRole $AGENT
   ```

   Every answer must hold:
   - `spotClearinghouseState`: USDC `total` ≈ the funding;
   - `frontendOpenOrders`: `[]`;
   - `userRateLimit`: `nRequestsCap − nRequestsUsed` ≥ 2 000;
   - `extraAgents`: lists `AGENT`, with `validUntil` ahead;
   - `userRole`: names `AGENT` as an agent of `MASTER`.
5. **The pre-arm margin check (XMM XH5b-1).** The P&L sums the perp
   `accountValue` and the spot USDC `total`. A dollar of margin counted in both
   would blind the −$50 bound by its size, and a flat account cannot show that.
   - By hand, in the venue's UI on this master, open one small position (one
     $15 clip of SOL).
   - Read `clearinghouseState` and `spotClearinghouseState`.
   - Compute `residual = (accountValue + spot USDC total) − (funded − fees + the
     position's unrealised P&L)`.
   - Log `marginSummary.totalMarginUsed` against the spot USDC `hold`.
   - **If |residual| ≥ half of `totalMarginUsed`, abort the arming.**
   - Otherwise close the position. `clearinghouseState` must then show no
     `assetPositions`: flat.
6. **No transfers while a session stands.** Make no deposit, withdrawal or
   spot↔perp class transfer on this master while a session's anchor stands
   (XH5b-1). Each one misreads the worth by its amount.

## B. Arming

Arm outside 00:00–00:30, 08:25–08:45 and 16:00–16:20Z, at least 15 min before a
scheduled drain, and while daily-restart owes no gated restart. The flip refuses
otherwise.

7. **`.env`.** For this session only, add these to the `.env` of the checkout the
   engine runs (the one the wrapper sources):

   ```
   HYPERLIQUID_XMM_AGENT_KEY=0x…     # the API wallet's key
   HYPERLIQUID_XMM_MASTER_ADDR=0x…   # the master
   ```

   Also add `HYPERLIQUID_EXCHANGE_HOST=api.hyperliquid.xyz` and
   `HYPERLIQUID_SOURCE=a` if they are absent. `HYPERLIQUID_WS_HOST` must be unset
   or `api.hyperliquid.xyz`.
8. **`scripts/xmm-flip.sh status`** must say `slot 6: PAPER at the next restart`.
9. **`scripts/xmm-flip.sh live`.** Its PREFLIGHT sends nothing. Read its report:
   - `exec: slot 6 LIVE name=xmm … transport=gateway perp=$150 resting=$300`.
   - `exec: slot 6 HALTS …`, with `alo_reject_pct=20` and the session bound at
     −$50.
   - The member: `lead_stale_ms=500 … rtt_pull_ms=2000 … ab_mode=0 ab_arms=off`.
     Its `phase=XH3(paper)` is a stale label: the exec lines and the arm's line
     show LIVE.
   - The arm's head: `MAINNET`, `own_account=true`, master `MASTER`, API wallet
     `AGENT`, `session loss bound $50.000000`.
   - The anchor: `no anchor yet` on a session's first boot. On a restart within a
     session it says `anchor RESTORED`, and that anchor must be this session's.
   - `top-ups of 10000 requests ($5.000000), at most 100000 a UTC day ($50.000000)`,
     followed by `… bought today`.
   - `xmm-preflight: PASS — slot 6's offline boot checks pass for LIVE on MAINNET (4 perps: BTC,ETH,SOL,XRP) … Nothing was sent.`
   - Last, `LIVE at the next restart`.
10. **Restart, watching.** `pgrep -fl 'multivenue-engin[e] run'` must show
    exactly one engine. Then `pkill -TERM -f 'multivenue-engine run'`: SIGTERM
    drains the engine, and launchd's KeepAlive relaunches it. Never use
    `launchctl kickstart -k`: it sends SIGKILL (docs/local-setup.md).

## C. After the boot, within 2 min

On any miss, halt slot 6 at once with
`printf '\nslot=6 reason=operator\n' >> ~/multivenue/exec.HALT` (the leading newline
keeps a hand-edited last line from swallowing it), confirm
`engine_exec_slot6_halted` reads non-zero (6 is the operator's halt; another code
means the slot had halted itself first), then go to §F.

11. **The tells:**

    ```sh
    grep -hE 'xmm: LIVE ARM ARMED|exec: slot 6 |slot 6 LIVE on the Hyperliquid gateway' \
      ~/multivenue/logs/launchd/engine.{out,err}.log | tail -6
    ```

    Look for:
    - the `MAINNET` line, with `own_account=true` and this master and API wallet;
    - the anchor line, as in §B9;
    - `running strategy-set with slot 6 LIVE on the Hyperliquid gateway (MAINNET) beside …`.
12. **`dm_availability`** must read `eligible` within the first read period
    (30 s). `unknown` after that, `refused` or `exhausted` all mean halt:

    ```sh
    curl -s 127.0.0.1:9191/state | python3 -c 'import json,sys; print(json.load(sys.stdin)["exec"]["arms"]["hyperliquid_gw"]["deadman"]["availability"])'
    ```
13. **`/metrics`:**

    ```sh
    curl -s 127.0.0.1:9191/metrics | grep -E '^engine_exec_hlgw_(ready|ws_up|own_account|halt_reason|budget_known|budget_remaining|open_orders|resting_usd_1e6|worth_usd_1e6|session_pnl_usd_1e6|pnl_anchor_usd_1e6|anchor_kept|leverage_dark|topup_day_used) |^engine_exec_slot6_halted |^engine_xmm_(placed|rejected_alo|rejected_other|filled)_total '
    ```

    | Metric | Must read |
    |---|---|
    | `engine_exec_hlgw_ready` | 1 |
    | `engine_exec_hlgw_ws_up` | 1 |
    | `engine_exec_hlgw_own_account` | 1 |
    | `engine_exec_hlgw_halt_reason` | 0 |
    | `engine_exec_hlgw_budget_known` | 1 |
    | `engine_exec_hlgw_budget_remaining` | ≥ 2 000 |
    | `engine_exec_hlgw_open_orders` | ≤ 16 |
    | `engine_exec_hlgw_resting_usd_1e6` | ≤ 300 000 000 |
    | `engine_exec_hlgw_worth_usd_1e6` | about the funding |
    | `engine_exec_hlgw_pnl_anchor_usd_1e6` | set, after the first read |
    | `engine_exec_hlgw_anchor_kept` | 1 |
    | `engine_exec_hlgw_leverage_dark` | 0 (3× cross on the four perps) |
    | `engine_exec_slot6_halted` | 0 |
    | `engine_xmm_placed_total` | rising |
    | `engine_xmm_rejected_other_total` | about 0 |
14. **The venue.** `frontendOpenOrders` for `MASTER` must match
    `engine_exec_hlgw_open_orders`. Optionally, read the leverage per coin:
    `for c in BTC ETH SOL XRP; do curl -s -X POST $I -H 'Content-Type: application/json' -d "{\"type\":\"activeAssetData\",\"user\":\"$MASTER\",\"coin\":\"$c\"}"; echo; done`.

## D. Monitoring

Read these every 30 min for the first 2 h, then hourly:

15. **The account and the slot:**
    - `engine_exec_hlgw_session_pnl_usd_1e6`. The bound halts the slot at −$50 by
      itself.
    - `engine_exec_hlgw_alo_rejects_total`.
    - `engine_xmm_congestion_cancels_total`.
    - `engine_exec_hlgw_topup_day_used` (≤ 100 000), and
      `engine_exec_hlgw_budget_remaining`.
    - Open orders against the venue.
    - `engine_xmm_fills_total`.
    - In the first hour, also `engine_exec_hlgw_congestion`. The IP share stays
      400 (XH5c-3); report frequent congestion pulls together with the host's
      other Hyperliquid REST traffic.
    - No transfer has been made (§A6).
16. **The priority latch** (`ab_mode` 2 only). A tripped latch reads as all three
    of:
    - `engine_xmm_priority_places_total` frozen;
    - `engine_xmm_priority_refused_total` ≥ 3;
    - `engine_xmm_priority_fee_usd_1e6_total` short of 25 000 000 ($25).

## E. Stop rules

17. **The slot halts itself, and cancels its own orders, on any of these.** Then
    work §F.
    - the −$50 session bound;
    - reads stale for 180 s;
    - a WebSocket gap of 5 s;
    - 5 rejects in a row;
    - 3 asset refusals;
    - a $20 reconciliation drift;
    - ALO rejects at ≥ 20 % of the last 200;
    - the address budget at its floor once the day's top-ups are spent;
    - a drain with a position held (Known limits).
18. **Anton stops the probe** (§F) on any of:
    - a position or order on the account that is not ours;
    - `dm_availability` not `eligible`;
    - a venue incident;
    - a gauge that disagrees with the venue;
    - a markout far below the bar over the day's fills (§G).

## F. Rollback, or the session's end

19. Pull its quotes at once: `printf '\nslot=6 reason=operator\n' >> ~/multivenue/exec.HALT`,
    then confirm `engine_exec_slot6_halted` reads non-zero (6, the operator's halt,
    or the code of the halt the slot took itself).
    `scripts/xmm-flip.sh paper` also does this, and so does a slot that has
    already halted.
20. Run `scripts/xmm-flip.sh paper`; it must report `PAPER at the next restart`.
    Then restart as in §B10 (`pkill -TERM`, never `kickstart -k`).
21. **The venue.** `frontendOpenOrders` for `MASTER` must be `[]`.
    - If any order of ours rests, press Cancel-All for that master in the
      Hyperliquid UI and read again.
    - Flatten any position by hand. The engine holds until flat and never
      flattens (H5).
22. Remove `HYPERLIQUID_XMM_AGENT_KEY` and `HYPERLIQUID_XMM_MASTER_ADDR` from `.env`.
23. **End the session**, so the next one anchors afresh:
    `mv ~/multivenue/xmm-pnl-anchor-mainnet.state ~/multivenue/logs/xmm-pnl-anchor-mainnet.state.$(date -u +%Y%m%dT%H%M%SZ)`.
    Keep the file in place only to continue the same session after a restart.
    Slot 6's operator halt line may stay in `exec.HALT`: the next
    `xmm-flip.sh live` archives and drops it.

## G. The report

24. **Daily.** Run the vault's report tool on the day's run directory:
    `python3 docs/research/xmm/xh5d4-report-2026-10-04/xh5d_report.py ~/multivenue/logs/run-<id> --out …`.
    It gives ρ, Kaplan–Meier and the 5 s markout per perp × placement × arm.
    Compare against the bar: a 5 s markout ≥ −0.3 bps over ≥ 2 000 fills, in two
    separate weeks.
25. **After the first 2 000 mainnet posts.** Re-size `rtt_pull_ms` to
    max(1500, ⌈1.5 × p99⌉), taking p99 from
    `engine_exec_hlgw_post_rtt_p99_ns`. The new value is an `xmm-probe.toml` change
    under review, followed by a flip.
