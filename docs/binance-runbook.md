# Binance execution — the operator's runbook (BX11)

Anton runs this. It covers the Binance arm's keys, the Demo tools, the
account tools, the fee measurement, the halts, deploying after a merge,
the arming preconditions and spot's own rules. The laws are in
`docs/risk-policy.md`, "BX — the Binance execution laws (BX-1 …
BX-20)"; the design is `docs/binance-exec-plan.md`.

**Status (BX8, 2026-10-10).** The arm boots USDⓈ-M and COIN-M futures
under the classic, Portfolio Margin (`pm`) and PM Pro (`pm_pro`) account
modes, and spot — bStocks included — under classic alone: makers on
USDⓈ-M classic only, IoC everywhere else. Options and Binance Stocks
refuse the boot (BX9–BX10). **No member arms (O-BX6)** before BX13. Spot's
own rules are §10.

Every command below runs from the checkout whose release binary you
vetted (`cargo build --release -p cli`), through `scripts/exec-smoke.sh`:
the script exports the keys from your `.env` (`MULTIVENUE_ENV_FILE`, else
`~/multivenue/.env`), names every exit code in one line, and refuses a
missing or too-old binary.

## 1. Keys and `.env`

| variables | read by | what |
|---|---|---|
| `BINANCE_API_KEY`, `BINANCE_ED25519_SEED` | the engine (a live Binance slot); the account tools on `--bn-network mainnet` | the live key: an Ed25519 key (the app's API management, "self-generated": you upload the public key), its 32-byte seed as 64 hex digits |
| `BINANCE_HMAC_API_KEY`, `BINANCE_HMAC_SECRET_KEY` | the engine under `signing = "hmac"` (REST only); the account tools under `--bn-ops-hmac` | the live HMAC pair |
| `BINANCE_DEMO_API_KEY`, `BINANCE_DEMO_ED25519_SEED`, `BINANCE_DEMO_HMAC_*` | the Demo tools only (probe, battery, drill, account tools on Demo) | Demo's keys (demo.binance.com) |

- The live key **trades what is armed and cannot withdraw** — futures
  trading when a futures product is armed, spot trading
  (`enableSpotAndMarginTrading`) when spot is: the boot asserts each and
  refuses otherwise. Spot needs the Ed25519 key (its fills arrive only on
  the WS API). Restrict the key to the engine host's IP.
- `.env` is mode 600. A file in the repo names the variables, never a
  value; no session reads `.env`.
- Disabling the key in the app stops everything the arm sends, whatever
  the engine does (kill switch 4, plan §3.11). Today the arm reads the
  refusals that follow as rejects: the slot halts `reject-streak` or
  `recon-stale`, not `venue-lock` (risk-policy BX-19).

## 2. The Demo tools

Demo orders are real orders on Demo's books; nothing reaches mainnet
(the binary refuses any other host).

- **The probe:** `scripts/exec-smoke.sh --binance-probe` (or
  `--binance-probe futures`) — the K-probes and the arming preconditions
  on Demo. 0 or 59.
- **The spot probe:** `scripts/exec-smoke.sh --binance-probe spot` — K1's
  Demo half: the clock on Demo's spot REST host (`demo-api.binance.com`),
  then `session.logon` (Ed25519) and `userDataStream.subscribe` on one
  socket to Demo's spot WS API (`demo-ws-api.binance.com`). It prints the
  logon's result keys (never a value) and the subscription's id, and
  places nothing. 0, or 59 (a logon or a subscription refused, or a
  probe that could not run).
- **The battery:** `scripts/exec-smoke.sh --binance um --lifecycle` (or
  `cm`, `spot`, or several, comma-separated: `um,cm`, `um,spot`,
  `um,cm,spot`; `--bn-rest` for the REST route and `--bn-hmac` for the
  HMAC key, futures only) — makers, the post-only refusal, the TTL, the
  sweep, the reconciliations and fills, then a flat read. 0, or 50–58
  naming the phase; with spot, 66 too (below).
- **The spot battery:** `scripts/exec-smoke.sh --binance spot --lifecycle`
  trades `--bn-spot-symbol` (default `BTCUSDT`; a USDT-quoted pair), bound
  as the engine binds its spot[0]: under the anchor's id 7, routed by its
  alias.
  Before the boot it reads the account — `canTrade`, the permissions and
  the row's verdict (printed: the boot decides), the base balance — and
  wants no order resting anywhere (else 50: clear it in the app). The
  boot adopts the held base balance within `--bn-adopt-usd` (default
  250 000 USD, at most 10 000 000; 0 adopts nothing, and a held base
  balance then refuses the boot: 51, naming the flag). Then
  `--bn-cycles` cycles, each an IoC BUY of `--bn-usd` across the ask and
  an IoC SELL of it, less the base-asset commission, across the bid —
  each order proven routed through the alias, each cycle by an agreeing
  reconciliation; the bar is 4 fills and 2 agreeing reconciliations. The
  end sells what the cycles bought and did not sell back — never the
  adopted balance — and reads the base balance within a step a BUY of the
  start's (58 if not). Spot rides the WS API with the Ed25519 Demo key:
  `--bn-rest`, `--bn-hmac` or `--bn-drill` beside spot is refused (1,
  nothing sent). A bStock (`--bn-spot-symbol TSLABUSDT`) puts the
  permission check to the Demo account: **66** if the account lacks the
  row's group — the check's verdict, nothing placed.
- **The gap drill:** `scripts/exec-smoke.sh --binance um --lifecycle
  --bn-drill gap` — two makers rest behind the touch under the dead-man;
  the gateway stops without its sweep; the venue's countdown must cancel
  both within 40 s (30 s and a 10 s margin), each one's status reading
  CANCELED, and no sooner than 10 s (the countdown less two heartbeats). 0 =
  the dead-man fired; 65 = it was not seen to (none in time — the run
  then cancels them by id — cancelled too soon, or a maker filled); 52 =
  the drill failed before the gateway was stopped (the dead-man
  untested).
- **The flatten drill:** `scripts/exec-smoke.sh --binance um --lifecycle
  --bn-drill flatten` (or `cm`) — one IoC opens a position, the gateway
  stops, and the flatten (§3.2) must close it: 0, or the flatten's own
  code (60–63); before the gateway stops, 57 (the opening IoC), 56 (its
  reconciliation) or 52 (the gateway did not stop).
- **Once a drill has stopped the gateway** (and no signal came), the end
  runs the flatten (§3.2) on Demo over each lane's symbol, the arm's
  orders cancelled by id first, before its flat read: 58 only if even that
  left something.
- **58, whatever the tool:** something was left on the Demo account, or
  the end could not read it. Read the account in the app and clear it
  before the next run.

## 3. The account tools

The engine asserts the account's settings at boot and never sets them
(BX-19); these tools set them and close positions, by your hand only.
**Run them with the engine stopped:** a leverage change on an owned row
while the engine runs locks the slot (`venue-lock`), and the flatten
refuses while an order of the arm's rests (on COIN-M the arm rests none,
so it cannot tell — it warns). **Classic accounts only:** on a Portfolio
Margin or PM Pro account, set and close in the app.

