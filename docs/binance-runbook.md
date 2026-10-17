# Binance execution — the operator's runbook (BX11; BX13)

Anton runs this. It covers the Binance arm's keys, the Demo tools, the
account tools, the fee measurement, the halts, deploying after a merge,
the arming preconditions, spot's and options' own rules, the Binance
Stocks tools and the R0 ramp on mainnet (§13). The laws are in
`docs/risk-policy.md`, "BX — the Binance execution laws (BX-1 …
BX-20)"; the design is `docs/binance-exec-plan.md`.

**Status (BX13, 2026-10-11).** The arm boots USDⓈ-M and COIN-M futures
under the classic, Portfolio Margin (`pm`) and PM Pro (`pm_pro`) account
modes, and spot — bStocks included — and options under classic alone:
makers on USDⓈ-M classic only, IoC everywhere else. Options arm alone or
beside USDⓈ-M only. Binance Stocks is built but the engine refuses it
(ruling S-3): only the operator's MAINNET battery drives it (§12).
**No member arms (O-BX6).** BX13 builds the R0 ramp — the battery on
MAINNET, yours to run (§13); a member's arming stays a ruling of its own.
Spot's own rules are §10, options' §11, Binance Stocks' §12.

Every command below runs from the checkout whose release binary you
vetted (`cargo build --release -p cli`), through `scripts/exec-smoke.sh`:
the script exports the keys from your `.env` (`MULTIVENUE_ENV_FILE`, else
`~/multivenue/.env`), names every exit code in one line, and refuses a
missing or too-old binary.

## 1. Keys and `.env`

