# Binance execution — the operator's runbook (BX11)

Anton runs this. It covers the Binance arm's keys, the Demo tools, the
account tools, the fee measurement, the halts, deploying after a merge
and the arming preconditions. The laws are in `docs/risk-policy.md`, "BX —
the Binance execution laws (BX-1 … BX-20)"; the design is
`docs/binance-exec-plan.md`.

**Status (BX11, 2026-10-09).** The arm boots USDⓈ-M and COIN-M futures
under the classic, Portfolio Margin (`pm`) and PM Pro (`pm_pro`) account
modes: makers on USDⓈ-M classic only, IoC everywhere else. Spot, options
and Binance Stocks refuse the boot (BX8–BX10). **No member arms (O-BX6)**
before BX13.

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

- The live key **trades futures and cannot withdraw**: the boot asserts
  both and refuses otherwise. Restrict it to the engine host's IP.
- `.env` is mode 600. A file in the repo names the variables, never a
  value; no session reads `.env`.
- Disabling the key in the app stops everything the arm sends, whatever
  the engine does (kill switch 4, plan §3.11). Today the arm reads the
  refusals that follow as rejects: the slot halts `reject-streak` or
  `recon-stale`, not `venue-lock` (risk-policy BX-19).

## 2. The Demo tools

Demo orders are real orders on Demo's books; nothing reaches mainnet
(the binary refuses any other host).

- **The probe:** `scripts/exec-smoke.sh --binance-probe` — the K-probes
  and the arming preconditions on Demo. 0 or 59.
- **The battery:** `scripts/exec-smoke.sh --binance um --lifecycle` (or
  `cm`, or `um,cm`; `--bn-rest` for the REST route, `--bn-hmac` for the
  HMAC key) — makers, the post-only refusal, the TTL, the sweep, the
  reconciliations and fills, then a flat read. 0, or 50–58 naming the
  phase.
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

A halt cancels orders; it never closes positions. This does.

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
| `recon-drift` | the book and the venue disagree past the threshold — or a frame could not be scanned, or an output was lost (unbounded drift) | read the account against the journal; a scan failure holds until a restart |
| `reject-streak` | rejects in a row (a filter, a revoked key's `-2015`) | the log names the codes |
| `budget-floor` | a 429, `-1003`, `-1015`: the arm sent nothing for at least 60 s | what else spends the IP's weight (`/state` `weight_used`) |
| `venue-lock` | a 418 or `-4400`…`-4402`; the account changed under the arm (an owned row's leverage, multi-assets on, a row isolated); PM's risk level or account status | read the account; `--binance-setup` restores the settings (classic; PM and PM Pro in the app); the restart asserts them again |
| `margin-risk` | the margin ratio at or above `halt_on_margin_ratio_1e6`, or a margin call | add margin, or `--binance-flatten` (classic; PM and PM Pro in the app) |
| `gateway-halt` | the gateway's own halt | the log names it |
| `pnl-gain`, `pnl-loss`, `asset-refusals`, `operator` | as on every venue: the session bound (E7, when set), the refusal streak, your `--halt-slot` | risk-policy "E6", "E7" |

- **The sweep** (`/state` `exec.arms.binance_gw.sweep`): `clear` — the
  venue confirmed nothing of the slot's rests; `working`; `stranded` —
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
  it is flat: `--binance-flatten … --bn-symbol <it>` (engine stopped).
- **Adoption past the cap:** positions on owned rows larger than the
  slot's `cap_instance_usd_1e6` refuse the boot: reduce them.
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
   battery (`--binance um,cm --lifecycle`) and the gap drill — exit 0
   each.
4. Restart the engine the usual way (`docs/local-setup.md`, "Always-on
   standing engine"). The Binance arm stays dark unless `exec.toml` arms
   a Binance slot (`--arm-live`).

## 8. Arming (BX13) — the preconditions

None of these is met by the code alone; each is yours.

- **The ruling:** O-BX6 lifted for the slot (BX13).
- **The gates, on the tree you arm:** gate 94 and 94b and the loopback
  set pass — `make alloc-assert` (the Mac, release: 119/119) and `cargo
  nextest run -p exec-binance` (`bn_e2e_loopback`, 39 tests). Gates 94
  and 94b pin USDⓈ-M classic's allocations only; the loopback set also
  exercises COIN-M, Portfolio Margin and PM Pro (not their margin calls);
  none is a WAN (risk-policy "BX12").
- **The account:** dedicated to the engine (a `shared` scope refuses the
  boot); `--binance-setup … --bn-network mainnet` read, then applied with
  `--confirm`; the TradFi agreement signed if a TradFi row is owned; BNB
  burn off.
- **The key:** Ed25519 (or the HMAC pair on the REST route), futures
  trading on, withdrawals off, IP-restricted.
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

Every non-zero code means: read the report above it before anything
else.