A change that gets no answer (none, or the venue's own "unknown": a 5xx,
`-1007`) has an unknown outcome:
the tool reads the account afterwards and judges what holds; it never
resends blind.

Each prints JSON lines (`{"ops": …}`): every read, every change it would
send, every answer, and an `END` line with its exit code.

### 3.1 Setup — `--binance-setup <um|cm>`

```
scripts/exec-smoke.sh --binance-setup um --bn-symbol BTCUSDT --bn-symbol ETHUSDT
scripts/exec-smoke.sh --binance-setup cm --bn-symbol BTCUSD_PERP
```

- It reads the position mode, the multi-assets mode (USDⓈ-M), and each
  `--bn-symbol`'s margin type and leverage; it changes only what differs
  from what the boot asserts: one-way, single-asset, CROSSED, a leverage
  within `--bn-leverage-cap` (default 20 — set it to `exec.toml`'s
  `max_leverage`). `--bn-leverage L` sets L on each symbol (L ≤ the cap);
  without it a leverage within the cap is left alone.
- COIN-M's settings are per symbol: `--bn-symbol` is required there.
- `--bn-sign-tradfi` signs the TradFi Perps agreement (USDⓈ-M). No
  endpoint reads it back: an unsigned TradFi row answers `-4411` at its
  first order and stops trading (the others go on).