| variables | read by | what |
|---|---|---|
| `BINANCE_API_KEY`, `BINANCE_ED25519_SEED` | the engine (a live Binance slot); the account tools on `--bn-network mainnet`; the Binance Stocks probe and battery (§12) | the live key: an Ed25519 key (the app's API management, "self-generated": you upload the public key), its 32-byte seed as 64 hex digits |
| `BINANCE_HMAC_API_KEY`, `BINANCE_HMAC_SECRET_KEY` | the engine under `signing = "hmac"` (REST only), and its options lane whenever options are armed (BX9: eapi refuses the Ed25519 key); the account tools under `--bn-ops-hmac`; the Binance Stocks probe and battery when `.env` names no Ed25519 key (§12) | the live HMAC pair |
| `BINANCE_DEMO_API_KEY`, `BINANCE_DEMO_ED25519_SEED`, `BINANCE_DEMO_HMAC_*` | the Demo tools only (probe, battery, drill, account tools on Demo; the options probe and battery sign eapi with the HMAC pair) | Demo's keys (demo.binance.com) |

- The live key **trades what is armed and cannot withdraw** — futures
  trading when a futures product is armed, spot trading
  (`enableSpotAndMarginTrading`) when spot is: the boot asserts each and
  refuses otherwise. Spot needs the Ed25519 key (its fills arrive only on
  the WS API). Restrict the key to the engine host's IP.
- **Options (BX9)** sign with the HMAC pair, whatever `signing` says: an
  options arm without it refuses the boot. The pair's key must trade
  options (`enableVanillaOptions`) and never withdraw — the boot asserts
  both — and **must be the same account's as the arm's key**: nothing
  the venue answers proves it (eapi names no UID; risk-policy BX-19).
  The E7 anchor records the options key's tag (§6).
- `.env` is mode 600. A file in the repo names the variables, never a
  value; no session reads `.env`.
- Disabling the key in the app stops everything the arm sends, whatever
  the engine does (kill switch 4, plan §3.11). Today the arm reads the
  refusals that follow as rejects: the slot halts `reject-streak` or
  `recon-stale`, not `venue-lock` (risk-policy BX-19).

## 2. The Demo tools

Demo orders are real orders on Demo's books; nothing reaches mainnet
(the binary refuses any other host). Binance Stocks has no Demo: its
probe and battery are §12's, on MAINNET.

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
- **The options probe:** `scripts/exec-smoke.sh --binance-probe options`
  — the options K-probes on Demo (OP0–OP9): which host serves eapi, the
  HMAC key's answers, an IoC long and a short (a `-6057` on the short is
  the account's Long & Short mode answering, not a failure), the reads
  the lane makes; the end reads the account again. 0, 59 (a probe could
  not run) or 58 (something of the probe's left).
- **The options battery:** `scripts/exec-smoke.sh --binance options
  --lifecycle` — options alone on the account (beside any other product
  refused: 1, nothing sent), REST-only on eapi with the HMAC pair. The
  row is BTC's first call above the index × 1.05 a day out; before each
  IoC the touch, the mark and the index are read and handed to the
  router as an options summary. `--bn-cycles` cycles alternate LONG and
  SHORT: with a two-sided book an IoC through the touch within
  `--bn-usd` and a reduce-only IoC closing what it filled, each fill's
  leg proven by an agreeing reconciliation; with no two-sided book
  (Demo's options books are usually empty) an IoC at the band's far edge
  the venue must end unfilled. The bar: two LONG and two SHORT IoCs
  answered and two agreeing reconciliations — fills reported, not
  required. The end reads the options positions flat (58 if not).
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
- Binance Stocks' fee (BX10) is USD per order with a $0.35 minimum: a fee
  journaled at that floor (notional under $350) is no rate — counted
  apart (`eq_floor`), never in a class.
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
- **A held cycle** (BX13, R3-1: `/state` `recon_held`; `J_RECON` code
  2): a reconciliation that landed while an order's trades were still
  being read is inconclusive — neither drift nor reconciled; the next
  runs at once when the read answers. Not a halt of its own: one now and
  then is normal on spot; a count that keeps climbing while
  `recon-stale` nears means the trades reads are not answering — read
  the log's `myTrades` answers.

## 6. A boot that does not arm

- **A BX-19 assertion** (`BootErr::Assert`, the log names it): hedge
  mode, multi-assets on, a row isolated or past `max_leverage` —
  `--binance-setup`, then boot again; the key's permissions or the
  account mode — the app (and `exec.toml`'s `account_mode`).
- **`UNRECONCILED: n unbound position(s)`:** the account holds a position
  on a symbol this boot did not bind; the slot refuses every order until
  it is flat: `--binance-flatten … --bn-symbol <it>` (engine stopped); a
  spot balance is sold, an option closed, in the app (an option bought
  after the boot's held read: a restart binds it, §11).
- **Adoption past the cap:** positions on owned rows larger than the
  slot's `cap_instance_usd_1e6` refuse the boot: reduce them. On spot the
  held base balances count, valued at the ticker (§10).
- **Spot's own refusals** (BX8): §10.
- **A boot that never reconciles** (it ends `Timeout` in its reconciliation
  phase, orphans counted): an order of an earlier boot's (`mv…`) rests on
  a symbol this boot did not bind, and the gateway cancels only on bound
  rows (risk-policy BX-9). Cancel it in the app, then boot again.
- **The E7 anchor refuses:** `binance-pnl-anchor.state` beside `exec.toml`
  is another key's or another account mode's session — or another equity
  basis's (BX9: options armed into a USDⓈ-M session, dropped from one, or
  under another options key). Deleting it ends that session (the session
  bound starts again from the next boot).
- **Options' own refusals** (BX9): §11.
- **`products` names `equity`:** the engine refuses Binance Stocks
  (ruling S-3; O-BX16, R17) — drop it; Stocks run only as §12's battery.
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
   (`--binance spot --lifecycle`); when options are, `--binance-probe
   options` and the options battery (`--binance options --lifecycle`) —
   exit 0 each.
4. Restart the engine the usual way (`docs/local-setup.md`, "Always-on
   standing engine"). The Binance arm stays dark unless `exec.toml` arms
   a Binance slot (`--arm-live`).

## 8. Arming (BX13) — the preconditions

None of these is met by the code alone; each is yours.

- **The ruling:** O-BX6 lifted for the slot (BX13).
- **The gates, on the tree you arm:** gates 94, 94b and 108 and the
  loopback set pass — `make alloc-assert` (the Mac, release: 125/125) and
  `cargo nextest run -p exec-binance` (`bn_e2e_loopback`, 71 tests).
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
  base asset; no session bound on the slot; §10's rules kept (BX13 built
  the code R2-2 and R2-4 owed: the venue's day count read before the
  first spot order, a SELL's reserve, spot's own weight window). And
  O-BX22 ruled, or a spot price feed:
  no spot mark reaches the router's ledger, so the slot's
  `cap_instance_usd_1e6` prices its other spot holdings at their last
  fill and an adopted one at the boot's ticker.
- **Options** (BX9), when armed: the options Demo exit seen on the binary
  (`--binance-probe options`, `--binance options --lifecycle`;
  risk-policy "BX9"); gates 110 and 111 on the tree; the HMAC pair this
  account's (§1). And what Demo has not yet shown (risk-policy "BX9",
  Q-3): **an options fill, long and short, booked and reconciled** (the
  BX9 runs filled nothing: Demo's books were empty); **an expiry**
  settled through the exercise record across 08:00Z — the record's real
  shape (which field is the settlement price; a short's `amount` signed
  negative, else every short's record is refused and the slot halts
  `recon-drift`); a mark of 0 on a live row; eapi's `-2013` for an
  unknown order; the options equity on mainnet.
- **`exec.toml`:** the slot names `binance`, `mode = "live"`, every halt
  key non-zero (`halt_on_margin_ratio_1e6` ≤ 799 999), `max_symbols`,
  `min_maker_ttl_ms` ≥ 5 000; the `[exec.binance]` block
  (`exec.toml.example` lists the variants this binary boots).
- **The worker:** its PMLR reader reads the adopted and settlement fill
  flags before any surface folds Binance fills (risk-policy "BX7") —
  built in BX13 (`FillRec.flags`; the position fold, the dashboard and
  `vrp_shadow` count neither as a trade).
- **The members:** every member trading the slot reads
  `FILL_FLAG_ADOPTED` as opening inventory, never a trade (risk-policy
  "BX7").
- **Binance Stocks** (BX10): not armable — the engine refuses it (S-3).
  Arming it needs R17's ruling (a member and an equity market-data
  source) and a build; §12's battery is its only driver.
- **The binary:** the Demo evidence of §7 on the very binary you arm.
- **R0** (BX13): the ramp's record on this account, every bar met
  (§13).
- **The boot:** `--arm-live <slot>`; the boot tells
  `binance: LIVE ARM ARMED — …` (or names what refused it).

## 9. Exit codes (`scripts/exec-smoke.sh`)

| codes | tool |
|---|---|
| 50–57 | the battery's phases (account, boot, cycle, post-only, TTL, sweep, reconciliation, fills); the Binance Stocks battery's are §12's, the R0 ramp's §13's |
| 58 | something left on the Demo account, or an end that could not read it — on the Binance Stocks battery and the R0 ramp, on the **LIVE** account (§12, §13: clear it at once) |
| 59 | the probe (the Binance Stocks probe too) |
| 60–64 | the account tools: a read, a refusal, the arm's orders resting, a position still open, mainnet without `--confirm`; 64 is also the Binance Stocks battery and the R0 ramp without `--bn-confirm-mainnet` (the plan printed, nothing sent) |
| 65 | the gap drill: the dead-man was not seen to cancel the makers |
| 66 | the spot battery: the permission check refused the boot — the account lacks a group the spot row needs (the bStock check); the Binance Stocks battery: `486410`, the US-equity disclaimer unsigned (§12); nothing placed either way |

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
  UTC day, an order that fills taking one off). BX13 (R2-2): the gateway
  reads the venue's own count and limit (`account.rateLimits.orders`) at
  each venue day's first cycle, and no spot order goes before it is read
  (the arm's `NotReady`; a refused read is asked again each cycle, a
  malformed one is a scan failure — and a scan failure halts the slot
  `recon-drift`, futures included, until a restart: §5). The arm then
  mirrors it at
  `orders_frac`: BUYs stop at the share — 160 000 at 80 % — and a SELL
  goes on into a reserve past it (half of what the limit leaves, at most
  10 000: to 170 000), so a sell-down is never starved by the day's buys;
  past the reserve every spot order waits for 00:00Z, the rest of the
  limit yours by hand. `/state` `spot_day` shows the count, both caps and
  whether the venue's count was read. An IoC member placing 80 orders
  every 10 s, most of them unfilled, reaches the share in about 6 h.
- **Spot's request weight** (BX13, R2-4): metered in a window of its own
  — the venue's spot pool, 6 000 a minute per IP, the arm's share 3 600 —
  at spot's weights (`account.status` 20, `openOrders` 80, `myTrades`
  20). Past 80 % an early reconciliation waits (the throttle), and an
  unread day's `myTrades` is retried only while the window holds it and a
  whole cycle more. `/state` `spot_weight`.
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

## 11. Options (BX9)

Options trade IoC-only on a classic account, REST-only on eapi with the
HMAC pair (§1), alone or beside USDⓈ-M (beside COIN-M, spot, PM or PM Pro
the boot refuses). The venue's countdown is a market maker's, so no
maker rests and nothing is cancelled: **no options cancel exists** — an
options order of ours resting at boot refuses it (`OptStray`): cancel it
in the app, then boot again.

- **What the arm binds:** the paper-selected chain (universe.toml's
  `[binance] options_underlyings` and its selection, at most 64 rows)
  and every option the account holds outside it — read once as the
  engine boots (at most 16 more), bound, priced and adopted under their
  own ids. A held option no listed row names, or more than 16, refuses
  the boot by name or by count: close it in the app, or configure its
  underlying. **Do not buy an option by hand while the engine boots or
  runs:** one bought after that read is unbound. Bought during the boot,
  the boot completes unreconciled (`UNRECONCILED` in its log line) and
  the slot refuses every order until the option is closed in the app or
  a restart binds it; bought while the engine runs, the cycles stop
  agreeing and the slot halts `recon-stale` after
  `halt_on_recon_stale_ms` (orders pass until then).
- **Writing:** `options_write = 1` lets the arm open shorts where the
  venue's `nakedSell` allows (BTC, ETH); elsewhere, or with 0, a SELL
  with no long is refused (`NotWritable`) and any other SELL goes
  reduce-only. The account must be in Long & Short Sell mode for writing:
  the venue answers `-6057` otherwise, and that underlying stops writing
  for the boot (its longs still trade).
- **Adoption:** a held long at the venue's mark (a mark of 0 is worth
  nothing); a held short at its initial margin's upper bound per contract
  (the mark + 15 % of the index × the unit), within the slot's
  `cap_instance_usd_1e6`. A held short on a row the arm may not write
  refuses the boot (`OptionShort`); an index the venue's own mark
  disagrees with prices nothing (`Inventory`).
- **Expiry (08:00Z):** a booked leg past its expiry is settling — not
  judged — while the arm reads the exercise records each cycle. A record
  that agrees with the leg (its whole position, the cash's sign the
  side's, the cash per contract the intrinsic value within the fee) books
  one settlement fill at the intrinsic value; one that disagrees books
  nothing and halts the slot `recon-drift`; no record within 2 h is drift
  too. **Do not exercise or close an expiring leg by hand.** A boot
  between an expiry and its settlement, with the expired option held, is
  refused by name if the venue no longer lists it: boot again once it
  has settled.
- **Stale marks:** an option whose summary is older than 30 s takes no
  opening order (`Unpriced`; an exit passes); a mark outside its
  no-arbitrage band is refused and the row keeps its last price.
- **eapi's weight** is its own (the arm's 240 of the IP's 400 a minute):
  keep `recon_every_ms` at the default 60 000 or more — 20 000 refuses
  the boot (its cycle leaves the day's read nothing). The boot's timeout
  grows with the rows: up to about 5 minutes with 64.
- **In session**, the options account reading `reduceOnly` or not
  `canTrade`, or the options stream's `RISK_LEVEL_CHANGE` to
  `REDUCE_ONLY`, is `venue-lock` (9 016, raised once).
- **The session bound** (E7) reads the options account's `equity`, summed
  with USDⓈ-M's margin balance when both are armed; no equity is judged
  until every part is read.

The boot's options refusals (the log names each):

| refusal | means | what to do |
|---|---|---|
| "options are armed but their HMAC key cannot be read" | no `BINANCE_HMAC_*` pair in `.env` | add the pair (this account's) |
| "BX9 proves options alone and beside USDⓈ-M only" / "a ruling (options beside Portfolio Margin or PM Pro)" | options armed with `coinm`, `spot`, `pm` or `pm_pro` | arm options alone, or with `usdm`, under classic |
| "… leaves the boot's day read nothing …" | `recon_every_ms` too fast for eapi's weight | raise `recon_every_ms` |
| "the account holds option `…`, which no listed row of the configured underlyings names" | a held option outside the configured underlyings (or expired, unlisted, unsettled) | close it in the app, configure its underlying, or wait for its settlement |
| "the account holds more than 16 options outside the selected chain" | too many held outside the chain | close some, or widen the selection |
| `Assert(NoTradePermission)` / `Assert(CanWithdraw)` | the options key does not trade options, or can withdraw | the key's settings, in the app |
| `Assert(ReduceOnly)` / `Assert(CannotTrade)` | the options account is reduce-only or cannot trade | the account, in the app |
| `OptStray(row)` | an options order of ours rests | cancel it in the app |
| `OptionShort(row)` | a held short on a row the arm may not write | close it, or set `options_write = 1` where the venue allows |
| `Inventory(…)` | held options past the adoption cap, or one with no price | close them, or raise the cap |

## 12. Binance Stocks (BX10)

**Binance Stocks has no Demo and no testnet: every order is real money on
your live account.** The arm is built and tested on the scripted venue;
the ENGINE refuses it (ruling S-3; no member trades it, O-BX16, and no
equity market data exists, R17): `products` naming `equity` refuses the
boot. Two operator tools reach it, both on MAINNET with the live key
(§1: the Ed25519 pair, else the HMAC pair), both through
`scripts/exec-smoke.sh`, which prints a MAINNET banner first.

**The read-only probe** — `scripts/exec-smoke.sh --binance-probe equity`
(add `--bn-stream-confirm` to open a listenKey and watch the order
stream for a minute): EQ0–EQ7 — the clock, AAPL's and SPY's rules, the
tokenized assets, AAPL's quote, the open orders, the last 7 days' orders
and executions (their shapes, never their values), an unknown order's
answer. It places nothing. 0 or 59. Its PASS of 2026-10-11 06:48Z pinned
the shapes the arm reads before any order.

**The battery** — the operator's alone. First without the word (it reads
and prints its plan; nothing is sent; exit 64), then, the plan read, the
same line with `--bn-confirm-mainnet`:

```
scripts/exec-smoke.sh --binance equity --lifecycle \
  --bn-eq-session RTH --bn-eq-tokenize 0 --bn-eq-quote USDC --bn-usd 6 \
  [--bn-eq-symbol AAPL] [--bn-eq-ttl-ms 60000] \
  [--bn-rest-bps 300] [--bn-cross-bps 50]
# the plan read:  … the same flags … --bn-confirm-mainnet
```

- **The D5 keys are required, no default:** `--bn-eq-session` (`RTH`,
  `EXTENDED` or `24H` — run it while that session is open: outside it the
  broker ends a DAY order unfilled or refuses it, and the run is short),
  `--bn-eq-tokenize` (`0`: the fill settles as the share; `1`: as its
  bStock — never one an armed engine's spot row holds) and
  `--bn-eq-quote` (the quote asset the ticker trades in). The values
  above are an example; the account and the ticker decide. **`--bn-usd`
  too** (6 … 25 for Stocks): the futures batteries' default, 60, is
  refused before anything is sent (exit 1). The bracketed flags show
  their defaults; `--bn-rest-bps` 50 … 900, `--bn-cross-bps` 1 … 300.
- **Without `--bn-confirm-mainnet`** it reads — the clock, the ticker's
  rules, its quote, the open orders (none may rest) — prints its plan
  (the two prices and quantities) and sends nothing (exit 64). Read the
  plan, then run it again with the word.
- **The run:** a BUY `--bn-rest-bps` under the bid — `ACCEPTED` by the
  broker, then cancelled by its member; a marketable BUY `--bn-cross-bps`
  over the ask, `--bn-usd` of notional (6 … 25; at least the ticker's
  minimum, $5; on a ticker that trades in whole shares, one share — the
  router's $50 clamp refuses a dearer one) — filled, or ended by its TTL;
  the SELL of what it
  bought (floored to the step: the dust stays, reported); three
  reconciliations in a row that agree. Every order is LIMIT DAY under the
  gateway's TTL (`--bn-eq-ttl-ms`, 5 000 … 300 000): the gateway cancels
  it at expiry.
- **The end, always** (after a failure or a Ctrl-C too): what it bought
  and did not sell offered once more (not after a signal, not once
  halted); every order of the run cancelled by its id; the open orders
  read empty, and the ticker's executions since the run began netting to
  what the run booked — else 58.
- **Its clamps:** one order $50, the holding $50, the day $200, two
  orders working; the halts `exec.toml.example`'s.
- **Cost at the minimum:** two fees at the $0.35 floor and the spread
  crossed twice — about a dollar.

**Before you run it:**

- **A dedicated account:** no manual Stocks order during the run, and
  none resting before it (the boot refuses a resting order by name,
  `EqStray`: cancel it in the app). A halt's sweep cancels the account's
  every equity order (`cancel-all`).
- **The US-equity disclaimer signed** in the Binance app: unsigned, the
  first place is refused `486410` — exit 66, nothing placed. The tool
  never signs it.
- **The key trades Stocks:** nothing at boot can assert it (the key's
  restrictions name no Stocks permission); a key that cannot is refused
  at the first place.
- **One Ctrl-C:** it ends the run short and lets the end sweep; a second
  kills at once — and an order left then rests WITHOUT its TTL until its
  session closes.

**When it does not end 0:**

| exit | means | what to do |
|---|---|---|
| 50 | the account: an order rests before the run (the boot's `EqStray`: on the run's ticker, or on one it does not trade), or the ticker is not tradable both ways (or at all) — or a place refused as not tradable later in the run | cancel it in the app; pick another ticker; past the first order, read the end's `left` |
| 51 | the boot (the log names the `BootErr`: an assertion, a timeout) | as named |
| 52 | the rest phase: the order was never `ACCEPTED` (the session may be closed) or not cancelled by its member — or any place refused with a code the tool does not map | run it while `--bn-eq-session` is open; read the refusal |
| 56 | no three agreeing reconciliations in a row after the SELL | read the report's legs; read the account's executions in the app |
| 57 | a fill missing (the BUY or the SELL ended unfilled) | widen `--bn-cross-bps`, or run inside the session; the end's `left` is empty only when nothing rests and the executions match the booking |
| 58 | **something left on the LIVE account** — an order resting, a holding not sold, or the ticker's executions since the run began disagreeing with what it booked — or its end could not read it | **at once:** cancel the order in the app (it rests WITHOUT its TTL until its session closes) and sell a holding left |
| 64 | no `--bn-confirm-mainnet`: the plan printed, nothing sent | read the plan; add the word |
| 66 | `486410`: the disclaimer is unsigned; nothing placed | sign it in the app |

**What the battery proves, and what it does not.** Its PASS is the first
sight of the live shapes the arm reads (the order stream's report, the
detail's executions, the history rows, a cancel's answer, Ed25519 on
`/sapi/v1/equity/*`) — risk-policy "BX10", the residuals. It does not arm
anything: an engine that trades Stocks needs R17's ruling (a member and
a market-data source) and a build.

## 13. The R0 ramp on MAINNET (BX13)

**Real money, on your live account.** The battery of §2 — the engine's
own composition: the router, the venue split, the arm, the gateway on its
thread, fill lane 4 — run on MAINNET with the engine's own key, to the
R0 bars (plan §5 BX13). Its PASS is the R0 record a member's arming
reads (§8). It arms no slot of the running engine and needs none: the
engine's Binance arm stays dark. The rulings (2026-10-11): R-1 USDⓈ-M,
COIN-M, spot and options, each at its minimum size; R-2 options' bar 4
round trips (2 LONG, 2 SHORT); R-3 the account classic — Portfolio Margin
and PM Pro are not run (K13, K14 carried).

**The command lines** — each first WITHOUT the word: it reads the live
account (the checks of §2's phase 0, the rows, the options row), prints
one `phase:"plan"` line per product (the row's tick, step and minimum
notional, R0's counts, the clamps) and sends nothing — exit 64. The plan
read, the same line with `--bn-confirm-mainnet`:

```
# 1. USDⓈ-M, COIN-M and spot (the WS API; Ed25519):
scripts/exec-smoke.sh --binance um,cm,spot --lifecycle --bn-mainnet \
  --bn-cm-symbol BTCUSD_PERP --bn-usd 12
#    the plan read: … the same flags … --bn-confirm-mainnet

# 2. Options, alone (eapi, the HMAC pair — Q-1):
scripts/exec-smoke.sh --binance options --lifecycle --bn-mainnet --bn-usd 50
#    … --bn-confirm-mainnet

# 3. The dead-man (USDⓈ-M on the WS API; two makers rest, the gateway
#    dies, the venue's countdown must cancel them):
scripts/exec-smoke.sh --binance um --lifecycle --bn-mainnet --bn-drill gap
#    … --bn-confirm-mainnet
```

- **R0's counts are fixed** on mainnet: 20 USDⓈ-M maker cycles (behind
  the touch: they never fill) and 3 round trips; 20 COIN-M round trips;
  20 spot round trips; 4 options round trips. `--bn-cycles` and
  `--bn-fill-pairs` are refused beside `--bn-mainnet`, and so is the
  flatten drill (R0's drill is the gap drill).
- **Adoption is 0 on mainnet** unless you name it: a spot base balance
  the account holds refuses the boot (exit 51, naming `--bn-adopt-usd`)
  — pass its value (USD) to adopt it as the run's starting inventory,
  never sold by the run; the plan line prints `adopt_usd` and the
  instance cap it makes.
- **Sizes:** every order is `--bn-usd` raised to its row's minimum
  notional — so the futures trade their minimum (USDⓈ-M's row minimum:
  BTCUSDT's is 50 USDT, read 2026-10-17 (PB0), no longer 100; ETHUSDT's
  20, SOLUSDT's 5 — the plan line prints each row's; COIN-M's one
  contract). Spot's BUY must leave a SELL above the minimum
  after the base-asset fee: 12 USD is about twice spot's minimum. Options'
  `--bn-usd` is the most premium an IoC through the touch may cost (a
  dearer touch ends unfilled at the band's edge — short of R0): read the
  options row and its book in the plan first.
- **`--bn-cm-symbol`:** Demo's default is `BNBUSD_PERP` (Demo's coin
  wallet holds BNB); on mainnet name a contract whose coin the account
  holds as margin.
- **The clamps:** one order 600 USD; the instance 2 000 USD past the
  adopted spot balance (none unless named); the day's buying 20 000 USD (mainnet's own,
  `CAP_DAY_MAINNET_USD_1E6`); 16 orders working; the halts
  `exec.toml.example`'s.
- **The order of runs:** 1, then 2, then 3 — each exit 0 before the next.
  Stop at the first non-zero and read its report.
- **The cost, roughly:** each futures round trip pays two taker fees on
  its minimum and the spread twice — USDⓈ-M's 3 and COIN-M's 20 some
  2–5 USD together; spot's 20 at 12 USD some 0.5–1 USD; options' 4 cross
  the options spread eight times on the row's minimum quantity (the
  plan's band says how wide) — the largest share.

**Before you run it (the checklist):**

- [ ] The binary: the deployed release that carries BX13, its §7 Demo
  evidence passed (`--binance um,cm --lifecycle`, the gap drill, spot's,
  options').
- [ ] Those Demo runs' `END` lines read for what R0 judges first on
  mainnet (Demo's bar does not): `journal_dropped`, `recs_dropped`,
  `fills_dropped`, `fills_unseen`, `fees_short`, `fees_extra`,
  `fees_unjudged`, `scan_failed` and `fills_lost` all 0; `clean_streak`
  ≥ 3; the margin samples ≥ 1 for the futures products and options the
  run armed (`margins_usdm`, `margins_coinm`, `margins_options`; spot
  has none) — the drills judge none of these: their own line is their
  bar. One that is not: stop and read why before mainnet.
- [ ] The engine's Binance arm dark: no `--arm-live`, no Binance slot
  armed in `exec.toml` — an armed engine and the battery on one account
  read each other's orders as orphans or strangers.
- [ ] The account dedicated and quiet: nothing resting anywhere; flat on
  the run's futures symbols; no manual trading during the run.
- [ ] `--binance-setup um --bn-network mainnet` and `… cm …` read, then
  applied with `--confirm`: one-way, single-asset (USDⓈ-M), each symbol
  CROSSED at a leverage within 20.
- [ ] Margin: USDT in USDⓈ-M; the contract's coin in COIN-M; USDT in spot
  for the BUYs (the base asset held is adopted, never sold by the run);
  the options account funded in USDT, its writing enabled for the SHORT
  round trips (`nakedSell`).
- [ ] The key (§1): Ed25519 with futures and spot trading, withdrawals
  off, IP-restricted; the HMAC pair for options (`BINANCE_HMAC_*`).
- [ ] BNB burn off for futures (a fee paid in BNB is reported, not
  refused); spot's as set, kept fixed during the run.
- [ ] The clock: clear of 00:00Z (the venue's day roll: spot's count, the
  day reads) and of options' 08:00Z expiry by half an hour; 30 minutes
  free.
- [ ] One Ctrl-C only: it ends the run short through its end (closes,
  sweep, the flat read); a second kills at once — USDⓈ-M's dead-man then
  cancels the makers within 30 s, and anything else is yours to clear.

**When it does not end 0 — on the LIVE account:**

| exit | means | what to do |
|---|---|---|
| 50 | the account check: not flat, an order resting, a setting BX-19 refuses, a key or a balance missing — nothing placed | `--binance-setup … --bn-network mainnet`; clear the account; read the line it names |
| 51 | the boot (the log names the `BootErr`) — nothing placed but the tap's listenKey | as named; a held spot balance past `--bn-adopt-usd` is named too |
| 52–55 | a maker cycle, the post-only refusal, the TTL, the halt's sweep — or R0's count short in that phase | read the report; the end closed, swept and read the account: `left` empty means flat with nothing resting |
| 56 | a reconciliation that never agreed, a halt — or R0 short of 3 clean reconciliations in a row, the journal not whole (a record dropped), a margin sample missing, a scan failure | read the legs in the report and the account's trades in the app |
| 57 | a fill missing, a round trip short of R0, a fill booked without its trade report (the streams lost a trade), an order's fee row missing or twice (order by order: `fees_short`, `fees_extra`) or unjudged, a fill lost | read the `r0` lines; widen nothing on mainnet without reading why |
| 58 | **something left on the LIVE account** — a position, an order — or its end could not read it | **at once:** `--binance-flatten <um\|cm> --bn-network mainnet --confirm --bn-cancel-first`, or the app; spot's and options' in the app |
| 64 | no `--bn-confirm-mainnet`: the plan printed, nothing sent | read the plan; add the word |
| 65 | the gap drill: the dead-man was not seen to cancel the makers (the run cancelled them by id) | read the drill's line; do not arm a USDⓈ-M maker until it passes |
| 66 | the spot permission check refused the boot — nothing placed | the account's spot permissions, in the app |

**The R0 record.** Every run prints one `{"phase":"r0",…}` line per bar
armed — `product`, `bar`, `have`, `want`, `bound` (`at_least` /
`at_most`), `met` — and the gap drill one for the dead-man. Copy them
into the vault (`docs/research/bx/r0-<date>.md`, never git) under this
head, one table per run:

```
# R0 — <date>, binary <git sha>, account classic, key <Ed25519|HMAC>
| run | exit | product | bar | have | want | met |
|---|---|---|---|---|---|---|
| um,cm,spot | 0 | usdm | maker cycles | 20 | 20 | true |
| …          |   |      |              |    |    |      |
Notes: the END line's fills, fees (and any `fees_bnb`), streak, the
venue's first fee rates (bn-fees), anything the plan or the run showed
that the docs did not.
```

R0 is met when every line of the three runs reads `met: true` and each
exit is 0 — among them, for each run: the journal whole (no record
dropped), three clean reconciliations in a row, every fill booked from
its trade report, exactly one `J_FEE` a trade — order by order — with
its asset judged, no scan failure, no fill lost. It changes nothing by itself: arming a
member is a ruling of its own (§8).

## 14. Pine on Binance — the fee source (PB12, OD-12; DARK)

Slot 8 (Pine) is paper only; this section is what the arming change will rely on.
`core_types::Fill` carries no fee, so a live Pine fill books the MODEL fee from
`[exec.slot.8.pine] fee_taker_ppm / fee_maker_ppm` and counts it on
`engine_pine_fee_modelled_live_total` (`/state` `pine.fee_modelled_live`).

1. Before `bn-fees` has N = 30 measured trades per class (BX-14), set both ppm keys to the
   account's read rate: `/fapi/v1/commissionRate` for the dedicated account, BNB burn OFF
   (PB0.5, a signed read the operator runs). The values are MODELLED until step 2.
2. Once `bn-fees` prints a measured row at N = 30 for USDⓈ-M taker / maker, replace the ppm
   keys with those rows (rounded UP to whole ppm).
3. `fee_modelled_live` counts every live fill booked with the model; it never stops counting
   (the venue's `J_FEE` journal stays the truth). A fee carrier on `Fill` is a later CAR.

## 15. Pine on Binance — the arming checklist (PB14; DARK)

Nothing here arms anything. Slot 8 is PAPER ONLY: the parse, boot and AI-plane refusals stand
(risk-policy "PVM — slot 8, paper only"), and the cli never sets the live permit (PL2). The law
is risk-policy "PVM — slot 8 on Binance USDⓈ-M"; this is the operator's list, in order. Every box
is yours; arming is your act alone, at R2.

**The rulings (yours):**

- [ ] OD-2, the symbol: one symbol, one script for R2. BTCUSDT keeps parity with the captured
  conformance chart (`BINANCE:BTCUSDT.P`); its minimum is **50 USDT** (PB0, 2026-10-17), so a
  unit is ~75–100 USDT. A 5-USDT-minimum perp (SOL, BNB, XRP, DOGE) allows ~8–10 USDT units.
- [ ] OD-4, the caps (O-E4: live equals paper): per script `unit ≥ 1.5 × minimum`,
  `cap_instance = unit`, `cap_day ≈ 8 × unit`, `max_open ≤ 2`; the slot's `max_order ≥ 2 ×
  cap_instance`; `halt_on_margin_ratio_1e6` well under 800 000; an E7 loss bound.
- [ ] OD-9 (R-P4 lifted for slot 8 paper on the running engine) and OD-10 (O-BX6 lifted for slot
  8, at R2 only, after the BX13 R0 record exists).

**The account (a dedicated sub-account, used only by slot 8 — OD-3, R-P11):**

- [ ] USDⓈ-M only (`[exec.binance] products = ["usdm"]`, OD-1); `account_scope = "dedicated"`,
  `account_mode = "classic"` (makers need the dead-man).
- [ ] §3.1 setup applied with `--confirm`: CROSSED margin, one-way, single-asset; leverage set
  by the tool, **≤ 3**, and the boot asserts it ≤ `max_leverage`. Funded ≥ the slot's
  `cap_instance_usd_1e6`, so account leverage stays ≤ 1×.
- [ ] BNB burn OFF; the commission rate read (`/fapi/v1/commissionRate`, PB0.5) into
  `[exec.slot.8.pine] fee_taker_ppm / fee_maker_ppm`, flagged modelled until N = 30 (§14).
- [ ] The key: Ed25519, futures trading on, withdrawals off, IP-restricted (§1, §8).
- [ ] PB0.3 / 0.4 / 0.6 read on this account (margin and position mode, the boot's trades page).
- [ ] PB0.2: the venue's `-4164` rule for reduce-only below the minimum confirmed — until then a
  dust exit stays refused (flatten by tool, §3.2). The code waits on it too:
  `bn_live::PB0_2_RECORDED = false` keeps the arm's `ro_min_exempt` off; flipping it is the
  PB0.2 change, with risk-policy item 4 and BX-5 amended.

**The evidence (on the tree and binary you arm):**

- [ ] The BX13 mainnet R0 record on this account (§13), every bar met.
- [ ] impl-08 R0 and R1 artifacts; `pin:U17-barstate-table` pinned; Q1 ruled (`protective`).
- [ ] PB15 green: `crate-gates.sh`; `cargo nextest run -p exec-binance -p strategy-pine -p
  exec-router -p cli`; `make alloc-assert` (your OK) at 0 B/op on the submit and settle paths;
  `make copy-audit` new = 0; the touched fuzz targets 300 s; the loopback readiness proof (PB10).
- [ ] `pine-verify --check --exec` passes the live artifact offline (PB9 / PB11): slot 8
  `venues = ["binance"]`, every Pine symbol `binance_usdm:*`, `max_order_usd_1e6 ≥` each
  symbol's minimum (`6009`), `max_symbols ≥` the Pine symbol count.
- [ ] `risk-reviewer` closed the risk-policy entry.

**What the boot refuses (fail-closed, each on its own):** `6006` an inexact tick or step or a
non-USDⓈ-M perpetual; `6009` `minQty ≠ stepSize` or a cap below a minimum; `6013` facts unlike
the arm's bound row, an unread day (a full trades page), resting orders not swept, a position on
a bound symbol no enabled script trades; `6005` a Live boot without `boot_exit_policy`. Read
the boot tell; a refusal arms nothing.

**At R2 (the arming change, yours):** lift the paper-only refusals for slot 8 on Binance only,
call PB9 from `exec_boot`, set the PL2 permit, edit `exec.toml` (`mode = "live"`, the caps
above), restart through `scripts/exec-smoke.sh` and `--arm-live 8`. One script, one symbol, the
smallest size. A liquidation / ADL fill disables its script without a flatten (OD-8): read the
`VENUE_FILL` incident and the account before anything else.