- `--bn-reassert` sends every change even where the read says it holds
  (each change's own "no need to change" answer counts as held).
- COIN-M in hedge mode: its symbols are read only once one-way holds, so a
  dry run shows the position-mode change alone — after a confirmed run,
  run it again without `--confirm` to read the symbols' changes.
- **Mainnet:** add `--bn-network mainnet`. Without `--confirm` it reads
  and prints every change and sends none (exit 64) — run that first and
  read it; then the same command with `--confirm`.
- Exit codes: 0 the settings read back as asserted; 60 a read failed; 61
  the venue refused a change (or a setting read back otherwise); 64
  mainnet without `--confirm`.

### 3.2 Flatten — `--binance-flatten <um|cm>` (O-BX9)

A halt cancels orders; it never closes positions. This does — on futures
only. A spot position is its base asset's balance, and no tool closes
it: with the arm stopped, sell the base asset in the app (§10).

```
scripts/exec-smoke.sh --binance-flatten um
scripts/exec-smoke.sh --binance-flatten um --bn-symbol BTCUSDT
scripts/exec-smoke.sh --binance-flatten um --bn-network mainnet --confirm
```

- A `--bn-symbol` the venue does not list is refused (exit 1, nothing
  sent); a listed one with no position says so and passes.
- Every non-zero position (or `--bn-symbol`'s) gets a **reduce-only IoC**
  of the position (whole steps, at most the row's LOT_SIZE maximum — the
  rest on a later try) through the touch — `--bn-close-bps`
  (default 20) the first try, doubling, at most 300 bps and inside the
  row's price band — then the position is read again; up to three tries a
  position. The verdict is a fresh read of the product's positions (or the
  named one's).
- On mainnet without `--confirm` it prints each cancel and each close's
  first try (side, quantity, price) — a position it cannot price as a
  `would_skip` line — and sends nothing (64).
- It refuses (62, nothing sent) while an order of the arm's rests: stop
  the engine first, or add `--bn-cancel-first` to cancel them by id
  before closing — every order of the arm's on the product, whatever
  `--bn-symbol` names.
- A close with no answer, the venue's "unknown" (a 5xx, `-1007`), or a
  refusal for nothing to reduce (`-2022`) is judged by the position read
  after it. A position that cannot be priced or closed is reported (an
  `ops` `skip` line) and the others still close.
- 63: the final read lists a position still open — close it in the app.
- It does **not** clear a halt. After it: investigate, delete `exec.HALT`
  (§5), restart; the boot finds the account flat.

## 4. The fees — `bn-fees` (BX-14)

```
target/release/multivenue-engine bn-fees <log-root or run-dir>…
```

- Read-only: it reads `binance-exec.pmlr` in each run directory (or in
  every `run-*` under a root) and prints JSON lines: per class (product ×
  perpetual, dated or TradFi × maker or taker) the trades, the notional,
  the commission, the rate in bps and whether the class has reached N
  (30; `--n` to change it).
- Once a USDⓈ-M kind's maker AND taker classes both reach N, it prints the
  `fees.toml` row they support (`<kind> = "<maker>:<taker>"`, whole bps
  rounded up). **It never writes `fees.toml`:** you copy the row in.
- COIN-M's classes are shown; they share the `bn` rows.
- Left out and counted apart: the venue's own trades (settlements,
  liquidations, ADL), a commission in another asset than the row's
  settlement asset (BNB above all) or naming none, a trade of no
  notional. A foreign trade carries no fee record at all. **Keep BNB burn off** in the app's fee settings: a commission
  paid in BNB measures nothing.
- Journals written before BX11 carry no fee records.

## 5. Halts

A halt is per slot and sticky: the router cancels the slot's orders and
sweeps them, refuses new ones (a cancel still passes), and writes
`exec.HALT` beside `exec.toml`, so the next boot starts the slot halted.
**To clear one: investigate the recorded reason, close what must be
closed (§3.2), delete `exec.HALT`, restart.** `--halt-slot N` boots a
slot already halted (your switch).

| reason (`/state`, `exec.HALT`) | on a Binance slot | what to do |
|---|---|---|
| `ws-gap` | the order route or the user stream down past `halt_on_ws_gap_ms` | the venue's dead-man cancels the makers within `countdown_ms`; find why the link died |
| `recon-stale` | no reconciliation within `halt_on_recon_stale_ms` | the REST reads failed or were starved: the log names them |
| `recon-drift` | the book and the venue disagree past the threshold — or a frame could not be scanned, or an output was lost (unbounded drift); on spot, an armed base asset moved outside the arm (§10) | read the account against the journal; a scan failure holds until a restart |
| `reject-streak` | rejects in a row (a filter, a revoked key's `-2015`) | the log names the codes |
| `budget-floor` | a 429, `-1003`, `-1015`: the arm sent nothing for at least 60 s | what else spends the IP's weight (`/state` `weight_used`) |
| `venue-lock` | a 418 or `-4400`…`-4402`; the account changed under the arm (an owned row's leverage, multi-assets on, a row isolated); PM's risk level or account status; a spot row's permission group gone, or the spot account unable to trade (`canTrade` false) — 9 015 | read the account; `--binance-setup` restores the settings (classic futures; PM and PM Pro in the app); spot's permissions are the account's, in the app; the restart asserts them again |
| `margin-risk` | the margin ratio at or above `halt_on_margin_ratio_1e6`, or a margin call | add margin, or `--binance-flatten` (classic; PM and PM Pro in the app) |
| `gateway-halt` | the gateway's own halt | the log names it |
| `pnl-gain`, `pnl-loss`, `asset-refusals`, `operator` | as on every venue: the session bound (E7, when set), the refusal streak, your `--halt-slot` | risk-policy "E6", "E7" |

- **The sweep** (`/state` `exec.arms.binance_gw.sweep`): `clear` — the
  venue confirmed nothing of the slot's rests (on spot nothing of ours
  ever rests: the listing confirms it); `working`; `stranded` —
  it could not confirm. Stranded: read the open orders in the app and
  cancel what is left (`--binance-flatten … --bn-cancel-first` cancels
  the arm's orders by id, and then closes positions).
- **What to watch:** `/state` `exec.arms.binance_gw` (the arm's levels and
  tallies), `/metrics` `engine_exec_bn_*`, the TUI's `bn` chip. Non-zero
  `in_doubt`, `anchor_unsaved`, `journal_dropped`, `jobs_dropped`,
  `fills_deferred` or `events_other` deserve a look.

## 6. A boot that does not arm

- **A BX-19 assertion** (`BootErr::Assert`, the log names it): hedge
  mode, multi-assets on, a row isolated or past `max_leverage` —
  `--binance-setup`, then boot again; the key's permissions or the
  account mode — the app (and `exec.toml`'s `account_mode`).
- **`UNRECONCILED: n unbound position(s)`:** the account holds a position
  on a symbol this boot did not bind; the slot refuses every order until
  it is flat: `--binance-flatten … --bn-symbol <it>` (engine stopped); a
  spot balance is sold in the app.
- **Adoption past the cap:** positions on owned rows larger than the
  slot's `cap_instance_usd_1e6` refuse the boot: reduce them. On spot the
  held base balances count, valued at the ticker (§10).
- **Spot's own refusals** (BX8): §10.
- **A boot that never reconciles** (it ends `Timeout` in its reconciliation
  phase, orphans counted): an order of an earlier boot's (`mv…`) rests on
  a symbol this boot did not bind, and the gateway cancels only on bound
  rows (risk-policy BX-9). Cancel it in the app, then boot again.
- **The E7 anchor refuses:** `binance-pnl-anchor.state` beside `exec.toml`
  is another key's or another account mode's session. Deleting it ends
  that session (the session bound starts again from the next boot).
- **The self-test or the vectors fail:** the binary is not fit to sign;
  do not run it.

## 7. Deploying after a merge

Deploying is yours, after you land `binance` on `main`:

1. `cargo build --release -p cli` in the main checkout.
2. The existing gates (`scripts/exec-smoke.sh --offline`, and the
   pre-restart gate as today).
3. On that binary, the Binance Demo evidence: `--binance-probe`, the
   battery (`--binance um,cm --lifecycle`) and the gap drill — and, when
   spot is to be armed, `--binance-probe spot` and the spot battery
   (`--binance spot --lifecycle`) — exit 0 each.
4. Restart the engine the usual way (`docs/local-setup.md`, "Always-on
   standing engine"). The Binance arm stays dark unless `exec.toml` arms
   a Binance slot (`--arm-live`).

## 8. Arming (BX13) — the preconditions

None of these is met by the code alone; each is yours.

- **The ruling:** O-BX6 lifted for the slot (BX13).
- **The gates, on the tree you arm:** gates 94, 94b and 108 and the
  loopback set pass — `make alloc-assert` (the Mac, release: 121/121) and
  `cargo nextest run -p exec-binance` (`bn_e2e_loopback`, 64 tests).
  Gates 94 and 94b pin USDⓈ-M classic's allocations, gate 108 spot's;
  the loopback set also exercises COIN-M, Portfolio Margin, PM Pro and
  spot (not the futures' margin calls beyond USDⓈ-M's); none is a WAN
  (risk-policy "BX12", "BX8").
- **The account:** dedicated to the engine (a `shared` scope refuses the
  boot); `--binance-setup … --bn-network mainnet` read, then applied with
  `--confirm`; the TradFi agreement signed if a TradFi row is owned; BNB
  burn off for futures (spot's: as set, kept fixed while armed, §10).
- **The key:** Ed25519 (or the HMAC pair on the REST route, futures
  only), futures trading on where futures are armed and spot trading
  where spot is, withdrawals off, IP-restricted.
- **Spot** (BX8), when armed: the spot Demo exit seen on the binary
  (`--binance-probe spot`, `--binance spot --lifecycle`; risk-policy
  "BX8"); gate 108 on the tree; spot instruments quoted in USDT, one per
  base asset; no session bound on the slot; §10's rules kept. And the
  code still owed (risk-policy "BX8", the risk review's R2-2 and R2-4):
  spot's day count seeded at boot from the WS API's
  `account.rateLimits.orders` and a reserve below 200 000 for a spot SELL;
  spot's request weight metered in a window of its own, or an unread day
  retried at most once a minute. And O-BX22 ruled, or a spot price feed:
  no spot mark reaches the router's ledger, so the slot's
  `cap_instance_usd_1e6` prices its other spot holdings at their last
  fill and an adopted one at the boot's ticker.
- **`exec.toml`:** the slot names `binance`, `mode = "live"`, every halt
  key non-zero (`halt_on_margin_ratio_1e6` ≤ 799 999), `max_symbols`,
  `min_maker_ttl_ms` ≥ 5 000; the `[exec.binance]` block
  (`exec.toml.example` lists the variants this binary boots).
- **The worker:** its PMLR reader reads the adopted and settlement fill
  flags before any surface folds Binance fills (risk-policy "BX7").
- **The members:** every member trading the slot reads
  `FILL_FLAG_ADOPTED` as opening inventory, never a trade (risk-policy
  "BX7").
- **The binary:** the Demo evidence of §7 on the very binary you arm.
- **The boot:** `--arm-live <slot>`; the boot tells
  `binance: LIVE ARM ARMED — …` (or names what refused it). The first
  mainnet fill seen on the user stream is BX13's first bar.

## 9. Exit codes (`scripts/exec-smoke.sh`)

| codes | tool |
|---|---|
| 50–57 | the battery's phases (account, boot, cycle, post-only, TTL, sweep, reconciliation, fills) |
| 58 | something left on the Demo account, or an end that could not read it |
| 59 | the probe |
| 60–64 | the account tools: a read, a refusal, the arm's orders resting, a position still open, mainnet without `--confirm` |
| 65 | the gap drill: the dead-man was not seen to cancel the makers |
| 66 | the spot battery: the permission check refused the boot — the account lacks a group the spot row needs (the bStock check); nothing placed |

Every non-zero code means: read the report above it before anything
else.

## 10. Spot (BX8)

Spot trades IoC-only on a classic account, on the WS API with the
Ed25519 key: its fills arrive only on the order session. Each spot
instrument is quoted in USDT and owns its base asset — one instrument
per base asset, and no armed base another's quote. The reconciliation
compares each armed base asset's balance (`free + locked`, to the
venue's 8 decimals) with what the arm booked, net of every commission
charged in it; the quote (USDT) and every unarmed asset are yours, never
compared.

- **Never move an armed base asset while the arm runs** — no deposit,
  withdrawal, convert, airdrop claim or dust conversion of it, and no
  trade of it by hand. The arm did not book it, so from the second cycle
  that sees it it is drift: past the slot's `halt_on_recon_drift_usd_1e6`
  the slot halts `recon-drift`; under it, `recon-stale` once no cycle has
  agreed for `halt_on_recon_stale_ms`. A deposit or airdrop onto an
  instrument whose explained balance is 0 is an unseen leg instead: the
  slot goes unreconciled and later halts `recon-stale`, not
  `recon-drift`. Stop the engine first; the next boot adopts what is held
  (below).
- **Turn off what moves balances on its own**, for every armed base
  asset: Simple Earn's auto-subscribe, Auto-Invest and Convert. Each moves
  the balance outside the arm: drift.
- **The day's unfilled-order count** (the venue's ORDERS pool: 200 000 a
  UTC day, an order that fills taking one off). The arm mirrors it at
  `orders_frac` and refuses past its share — 160 000 at 80 % — every spot
  order, sells included, until 00:00Z: the exit is then by hand, in the
  app. The count is not in `/state`; an IoC member placing 80 orders every
  10 s, most of them unfilled, reaches the share in about 6 h. The mirror
  starts at 0 at every boot (the venue's own count is not read): after a
  restart the venue may refuse first, with `-1015`, which the gateway
  reads as the budget — the whole gateway goes quiet, futures included
  (no heartbeat: the venue's dead-man cancels the futures' makers), and
  the slot halts `budget-floor`. Both are carried to BX13 (§8).
- **Keep the BNB-burn setting fixed while armed.** It decides the asset
  each commission is charged in: the base asset on a BUY with it off, BNB
  with it on. The reconciliation follows each trade's own asset either
  way; what changes with it is the gap between the router's ledger, which
  books the gross quantity, and the balance (with the burn off a member's
  SELL of the ledger's whole holding is refused `-2010`), and what
  `bn-fees` can measure (§4).
- **Adoption.** A base balance held on an owned spot instrument at boot
  is adopted as the slot's starting inventory, valued at the venue's
  ticker, within the slot's `cap_instance_usd_1e6` (a slot naming another
  venue too adopts nothing: its cap is 0) — on Demo, within
  `--bn-adopt-usd`. Past the cap, or with no price, the boot is refused
  (`Inventory`). **With `--bn-adopt-usd 0`, or an adoption cap of 0, any
  held base balance of a millionth or more refuses the boot:** sell or
  convert it first (engine stopped), or raise the cap. Less than a
  millionth is dust: never adopted, never drift.
- **A stray order refuses the boot.** No spot cancel exists: an order of
  ours (`mv…`, of this boot's epoch or an earlier one) resting on a spot
  instrument the boot binds refuses it (`SpotStray`, naming the row) —
  cancel it in the app, then boot again. On a spot symbol the boot does
  not bind, the boot holds in its reconciliation to its timeout instead
  (§6). A stranger's resting order is reported every cycle, never
  cancelled.
- **No flatten tool** (§3.2): sell the base asset in the app, the engine
  stopped.
- **In session**, a permission group a spot row needs gone, or the spot
  account unable to trade, is `venue-lock` (9 015, raised once).

The boot's spot refusals (the log names each):

| refusal | means | what to do |
|---|---|---|
| "quoted in …" | a spot instrument not quoted in USDT | drop it from universe.toml's `[binance] spot` (the arm binds every symbol listed there; its market data goes with it) |
| "arms spot with order_transport = …" (the gateway's `SpotRoute`) | `order_transport = "rest"` or `signing = "hmac"` with spot | the WS API and the Ed25519 key |
| `SpotAsset(row)` | a base asset armed twice, or another armed instrument's quote | one instrument per base asset |
| "a ruling (spot beside Portfolio Margin or PM Pro)" | spot under `pm` or `pm_pro` | spot is classic-only |
| a session bound | `halt_on_gain_usd_1e6` or `halt_on_loss_usd_1e6` on a slot arming spot | drop the bound: spot has no USD equity it reads |
| `Assert(NoTradePermission)` | the key does not trade spot | the key's settings, in the app |
| `Assert(SymbolNotPermitted)` | a spot row's permission groups are not the account's (the bStock check) | the account's permissions, or drop the instrument |
| `Assert(CannotTrade)` | the spot account answers `canTrade: false` | the account, in the app |
| `SpotStray(row)` | an order of ours rests on spot | cancel it in the app |
| `Inventory(…)` | a held balance past the adoption cap, or one with no price | sell it, or raise the cap |
| "the market-data hosts … are not on the order network" | with spot armed, `BINANCE_WS_HOST` or `BINANCE_REST_HOST` names a host outside `*.binance.com` (e.g. `data-api.binance.vision`) or carries a port: either reads as no network (a mirror inside `*.binance.com` is mainnet) | name `*.binance.com` hosts, without a port |
