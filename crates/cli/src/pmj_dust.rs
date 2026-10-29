// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **PMJ J17 — the dust battery** (`multivenue-engine pmj-dust`,
//! `scripts/pmj-dust.sh`; plan-J5): a few $5 Limit BUYs on Jupiter
//! Predict from slot 11's own wallet, each recorded in full, to MEASURE
//! what slot 11's paper law assumes — the decision → keeper-print delay D,
//! the fill share, the fee — before any live arming of slot 11.
//!
//! COPY-DOCTRINE: a cold operator verb — one process per battery run, at
//! most one order per five-minute window; it allocates and copies freely
//! (the records, the raw answers, the ledger, the book series). It is
//! reached only from the binary's `pmj-dust` verb: never linked into,
//! reached by or run on a thread of the engine loop.
//!
//! ## Refusals before the secrets (exit 78; a review trigger 77)
//!
//! The ledger and the lock live in `~/multivenue` (`$HOME/multivenue`),
//! whatever `EXEC_TOML` says; no flag or variable moves them (a second
//! ledger would be a second $50 a day) — `MULTIVENUE_DIR` set to anything
//! but `$HOME/multivenue` refuses. The sticky halt is written in BOTH
//! places — beside the exec artifact the engine boots from
//! (`strategy.conf`'s `EXEC_TOML`, absolute, else `~/multivenue/exec.toml`)
//! and in `~/multivenue` — and both are watched, as are both `exec.HALT`s.
//!
//! In order ([`preflight`]):
//! 1. the arguments — `--orders` 1..=40, the battery id's grammar,
//!    `--path` (`rpc` refuses, below);
//! 2. the BUILD-TIME dust gate (`exec_boot::PMJ_GATES_BUILT.dust`, the
//!    `PMJ-DUST-GATE` line — none exists today, so the verb refuses in
//!    practice). These two come before the environment is read at all;
//! 3. `MULTIVENUE_DIR`; `~/multivenue` a directory; `strategy.conf`
//!    SOURCED as `daily-restart.sh`'s `pmj_conf_get` sources it (a clean
//!    `zsh -f`, no environment of ours), its `EXEC_TOML` absolute and its
//!    directory there;
//! 4. the exclusive `flock` on `~/multivenue/pmj-dust.lock`;
//! 5. a standing `pmj-dust.HALT` (beside the exec artifact or in
//!    `~/multivenue`; fail closed: unreadable stands);
//! 6. `exec.HALT` naming slot 11, or unreadable, beside the exec artifact
//!    or in `~/multivenue` (`pmj_live::check_halt_file`);
//! 7. **slot 11 live anywhere on the host** — the engine and the battery
//!    would share one wallet: `ARM_LIVE` naming 11, any
//!    `~/multivenue/exec*.toml` (and the exec artifact) marking
//!    `[exec.slot.11]` live — read as `daily-restart.sh`'s
//!    `pmj_file_arms_11` reads it and wider (a spaced header, a `'live'`
//!    literal: `daily-restart.sh`'s own parser is narrower, an open item);
//!    an unreadable file refuses; `enabled = 0` arms nothing — or the
//!    engine on 127.0.0.1:9191, when reachable, armed for slot 11 (`/state`
//!    not paper with an exec artifact, and `/metrics` carrying the
//!    `engine_pmj_arm_` / `engine_pmj_gw_` family, which registers only with
//!    slot 11 live); a `/state` or `/metrics` answer that does not read
//!    refuses here;
//! 8. `pmj.toml`'s guard keys reviewed (`pmj_live::guard_from_keys`, as the
//!    live boot's `pmj_preboot`: an empty allowlist or an unset pin
//!    refuses, `build_extra` required), `min_sol_lamports` set,
//!    `guard_program_slot` pinned (not 0), and the R-PMJ-S80 ceilings
//!    (PROVISIONAL): `guard_max_sol_lamports` ≤ [`DUST_MAX_SOL_LAMPORTS`]
//!    (0 accepted — J0 proposed 0) and `guard_max_cu_price` ≤
//!    [`DUST_MAX_CU_PRICE`];
//! 9. the offline self-test (`pmj_exec::self_test`);
//! 10. the ledger read whole, the battery under its 40 and (live) the UTC
//!     day under its $50; the record file;
//! 11. **the review triggers, once a run** (never per order): the chain
//!     half of `pmj-exec --key-check` on public Solana RPC, no secret — the
//!     Predict program's deploy slot against `guard_program_slot`, the
//!     vault's lookup-table entry, the deposit mint, a pinned account
//!     closed. A review trigger writes `pmj-dust.HALT` and exits 77; any
//!     other failure there refuses (78).
//!
//! Then the secrets, exactly as the live boot loads them:
//! `PMJ_WALLET_PUBKEY` (the guard is built for it first), the 0600 keypair
//! file (`pmj_live::wallet_from_env`), `JUP_API_KEY`. `--dry-run` loads and
//! checks the keypair file, then drops the signer before any request: a
//! dry run holds no key that could sign. `scripts/pmj-dust.sh` sources only
//! a regular 0600 `.env` of the user's that sets no `strategy.conf` key, and
//! hands the binary nothing but `HOME`, `PATH` and those keys (and
//! `PMJ_SOLANA_RPC`). The env file is `MULTIVENUE_ENV_FILE` (absolute) when
//! set, else the repo's `.env`; the wrapper consumes the variable (the
//! binary never sees it). Then, at the run's start, requirement (a) and the SOL
//! ceiling (below).
//!
//! ## The interlock (constants, not configuration)
//!
//! [`DUST_DEPOSIT_MICRO`] $5 an order; [`DUST_DAY_CAP_MICRO`] $50 of
//! deposits a UTC day across runs — the ledger `pmj-dust-ledger.tsv`,
//! where each order's `RESERVED` row is written and fsynced BEFORE its
//! build is signed (a crash still counts it; every reservation counts,
//! whatever became of the order); [`DUST_BATTERY_MAX`] 40 orders a battery
//! id; BTC's 5-minute markets only (the engine's own discovery,
//! `Pm15Discovery` of the 5-minute period, one family `btc`); requirement
//! (a) at start — the deposit mint's balance at most the $50 day cap and
//! SOL at least `pmj.toml` `min_sol_lamports` (`pmj_live::read_balances` +
//! `judge_wallet`) — and SOL at most [`DUST_MAX_SOL_START_LAMPORTS`]; a
//! run lasts at most two hours ([`RUN_WALL_MAX_NS`], the ≤ 2 h law) and
//! starts no order whose deadline plus its worst-case tail
//! ([`TAIL_WORST_NS`], from the constants) would pass them — a battery
//! continues with another run under the same id.
//!
//! **SOL** (R-PMJ-S80, PROVISIONAL). A sent order may take at most
//! [`DUST_SOL_FALL_MAX_LAMPORTS`] (J0's measured 5 869 500 — fees and the
//! rent the program pays by CPI — + [`SOL_FALL_MARGIN_LAMPORTS`] + 5 000 a
//! signature + the priority at [`DUST_MAX_CU_PRICE`]); the order account's
//! and the escrow's rent come back at the order's close (+4 155 440 in
//! J0), the position's (1 686 560) stays until the position is closed. No
//! order starts with less than `min_sol_lamports` + one order's bound in
//! the wallet (exit 75). **Sizing `min_sol_lamports`:** it is the SOL the
//! wallet keeps for what follows the battery — claim and close
//! transactions (5 000 + the priority each) and the engine's own fees —
//! and the live boot's floor; 50 000 000 (0.05 SOL) is the live boot's
//! hint. Fund the wallet with `min_sol_lamports` + n × 6 619 500 for n
//! orders, at most [`DUST_MAX_SOL_START_LAMPORTS`] (0.15 SOL): a full
//! day's 10 orders above a 50 000 000 floor fit with ≈ 5 orders to spare;
//! a floor over 83 805 000 cannot fund a full day under the ceiling.
//!
//! ## One order
//!
//! 1. The window: τ (seconds to the close) from a stratified schedule over
//!    [3, 30) s — nine 3-second cells cycled by the battery's order count,
//!    a uniform jitter inside the cell from the run's seed ([`tau_of`]) —
//!    on the first BTC 5-minute window whose decision is at least
//!    [`LEAD_NS`] away; the halt files; its market from the engine's
//!    discovery (Gamma's venue law, then the CLOB's).
//! 2. 2 s before the decision: the balances (the debit and SOL checks'
//!    "before"; the SOL floor), then the host check — its `zsh`, `/state`
//!    and `/metrics` reads happen here, never inside the decision →
//!    execute path they would lengthen.
//! 3. From decision − 1 s to decision + 10 s, both tokens' CLOB books every
//!    250 ms (public `GET clob.polymarket.com/book?token_id=`, its own
//!    thread). At the decision both books are read: the favourite — the
//!    token with the higher best ask — only if its ask is in [0.85, 0.98]
//!    ([`choose`]); otherwise the window is skipped and recorded.
//! 4. Limit = min(ask + 0.02, 0.99). **Contracts are the venue's, bounded
//!    by the guard:** the request carries only the deposit and, through
//!    `pmj.toml`'s `build_extra`, the Limit order and its limit (the body
//!    jup-gw renders, `render::order_body`); the build answers
//!    `contractsMicro`, and `tx::guard_order` signs only when the signed
//!    `contracts` equal it, are not zero, and `contracts × max_fill_price
//!    ≤ deposit × 1e6` with `max_fill_price ≤` the limit — so at most
//!    `floor(5e6 × 1e6 / limit)` micro contracts (5.05 at the 0.99 cap,
//!    above the market's 5-share minimum, which is checked before the build).
//! 5. Build → jup-gw's answer checks ([`check_build`], `gw::prepare` line
//!    for line) → `tx::guard_order` with exactly jup-gw's parameters (the
//!    deposit, the limit, the answer's order / position keys, market hash
//!    and contracts; the reviewed `guard_*` keys) → the halt files once
//!    more (file reads only) → **late?** → the ledger reservation (fsync)
//!    and its record line (fsync) → **late?** once more → our signature in
//!    its slot (`SolTx::sign_into`) → `POST /execute` with the build's
//!    context (path A). **Late** is jup-gw's own LATE rule — the execute
//!    would leave within `gw::MIN_LEAD_NS_DEFAULT` (1 s) of the window's
//!    close — or the tail's bound, past the scheduled decision +
//!    [`SEND_LATE_MAX_NS`]: the order is dropped unsigned (a reservation
//!    already made is closed `UNSENT` and still counts), an ordinary
//!    failure. The record keeps the tool's own overhead (decision →
//!    execute sent), flagged past [`OVERHEAD_FLAG_NS`] (the post-processor
//!    keeps a flagged order out of its D files).
//! 6. `GET /orders/status/{orderPubkey}` every 250 ms until a terminal
//!    status with its fill numbers (`StatusAnswer::record`), or the
//!    window's close + 30 s (jup-gw's cadence and deadline).
//! 7. The balances after (no sooner than 30 s after the execute,
//!    `gw::DEBIT_SETTLE_NS`); the debit check; the SOL check; the
//!    positions.
//!
//! **Landing paths.** (A) `POST /execute` is built. (B) own-RPC
//! `sendTransaction` is NOT: J0 has not shown whether Jupiter's build
//! arrives already co-signed by `AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf`;
//! if Jupiter co-signs only at `/execute`, path B cannot exist —
//! `--path rpc` refuses (R-PMJ-S79, PROVISIONAL). Every record names its
//! path.
//!
//! ## Stops
//!
//! **Sticky** (exit 77: `pmj-dust.HALT` written with the reason, beside the
//! exec artifact and in `~/multivenue`; nothing signs until the operator
//! removes it):
//! - any refusal of the build by the guard or by jup-gw's answer checks
//!   (recorded unsigned);
//! - an order not at a terminal status with its numbers by the window's
//!   close + 30 s (an in-doubt execute included; its ledger row closed
//!   `IN_DOUBT`; the text says it may land until 300 s after its execute);
//! - a fill the battery must not believe ([`fill_implausible`], the arm's
//!   own laws): an average outside (0, 1), above the order's limit, or
//!   more contracts than were signed;
//! - the deposit mint's balance falling by more than the deposit plus 1 %
//!   ([`DEBIT_SLACK_MICRO`]) across a terminal order, by more than the 1 %
//!   with no order in flight, or no balance read after an order (3 tries);
//! - SOL falling across a sent order by more than
//!   [`DUST_SOL_FALL_MAX_LAMPORTS`], or at all when nothing was sent;
//! - a `GET /positions` key (first page, contracts, not claimed) that no
//!   ledger row carries;
//! - `exec.HALT` naming slot 11, a `pmj-dust.HALT` appearing (kept as it
//!   is), or slot 11 live on the host, mid-battery (an order in flight is
//!   polled to its end first);
//! - a reservation of an earlier run with no closing row whose status does
//!   not read terminal (a crash between reservation and record: its row is
//!   closed `ORPHAN`);
//! - a review trigger at the run's start.
//!
//! **Not sticky** (exit 75): [`BUILD_FAILS_MAX`] consecutive failures (a
//! build's transport, a venue non-200 before signing, an unreadable answer;
//! a late order; a failed discovery fetch; a failed balance read before the
//! decision; a `/state` or `/metrics` answer that does not read at the
//! host check; an execute that never left the host — its reservation still
//! counts); the wallet's SOL under `min_sol_lamports` + one order's bound
//! before an order; the day cap; the battery's 40; the 2-hour bound; a
//! failed positions read after an order (the next run reads them first).
//! A skip and a single venue refusal before signing are recorded and the
//! battery goes on.
//!
//! **Claims are never sent:** at the end the claimable positions are
//! listed, read-only.
//!
//! ## The battery and the engine (the procedure)
//!
//! One wallet, one trader at a time. While a battery holds `pmj-dust.lock`
//! or its `pmj-dust.HALT` stands, slot 11 does not arm: `pmj_live`'s
//! `pmj_preboot` (which refuses a live slot 11 when `HOME` is unset or
//! empty: the lock and the ledger live only in `~/multivenue`) and
//! `scripts/pmj-flip.sh live` (through `pmj-exec --dust-probe`) refuse. After a battery, before arming slot 11: let
//! every order end; claim (by hand) or let settle every position the
//! battery left — `pmj-exec --key-check`, which the flip and the daily
//! restart's gate run, refuses (78) while the wallet's `GET /positions`
//! lists, in two reads 10 s apart, a position `pmj-held.state` does not
//! hold (one seen only in the second read passes: it appeared between the
//! reads — the engine's own order in flight, no battery's leftover) — or
//! arm slot 11 on a fresh wallet; remove a `pmj-dust.HALT` only
//! after reconciling the wallet. The battery in turn refuses while slot 11
//! is live anywhere on the host (refusal 7).
//!
//! **These coexistence checks are per `$HOME` and per host:** the lock, the
//! ledger and the halts are this `$HOME`'s `~/multivenue` (and the exec
//! artifact its `strategy.conf` names), the engine seen is the one on this
//! host's 127.0.0.1:9191. Another user's `$HOME` or another host running
//! the same wallet is invisible to them — one wallet, one `$HOME`, one
//! host.
//!
//! ## Records
//!
//! One JSON line per event in `~/multivenue/research/pmj/dust/<battery>.jsonl`
//! (directory 0700, file 0600, fsynced per line; outside git): `chain`,
//! `run`, `skip`, `fail`, `hostile`, `reserve`, `order`, `orphan`, `stop`,
//! `end`. An `order` line carries the battery, seed, τ cell and τ;
//! `t_decision_ns` (wall); the host check's duration; the build's send /
//! answer times, body and raw answer; the guard's end, the sign time and
//! our transaction signature; the execute's send and answer times
//! (`t_land_ack_ns`), signature, slot and raw answer, and the tool's
//! overhead (`overhead_ns`, decision → execute sent; `overhead_flagged`
//! past 2 s) beside `t_guard_done_ns`; every status poll
//! (time, HTTP, raw — bounded; a repeat marked `same`); the terminal
//! status, filled contracts, average price and every member whose name
//! carries `fee`, raw; the order and position keys, market, condition and
//! token ids, side, deposit, limit and contracts; both tokens' book series
//! (wall ns, best ask, its size, the book's own timestamp); the balances
//! and SOL before and after. Never a secret, never the API key; nothing
//! holding key material derives `Debug`.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use core_crypto::{base58_decode, base64_decode};
use core_net::{Header, HttpsReq, Method, PostErr};
use core_types::pm_roll::{pm5_window_at, pm5_window_end_ns, PmPeriod};
use exec_jupiter::client::{JupClient, JUP_HOST};
use exec_jupiter::gw::{
    BLOCKHASH_LIFE_NS, BODY_MIN, DEADLINE_AFTER_CLOSE_NS, DEBIT_SETTLE_NS, MIN_DEPOSIT_MICRO, MIN_LEAD_NS_DEFAULT,
    POLL_EVERY_NS,
};
use exec_jupiter::json::{
    scan_build, scan_error, scan_execute, scan_positions, scan_status, BuildAnswer, ErrKind,
    ErrorAnswer, ExecStatus, ExecuteAnswer, StatusAnswer,
};
use exec_jupiter::render::{execute_body, order_body, status_path, Outcome, CONTEXT_MAX, PATH_MAX};
use exec_jupiter::tx::{guard_order, CreateOrderView, OrderAsk, SolTx, TxErr, TxGuard, KEY_LEN, SIG_LEN, TX_MAX};
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_discovery::{FetchErr, Pm15Discovery, CLOB_HOST};
use rustls::ClientConfig;
use signer_ed25519::Ed25519Signer;
use zeroize::Zeroize;

use crate::exec_boot::{PmjGates, PMJ_GATES_BUILT};
use crate::pmj_boot::PmjLiveKeys;
use crate::pmj_exec::KeyCheckFail;
use crate::pmj_live::{
    api_key_from_env, b58, guard_from_keys, judge_wallet, jup_refusal, key58, read_balances, usd,
    wallet_from_env, PmjGuard, ENV_PUBKEY, HEAD_CAP, RESP_CAP,
};

#[cfg(test)]
mod tests;

// --- the interlock (constants, never configuration) ------------------------

/// Every dust order's deposit, deposit-mint base units = micro USD ($5,
/// the venue's least: `gw::MIN_DEPOSIT_MICRO`).
pub const DUST_DEPOSIT_MICRO: u64 = 5_000_000;
/// Deposits reserved per UTC day, across runs and batteries ($50 — the
/// dust frame's day cap, `exec_boot::PMJ_DUST_CAP_DAY_USD_1E6`).
pub const DUST_DAY_CAP_MICRO: u64 = 50_000_000;
/// Orders (reservations) one battery id may hold.
pub const DUST_BATTERY_MAX: u32 = 40;
/// The one family the battery trades: BTC's 5-minute markets.
pub const DUST_ASSET: &str = "btc";
/// The τ schedule: the first cell's start, ns.
pub const TAU_MIN_NS: u64 = 3_000_000_000;
/// A τ cell's width, ns.
pub const TAU_CELL_NS: u64 = 3_000_000_000;
/// The τ cells (nine × 3 s: [3, 30) s).
pub const TAU_CELLS: u32 = 9;
/// The favourite's least best ask, ×1e6 (0.85).
pub const ASK_MIN_1E6: i64 = 850_000;
/// The favourite's highest best ask, ×1e6 (0.98).
pub const ASK_MAX_1E6: i64 = 980_000;
/// The limit above the ask, ×1e6 (0.02).
pub const LIMIT_OVER_ASK_1E6: i64 = 20_000;
/// The highest limit, ×1e6 (0.99).
pub const LIMIT_MAX_1E6: i64 = 990_000;
/// The book series' cadence, ns.
pub const BOOK_EVERY_NS: u64 = 250_000_000;
/// The book series starts this long before the decision, ns.
pub const BOOK_BEFORE_NS: u64 = 1_000_000_000;
/// The book series ends this long after the decision, ns.
pub const BOOK_AFTER_NS: u64 = 10_000_000_000;
/// The debit check's slack: 1 % of the deposit, micro USD.
pub const DEBIT_SLACK_MICRO: u64 = DUST_DEPOSIT_MICRO / 100;
/// Consecutive failures that stop a run (not sticky): a build, a
/// discovery fetch, a balance read before the decision, an execute that
/// never left the host.
pub const BUILD_FAILS_MAX: u32 = 3;
/// A run's wall-clock bound (the ≤ 2 h law), ns.
pub const RUN_WALL_MAX_NS: u64 = 7_200_000_000_000;
/// The least time from the window's choice to its decision, ns.
pub const LEAD_NS: u64 = 15_000_000_000;
/// The balances before an order are read this long before its decision, ns.
pub const BALANCE_BEFORE_NS: u64 = 2_000_000_000;
/// The engine's loopback `/state` and `/metrics`.
pub const STATE_ADDR: &str = "127.0.0.1:9191";
/// Exit: the battery stopped, not sticky.
pub const EXIT_STOPPED: u8 = 75;
/// Exit: a sticky stop — `pmj-dust.HALT` written.
pub const EXIT_STICKY: u8 = 77;
/// Exit: refused before any order this run.
pub const EXIT_REFUSED: u8 = 78;
/// The sticky halt file: written beside the exec artifact the engine
/// boots from AND in `~/multivenue`; both are watched.
pub const HALT_FILE: &str = "pmj-dust.HALT";
/// The lock file, in `~/multivenue` (whatever `EXEC_TOML` says).
pub const LOCK_FILE: &str = "pmj-dust.lock";
/// The deposit ledger, in `~/multivenue` (whatever `EXEC_TOML` says).
pub const LEDGER_FILE: &str = "pmj-dust-ledger.tsv";

/// **R-PMJ-S80 (PROVISIONAL — the session's choice, Anton to confirm):**
/// the most `pmj.toml` `guard_max_sol_lamports` may say for a dust battery
/// — a CEILING only (0 ..= 6 000 000 accepted). That key bounds the SOL our
/// wallet's own top-level instructions may move; the program's rent is
/// paid inside its CPI, which the guard cannot see (J0 proposed 0: "top-level
/// System lamports from the wallet seen: 0", vault
/// `docs/research/pmj/j0/runs/public-tape-2-stdout.txt`). The SOL that may
/// leave across an order is [`DUST_SOL_FALL_MAX_LAMPORTS`], not this.
pub const DUST_MAX_SOL_LAMPORTS: u64 = 6_000_000;
/// **R-PMJ-S80 (PROVISIONAL):** the most `pmj.toml` `guard_max_cu_price`
/// may say, µlamports per compute unit: J0's public tape saw
/// `SetComputeUnitPrice` 50 000 on every create-order (vault
/// `j0/evidence-public-2026-10-19.txt`: 17 500 lamports of priority on 49
/// 715 CU) and proposed a cap of about twice it — 100 000.
pub const DUST_MAX_CU_PRICE: u64 = 100_000;
/// Solana's fee per signature, lamports.
pub const SIG_FEE_LAMPORTS: u64 = 5_000;
/// The most compute units one transaction may ask.
pub const CU_LIMIT_MAX: u64 = 1_400_000;
/// **J0's measured SOL fall of one create-order** (vault
/// `docs/research/pmj/j0/runs/public-tape-3-stdout.txt`, "rent per order",
/// 2026-10-19: two owners' `order_created` −5 869 500 each, their
/// `order_closed` +4 155 440): 27 500 of fees (two signatures and the
/// priority at 50 000 µlamports) and 5 842 000 of rent the program creates
/// by CPI — 1 686 560 (204 B), 2 667 000 (397 B) and the escrow token
/// account's 1 488 440 (165 B) (`public-tape-2-stdout.txt`, the CPI
/// `CreateAccount`s). The position's rent (1 686 560) stays locked until
/// the position is closed.
pub const J0_SOL_FALL_LAMPORTS: u64 = 5_869_500;
/// The margin over J0's fall: about 10 % of it, and less than the
/// rent-exempt minimum of ANY extra account (890 880 lamports for zero
/// bytes; a token account 2 039 280) — rent is deterministic per account
/// size, so this absorbs the measurement (one transaction's delta) without
/// admitting a fourth account or a bigger one: either stops the battery
/// sticky for a look.
pub const SOL_FALL_MARGIN_LAMPORTS: u64 = 600_000;
/// The most signatures a dust build carries: our wallet and the one
/// co-signer `pmj.toml` may pin (the guard and the answer checks refuse
/// any other).
pub const DUST_SIGS_MAX: u64 = 2;
/// **The SOL fall bound of one sent order** (risk review round 3, MINOR 2;
/// R-PMJ-S80, PROVISIONAL): J0's fall + the margin + 5 000 a signature at
/// [`DUST_SIGS_MAX`] + the priority at [`DUST_MAX_CU_PRICE`] over
/// [`CU_LIMIT_MAX`] units — 6 619 500 lamports. Independent of
/// `guard_max_sol_lamports`. Nothing sent: no SOL may go.
pub const DUST_SOL_FALL_MAX_LAMPORTS: u64 = J0_SOL_FALL_LAMPORTS
    + SOL_FALL_MARGIN_LAMPORTS
    + SIG_FEE_LAMPORTS * DUST_SIGS_MAX
    + DUST_MAX_CU_PRICE * CU_LIMIT_MAX / 1_000_000;
/// A UTC day's orders at most: the day cap over the deposit (10).
pub const DUST_DAY_ORDERS: u64 = DUST_DAY_CAP_MICRO / DUST_DEPOSIT_MICRO;
/// **The most SOL the wallet may hold at a run's start** (risk review round
/// 3, MINOR 3; R-PMJ-S80, PROVISIONAL), lamports: 0.15 SOL. A full day's
/// [`DUST_DAY_ORDERS`] orders at their bound (66 195 000, their position
/// rent unclaimed included) above a `min_sol_lamports` floor of 50 000 000
/// (the live boot's hint) fit with ≈ 33 800 000 (≈ 5 orders) to spare; a
/// dust wallet holds no more SOL than a dust day needs.
pub const DUST_MAX_SOL_START_LAMPORTS: u64 = 150_000_000;
/// An order whose overhead (decision → execute sent) exceeds this is
/// flagged: the post-processor keeps it out of the D files.
pub const OVERHEAD_FLAG_NS: u64 = 2_000_000_000;
/// One request's worst case: `core_net`'s request deadline (`HttpsReq`
/// rides `HttpsPost`'s raw form).
pub const REQ_WORST_NS: u64 = core_net::https_post::REQ_DEADLINE.as_nanos() as u64;
/// The gap between two balance reads after an order, ns.
pub const AFTER_GAP_NS: u64 = 2_000_000_000;
/// **The tail's bound: an execute is never sent later than the scheduled
/// decision plus this** (three requests: the two decision books and the
/// build). Past it the order is dropped unsigned — the bound
/// [`TAIL_WORST_NS`] rests on. What keeps an execute before the window's
/// close is jup-gw's own LATE rule (`gw::MIN_LEAD_NS_DEFAULT`), checked
/// beside it (module docs, "One order").
pub const SEND_LATE_MAX_NS: u64 = 3 * REQ_WORST_NS;
/// **The worst-case tail past an order's deadline** (the window's close +
/// `gw::DEADLINE_AFTER_CLOSE_NS`), from the constants: the first balance
/// read after the order starts at the later of a last status poll sent
/// just before the deadline (`REQ`) and the settle time after an execute
/// sent at most [`SEND_LATE_MAX_NS`] after a decision at least
/// [`TAU_MIN_NS`] before the close (`SEND_LATE_MAX + DEBIT_SETTLE − TAU_MIN
/// − DEADLINE_AFTER_CLOSE` past the deadline); then [`AFTER_TRIES`]
/// balance reads and their gaps; the positions read; the run's closing
/// read of the claimable positions. A run starts no order whose deadline
/// plus this passes its two hours (41 s today; a test pins it and plays
/// the worst case on a clock).
pub const TAIL_WORST_NS: u64 = {
    let settle_over = (SEND_LATE_MAX_NS + DEBIT_SETTLE_NS).saturating_sub(TAU_MIN_NS + DEADLINE_AFTER_CLOSE_NS);
    let first = if settle_over > REQ_WORST_NS { settle_over } else { REQ_WORST_NS };
    first + AFTER_TRIES as u64 * REQ_WORST_NS + (AFTER_TRIES as u64 - 1) * AFTER_GAP_NS + 2 * REQ_WORST_NS
};

const _: () = assert!(DUST_DEPOSIT_MICRO == MIN_DEPOSIT_MICRO);
const _: () = assert!(DUST_DAY_CAP_MICRO as i64 == crate::exec_boot::PMJ_DUST_CAP_DAY_USD_1E6);
const _: () = assert!(
    DUST_DEPOSIT_MICRO as i64 + core_config::pmj::PMJ_SIZING_SLACK_1E6 == crate::exec_boot::PMJ_DUST_MAX_ORDER_USD_1E6
);
const _: () = assert!((DUST_DEPOSIT_MICRO as i64) < crate::exec_boot::PMJ_DUST_CAP_INSTANCE_USD_1E6);
const _: () = assert!(DUST_DAY_CAP_MICRO as i64 <= crate::exec_boot::PMJ_DUST_LOSS_USD_1E6);
const _: () = assert!(DEADLINE_AFTER_CLOSE_NS + TAIL_WORST_NS < 300_000_000_000);
const _: () = assert!(TAU_MIN_NS + TAU_CELLS as u64 * TAU_CELL_NS == 30_000_000_000);
const _: () = assert!(TAU_MIN_NS > MIN_LEAD_NS_DEFAULT);
const _: () = assert!(SOL_FALL_MARGIN_LAMPORTS < 890_880);
const _: () = assert!(DUST_SOL_FALL_MAX_LAMPORTS == 6_619_500);
const _: () = assert!(DUST_DAY_ORDERS == 10);
const _: () = assert!(DUST_DAY_ORDERS * DUST_SOL_FALL_MAX_LAMPORTS + 50_000_000 < DUST_MAX_SOL_START_LAMPORTS);

const NS: u64 = 1_000_000_000;
/// A status poll's raw answer kept in the record.
const RAW_MAX: usize = 4_096;
/// The build's raw answer kept in the record (the base64 transaction fits).
const RAW_BUILD_MAX: usize = 8_192;
/// The book answer window.
const BOOK_RESP_CAP: usize = 256 * 1024;
/// Tries of the balance read after an order, [`AFTER_GAP_NS`] apart.
pub const AFTER_TRIES: u32 = 3;
/// `/state`'s and `/metrics`' bound.
const STATE_MAX: usize = 8 << 20;

const P_ORDERS: &[u8] = b"/prediction/v1/orders";
const P_EXECUTE: &[u8] = b"/prediction/v1/execute";
const P_POSITIONS_Q: &str = "/prediction/v1/positions?ownerPubkey=";

/// `--path rpc`'s refusal (module docs, "Landing paths").
const RPC_REFUSAL: &str = "--path rpc (own-RPC sendTransaction, landing path B) is not built: J0 \
     has not shown whether Jupiter's build arrives already co-signed by \
     AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf — if Jupiter co-signs only at /execute, path B \
     cannot exist (R-PMJ-S79, PROVISIONAL). Use --path execute.";

// --- the verb's failures -----------------------------------------------------

/// Why a battery run ended without completing (module docs, "Stops").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DustFail {
    /// Refused before any order of this run (exit 78).
    Refused(String),
    /// Stopped, not sticky (exit 75).
    Stopped(String),
    /// A sticky stop (exit 77): `pmj-dust.HALT` was written.
    Sticky(String),
}

impl DustFail {
    /// The binary's exit code.
    #[must_use]
    pub const fn exit_code(&self) -> u8 {
        match self {
            DustFail::Refused(_) => EXIT_REFUSED,
            DustFail::Stopped(_) => EXIT_STOPPED,
            DustFail::Sticky(_) => EXIT_STICKY,
        }
    }

    /// The text.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            DustFail::Refused(m) | DustFail::Stopped(m) | DustFail::Sticky(m) => m,
        }
    }
}

impl core::fmt::Display for DustFail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DustFail::Refused(m) => write!(f, "REFUSED: {m}"),
            DustFail::Stopped(m) => write!(f, "STOPPED: {m}"),
            DustFail::Sticky(m) => write!(f, "HALTED (sticky, {HALT_FILE} written): {m}"),
        }
    }
}

use DustFail::{Refused, Stopped, Sticky};

/// The verb's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DustArgs {
    /// The battery id: `[A-Za-z0-9][A-Za-z0-9_.-]{0,63}`.
    pub battery: String,
    /// Orders this run (1 ..= 40, with the battery's earlier ones ≤ 40).
    pub orders: u32,
    /// `execute` (path A) or `rpc` (refused).
    pub path: String,
    /// Build, guard and record; never sign, never land.
    pub dry_run: bool,
    /// The τ jitter's seed (default: from the clock).
    pub seed: Option<u64>,
    /// `pmj.toml` (default `~/multivenue/pmj.toml`).
    pub pmj: Option<PathBuf>,
}

// --- small pure pieces ------------------------------------------------------

/// The battery id's grammar (it names a file).
fn battery_ok(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// The landing path's word: only `execute` is built.
fn land_path(word: &str) -> Result<&'static str, DustFail> {
    match word {
        "execute" => Ok("execute"),
        "rpc" => Err(Refused(String::from(RPC_REFUSAL))),
        other => Err(Refused(format!("--path {other}: the paths are execute | rpc"))),
    }
}

/// The run's PRNG (splitmix64): the τ jitter, reproducible from the seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// τ for `cell` (taken modulo [`TAU_CELLS`]): the cell's start plus a
/// uniform jitter inside it, ns.
fn tau_of(cell: u32, rng: &mut Rng) -> u64 {
    TAU_MIN_NS + u64::from(cell % TAU_CELLS) * TAU_CELL_NS + rng.next() % TAU_CELL_NS
}

/// What the decision's two books say.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Choice {
    /// Buy `outcome` (0 Up, 1 Down) at `limit`; its best ask `ask` (×1e6).
    Buy { outcome: u8, ask: i64, limit: i64 },
    /// No order this window.
    Skip(&'static str),
}

/// The skip rule: the favourite — the token with the higher best ask —
/// only if its ask is in [0.85, 0.98]; the limit min(ask + 0.02, 0.99).
fn choose(up: Option<i64>, down: Option<i64>) -> Choice {
    let (Some(u), Some(d)) = (up, down) else {
        return Choice::Skip("a token's book shows no ask");
    };
    if u == d {
        return Choice::Skip("no favourite: the two best asks are equal");
    }
    let (outcome, ask) = if u > d { (0u8, u) } else { (1u8, d) };
    if !(ASK_MIN_1E6..=ASK_MAX_1E6).contains(&ask) {
        return Choice::Skip("the favourite's best ask is outside [0.85, 0.98]");
    }
    Choice::Buy {
        outcome,
        ask,
        limit: (ask + LIMIT_OVER_ASK_1E6).min(LIMIT_MAX_1E6),
    }
}

/// The contracts a deposit buys at `limit` at most (the guard's bound,
/// `contracts × max_fill_price ≤ deposit × 1e6`), micro contracts.
fn contracts_max(limit_1e6: i64) -> u64 {
    match u64::try_from(limit_1e6) {
        Ok(l) if l > 0 => DUST_DEPOSIT_MICRO * 1_000_000 / l,
        _ => 0,
    }
}

/// **A fill the battery must not believe** (risk review round 2, M4; the
/// arm's own laws, `exec_jupiter::arm` `on_fill`): anything filled at an
/// average outside (0, 1) — the binary law — or above the order's own
/// limit (a Limit order fills at or under its cap: the evidence the guard
/// rests on), or more contracts than were signed. `None`: plausible.
fn fill_implausible(filled_micro: u64, avg_px_micro: u64, limit_micro: u64, signed_contracts: u64) -> Option<String> {
    if filled_micro > signed_contracts {
        return Some(format!(
            "filled {filled_micro} micro contracts, more than the {signed_contracts} signed"
        ));
    }
    if filled_micro == 0 {
        return None;
    }
    if avg_px_micro == 0 || avg_px_micro >= 1_000_000 {
        return Some(format!("an average fill price of {avg_px_micro} ×1e-6 is outside (0, 1) — the binary law"));
    }
    if avg_px_micro > limit_micro {
        return Some(format!(
            "an average fill price of {avg_px_micro} ×1e-6 is above the order's limit {limit_micro} — the \
             Limit order's evidence is falsified"
        ));
    }
    None
}

/// `YYYY-MM-DD` of a Unix second (UTC; the civil calendar).
fn utc_date(unix_s: u64) -> String {
    let (y, m, d) = civil(unix_s);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DDTHH:MM:SSZ` of a Unix ns.
fn utc_stamp(unix_ns: u64) -> String {
    let s = unix_ns / NS;
    let (y, m, d) = civil(s);
    let t = s % 86_400;
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", t / 3_600, t / 60 % 60, t % 60)
}

/// (year, month, day) of a Unix second (H. Hinnant's `civil_from_days`).
fn civil(unix_s: u64) -> (i64, i64, i64) {
    let z = i64::try_from(unix_s / 86_400).unwrap_or(0) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// A decimal string (`"0.97"`, `"12.5"`) as ×1e6; digits past the sixth
/// decimal are dropped; anything else (a sign, an exponent) is `None`.
fn dec_1e6(s: &str) -> Option<i64> {
    let s = s.trim();
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    if (int.is_empty() && frac.is_empty())
        || !int.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let i: i64 = if int.is_empty() { 0 } else { int.parse().ok()? };
    let mut f = 0i64;
    let mut k = 0u32;
    for b in frac.bytes().take(6) {
        f = f * 10 + i64::from(b - b'0');
        k += 1;
    }
    while k < 6 {
        f *= 10;
        k += 1;
    }
    i.checked_mul(1_000_000)?.checked_add(f)
}

/// A base58 key of exactly 32 bytes (`gw::key_of`).
fn key_of(text: &[u8]) -> Option<[u8; KEY_LEN]> {
    let mut k = [0u8; KEY_LEN];
    match base58_decode(text, &mut k) {
        Ok(KEY_LEN) => Some(k),
        _ => None,
    }
}

/// At most `max` bytes of `b`, as text.
fn lossy(b: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&b[..b.len().min(max)]).into_owned()
}

/// The wall clock, Unix ns.
fn wall_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Sleep until wall time `t` (Unix ns), in steps of at most 50 ms, or
/// until `stop` is raised.
fn sleep_until_wall(t: u64, stop: Option<&AtomicBool>) {
    loop {
        let now = wall_now();
        if now >= t || stop.is_some_and(|s| s.load(Ordering::Acquire)) {
            return;
        }
        std::thread::sleep(Duration::from_nanos((t - now).min(50_000_000)));
    }
}

// --- a small JSON reader (cold: the venue's answers, /state, the test venue) --

/// A JSON value.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Jv {
    Null,
    Bool(bool),
    /// The number's text, as written.
    Num(String),
    Str(String),
    Arr(Vec<Jv>),
    Obj(Vec<(String, Jv)>),
}

/// Nesting deeper than this does not read.
const JSON_DEPTH_MAX: usize = 64;

impl Jv {
    /// The whole of `b` as one value (`None`: not JSON, or deeper than
    /// [`JSON_DEPTH_MAX`]).
    fn parse(b: &[u8]) -> Option<Jv> {
        let mut p = JsonP { b, i: 0 };
        let v = p.value(0)?;
        p.ws();
        if p.i != b.len() {
            return None;
        }
        Some(v)
    }

    fn get(&self, k: &str) -> Option<&Jv> {
        match self {
            Jv::Obj(m) => m.iter().find(|(x, _)| x == k).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `a.b.c`.
    fn path(&self, p: &str) -> Option<&Jv> {
        let mut v = self;
        for k in p.split('.') {
            v = v.get(k)?;
        }
        Some(v)
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Jv::Str(s) => Some(s),
            _ => None,
        }
    }

    /// A non-negative integer, written as a number or as a string of digits.
    fn as_u64(&self) -> Option<u64> {
        match self {
            Jv::Num(s) | Jv::Str(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) => s.parse().ok(),
            _ => None,
        }
    }

    /// A decimal, as a number or a string, ×1e6.
    fn as_1e6(&self) -> Option<i64> {
        match self {
            Jv::Num(s) | Jv::Str(s) => dec_1e6(s),
            _ => None,
        }
    }

    fn items(&self) -> &[Jv] {
        match self {
            Jv::Arr(a) => a,
            _ => &[],
        }
    }

    /// The value written back as JSON.
    fn to_json(&self, out: &mut String) {
        match self {
            Jv::Null => out.push_str("null"),
            Jv::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Jv::Num(s) => out.push_str(s),
            Jv::Str(s) => esc(s, out),
            Jv::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.to_json(out);
                }
                out.push(']');
            }
            Jv::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    esc(k, out);
                    out.push(':');
                    v.to_json(out);
                }
                out.push('}');
            }
        }
    }
}

struct JsonP<'a> {
    b: &'a [u8],
    i: usize,
}

impl JsonP<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.i) == Some(&c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<Jv> {
        if depth > JSON_DEPTH_MAX {
            return None;
        }
        self.ws();
        match *self.b.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.eat(b'}') {
                    return Some(Jv::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if !self.eat(b':') {
                        return None;
                    }
                    let v = self.value(depth + 1)?;
                    m.push((k, v));
                    self.ws();
                    if self.eat(b'}') {
                        return Some(Jv::Obj(m));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                self.ws();
                if self.eat(b']') {
                    return Some(Jv::Arr(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    self.ws();
                    if self.eat(b']') {
                        return Some(Jv::Arr(a));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'"' => Some(Jv::Str(self.string()?)),
            b't' => self.word(b"true", Jv::Bool(true)),
            b'f' => self.word(b"false", Jv::Bool(false)),
            b'n' => self.word(b"null", Jv::Null),
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn word(&mut self, w: &[u8], v: Jv) -> Option<Jv> {
        if self.b.get(self.i..self.i + w.len()) == Some(w) {
            self.i += w.len();
            Some(v)
        } else {
            None
        }
    }

    fn digits(&mut self) -> usize {
        let s = self.i;
        while self.i < self.b.len() && self.b[self.i].is_ascii_digit() {
            self.i += 1;
        }
        self.i - s
    }

    fn number(&mut self) -> Option<Jv> {
        let s = self.i;
        self.eat(b'-');
        if self.digits() == 0 {
            return None;
        }
        if self.eat(b'.') && self.digits() == 0 {
            return None;
        }
        if self.eat(b'e') || self.eat(b'E') {
            if !self.eat(b'+') {
                self.eat(b'-');
            }
            if self.digits() == 0 {
                return None;
            }
        }
        String::from_utf8(self.b[s..self.i].to_vec()).ok().map(Jv::Num)
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = self.b.get(self.i..self.i + 4)?;
        let mut v = 0u32;
        for &c in h {
            v = v * 16 + char::from(c).to_digit(16)?;
        }
        self.i += 4;
        Some(v)
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat(b'"') {
            return None;
        }
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            if (0xD800..0xDC00).contains(&hi) {
                                if !(self.eat(b'\\') && self.eat(b'u')) {
                                    return None;
                                }
                                let lo = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&lo) {
                                    return None;
                                }
                                char::from_u32(0x1_0000 + ((hi - 0xD800) << 10) + (lo - 0xDC00))?
                            } else {
                                char::from_u32(hi)?
                            }
                        }
                        _ => return None,
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return None,
                c => out.push(c),
            }
        }
    }
}

/// `s` as a JSON string.
fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Every member whose name carries `fee` (ASCII case-insensitive), at
/// any depth: `(path, the value as JSON)` — the venue's fee words, raw.
fn fee_fields(v: &Jv, path: &str, out: &mut Vec<(String, String)>) {
    match v {
        Jv::Obj(m) => {
            for (k, x) in m {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                if k.to_ascii_lowercase().contains("fee") {
                    let mut s = String::new();
                    x.to_json(&mut s);
                    out.push((p.clone(), s));
                }
                fee_fields(x, &p, out);
            }
        }
        Jv::Arr(a) => {
            for (i, x) in a.iter().enumerate() {
                fee_fields(x, &format!("{path}[{i}]"), out);
            }
        }
        _ => {}
    }
}

// --- one record line --------------------------------------------------------

/// One JSON object, built member by member; [`Rec::done`] names its kind.
struct Rec(String);

impl Rec {
    fn new() -> Self {
        Self(String::from("{"))
    }

    fn key(&mut self, k: &str) {
        if self.0.len() > 1 {
            self.0.push(',');
        }
        esc(k, &mut self.0);
        self.0.push(':');
    }

    fn str(&mut self, k: &str, v: &str) -> &mut Self {
        self.key(k);
        esc(v, &mut self.0);
        self
    }

    fn u64(&mut self, k: &str, v: u64) -> &mut Self {
        self.key(k);
        let _ = write!(self.0, "{v}");
        self
    }

    fn i64(&mut self, k: &str, v: i64) -> &mut Self {
        self.key(k);
        let _ = write!(self.0, "{v}");
        self
    }

    fn bool(&mut self, k: &str, v: bool) -> &mut Self {
        self.key(k);
        self.0.push_str(if v { "true" } else { "false" });
        self
    }

    fn opt_i64(&mut self, k: &str, v: Option<i64>) -> &mut Self {
        match v {
            Some(x) => self.i64(k, x),
            None => self.raw(k, "null"),
        }
    }

    /// `json` as written (the caller rendered it).
    fn raw(&mut self, k: &str, json: &str) -> &mut Self {
        self.key(k);
        self.0.push_str(json);
        self
    }

    /// A raw answer, bounded: its text (at most `max` bytes) and its length.
    fn body(&mut self, k: &str, b: &[u8], max: usize) -> &mut Self {
        self.str(k, &lossy(b, max));
        self.u64(&format!("{k}_len"), b.len() as u64)
    }

    fn done(mut self, kind: &str) -> String {
        self.str("kind", kind);
        self.0.push('}');
        self.0
    }
}

// --- the CLOB book ------------------------------------------------------------

/// The top of one token's book.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct BookTop {
    /// The best (lowest) ask, ×1e6 (`None`: no ask).
    ask: Option<i64>,
    /// Shares at the best ask, ×1e6.
    ask_size: i64,
    /// The best (highest) bid, ×1e6.
    bid: Option<i64>,
    /// The book's own `timestamp`, as written.
    ts: String,
}

/// A public `GET /book?token_id=` answer: the best ask, its size, the
/// best bid and the book's timestamp. A level that does not read refuses
/// the whole answer (never a guessed touch).
fn parse_book(body: &[u8]) -> Result<BookTop, String> {
    let j = Jv::parse(body).ok_or_else(|| String::from("the book answer is not JSON"))?;
    let side = |name: &str| -> Result<Vec<(i64, i64)>, String> {
        let Some(levels) = j.get(name) else {
            return Err(format!("the book answer has no `{name}`"));
        };
        let mut v = Vec::with_capacity(levels.items().len());
        for l in levels.items() {
            let px = l.get("price").and_then(Jv::as_1e6);
            let sz = l.get("size").and_then(Jv::as_1e6);
            match (px, sz) {
                (Some(p), Some(s)) if p > 0 => v.push((p, s)),
                _ => return Err(format!("a `{name}` level does not read")),
            }
        }
        Ok(v)
    };
    let asks = side("asks")?;
    let bids = side("bids")?;
    let ask = asks.iter().filter(|l| l.1 > 0).map(|l| l.0).min();
    let ask_size = ask.map_or(0, |a| asks.iter().filter(|l| l.0 == a).map(|l| l.1).sum());
    let bid = bids.iter().filter(|l| l.1 > 0).map(|l| l.0).max();
    let ts = match j.get("timestamp") {
        Some(Jv::Str(s) | Jv::Num(s)) => s.clone(),
        _ => String::new(),
    };
    Ok(BookTop { ask, ask_size, bid, ts })
}

/// One book read of the series.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BookSample {
    t_send: u64,
    t_ans: u64,
    top: Result<BookTop, String>,
}

/// A series as compact rows: `[t_send_ns, t_ans_ns, best_ask_1e6|null,
/// ask_size_1e6, "book ts", "error"|null]`.
fn series_json(s: &[BookSample]) -> String {
    let mut o = String::from("[");
    for (i, x) in s.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let _ = write!(o, "[{},{},", x.t_send, x.t_ans);
        match &x.top {
            Ok(t) => {
                match t.ask {
                    Some(a) => {
                        let _ = write!(o, "{a}");
                    }
                    None => o.push_str("null"),
                }
                let _ = write!(o, ",{},", t.ask_size);
                esc(&t.ts, &mut o);
                o.push_str(",null]");
            }
            Err(e) => {
                o.push_str("null,0,null,");
                esc(e, &mut o);
                o.push(']');
            }
        }
    }
    o.push(']');
    o
}

/// A book read's members in a record (`prefix_ask_1e6`, …).
fn book_fields(r: &mut Rec, prefix: &str, t0: u64, t1: u64, b: &Result<BookTop, String>) {
    r.u64(&format!("{prefix}_t_send_ns"), t0).u64(&format!("{prefix}_t_ans_ns"), t1);
    match b {
        Ok(t) => {
            r.opt_i64(&format!("{prefix}_ask_1e6"), t.ask)
                .i64(&format!("{prefix}_ask_size_1e6"), t.ask_size)
                .opt_i64(&format!("{prefix}_bid_1e6"), t.bid)
                .str(&format!("{prefix}_ts"), &t.ts);
        }
        Err(e) => {
            r.str(&format!("{prefix}_error"), e);
        }
    }
}

// --- the ledger (pmj-dust-ledger.tsv, beside exec.toml) ------------------------

const LEDGER_HEAD: &str = "# pmj-dust-ledger v1 (PMJ J17) — kind\tdate(UTC)\tunix_ns\tbattery\tidx\t\
deposit_micro\torder\tposition\tmarket_id\toutcome\tnote — every RESERVED row counts toward its UTC \
day's $50 (written and fsynced before its build is signed); a DONE / UNSENT / IN_DOUBT / ORPHAN row \
closes the reservation of its order\n";

/// A ledger row's kind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum RowKind {
    /// Written and fsynced before the build is signed: counts toward the day.
    Reserved,
    /// Terminal status read.
    Done,
    /// The signed transaction never left the host.
    Unsent,
    /// Not terminal by the deadline (a sticky stop).
    InDoubt,
    /// A reservation an earlier run never closed, unreadable at start
    /// (a sticky stop).
    Orphan,
}

impl RowKind {
    const fn word(self) -> &'static str {
        match self {
            RowKind::Reserved => "RESERVED",
            RowKind::Done => "DONE",
            RowKind::Unsent => "UNSENT",
            RowKind::InDoubt => "IN_DOUBT",
            RowKind::Orphan => "ORPHAN",
        }
    }

    fn of(w: &str) -> Option<Self> {
        Some(match w {
            "RESERVED" => RowKind::Reserved,
            "DONE" => RowKind::Done,
            "UNSENT" => RowKind::Unsent,
            "IN_DOUBT" => RowKind::InDoubt,
            "ORPHAN" => RowKind::Orphan,
            _ => return None,
        })
    }
}

/// One ledger row.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    kind: RowKind,
    date: String,
    ns: u64,
    battery: String,
    idx: u32,
    deposit: u64,
    order: String,
    position: String,
    market_id: u64,
    outcome: u8,
    note: String,
}

impl Row {
    fn line(&self) -> String {
        let note: String = self.note.chars().map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c }).collect();
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{note}\n",
            self.kind.word(),
            self.date,
            self.ns,
            self.battery,
            self.idx,
            self.deposit,
            self.order,
            self.position,
            self.market_id,
            self.outcome,
        )
    }

    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 11 || f[1].len() != 10 {
            return None;
        }
        Some(Self {
            kind: RowKind::of(f[0])?,
            date: f[1].to_string(),
            ns: f[2].parse().ok()?,
            battery: f[3].to_string(),
            idx: f[4].parse().ok()?,
            deposit: f[5].parse().ok()?,
            order: f[6].to_string(),
            position: f[7].to_string(),
            market_id: f[8].parse().ok()?,
            outcome: f[9].parse().ok()?,
            note: f[10].to_string(),
        })
    }
}

/// The ledger: every row, appended and fsynced one at a time.
struct Ledger {
    path: PathBuf,
    rows: Vec<Row>,
}

impl Ledger {
    /// Read the ledger whole (absent: empty). A line that does not read
    /// refuses: the day cap is never judged on a ledger read in part.
    fn open(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let mut rows = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            rows.push(Row::parse(line).ok_or_else(|| {
                format!(
                    "{} line {}: does not read as a ledger row — the day cap is never judged on \
                     a ledger read in part; fix it by hand",
                    path.display(),
                    n + 1
                )
            })?);
        }
        Ok(Self { path: path.to_path_buf(), rows })
    }

    /// The deposits reserved on UTC day `date`.
    fn day_used(&self, date: &str) -> u64 {
        self.rows
            .iter()
            .filter(|r| r.kind == RowKind::Reserved && r.date == date)
            .map(|r| r.deposit)
            .sum()
    }

    /// The reservations of `battery`.
    fn battery_orders(&self, battery: &str) -> u32 {
        let n = self.rows.iter().filter(|r| r.kind == RowKind::Reserved && r.battery == battery).count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// Every reserved order's position key (base58): the positions that
    /// are ours.
    fn positions(&self) -> BTreeSet<String> {
        self.rows
            .iter()
            .filter(|r| r.kind == RowKind::Reserved)
            .map(|r| r.position.clone())
            .collect()
    }

    /// The reservations no later row closes.
    fn orphans(&self) -> Vec<Row> {
        self.rows
            .iter()
            .filter(|r| {
                r.kind == RowKind::Reserved
                    && !self.rows.iter().any(|c| c.kind != RowKind::Reserved && c.order == r.order)
            })
            .cloned()
            .collect()
    }

    /// Append `row` and fsync it (and the directory when the file is new).
    fn append(&mut self, row: Row) -> Result<(), String> {
        let fresh = !self.path.exists();
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        let mut text = String::new();
        if fresh {
            text.push_str(LEDGER_HEAD);
        }
        text.push_str(&row.line());
        f.write_all(text.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{}: {e}", self.path.display()))?;
        if fresh {
            sync_dir(&self.path);
        }
        self.rows.push(row);
        Ok(())
    }
}

/// fsync `path`'s directory (a new file's name), best effort.
fn sync_dir(path: &Path) {
    if let Some(d) = path.parent() {
        if let Ok(f) = std::fs::File::open(d) {
            let _ = f.sync_all();
        }
    }
}

// --- the sticky halt, the lock, the record file ------------------------------

/// The sticky halt's text, when the file exists (at most 2 KiB of it).
/// **Fail closed:** anything but "no such file" reads as halted — a file
/// that is not regular, cannot be stat'ed, opened or read.
fn read_halt(path: &Path) -> Option<String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => return Some(format!("(unreadable: {e})")),
    };
    if !meta.is_file() {
        return Some(String::from("(unreadable: not a regular file)"));
    }
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => return Some(format!("(unreadable: {e})")),
    };
    let mut b = [0u8; 2_048];
    match f.read(&mut b) {
        Ok(n) => Some(lossy(&b[..n], 2_048)),
        Err(e) => Some(format!("(unreadable: {e})")),
    }
}

/// The first of `halts` that stands: its path and text.
fn halt_standing(halts: &[PathBuf]) -> Option<(PathBuf, String)> {
    halts.iter().find_map(|h| read_halt(h).map(|t| (h.clone(), t)))
}

/// **PMJ J17 (risk review round 2, M3) — slot 11 does not arm beside a
/// dust battery**: refused while `pmj-dust.HALT` stands (fail closed:
/// unreadable stands) beside `exec_toml` or in `mv` (`~/multivenue`), or
/// while a battery holds `pmj-dust.lock` there — a non-blocking `flock`
/// probe of the battery's own lock (an absent lock file is no battery; one
/// that cannot be opened refuses). `pmj_live::pmj_preboot` and
/// `pmj-exec --dust-probe` (the flip's) run it.
///
/// # Errors
/// The battery's trace, or what could not be read.
pub(crate) fn dust_blocks_arming(exec_toml: &Path, mv: Option<&Path>) -> Result<(), String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    dirs.push(exec_toml.parent().filter(|d| !d.as_os_str().is_empty()).map_or_else(|| PathBuf::from("."), Path::to_path_buf));
    if let Some(m) = mv {
        if !dirs.iter().any(|d| d == m) {
            dirs.push(m.to_path_buf());
        }
    }
    for d in &dirs {
        if let Some(t) = read_halt(&d.join(HALT_FILE)) {
            return Err(format!(
                "{} stands (a dust battery's sticky stop): slot 11 does not arm beside it — reconcile the \
                 wallet and remove it first:\n{t}",
                d.join(HALT_FILE).display()
            ));
        }
        let lock = d.join(LOCK_FILE);
        let f = match std::fs::OpenOptions::new().read(true).open(&lock) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("{}: {e} — slot 11 does not arm past a lock it cannot probe", lock.display())),
        };
        // SAFETY: `flock` on a descriptor this function owns; it touches no
        // memory of ours. A lock taken here is released when `f` drops.
        let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(format!(
                "{} is held — a dust battery is running on this wallet: slot 11 does not arm beside it",
                lock.display()
            ));
        }
    }
    Ok(())
}

/// Write the sticky halt file (a temporary, fsync, rename).
fn write_halt(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("HALT.tmp");
    let r = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    sync_dir(path);
    r.map_err(|e| format!("{}: {e}", path.display()))
}

/// The exclusive lock held for the run (released when the process ends).
pub(crate) struct DustLock {
    _file: std::fs::File,
}

/// Take `path`'s exclusive `flock`, never waiting.
pub(crate) fn take_lock(path: &Path) -> Result<DustLock, String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // SAFETY: `flock` on a descriptor this function owns (kept open in the
    // returned guard); it touches no memory of ours.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(format!(
            "{} is locked by another pmj-dust (flock) — one battery at a time",
            path.display()
        ));
    }
    Ok(DustLock { _file: f })
}

/// The battery's record: `<dir>/<battery>.jsonl`, one line per event.
struct Out {
    file: std::fs::File,
    path: PathBuf,
}

impl Out {
    /// Open (append) `<dir>/<battery>.jsonl`; the directory is made 0700,
    /// the file 0600.
    fn open(dir: &Path, battery: &str) -> Result<Self, String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("{battery}.jsonl"));
        let file = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self { file, path })
    }

    /// One line, fsynced.
    fn line(&mut self, rec: &str) -> Result<(), String> {
        let mut l = String::with_capacity(rec.len() + 1);
        l.push_str(rec);
        l.push('\n');
        self.file
            .write_all(l.as_bytes())
            .and_then(|()| self.file.sync_data())
            .map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

// --- slot 11 live anywhere on the host ----------------------------------------

/// `strategy.conf`'s `ARM_LIVE` and `EXEC_TOML`, the file SOURCED in a
/// clean `zsh -f` (no environment of ours: no secret reaches it) as
/// `daily-restart.sh`'s `pmj_conf_get` and the wrapper read it. An absent
/// file is empty; a host without `zsh` refuses (fail closed).
pub(crate) fn conf_sourced(conf: &Path, zsh: &Path, home: &Path) -> Result<(String, String), String> {
    if !conf.exists() {
        return Ok((String::new(), String::new()));
    }
    let out = std::process::Command::new(zsh)
        .arg("-f")
        .arg("-c")
        .arg(r#"unset ARM_LIVE EXEC_TOML; set +u; . "$1" >/dev/null 2>&1; print -rN -- "${ARM_LIVE:-}" "${EXEC_TOML:-}""#)
        .arg("pmj-dust")
        .arg(conf)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "{} could not be sourced ({}: {e}) — refusing rather than assuming slot 11 is not \
                 armed",
                conf.display(),
                zsh.display()
            )
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    let parts: Vec<&str> = text.split('\0').collect();
    if parts.len() != 3 || !parts[2].is_empty() {
        return Err(format!(
            "{} did not read as KEY=VALUE lines (zsh printed an unexpected answer) — refusing",
            conf.display()
        ));
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// An `ARM_LIVE` spec names slot 11 as `daily-restart.sh`'s
/// `pmj_spec_names_11` reads it: comma-separated parts, every blank
/// removed, one leading `+` dropped, a decimal number compared as a number.
fn arm_live_names_11(spec: &str) -> bool {
    spec.split(',').any(|part| {
        let p: String = part.chars().filter(|c| !c.is_whitespace()).collect();
        let p = p.strip_prefix('+').unwrap_or(&p);
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p.trim_start_matches('0') == "11"
    })
}

/// An exec artifact's text marks `[exec.slot.11]` live, read as
/// `daily-restart.sh`'s `pmj_file_arms_11` awk reads it, and WIDER (risk
/// review round 2): a spaced header (`[ exec.slot.11 ]`) and a TOML
/// literal string (`mode = 'live'`) count too — the safe direction;
/// `daily-restart.sh`'s own parser is narrower (an open item recorded by
/// the coordinator; that script is not edited here). A file with a line
/// `enabled = 0` arms nothing (its `grep`).
fn exec_text_arms_11(text: &str) -> bool {
    let blank = |c: char| c == ' ' || c == '\t';
    if text.lines().any(|l| {
        l.trim_start_matches(|c: char| c.is_ascii_whitespace())
            .strip_prefix("enabled")
            .map(|r| r.trim_start_matches(|c: char| c.is_ascii_whitespace()))
            .and_then(|r| r.strip_prefix('='))
            .map(|r| r.trim_start_matches(|c: char| c.is_ascii_whitespace()))
            .is_some_and(|r| r.starts_with('0'))
    }) {
        return false;
    }
    let mut in11 = false;
    let mut found = false;
    for l in text.lines() {
        let t = l.trim_start_matches(blank);
        if let Some(inner) = t.strip_prefix('[') {
            in11 = inner.split_once(']').is_some_and(|(name, rest)| {
                let rest = rest.trim_start_matches(blank);
                name.trim_matches(blank) == "exec.slot.11" && (rest.is_empty() || rest.starts_with('#'))
            });
        }
        if in11
            && t.strip_prefix("mode")
                .map(|r| r.trim_start_matches(blank))
                .and_then(|r| r.strip_prefix('='))
                .map(|r| r.trim_start_matches(blank))
                .is_some_and(|r| r.starts_with("\"live\"") || r.starts_with("'live'"))
        {
            found = true;
        }
    }
    found
}

/// The engine on loopback, as far as slot 11 goes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StateSeen {
    /// Nothing answers.
    Unreachable,
    /// A paper boot, or no exec artifact, or slot 11's arm family absent.
    NotArmed,
    /// Slot 11's arm is registered: slot 11 is live.
    Armed,
}

/// Why a loopback GET failed.
enum GetErr {
    /// No connection.
    Unreachable,
    /// Connected, then failed or answered badly.
    Bad(String),
}

/// One plain-HTTP GET of `path` on `addr` (the notifier's shape): the
/// body of a 200.
fn http_get(addr: SocketAddr, path: &str) -> Result<Vec<u8>, GetErr> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).map_err(|_| GetErr::Unreachable)?;
    let bad = |what: &str| GetErr::Bad(format!("{addr}{path}: {what}"));
    s.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| bad(&e.to_string()))?;
    s.set_write_timeout(Some(Duration::from_secs(3))).map_err(|e| bad(&e.to_string()))?;
    let mut req = [0u8; 256];
    let n = core_net::http1::write_get_request(&mut req, b"127.0.0.1", path.as_bytes(), b"multivenue-pmj-dust")
        .map_err(|_| bad("the request does not fit"))?;
    s.write_all(&req[..n]).map_err(|e| bad(&e.to_string()))?;
    let mut buf: Vec<u8> = Vec::with_capacity(256 << 10);
    let mut chunk = [0u8; 64 << 10];
    loop {
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(k) => {
                buf.extend_from_slice(&chunk[..k]);
                if buf.len() > STATE_MAX {
                    return Err(bad("the answer is over its bound"));
                }
                if let core_net::http1::HttpResult::Complete {
                    framing: core_net::http1::BodyFraming::ContentLength(len),
                    body_start,
                    ..
                } = core_net::http1::read_response(&buf)
                {
                    if buf.len() >= body_start + usize::try_from(len).unwrap_or(usize::MAX) {
                        break;
                    }
                }
            }
            Err(e) => return Err(bad(&e.to_string())),
        }
    }
    match core_net::http1::read_response(&buf) {
        core_net::http1::HttpResult::Complete {
            status,
            body_start,
            framing,
            ..
        } => {
            if status != 200 {
                return Err(bad(&format!("HTTP {status}")));
            }
            let end = match framing {
                core_net::http1::BodyFraming::ContentLength(n) => {
                    body_start + usize::try_from(n).unwrap_or(usize::MAX)
                }
                core_net::http1::BodyFraming::CloseDelimited => buf.len(),
                core_net::http1::BodyFraming::Chunked => {
                    match core_net::http1::dechunk_in_place(&mut buf[body_start..]) {
                        core_net::http1::DechunkResult::Complete { length } => body_start + length,
                        _ => return Err(bad("a bad chunked body")),
                    }
                }
            };
            buf.get(body_start..end).map(<[u8]>::to_vec).ok_or_else(|| bad("a short body"))
        }
        _ => Err(bad("not an HTTP answer")),
    }
}

/// Is slot 11 armed on the engine at `addr`? Unreachable is not armed (the
/// files decide); reachable but unreadable refuses (fail closed).
fn state_check(addr: SocketAddr) -> Result<StateSeen, String> {
    let body = match http_get(addr, "/state") {
        Ok(b) => b,
        Err(GetErr::Unreachable) => return Ok(StateSeen::Unreachable),
        Err(GetErr::Bad(e)) => return Err(e),
    };
    let j = Jv::parse(&body).ok_or_else(|| format!("{addr}/state is not JSON"))?;
    let paper = j
        .path("boot.paper")
        .and_then(Jv::as_u64)
        .ok_or_else(|| format!("{addr}/state has no boot.paper"))?;
    let configured = j
        .path("exec.configured")
        .and_then(Jv::as_u64)
        .ok_or_else(|| format!("{addr}/state has no exec.configured"))?;
    if paper != 0 || configured == 0 {
        return Ok(StateSeen::NotArmed);
    }
    let m = match http_get(addr, "/metrics") {
        Ok(b) => b,
        Err(GetErr::Unreachable) => return Err(format!("{addr}/metrics did not answer after /state did")),
        Err(GetErr::Bad(e)) => return Err(e),
    };
    let text = String::from_utf8_lossy(&m);
    if text.contains("engine_pmj_arm_") || text.contains("engine_pmj_gw_") {
        Ok(StateSeen::Armed)
    } else {
        Ok(StateSeen::NotArmed)
    }
}

/// Where slot 11 could be armed on this host.
#[derive(Clone, Debug)]
struct HostCheck {
    /// `strategy.conf`.
    conf: PathBuf,
    /// `~/multivenue` (its `exec*.toml`).
    mv: PathBuf,
    /// `HOME` for the sourcing.
    home: PathBuf,
    /// Exec artifacts read besides the `exec*.toml` there.
    exec_tomls: Vec<PathBuf>,
    /// The engine's loopback port.
    state: SocketAddr,
    /// The shell `strategy.conf` is sourced with.
    zsh: PathBuf,
}

/// Why the host check did not pass.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HostErr {
    /// Slot 11 is (or may be) live: `ARM_LIVE`, an exec artifact (or one
    /// that cannot be read), `strategy.conf` that cannot be sourced, the
    /// engine's arm family on `/metrics`.
    Live(String),
    /// `/state` or `/metrics` answered but did not read: mid-battery an
    /// ordinary failure (risk review round 2, MINOR 5), at the preflight a
    /// refusal.
    Transient(String),
}

impl HostErr {
    fn text(&self) -> &str {
        match self {
            HostErr::Live(m) | HostErr::Transient(m) => m,
        }
    }
}

/// **Slot 11 live anywhere on the host** (module docs): `Err` names where.
fn host_live(h: &HostCheck) -> Result<(), HostErr> {
    let (arm_live, exec_in_force) = conf_sourced(&h.conf, &h.zsh, &h.home).map_err(HostErr::Live)?;
    if arm_live_names_11(&arm_live) {
        return Err(HostErr::Live(format!(
            "{}'s ARM_LIVE ({arm_live}) names slot 11 — the engine would trade the same wallet",
            h.conf.display()
        )));
    }
    let mut files: Vec<PathBuf> = h.exec_tomls.clone();
    if !exec_in_force.is_empty() {
        files.push(PathBuf::from(&exec_in_force));
    }
    if let Ok(rd) = std::fs::read_dir(&h.mv) {
        let mut names: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("exec") && n.ends_with(".toml"))
            })
            .collect();
        names.sort();
        files.extend(names);
    }
    files.dedup();
    for f in &files {
        if !f.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(f).map_err(|e| {
            HostErr::Live(format!(
                "{} is unreadable ({e}) — refusing rather than assuming slot 11 is not live",
                f.display()
            ))
        })?;
        if exec_text_arms_11(&text) {
            return Err(HostErr::Live(format!(
                "{} marks [exec.slot.11] live — the engine would trade the same wallet",
                f.display()
            )));
        }
    }
    match state_check(h.state).map_err(HostErr::Transient)? {
        StateSeen::Armed => Err(HostErr::Live(format!(
            "the engine on {} runs slot 11 live (its arm family is on /metrics) — the same wallet",
            h.state
        ))),
        StateSeen::Unreachable | StateSeen::NotArmed => Ok(()),
    }
}

/// `exec.HALT` beside each of `exec_tomls` does not name slot 11 and reads
/// whole (`pmj_live::check_halt_file`, the live boot's own reading).
fn exec_halt_clear(exec_tomls: &[PathBuf]) -> Result<(), String> {
    for p in exec_tomls {
        crate::pmj_live::check_halt_file(p)?;
    }
    Ok(())
}

// --- the seams: the network and the clock, the signer -------------------------

/// Why the window's market could not be had.
#[derive(Clone, Debug, PartialEq, Eq)]
enum WinErr {
    /// Transport, status or not yet listed: a failure.
    Fetch(String),
    /// The venue law refused it: a skip.
    Law(String),
}

/// Everything the battery reaches outside the process: Jupiter, the
/// window's market, the CLOB books, the clock, the host's arming. The
/// production [`LiveIo`]; the tests' scripted one.
trait DustIo {
    /// The Jupiter client.
    type Jup: JupClient;
    fn jup(&mut self) -> &mut Self::Jup;
    /// The wall clock, Unix ns.
    fn wall_ns(&mut self) -> u64;
    /// Sleep until wall time `t`.
    fn sleep_until(&mut self, t: u64);
    /// BTC's 5-minute window `window`, under the engine's discovery law.
    fn window(&mut self, window: u32) -> Result<PmWindowSpec, WinErr>;
    /// One read of `token`'s CLOB book.
    fn book(&mut self, token: &[u8]) -> Result<BookTop, String>;
    /// Start reading both tokens' books every [`BOOK_EVERY_NS`] over
    /// `[from, to]` (wall ns).
    fn series_start(&mut self, up: &[u8], down: &[u8], from: u64, to: u64);
    /// The series (Up's, Down's): at its end, or now when `early`.
    fn series_take(&mut self, early: bool) -> (Vec<BookSample>, Vec<BookSample>);
    /// Slot 11 is live nowhere on the host ([`host_live`]).
    fn host_live(&mut self) -> Result<(), HostErr>;
}

/// What signs a guarded build. The wallet's [`Ed25519Signer`] in a live
/// run; [`NoSigner`] in a dry run, which holds no key.
trait DustSigner {
    fn sign(&mut self, tx: &SolTx, buf: &mut [u8], idx: u8) -> Result<(), TxErr>;
}

impl DustSigner for Ed25519Signer {
    fn sign(&mut self, tx: &SolTx, buf: &mut [u8], idx: u8) -> Result<(), TxErr> {
        tx.sign_into(buf, idx, self)
    }
}

/// A dry run's signer: there is no key, so it cannot sign.
struct NoSigner;

impl DustSigner for NoSigner {
    fn sign(&mut self, _tx: &SolTx, _buf: &mut [u8], _idx: u8) -> Result<(), TxErr> {
        Err(TxErr::NotSigner)
    }
}

/// One `GET /book` on `c`.
fn book_on(c: &mut HttpsReq, token: &[u8]) -> Result<BookTop, String> {
    if token.is_empty() || !token.iter().all(u8::is_ascii_digit) {
        return Err(String::from("a token id that is not decimal"));
    }
    let mut target = Vec::with_capacity(16 + token.len());
    target.extend_from_slice(b"/book?token_id=");
    target.extend_from_slice(token);
    let (http, range) = c.request(Method::Get, &target, &[], 0).map_err(|e| format!("GET /book: {e}"))?;
    let body = c.resp().get(range).unwrap_or(&[]);
    if http != 200 {
        return Err(format!("GET /book answered HTTP {http}"));
    }
    parse_book(body)
}

type SeriesOut = (HttpsReq, Vec<BookSample>, Vec<BookSample>);

/// The book series' thread: both tokens every [`BOOK_EVERY_NS`] from
/// `from` to `to` (a tick missed while a read ran late is skipped, never
/// bunched).
fn series_run(mut c: HttpsReq, up: Vec<u8>, down: Vec<u8>, from: u64, to: u64, stop: &AtomicBool) -> SeriesOut {
    let mut a = Vec::new();
    let mut b = Vec::new();
    let mut t = from;
    while t <= to && !stop.load(Ordering::Acquire) {
        sleep_until_wall(t, Some(stop));
        if stop.load(Ordering::Acquire) {
            break;
        }
        for (tok, out) in [(&up, &mut a), (&down, &mut b)] {
            let t0 = wall_now();
            let top = book_on(&mut c, tok);
            out.push(BookSample { t_send: t0, t_ans: wall_now(), top });
        }
        t += BOOK_EVERY_NS;
        let now = wall_now();
        if t < now {
            t += (now - t).div_ceil(BOOK_EVERY_NS) * BOOK_EVERY_NS;
        }
    }
    (c, a, b)
}

/// The production IO: one keep-alive client each to Jupiter and the CLOB,
/// a third for the series' thread, and the engine's 5-minute discovery.
struct LiveIo {
    jup: HttpsReq,
    clob: HttpsReq,
    disc: Pm15Discovery,
    series_client: Option<HttpsReq>,
    series: Option<(std::thread::JoinHandle<SeriesOut>, Arc<AtomicBool>)>,
    host: HostCheck,
}

impl LiveIo {
    fn new(tls: Arc<ClientConfig>, host: HostCheck) -> Result<Self, String> {
        let jup = HttpsReq::new(JUP_HOST, 443, Arc::clone(&tls), HEAD_CAP, BODY_MIN, RESP_CAP)
            .map_err(|e| format!("the Jupiter client: {e:?}"))?;
        let clob = HttpsReq::new(CLOB_HOST, 443, Arc::clone(&tls), 512, 0, BOOK_RESP_CAP)
            .map_err(|e| format!("the CLOB client: {e:?}"))?;
        let series = HttpsReq::new(CLOB_HOST, 443, Arc::clone(&tls), 512, 0, BOOK_RESP_CAP)
            .map_err(|e| format!("the CLOB series client: {e:?}"))?;
        let disc = Pm15Discovery::new_for(PmPeriod::M5, tls, &[DUST_ASSET.as_bytes()])
            .map_err(|e| format!("the 5-minute discovery's clients (Gamma, CLOB): {e:?}"))?;
        Ok(Self {
            jup,
            clob,
            disc,
            series_client: Some(series),
            series: None,
            host,
        })
    }
}

impl DustIo for LiveIo {
    type Jup = HttpsReq;

    fn jup(&mut self) -> &mut HttpsReq {
        &mut self.jup
    }

    fn wall_ns(&mut self) -> u64 {
        wall_now()
    }

    fn sleep_until(&mut self, t: u64) {
        sleep_until_wall(t, None);
    }

    fn window(&mut self, window: u32) -> Result<PmWindowSpec, WinErr> {
        let mut spec = PmWindowSpec::EMPTY;
        match self.disc.fetch(0, window, &mut spec) {
            Ok(()) => Ok(spec),
            Err(FetchErr::Fetch) => Err(WinErr::Fetch(format!("BTC 5-minute window {window}: not fetched"))),
            Err(FetchErr::Law(e)) => Err(WinErr::Law(format!("BTC 5-minute window {window}: {e:?}"))),
        }
    }

    fn book(&mut self, token: &[u8]) -> Result<BookTop, String> {
        book_on(&mut self.clob, token)
    }

    fn series_start(&mut self, up: &[u8], down: &[u8], from: u64, to: u64) {
        let Some(c) = self.series_client.take() else {
            return;
        };
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let (u, d) = (up.to_vec(), down.to_vec());
        match std::thread::Builder::new()
            .name(String::from("pmj-dust-book"))
            .spawn(move || series_run(c, u, d, from, to, &flag))
        {
            Ok(h) => self.series = Some((h, stop)),
            Err(e) => eprintln!("pmj-dust: the book series' thread did not start ({e}) — no series this order"),
        }
    }

    fn series_take(&mut self, early: bool) -> (Vec<BookSample>, Vec<BookSample>) {
        let Some((h, stop)) = self.series.take() else {
            return (Vec::new(), Vec::new());
        };
        if early {
            stop.store(true, Ordering::Release);
        }
        match h.join() {
            Ok((c, a, b)) => {
                self.series_client = Some(c);
                (a, b)
            }
            Err(_) => (Vec::new(), Vec::new()),
        }
    }

    fn host_live(&mut self) -> Result<(), HostErr> {
        host_live(&self.host)
    }
}

// --- jup-gw's answer checks, the guard, the landing ------------------------------

/// The order asked.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Ask {
    market_id: u64,
    outcome: u8,
    deposit: u64,
    limit: u64,
}

/// A build that passed every check and the guard: unsigned.
struct Built {
    parsed: SolTx,
    tx_len: usize,
    ctx_len: usize,
    signer_idx: u8,
    order_key: [u8; KEY_LEN],
    position_key: [u8; KEY_LEN],
    view: CreateOrderView,
    contracts: u64,
    cost: u64,
    fee_total: u64,
    fee_venue: u64,
    fee_protocol: u64,
    max_buy: u64,
    last_valid_block_height: u64,
    market_hash: String,
}

/// Why a build was not signed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum BuildFail {
    /// Transport, a non-200, an answer that does not read: an ordinary
    /// failure (nothing built is trusted, nothing was signed).
    Ordinary(String),
    /// The answer is not the order asked, or the guard refused the
    /// transaction: a sticky stop, unsigned.
    Hostile(String),
}

/// `requiredSigners` names our wallet alone — or, with the dormant
/// co-signer amendment pinned, our wallet and the pinned key (`gw::signers_ok`).
fn signers_ok(a: &BuildAnswer, body: &[u8], wallet_b58: &[u8], cosigner_b58: &[u8]) -> bool {
    let r0 = a.required[0].of(body);
    match a.n_required {
        1 => r0 == wallet_b58,
        2 if !cosigner_b58.is_empty() => {
            let r1 = a.required[1].of(body);
            (r0 == wallet_b58 && r1 == cosigner_b58) || (r0 == cosigner_b58 && r1 == wallet_b58)
        }
        _ => false,
    }
}

/// **The build's answer → an unsigned, guarded transaction, or why not**
/// — `gw::prepare` line for line (its checks and their order), then
/// `tx::guard_order` with exactly the parameters jup-gw passes (the
/// deposit; the member's limit; the answer's order and position keys,
/// market hash and contracts; the reviewed guard). Signing is the
/// caller's, after the ledger reservation.
#[allow(clippy::too_many_arguments)]
fn check_build(
    http: u16,
    body: &[u8],
    ask: &Ask,
    wallet_b58: &[u8],
    cosigner_b58: &[u8],
    g: &TxGuard,
    tx: &mut [u8; TX_MAX],
    ctx: &mut [u8; CONTEXT_MAX],
) -> Result<Built, BuildFail> {
    use BuildFail::{Hostile, Ordinary};
    if http != 200 {
        return Err(Ordinary(jup_refusal("POST /prediction/v1/orders", http, body)));
    }
    let mut a = BuildAnswer::default();
    if let Err(e) = scan_build(body, &mut a) {
        return Err(Ordinary(format!("BUILD_UNREADABLE: the build answer did not read ({e:?})")));
    }
    if !a.keeper_order {
        return Err(Hostile(String::from(
            "NOT_KEEPER: the answer's execution.context.type is not create_order",
        )));
    }
    if a.max_buy_price_micro > ask.limit {
        return Err(Hostile(format!(
            "PRICE_CAP: the answer's maxBuyPriceUsd {} is above the limit {}",
            a.max_buy_price_micro, ask.limit
        )));
    }
    if !signers_ok(&a, body, wallet_b58, cosigner_b58) {
        return Err(Hostile(format!(
            "SIGNERS: requiredSigners names {} key(s), not our wallet alone (LAW PMJ-G; the \
             co-signer amendment is dormant unless pmj.toml pins it)",
            a.n_required
        )));
    }
    let want = format!("POLY-{}-{}", ask.market_id, ask.outcome);
    if a.market_id.of(body) != want.as_bytes()
        || a.is_yes != Some(true)
        || a.is_buy != Some(true)
        || a.user_pubkey.of(body) != wallet_b58
        || !a.has_cost
        || a.cost_micro > ask.deposit
    {
        return Err(Hostile(format!(
            "MISMATCH: the answer is not the order asked ({want}, a buy, our wallet, a cost within \
             {}): marketId {}, isYes {:?}, isBuy {:?}, cost {}",
            ask.deposit,
            lossy(a.market_id.of(body), 64),
            a.is_yes,
            a.is_buy,
            a.cost_micro
        )));
    }
    let Some(order_key) = key_of(a.order_pubkey.of(body)) else {
        return Err(Ordinary(String::from("BUILD_UNREADABLE: orderPubkey is not a key")));
    };
    let Some(position_key) = key_of(a.position_pubkey.of(body)) else {
        return Err(Ordinary(String::from("BUILD_UNREADABLE: positionPubkey is not a key")));
    };
    let c = a.context.of(body);
    if c.len() > CONTEXT_MAX {
        return Err(Ordinary(String::from("BUILD_UNREADABLE: the execution context is over its bound")));
    }
    let order_ask = OrderAsk {
        deposit: ask.deposit,
        limit_micro: ask.limit,
        order: order_key,
        position: position_key,
        market_hash: a.market_id_hash.of(body),
        contracts: a.contracts_micro,
    };
    // COPY: the server's base64 transaction decoded into the fixed 1 232 B
    // buffer the guard parses and our signature is written into (jup-gw's
    // `guard_and_sign`) — rejected: none.
    let n = base64_decode(a.transaction.of(body), &mut tx[..])
        .map_err(|_| Hostile(format!("GUARD {:?}: the transaction is not base64 within 1 232 B", TxErr::Length)))?;
    let parsed = SolTx::parse(&tx[..n]).map_err(|e| Hostile(format!("GUARD {e:?}: the transaction does not parse")))?;
    let (passed, view) =
        guard_order(&tx[..n], &parsed, g, &order_ask).map_err(|e| Hostile(format!("GUARD {e:?}")))?;
    ctx[..c.len()].copy_from_slice(c);
    Ok(Built {
        parsed,
        tx_len: n,
        ctx_len: c.len(),
        signer_idx: passed.signer_idx,
        order_key,
        position_key,
        view,
        contracts: a.contracts_micro,
        cost: a.cost_micro,
        fee_total: a.fee_total_micro,
        fee_venue: a.fee_venue_micro,
        fee_protocol: a.fee_protocol_micro,
        max_buy: a.max_buy_price_micro,
        last_valid_block_height: a.last_valid_block_height,
        market_hash: lossy(a.market_id_hash.of(body), 128),
    })
}

/// The execute's verdict (`gw::landing`): only `Success` lands; every other
/// answer came after the signed transaction may have left the host.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landing {
    Landed,
    /// In doubt: polled like a landed order.
    Doubt(String),
    /// Nothing left the host: the signed transaction dies here.
    NotSent(String),
}

impl Landing {
    fn word(&self) -> &'static str {
        match self {
            Landing::Landed => "landed",
            Landing::Doubt(_) => "in_doubt",
            Landing::NotSent(_) => "not_sent",
        }
    }
}

fn landing(ans: &Result<(u16, Vec<u8>), PostErr>) -> Landing {
    match ans {
        Err(PostErr { left_host: false, .. }) => Landing::NotSent(String::from("the execute never left the host")),
        Err(e) => Landing::Doubt(format!("the execute failed after it may have left the host ({e})")),
        Ok((200, b)) => {
            let mut a = ExecuteAnswer::default();
            match scan_execute(b, &mut a) {
                Ok(()) if a.status == ExecStatus::Success => Landing::Landed,
                Ok(()) => Landing::Doubt(format!("Failed: {}", lossy(a.error.of(b), 256))),
                Err(_) => Landing::Doubt(String::from("the execute's answer did not read")),
            }
        }
        Ok((h, b)) => Landing::Doubt(jup_refusal("POST /prediction/v1/execute", *h, b)),
    }
}

/// One status read.
enum PollRead {
    /// Terminal, with its fill numbers (`StatusAnswer::record`).
    Terminal(StatusAnswer),
    /// Read, not terminal (or terminal without readable numbers).
    Open,
    /// Jupiter's own `invalid_request_error` 404: not known (yet).
    NotFound,
    /// Nothing that reads.
    Unread,
}

struct Poll {
    t0: u64,
    t1: u64,
    http: Option<u16>,
    body: Vec<u8>,
    read: PollRead,
}

// --- the run --------------------------------------------------------------------

/// What a run is told (the preflight's findings).
struct RunCfg {
    battery: String,
    orders: u32,
    dry_run: bool,
    seed: u64,
    min_sol: u64,
    /// `exec.HALT` is watched beside each (the boot's exec artifact and
    /// `~/multivenue/exec.toml`).
    exec_tomls: Vec<PathBuf>,
    /// `pmj-dust.HALT`: beside the boot's exec artifact, then in
    /// `~/multivenue` — every one watched, every one written on a sticky
    /// stop.
    halts: Vec<PathBuf>,
    /// The run's start (the preflight's clock), wall ns.
    t_start: u64,
    run_max_ns: u64,
}

/// What one attempt came to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Step {
    /// An order: reserved and signed (a dry run: guarded).
    Placed,
    /// The window was skipped.
    Skipped,
    /// An ordinary failure (counts toward [`BUILD_FAILS_MAX`]).
    Failed,
}

/// The positions read: the unknown keys and the claimable ones.
#[derive(Default)]
struct PosRead {
    unknown: Vec<String>,
    /// `(key, contracts micro, payout micro)`.
    claimable: Vec<(String, u64, u64)>,
    raw: String,
}

/// A stop found while finishing an order (its record is written first).
enum Halt {
    Sticky(String),
    Stopped(String),
}

/// One battery run. Holds no secret but a borrow of the API key (the
/// caller wipes it); derives nothing that could print it.
struct Run<'a, I: DustIo, S: DustSigner> {
    io: I,
    signer: S,
    key: &'a [u8],
    cfg: &'a RunCfg,
    guard: &'a PmjGuard,
    wallet_b58: String,
    mint_b58: String,
    cosigner_b58: String,
    ledger: Ledger,
    out: Out,
    tx: Box<[u8; TX_MAX]>,
    ctx: Box<[u8; CONTEXT_MAX]>,
    /// The last balance read with no order in flight, or after one.
    last_after: Option<u64>,
    /// The signer's calls (the record and the tests).
    signs: u32,
    /// When the last execute was sent (wall ns).
    last_exec_ns: u64,
}

impl<'a, I: DustIo, S: DustSigner> Run<'a, I, S> {
    fn new(io: I, signer: S, key: &'a [u8], cfg: &'a RunCfg, guard: &'a PmjGuard, ledger: Ledger, out: Out) -> Self {
        let g = &guard.guard;
        Self {
            io,
            signer,
            key,
            cfg,
            guard,
            wallet_b58: b58(&g.wallet),
            mint_b58: b58(&guard.mint),
            cosigner_b58: if g.has_cosigner { b58(&g.cosigner) } else { String::new() },
            ledger,
            out,
            tx: Box::new([0; TX_MAX]),
            ctx: Box::new([0; CONTEXT_MAX]),
            last_after: None,
            signs: 0,
            last_exec_ns: 0,
        }
    }

    fn write(&mut self, line: &str) -> Result<(), DustFail> {
        self.out
            .line(line)
            .map_err(|e| Stopped(format!("the battery's record could not be written: {e}")))
    }

    /// A sticky stop: `pmj-dust.HALT` written with the reason, a `stop` line.
    fn sticky(&mut self, why: &str) -> DustFail {
        let at = self.io.wall_ns();
        let text = format!(
            "pmj-dust HALT {} battery={}\nreason: {why}\nNothing signs while this file exists. Reconcile \
             the wallet (its orders, positions and balance) by hand against {}, then remove this file \
             (risk-policy PMJ J17).\n",
            utc_stamp(at),
            self.cfg.battery,
            self.out.path.display()
        );
        let mut w = Ok(());
        for h in &self.cfg.halts {
            if let Err(e) = write_halt(h, &text) {
                w = Err(e);
            }
        }
        let mut r = Rec::new();
        r.str("battery", &self.cfg.battery).u64("t_ns", at).str("stop", "sticky").str("why", why);
        if let Err(e) = &w {
            r.str("halt_write_error", e);
        }
        let _ = self.out.line(&r.done("stop"));
        match w {
            Ok(()) => Sticky(String::from(why)),
            Err(e) => Sticky(format!(
                "{why} — AND {HALT_FILE} could not be written ({e}): do not run pmj-dust again before \
                 reconciling"
            )),
        }
    }

    /// A stop that is not sticky: a `stop` line.
    fn stopped(&mut self, why: &str) -> DustFail {
        let mut r = Rec::new();
        r.str("battery", &self.cfg.battery).u64("t_ns", self.io.wall_ns()).str("stop", "stopped").str("why", why);
        let _ = self.out.line(&r.done("stop"));
        Stopped(String::from(why))
    }

    /// The halt files between orders and before a signature (cheap: file
    /// reads only): `exec.HALT` naming slot 11, or a `pmj-dust.HALT` that
    /// stands (fail closed) — a sticky stop, the standing halt kept as it
    /// is (never rewritten over).
    fn halts_clear(&mut self, when: &str) -> Result<(), DustFail> {
        if let Err(e) = exec_halt_clear(&self.cfg.exec_tomls) {
            return Err(self.sticky(&format!("{when}: {e}")));
        }
        if let Some((h, t)) = halt_standing(&self.cfg.halts) {
            let why = format!("{when}: {} stands:\n{t}", h.display());
            let mut r = Rec::new();
            r.str("battery", &self.cfg.battery).u64("t_ns", self.io.wall_ns()).str("stop", "sticky").str("why", &why);
            let _ = self.out.line(&r.done("stop"));
            return Err(Sticky(why));
        }
        Ok(())
    }

    fn balances(&mut self) -> Result<(u64, u64), String> {
        read_balances(self.io.jup(), self.key, &self.wallet_b58, &self.mint_b58)
    }

    /// One `GET /orders/status/{order}` (`gw::read_status`'s reading).
    fn poll_once(&mut self, order_b58: &str) -> Poll {
        let mut path = [0u8; PATH_MAX];
        let t0 = self.io.wall_ns();
        let Some(pn) = status_path(&mut path, order_b58.as_bytes()) else {
            return Poll { t0, t1: t0, http: None, body: b"the status path does not fit".to_vec(), read: PollRead::Unread };
        };
        let key: Header<'_> = (b"x-api-key", self.key);
        let res = self.io.jup().request(Method::Get, &path[..pn], &[key], 0);
        let t1 = self.io.wall_ns();
        let (http, range) = match res {
            Ok(x) => x,
            Err(e) => {
                return Poll { t0, t1, http: None, body: format!("transport: {e}").into_bytes(), read: PollRead::Unread };
            }
        };
        let body = self.io.jup().resp().get(range).unwrap_or(&[]).to_vec();
        let read = match http {
            200 => {
                let mut a = StatusAnswer::default();
                match scan_status(&body, &mut a) {
                    Ok(()) if a.record().is_some() => PollRead::Terminal(a),
                    Ok(()) => PollRead::Open,
                    Err(_) => PollRead::Unread,
                }
            }
            404 => {
                let mut e = ErrorAnswer::default();
                if scan_error(&body, &mut e).is_ok() && e.kind == ErrKind::InvalidRequest {
                    PollRead::NotFound
                } else {
                    PollRead::Unread
                }
            }
            _ => PollRead::Unread,
        };
        Poll { t0, t1, http: Some(http), body, read }
    }

    /// `GET /positions` (first page, read-only — J15's reading): positions
    /// with contracts, not claimed, whose key no ledger reservation (nor
    /// `current`) carries are unknown; claimable ones are listed.
    fn positions(&mut self, current: Option<&str>) -> Result<PosRead, String> {
        let target = format!("{P_POSITIONS_Q}{}", self.wallet_b58);
        let key: Header<'_> = (b"x-api-key", self.key);
        let (http, range) = self
            .io
            .jup()
            .request(Method::Get, target.as_bytes(), &[key], 0)
            .map_err(|e| format!("GET /prediction/v1/positions: {e}"))?;
        let body = self.io.jup().resp().get(range).unwrap_or(&[]).to_vec();
        if http != 200 {
            return Err(jup_refusal("GET /prediction/v1/positions", http, &body));
        }
        let known = self.ledger.positions();
        let mut out = PosRead::default();
        scan_positions(&body, &mut |p| {
            let text = p.pubkey.of(&body);
            let k58 = key_of(text).map(|k| b58(&k));
            if p.claimed {
                return;
            }
            if p.claimable {
                out.claimable.push((k58.clone().unwrap_or_else(|| lossy(text, 64)), p.contracts_micro, p.payout_micro));
            }
            if p.contracts_micro == 0 {
                return;
            }
            let ours = k58.as_deref().is_some_and(|k| known.contains(k) || current == Some(k));
            if !ours {
                out.unknown.push(lossy(text, 64));
            }
        })
        .map_err(|e| format!("the positions answer did not read ({e:?})"))?;
        out.raw = lossy(&body, RAW_MAX);
        Ok(out)
    }

    /// The run's start: requirement (a), the run line, the orphans, the
    /// positions.
    fn start(&mut self) -> Result<(), DustFail> {
        let t = self.io.wall_ns();
        let (dep, sol) = self
            .balances()
            .map_err(|e| Refused(format!("requirement (a): the wallet read: {e}")))?;
        judge_wallet(dep, sol, DUST_DAY_CAP_MICRO as i64, self.cfg.min_sol)
            .map_err(|e| Refused(format!("requirement (a) at the dust day cap ($50): {e}")))?;
        if sol > DUST_MAX_SOL_START_LAMPORTS {
            return Err(Refused(format!(
                "the wallet holds {sol} lamports of SOL, over the dust ceiling {DUST_MAX_SOL_START_LAMPORTS} \
                 (R-PMJ-S80, PROVISIONAL: a dust day's {DUST_DAY_ORDERS} orders at \
                 {DUST_SOL_FALL_MAX_LAMPORTS} each above the min_sol_lamports floor) — move the excess out first"
            )));
        }
        self.last_after = Some(dep);
        let g = &self.guard.guard;
        let mut r = Rec::new();
        r.str("battery", &self.cfg.battery)
            .u64("run_seed", self.cfg.seed)
            .u64("orders_asked", u64::from(self.cfg.orders))
            .bool("dry_run", self.cfg.dry_run)
            .str("path", "execute")
            .u64("t_ns", t)
            .str("t_utc", &utc_stamp(t))
            .str("wallet", &self.wallet_b58)
            .str("deposit_mint", &self.mint_b58)
            .str("deposit_mint_name", self.guard.mint_name)
            .u64("guard_programs", u64::from(g.n_programs))
            .u64("guard_alts", u64::from(g.n_alts))
            .bool("guard_cosigner", g.has_cosigner)
            .u64("build_extra", self.guard.build_extra.len() as u64)
            .u64("deposit_micro", DUST_DEPOSIT_MICRO)
            .u64("day_cap_micro", DUST_DAY_CAP_MICRO)
            .u64("battery_max", u64::from(DUST_BATTERY_MAX))
            .u64("day_used_micro", self.ledger.day_used(&utc_date(t / NS)))
            .u64("battery_reserved", u64::from(self.ledger.battery_orders(&self.cfg.battery)))
            .u64("balance_micro", dep)
            .u64("sol_lamports", sol)
            .u64("min_sol_lamports", self.cfg.min_sol);
        self.write(&r.done("run"))?;
        self.reconcile_orphans()?;
        match self.positions(None) {
            Ok(p) if !p.unknown.is_empty() => {
                Err(self.sticky(&format!("at start, positions no reservation of ours carries: {:?}", p.unknown)))
            }
            Ok(_) => Ok(()),
            Err(e) => Err(Refused(format!("the positions read at start: {e}"))),
        }
    }

    /// Reservations an earlier run never closed (a crash between the
    /// reservation and the order's end): one status read each — terminal
    /// with its numbers closes it `DONE`; anything else closes it `ORPHAN`
    /// and stops sticky.
    fn reconcile_orphans(&mut self) -> Result<(), DustFail> {
        for row in self.ledger.orphans() {
            let p = self.poll_once(&row.order);
            let mut r = Rec::new();
            r.str("battery", &row.battery)
                .u64("order_idx", u64::from(row.idx))
                .str("order_pubkey", &row.order)
                .str("position_pubkey", &row.position)
                .u64("reserved_ns", row.ns)
                .u64("t_send_ns", p.t0)
                .u64("t_ans_ns", p.t1)
                .body("raw", &p.body, RAW_MAX);
            let now = self.io.wall_ns();
            let close = |kind: RowKind, note: &str| Row {
                kind,
                date: utc_date(now / NS),
                ns: now,
                note: String::from(note),
                ..row.clone()
            };
            if let PollRead::Terminal(a) = p.read {
                r.str("state", &format!("{:?}", a.state)).u64("filled_micro", a.filled_micro).u64("avg_px_micro", a.avg_px_micro);
                self.ledger
                    .append(close(RowKind::Done, "reconciled at a later start"))
                    .map_err(|e| Stopped(format!("the ledger could not be written: {e}")))?;
                self.write(&r.done("orphan"))?;
                continue;
            }
            let _ = self.ledger.append(close(RowKind::Orphan, "no terminal status at a later start"));
            self.write(&r.done("orphan"))?;
            return Err(self.sticky(&format!(
                "order {} (battery {}, idx {}, reserved {}) has no terminal record — an earlier run \
                 ended between its reservation and its end; its status does not read terminal",
                row.order,
                row.battery,
                row.idx,
                utc_stamp(row.ns)
            )));
        }
        Ok(())
    }

    /// The battery (module docs, "One order"): `Ok` with the number of
    /// orders when `orders` were placed.
    fn battery(&mut self) -> Result<u32, DustFail> {
        let t_end = self.cfg.t_start.saturating_add(self.cfg.run_max_ns);
        self.start()?;
        let mut rng = Rng(self.cfg.seed);
        let mut placed = 0u32;
        let mut fails = 0u32;
        let mut floor = 0u32;
        while placed < self.cfg.orders {
            let now = self.io.wall_ns();
            let reserved = self.ledger.battery_orders(&self.cfg.battery);
            if !self.cfg.dry_run {
                if reserved >= DUST_BATTERY_MAX {
                    return Err(self.stopped(&format!("battery {} holds its {DUST_BATTERY_MAX} orders", self.cfg.battery)));
                }
                let used = self.ledger.day_used(&utc_date(now / NS));
                if used + DUST_DEPOSIT_MICRO > DUST_DAY_CAP_MICRO {
                    return Err(self.stopped(&format!(
                        "the day cap: {} of {} reserved on {} UTC",
                        usd(used as i64),
                        usd(DUST_DAY_CAP_MICRO as i64),
                        utc_date(now / NS)
                    )));
                }
            }
            self.halts_clear("mid-battery")?;
            let idx = if self.cfg.dry_run { placed } else { reserved };
            match self.attempt(idx, idx % TAU_CELLS, &mut rng, &mut floor, t_end)? {
                Step::Placed => {
                    placed += 1;
                    fails = 0;
                }
                Step::Skipped => {}
                Step::Failed => {
                    fails += 1;
                    if fails >= BUILD_FAILS_MAX {
                        return Err(self.stopped(&format!(
                            "{BUILD_FAILS_MAX} consecutive failures (a build, a discovery fetch, a \
                             balance read before the decision, or an execute that never left the host)"
                        )));
                    }
                }
            }
        }
        Ok(placed)
    }

    /// One window (module docs, "One order").
    #[allow(clippy::too_many_lines)]
    fn attempt(&mut self, idx: u32, cell: u32, rng: &mut Rng, floor: &mut u32, t_end: u64) -> Result<Step, DustFail> {
        let tau = tau_of(cell, rng);
        let now = self.io.wall_ns();
        let mut w = pm5_window_at(now / NS).max(*floor);
        while pm5_window_end_ns(w).saturating_sub(tau) < now.saturating_add(LEAD_NS) {
            w += 1;
        }
        *floor = w + 1;
        let close = pm5_window_end_ns(w);
        if close + DEADLINE_AFTER_CLOSE_NS + TAIL_WORST_NS > t_end {
            return Err(self.stopped(
                "the ≤ 2 h law: the next order's worst case (its deadline + TAIL_WORST_NS) would end \
                 past this run's two hours — continue the battery with another run",
            ));
        }
        // The halt files at the top of every attempt.
        self.halts_clear("mid-battery")?;
        let td = close - tau;
        let mut r = Rec::new();
        r.str("battery", &self.cfg.battery)
            .u64("run_seed", self.cfg.seed)
            .u64("order_idx", u64::from(idx))
            .str("path", "execute")
            .bool("dry_run", self.cfg.dry_run)
            .u64("tau_cell", u64::from(cell))
            .u64("tau_ns", tau)
            .u64("window", u64::from(w))
            .u64("window_close_ns", close)
            .u64("t_decision_sched_ns", td);
        // 1. The window's market, from the engine's discovery.
        let spec = match self.io.window(w) {
            Ok(s) => s,
            Err(WinErr::Law(e)) => {
                r.str("skip", "the venue law refused the window").str("why", &e);
                self.write(&r.done("skip"))?;
                return Ok(Step::Skipped);
            }
            Err(WinErr::Fetch(e)) => {
                r.str("fail", "discovery").str("why", &e);
                self.write(&r.done("fail"))?;
                return Ok(Step::Failed);
            }
        };
        let up = lossy(spec.up.as_bytes(), 96);
        let down = lossy(spec.down.as_bytes(), 96);
        r.u64("market_id", spec.market_id)
            .str("condition_id", &lossy(spec.condition_id(), 66))
            .str("token_up", &up)
            .str("token_down", &down)
            .i64("tick_1e6", spec.tick_1e6)
            .i64("min_size_1e6", spec.min_size_1e6);
        if spec.market_id == 0 {
            r.str("skip", "the window's market has no Gamma id (Jupiter names markets by it)");
            self.write(&r.done("skip"))?;
            return Ok(Step::Skipped);
        }
        // 2. The balances before (the debit check's "before").
        self.io.sleep_until(td.saturating_sub(BALANCE_BEFORE_NS));
        let tb = self.io.wall_ns();
        let before = match self.balances() {
            Ok(v) => v,
            Err(e) => {
                r.str("fail", "the balances before").str("why", &e);
                self.write(&r.done("fail"))?;
                return Ok(Step::Failed);
            }
        };
        r.u64("t_balance_before_ns", tb).u64("balance_before_micro", before.0).u64("sol_before", before.1);
        if let Some(prev) = self.last_after {
            if before.0.saturating_add(DEBIT_SLACK_MICRO) < prev {
                let why = format!(
                    "the debit check: the deposit mint's balance fell from {} to {} with no order in flight",
                    usd(prev as i64),
                    usd(before.0 as i64)
                );
                r.str("stop", &why);
                self.write(&r.done("fail"))?;
                return Err(self.sticky(&why));
            }
        }
        // The wallet's SOL before the order (risk review round 3, MINOR 3;
        // the read just made, no other request): one order's bound above
        // the floor, or the battery stops — not sticky.
        let sol_floor = self.cfg.min_sol.saturating_add(DUST_SOL_FALL_MAX_LAMPORTS);
        if before.1 < sol_floor {
            let why = format!(
                "the wallet's SOL {} lamports is under one order's bound {DUST_SOL_FALL_MAX_LAMPORTS} above \
                 pmj.toml min_sol_lamports {} ({sol_floor}) — fund SOL (at most {DUST_MAX_SOL_START_LAMPORTS} \
                 at a run's start) and run again",
                before.1, self.cfg.min_sol
            );
            r.str("stop", &why);
            self.write(&r.done("fail"))?;
            return Err(self.stopped(&why));
        }
        // The host check (a zsh spawn, /state, /metrics) here, the last step
        // before the decision: never inside the decision → execute path it
        // would lengthen (risk review round 2, MINOR 5). Slot 11 live is a
        // sticky stop; a /state or /metrics answer that does not read is an
        // ordinary failure.
        let t_h0 = self.io.wall_ns();
        let host = self.io.host_live();
        r.u64("host_check_ns", self.io.wall_ns().saturating_sub(t_h0));
        match host {
            Ok(()) => {}
            Err(HostErr::Live(e)) => {
                r.str("stop", &e);
                self.write(&r.done("fail"))?;
                return Err(self.sticky(&format!("mid-battery: {e}")));
            }
            Err(HostErr::Transient(e)) => {
                self.last_after = Some(before.0);
                r.str("fail", "the host check").str("why", &e);
                self.write(&r.done("fail"))?;
                return Ok(Step::Failed);
            }
        }
        // 3. The book series, and the decision's two books.
        self.io.sleep_until(td.saturating_sub(BOOK_BEFORE_NS));
        self.io.series_start(spec.up.as_bytes(), spec.down.as_bytes(), td.saturating_sub(BOOK_BEFORE_NS), td + BOOK_AFTER_NS);
        self.io.sleep_until(td);
        let t_u0 = self.io.wall_ns();
        let bu = self.io.book(spec.up.as_bytes());
        let t_u1 = self.io.wall_ns();
        let bd = self.io.book(spec.down.as_bytes());
        let t_d1 = self.io.wall_ns();
        book_fields(&mut r, "book_up", t_u0, t_u1, &bu);
        book_fields(&mut r, "book_down", t_u1, t_d1, &bd);
        let choice = match (&bu, &bd) {
            (Ok(u), Ok(d)) => choose(u.ask, d.ask),
            _ => Choice::Skip("a decision book did not read"),
        };
        let t_decision = self.io.wall_ns();
        r.u64("t_decision_ns", t_decision);
        let (outcome, ask, limit) = match choice {
            Choice::Buy { outcome, ask, limit } => (outcome, ask, limit),
            Choice::Skip(why) => {
                let _ = self.io.series_take(true);
                self.last_after = Some(before.0);
                r.str("skip", why);
                self.write(&r.done("skip"))?;
                return Ok(Step::Skipped);
            }
        };
        let cmax = contracts_max(limit);
        if i64::try_from(cmax).unwrap_or(i64::MAX) < spec.min_size_1e6 {
            let _ = self.io.series_take(true);
            self.last_after = Some(before.0);
            r.str("skip", "the deposit at the limit buys under the market's minimum size");
            self.write(&r.done("skip"))?;
            return Ok(Step::Skipped);
        }
        let (token, other) = if outcome == 0 { (&up, &down) } else { (&down, &up) };
        r.str("side", if outcome == 0 { "Up" } else { "Down" })
            .u64("outcome", u64::from(outcome))
            .str("token_id", token)
            .str("token_other", other)
            .i64("ask_decision_1e6", ask)
            .u64("deposit_micro", DUST_DEPOSIT_MICRO)
            .i64("limit_micro", limit)
            .u64("contracts_max_micro", cmax);
        // 4. The build: jup-gw's body, jup-gw's checks, the guard.
        let ask_at = Ask {
            market_id: spec.market_id,
            outcome,
            deposit: DUST_DEPOSIT_MICRO,
            limit: u64::try_from(limit).unwrap_or(0),
        };
        let fin = |s: &mut Self| {
            let _ = s.io.series_take(true);
            s.last_after = Some(before.0);
        };
        let Some(n) = order_body(
            self.io.jup().body_mut(),
            self.wallet_b58.as_bytes(),
            spec.market_id,
            if outcome == 0 { Outcome::Up } else { Outcome::Down },
            DUST_DEPOSIT_MICRO,
            self.mint_b58.as_bytes(),
            &self.guard.build_extra,
            ask_at.limit,
        ) else {
            fin(self);
            r.str("fail", "the build body does not fit its window");
            self.write(&r.done("fail"))?;
            return Ok(Step::Failed);
        };
        r.str("build_body", &lossy(&self.io.jup().body_mut()[..n], 1_024));
        let key: Header<'_> = (b"x-api-key", self.key);
        let t_b0 = self.io.wall_ns();
        let res = self.io.jup().request(Method::Post, P_ORDERS, &[key], n);
        let t_b1 = self.io.wall_ns();
        r.u64("t_build_send_ns", t_b0).u64("t_build_ans_ns", t_b1);
        let (http, body) = match res {
            Ok((h, range)) => (h, self.io.jup().resp().get(range).unwrap_or(&[]).to_vec()),
            Err(e) => {
                fin(self);
                r.str("fail", "build transport").str("why", &e.to_string());
                self.write(&r.done("fail"))?;
                return Ok(Step::Failed);
            }
        };
        r.u64("build_http", u64::from(http)).body("build_raw", &body, RAW_BUILD_MAX);
        let checked = check_build(
            http,
            &body,
            &ask_at,
            self.wallet_b58.as_bytes(),
            self.cosigner_b58.as_bytes(),
            &self.guard.guard,
            &mut self.tx,
            &mut self.ctx,
        );
        let b = match checked {
            Ok(b) => b,
            Err(BuildFail::Ordinary(e)) => {
                fin(self);
                r.str("fail", "build").str("why", &e);
                self.write(&r.done("fail"))?;
                return Ok(Step::Failed);
            }
            Err(BuildFail::Hostile(e)) => {
                fin(self);
                r.str("guard", &e).bool("signed", false);
                self.write(&r.done("hostile"))?;
                return Err(self.sticky(&format!("the build was refused unsigned — {e}")));
            }
        };
        let order_b58 = b58(&b.order_key);
        let pos_b58 = b58(&b.position_key);
        let n_sigs = u64::from(b.parsed.n_sigs);
        r.str("guard", "pass")
            .u64("n_signatures", n_sigs)
            .str("order_pubkey", &order_b58)
            .str("position_pubkey", &pos_b58)
            .u64("contracts_micro", b.contracts)
            .u64("signed_contracts_micro", b.view.contracts)
            .u64("signed_cap_micro", b.view.max_fill_price_usd)
            .u64("signed_deposit_micro", b.view.deposit_amount)
            .u64("create_order_version", u64::from(b.view.version))
            .u64("answer_max_buy_micro", b.max_buy)
            .u64("cost_micro", b.cost)
            .u64("fee_est_total_micro", b.fee_total)
            .u64("fee_est_venue_micro", b.fee_venue)
            .u64("fee_est_protocol_micro", b.fee_protocol)
            .str("market_id_hash", &b.market_hash)
            .u64("last_valid_block_height", b.last_valid_block_height)
            .u64("signer_idx", u64::from(b.signer_idx));
        r.u64("t_guard_done_ns", self.io.wall_ns());
        // 5. The halt files once more (file reads only), before any signature.
        if let Err(e) = self.halts_clear("before signing") {
            fin(self);
            r.bool("signed", false).str("stop", e.message());
            self.write(&r.done("order"))?;
            return Err(e);
        }
        // Late (risk review round 3, MINOR 1): jup-gw's LATE rule — the
        // execute leaves no later than the window's close less
        // `gw::MIN_LEAD_NS_DEFAULT` — and the tail's bound, the scheduled
        // decision + `SEND_LATE_MAX_NS`. Checked here, before the
        // reservation, and again after its fsyncs, just before the
        // signature. Nothing signed either way: an ordinary failure.
        let late = |t: u64| -> Option<String> {
            if t.saturating_add(MIN_LEAD_NS_DEFAULT) >= close {
                Some(format!(
                    "the execute would leave {} ms before the window's close, inside jup-gw's {} ms LATE \
                     lead (gw::MIN_LEAD_NS_DEFAULT)",
                    close.saturating_sub(t) / 1_000_000,
                    MIN_LEAD_NS_DEFAULT / 1_000_000
                ))
            } else if t > td.saturating_add(SEND_LATE_MAX_NS) {
                Some(format!(
                    "the execute would leave {} ms after the scheduled decision, past the {} ms bound \
                     (SEND_LATE_MAX_NS, the tail's)",
                    t.saturating_sub(td) / 1_000_000,
                    SEND_LATE_MAX_NS / 1_000_000
                ))
            } else {
                None
            }
        };
        let t_late = self.io.wall_ns();
        if let Some(why) = late(t_late) {
            fin(self);
            r.bool("signed", false).str("fail", "late").str("why", &why);
            self.write(&r.done("fail"))?;
            return Ok(Step::Failed);
        }
        if self.cfg.dry_run {
            r.bool("signed", false).str("note", "dry run: built and guarded, never signed, never landed");
            let halt = self.finish(&mut r, td, outcome, before, 0, None);
            self.write(&r.done("order"))?;
            return match halt {
                Some(Halt::Sticky(why)) => Err(self.sticky(&why)),
                Some(Halt::Stopped(why)) => Err(self.stopped(&why)),
                None => Ok(Step::Placed),
            };
        }
        // 6. The reservation, fsynced before the signature.
        let t_res = self.io.wall_ns();
        let row = Row {
            kind: RowKind::Reserved,
            date: utc_date(t_res / NS),
            ns: t_res,
            battery: self.cfg.battery.clone(),
            idx,
            deposit: DUST_DEPOSIT_MICRO,
            order: order_b58.clone(),
            position: pos_b58.clone(),
            market_id: spec.market_id,
            outcome,
            note: String::new(),
        };
        if let Err(e) = self.ledger.append(row.clone()) {
            fin(self);
            r.bool("signed", false).str("stop", &e);
            self.write(&r.done("order"))?;
            return Err(self.stopped(&format!("the reservation could not be written (nothing signed): {e}")));
        }
        let close_row = |kind: RowKind, at: u64, note: String| Row {
            kind,
            date: utc_date(at / NS),
            ns: at,
            note,
            ..row.clone()
        };
        let mut rr = Rec::new();
        rr.str("battery", &self.cfg.battery)
            .u64("order_idx", u64::from(idx))
            .u64("reserved_ns", t_res)
            .str("order_pubkey", &order_b58)
            .str("position_pubkey", &pos_b58)
            .u64("deposit_micro", DUST_DEPOSIT_MICRO);
        if let Err(e) = self.out.line(&rr.done("reserve")) {
            // Nothing is signed: the series is joined, the reservation
            // closed, the run stopped.
            fin(self);
            let now = self.io.wall_ns();
            let _ = self.ledger.append(close_row(RowKind::Unsent, now, String::from("the record could not be written; nothing signed")));
            return Err(self.stopped(&format!("the battery's record could not be written (nothing signed): {e}")));
        }
        r.u64("reserved_ns", t_res);
        // Late, once more: after the fsyncs, just before the signature. The
        // reservation is closed UNSENT (it still counts toward the day).
        let t_late = self.io.wall_ns();
        if let Some(why) = late(t_late) {
            fin(self);
            let _ = self.ledger.append(close_row(RowKind::Unsent, t_late, format!("late, nothing signed: {why}")));
            r.bool("signed", false).str("fail", "late").str("why", &why);
            self.write(&r.done("fail"))?;
            return Ok(Step::Failed);
        }
        // 7. Our signature, in its slot.
        let signed = self.signer.sign(&b.parsed, &mut self.tx[..b.tx_len], b.signer_idx);
        self.signs += 1;
        let t_sign = self.io.wall_ns();
        if let Err(e) = signed {
            fin(self);
            let _ = self.ledger.append(close_row(RowKind::Unsent, t_sign, format!("the signer refused: {e:?}")));
            r.bool("signed", false).str("stop", &format!("the signer refused: {e:?}"));
            self.write(&r.done("order"))?;
            return Err(self.stopped(&format!("the signer refused ({e:?}): nothing was sent")));
        }
        let so = b.parsed.sigs_off as usize + usize::from(b.signer_idx) * SIG_LEN;
        let mut sig = [0u8; SIG_LEN];
        sig.copy_from_slice(&self.tx[so..so + SIG_LEN]);
        let mut sig58 = [0u8; 96];
        let sig_len = core_crypto::base58_encode(&sig, &mut sig58).unwrap_or(0);
        r.bool("signed", true).u64("t_sign_ns", t_sign).str("tx_signature", &lossy(&sig58[..sig_len], 96));
        // 8. The execute (path A).
        let Some(n) = execute_body(self.io.jup().body_mut(), &self.tx[..b.tx_len], &self.ctx[..b.ctx_len], t_decision) else {
            fin(self);
            let _ = self.ledger.append(close_row(RowKind::Unsent, t_sign, String::from("the execute body does not fit")));
            r.str("stop", "the execute body does not fit");
            self.write(&r.done("order"))?;
            return Err(self.stopped("the execute body does not fit its window: nothing was sent"));
        };
        let key: Header<'_> = (b"x-api-key", self.key);
        let t_e0 = self.io.wall_ns();
        self.last_exec_ns = t_e0;
        // The tool's own overhead in D: decision → execute sent (flagged
        // past OVERHEAD_FLAG_NS: kept out of the D files).
        let overhead = t_e0.saturating_sub(t_decision);
        r.u64("overhead_ns", overhead).bool("overhead_flagged", overhead > OVERHEAD_FLAG_NS);
        let res = self.io.jup().request(Method::Post, P_EXECUTE, &[key], n);
        let t_e1 = self.io.wall_ns();
        let ans = match res {
            Ok((h, range)) => Ok((h, self.io.jup().resp().get(range).unwrap_or(&[]).to_vec())),
            Err(e) => Err(e),
        };
        let verdict = landing(&ans);
        r.u64("t_exec_send_ns", t_e0).u64("t_land_ack_ns", t_e1).str("exec_verdict", verdict.word());
        match &verdict {
            Landing::Doubt(why) | Landing::NotSent(why) => {
                r.str("exec_why", why);
            }
            Landing::Landed => {}
        }
        match &ans {
            Ok((h, body)) => {
                r.u64("exec_http", u64::from(*h)).body("exec_raw", body, RAW_MAX);
                let mut ea = ExecuteAnswer::default();
                if *h == 200 && scan_execute(body, &mut ea).is_ok() {
                    r.str("exec_signature", &lossy(ea.signature.of(body), 128));
                }
                let slot = Jv::parse(body).and_then(|j| j.get("slot").and_then(Jv::as_u64));
                r.opt_i64("exec_slot", slot.and_then(|s| i64::try_from(s).ok()));
            }
            Err(e) => {
                r.str("exec_error", &e.to_string());
            }
        }
        if let Landing::NotSent(why) = &verdict {
            let _ = self.ledger.append(close_row(RowKind::Unsent, t_e1, why.clone()));
            let halt = self.finish(&mut r, td, outcome, before, 0, Some(pos_b58.as_str()));
            self.write(&r.done("order"))?;
            return match halt {
                Some(Halt::Sticky(why)) => Err(self.sticky(&why)),
                Some(Halt::Stopped(why)) => Err(self.stopped(&why)),
                None => Ok(Step::Failed),
            };
        }
        // 9. The status, every 250 ms, to a terminal record or the deadline.
        let deadline = close + DEADLINE_AFTER_CLOSE_NS;
        let mut polls = String::from("[");
        let mut n_polls = 0u64;
        let mut last: Vec<u8> = Vec::new();
        let mut terminal: Option<(StatusAnswer, Vec<u8>, u64)> = None;
        let mut halt_seen: Option<String> = None;
        loop {
            if self.io.wall_ns() >= deadline {
                break;
            }
            if halt_seen.is_none() {
                if let Err(e) = exec_halt_clear(&self.cfg.exec_tomls) {
                    halt_seen = Some(e);
                }
            }
            let p = self.poll_once(&order_b58);
            if n_polls > 0 {
                polls.push(',');
            }
            n_polls += 1;
            let mut pr = Rec::new();
            pr.u64("t_send_ns", p.t0).u64("t_ans_ns", p.t1);
            match p.http {
                Some(h) => pr.u64("http", u64::from(h)),
                None => pr.raw("http", "null"),
            };
            if p.body == last {
                pr.bool("same", true);
            } else {
                pr.body("raw", &p.body, RAW_MAX);
                last.clone_from(&p.body);
            }
            let word = match &p.read {
                PollRead::Terminal(_) => "terminal",
                PollRead::Open => "open",
                PollRead::NotFound => "not_found",
                PollRead::Unread => "unread",
            };
            pr.str("read", word);
            let mut s = pr.0;
            s.push('}');
            polls.push_str(&s);
            if let PollRead::Terminal(a) = p.read {
                terminal = Some((a, p.body, p.t1));
                break;
            }
            self.io.sleep_until(p.t0 + POLL_EVERY_NS);
        }
        polls.push(']');
        r.raw("polls", &polls).u64("n_polls", n_polls);
        let Some((a, tbody, t_term)) = terminal else {
            let now = self.io.wall_ns();
            let _ = self.ledger.append(close_row(RowKind::InDoubt, now, format!("{} — not terminal by the deadline", verdict.word())));
            let _ = self.finish(&mut r, td, outcome, before, DUST_DEPOSIT_MICRO, Some(pos_b58.as_str()));
            r.bool("terminal", false);
            self.write(&r.done("order"))?;
            return Err(self.sticky(&format!(
                "order {order_b58} ({}) did not reach a terminal status with its fill numbers by the \
                 window's close + 30 s — reconcile it by hand; it may still land while its blockhash \
                 lives: do not treat it as never landed before {} s after its execute ({}), and only \
                 if its status still reads no order then",
                verdict.word(),
                BLOCKHASH_LIFE_NS / NS,
                utc_stamp(t_e0)
            )));
        };
        let jt = Jv::parse(&tbody);
        let mut fees = Vec::new();
        if let Some(j) = &jt {
            fee_fields(j, "", &mut fees);
        }
        let mut fj = String::from("[");
        for (i, (k, v)) in fees.iter().enumerate() {
            if i > 0 {
                fj.push(',');
            }
            fj.push('[');
            esc(k, &mut fj);
            fj.push(',');
            esc(v, &mut fj);
            fj.push(']');
        }
        fj.push(']');
        let status_word = jt.as_ref().and_then(|j| j.get("status")).and_then(Jv::as_str).unwrap_or("").to_string();
        r.bool("terminal", true)
            .u64("t_terminal_ns", t_term)
            .str("terminal_state", &format!("{:?}", a.state))
            .str("terminal_status", &status_word)
            .u64("filled_micro", a.filled_micro)
            .u64("avg_px_micro", a.avg_px_micro)
            .raw("fee_fields", &fj)
            .body("terminal_raw", &tbody, RAW_MAX);
        if let Err(e) = self.ledger.append(close_row(
            RowKind::Done,
            t_term,
            format!("{:?} filled_micro={} avg_px_micro={}", a.state, a.filled_micro, a.avg_px_micro),
        )) {
            r.str("ledger_error", &e);
        }
        // The fill's plausibility (risk review round 2, M4; the arm's own
        // laws, `arm.rs` `on_fill`): a price at or past the binary law's
        // bounds, a price above the order's own limit — the evidence that a
        // Limit order fills at or under its cap — or more contracts than
        // were signed.
        let implausible = fill_implausible(a.filled_micro, a.avg_px_micro, ask_at.limit, b.view.contracts);
        if let Some(why) = &implausible {
            r.str("fill_implausible", why);
        }
        // 10. The series, the balances after, the debit and SOL checks, the
        // positions.
        let halt = self.finish(&mut r, td, outcome, before, DUST_DEPOSIT_MICRO, Some(pos_b58.as_str()));
        self.write(&r.done("order"))?;
        if let Some(why) = implausible {
            let also = match &halt {
                Some(Halt::Sticky(w) | Halt::Stopped(w)) => format!(" (and: {w})"),
                None => String::new(),
            };
            return Err(self.sticky(&format!("order {order_b58}: {why}{also}")));
        }
        match halt {
            Some(Halt::Sticky(why)) => Err(self.sticky(&why)),
            Some(Halt::Stopped(why)) => Err(self.stopped(&why)),
            None => match halt_seen {
                Some(e) => Err(self.sticky(&format!("mid-battery, while an order was in flight: {e}"))),
                None => Ok(Step::Placed),
            },
        }
    }

    /// An order's end: the book series (at decision + 10 s), the balances
    /// after (3 tries; when a deposit may have left, no sooner than
    /// `gw::DEBIT_SETTLE_NS` after the execute — jup-gw's room for the
    /// balance answer to show the debit), the debit check, the positions —
    /// the stop it finds, if any, once its fields are in `r`.
    #[allow(clippy::too_many_arguments)]
    fn finish(
        &mut self,
        r: &mut Rec,
        td: u64,
        outcome: u8,
        before: (u64, u64),
        sent: u64,
        current: Option<&str>,
    ) -> Option<Halt> {
        let (before, sol_before) = before;
        self.io.sleep_until(td + BOOK_AFTER_NS);
        let (u, d) = self.io.series_take(false);
        let (mine, other) = if outcome == 0 { (u, d) } else { (d, u) };
        r.raw("book_series", &series_json(&mine)).raw("book_series_other", &series_json(&other));
        if sent > 0 {
            self.io.sleep_until(self.last_exec_ns.saturating_add(DEBIT_SETTLE_NS));
        }
        let mut after = None;
        let mut last_err = String::new();
        for k in 0..AFTER_TRIES {
            if k > 0 {
                let t = self.io.wall_ns();
                self.io.sleep_until(t + AFTER_GAP_NS);
            }
            let t = self.io.wall_ns();
            match self.balances() {
                Ok(v) => {
                    after = Some((t, v));
                    break;
                }
                Err(e) => last_err = e,
            }
        }
        let Some((ta, (dep, sol))) = after else {
            r.str("balance_after_error", &last_err);
            return Some(Halt::Sticky(format!(
                "the debit check could not read the balance after the order ({AFTER_TRIES} tries): {last_err}"
            )));
        };
        let fall = before.saturating_sub(dep);
        r.u64("t_balance_after_ns", ta).u64("balance_after_micro", dep).u64("sol_after", sol).u64("debit_micro", fall);
        self.last_after = Some(dep);
        if fall > sent + DEBIT_SLACK_MICRO {
            return Some(Halt::Sticky(format!(
                "the debit check: the deposit mint's balance fell by {} across the order, more than its \
                 deposit {} plus 1 %",
                usd(fall as i64),
                usd(sent as i64)
            )));
        }
        // SOL (risk review round 3, MINOR 2): a sent order may cost at most
        // DUST_SOL_FALL_MAX_LAMPORTS — J0's measured fall (fees and the
        // program's rent, paid by CPI) + a margin + the signatures and the
        // priority at their caps (R-PMJ-S80); nothing when nothing was sent.
        let sol_fall = sol_before.saturating_sub(sol);
        let sol_allowed = if sent > 0 { DUST_SOL_FALL_MAX_LAMPORTS } else { 0 };
        r.u64("sol_fall_lamports", sol_fall).u64("sol_allowed_lamports", sol_allowed);
        if sol_fall > sol_allowed {
            return Some(Halt::Sticky(format!(
                "the SOL check: the wallet's SOL fell by {sol_fall} lamports across the order, more than \
                 the {sol_allowed} it may cost (DUST_SOL_FALL_MAX_LAMPORTS: J0's {J0_SOL_FALL_LAMPORTS} + \
                 {SOL_FALL_MARGIN_LAMPORTS} + {SIG_FEE_LAMPORTS} × {DUST_SIGS_MAX} signatures + \
                 {DUST_MAX_CU_PRICE} µlamports × {CU_LIMIT_MAX} / 1e6; 0 when nothing was sent)"
            )));
        }
        match self.positions(current) {
            Ok(p) => {
                r.str("positions_raw", &p.raw);
                if p.unknown.is_empty() {
                    None
                } else {
                    Some(Halt::Sticky(format!("positions no reservation of ours carries: {:?}", p.unknown)))
                }
            }
            Err(e) => {
                r.str("positions_error", &e);
                Some(Halt::Stopped(format!(
                    "the positions read after the order failed ({e}) — the next run reads them before \
                     anything else"
                )))
            }
        }
    }

    /// The claimable positions, read-only (never claimed here).
    fn claimable_text(&mut self) -> String {
        match self.positions(None) {
            Ok(p) => {
                let mut r = Rec::new();
                let mut list = String::from("[");
                let mut text = String::new();
                for (i, (k, c, pay)) in p.claimable.iter().enumerate() {
                    if i > 0 {
                        list.push(',');
                    }
                    list.push('[');
                    esc(k, &mut list);
                    let _ = write!(list, ",{c},{pay}]");
                    let _ = write!(text, "\n  {k}: {} contracts, payout {}", *c as f64 / 1e6, usd(*pay as i64));
                }
                list.push(']');
                r.str("battery", &self.cfg.battery).u64("t_ns", self.io.wall_ns()).raw("claimable", &list);
                let _ = self.out.line(&r.done("end"));
                if p.claimable.is_empty() {
                    String::from("claimable positions: none")
                } else {
                    format!(
                        "claimable positions (read-only — pmj-dust never claims; claim them by hand):{text}"
                    )
                }
            }
            Err(e) => format!("claimable positions: the read failed ({e})"),
        }
    }
}

// --- the preflight: every refusal before the secrets ---------------------------

/// The chain half of `pmj-exec --key-check`, injected (the tests' scripted
/// RPC; production builds its own public-RPC client).
type ChainFn = Box<dyn Fn(&PmjLiveKeys, &PmjGuard) -> Result<String, KeyCheckFail>>;

/// What the preflight reads from the process (the tests inject theirs).
struct PreEnv {
    /// The gate lines this binary was built with.
    gates: PmjGates,
    home: PathBuf,
    /// `MULTIVENUE_DIR` as set (anything but `$HOME/multivenue` refuses).
    mv_env: Option<std::ffi::OsString>,
    /// The engine's loopback port.
    state: SocketAddr,
    /// The shell `strategy.conf` is sourced with.
    zsh: PathBuf,
    /// The offline self-test.
    self_test: fn() -> Result<String, String>,
    /// `pmj.toml`'s live keys.
    load_keys: fn(Option<&Path>) -> Result<PmjLiveKeys, String>,
    /// The chain half (public Solana RPC).
    chain: ChainFn,
    /// The wall clock at the preflight, Unix ns.
    now_ns: u64,
}

impl PreEnv {
    fn process(tls: Arc<ClientConfig>) -> Result<Self, DustFail> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| Refused(String::from("HOME is not set")))?;
        let state = STATE_ADDR
            .parse()
            .map_err(|_| Refused(format!("{STATE_ADDR} is not an address")))?;
        Ok(Self {
            gates: PMJ_GATES_BUILT,
            home,
            mv_env: std::env::var_os("MULTIVENUE_DIR"),
            state,
            zsh: PathBuf::from("/bin/zsh"),
            self_test: crate::pmj_exec::self_test,
            load_keys,
            chain: Box::new(move |k, g| crate::pmj_exec::chain_check(Arc::clone(&tls), k, g)),
            now_ns: wall_now(),
        })
    }

    /// `~/multivenue`.
    fn mv(&self) -> PathBuf {
        self.home.join("multivenue")
    }
}

/// `pmj.toml`'s live keys (`pmj_exec::key_check`'s reading).
fn load_keys(pmj: Option<&Path>) -> Result<PmjLiveKeys, String> {
    let path = match pmj {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::pmj::default_pmj_path().map_err(|e| e.to_string())?),
    };
    let (file, _) = core_config::pmj::load(&path).map_err(|e| e.to_string())?;
    Ok(crate::pmj_boot::live_keys_of(&file))
}

/// What the preflight established (the lock is held while this lives).
struct Pre {
    cfg: RunCfg,
    keys: PmjLiveKeys,
    host: HostCheck,
    ledger: Ledger,
    out: Out,
    lock: DustLock,
}

/// The dust ceilings and pins of `pmj.toml`'s guard keys (risk review
/// round 2: M2's deploy pin, MINOR 4's R-PMJ-S80 ceilings).
fn dust_keys_ok(keys: &PmjLiveKeys) -> Result<(), String> {
    if keys.min_sol_lamports == 0 {
        return Err(String::from(
            "pmj.toml `min_sol_lamports` is unset — requirement (a) needs the wallet's SOL floor",
        ));
    }
    if keys.guard_program_slot == 0 {
        return Err(String::from(
            "pmj.toml `guard_program_slot` is unset — the battery signs only against a reviewed, pinned \
             Predict deploy (run scripts/pmj-exec.sh key-check, review the deploy, pin its slot)",
        ));
    }
    if keys.guard_max_sol_lamports > DUST_MAX_SOL_LAMPORTS {
        return Err(format!(
            "pmj.toml `guard_max_sol_lamports` = {} is over the dust ceiling {DUST_MAX_SOL_LAMPORTS} \
             (R-PMJ-S80, PROVISIONAL)",
            keys.guard_max_sol_lamports
        ));
    }
    if keys.guard_max_cu_price > DUST_MAX_CU_PRICE {
        return Err(format!(
            "pmj.toml `guard_max_cu_price` = {} is over the dust ceiling {DUST_MAX_CU_PRICE} µlamports \
             (R-PMJ-S80, PROVISIONAL)",
            keys.guard_max_cu_price
        ));
    }
    Ok(())
}

/// **Every refusal before the secrets** (module docs, "Refusals"), in
/// order: the arguments; the build-time dust gate; `MULTIVENUE_DIR`;
/// `strategy.conf` (sourced) and an absolute `EXEC_TOML`; the exclusive
/// lock; a standing `pmj-dust.HALT` (either place; fail closed);
/// `exec.HALT` (either place); slot 11 live anywhere on the host;
/// `pmj.toml`'s guard keys reviewed, the SOL floor set, the deploy pinned,
/// the dust ceilings; the offline self-test; the ledger, the battery's 40
/// and the day's $50; the record file; then the one networked step, the
/// chain half of the key check (public Solana RPC, no secret) — a review
/// trigger there writes the sticky halt and exits 77.
fn preflight(args: &DustArgs, env: &PreEnv) -> Result<Pre, DustFail> {
    args_and_gate(args, env.gates)?;
    let mv = env.mv();
    if let Some(v) = &env.mv_env {
        if Path::new(v) != mv.as_path() {
            return Err(Refused(format!(
                "MULTIVENUE_DIR is set ({}) — the ledger, the halts and the lock live in {} only; unset it",
                PathBuf::from(v).display(),
                mv.display()
            )));
        }
    }
    if !mv.is_dir() {
        return Err(Refused(format!(
            "{}: no such directory (the ledger, the lock and a sticky halt live there)",
            mv.display()
        )));
    }
    let conf = mv.join("strategy.conf");
    let (_, in_force) = conf_sourced(&conf, &env.zsh, &env.home).map_err(Refused)?;
    if !in_force.is_empty() && !Path::new(&in_force).is_absolute() {
        return Err(Refused(format!(
            "{}'s EXEC_TOML = `{in_force}` is not an absolute path",
            conf.display()
        )));
    }
    // The exec artifact the engine boots from: a sticky halt is written
    // beside it too, and its exec.HALT watched. The ledger and the lock stay
    // in ~/multivenue whatever it says (a second ledger would be a second
    // $50 a day).
    let default_toml = mv.join("exec.toml");
    let exec_toml = if in_force.is_empty() { default_toml.clone() } else { PathBuf::from(&in_force) };
    let dir = exec_toml
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    if !dir.is_dir() {
        return Err(Refused(format!("{}: no such directory (beside the exec artifact)", dir.display())));
    }
    let lock = take_lock(&mv.join(LOCK_FILE)).map_err(Refused)?;
    let mut halts = vec![dir.join(HALT_FILE)];
    if !halts.contains(&mv.join(HALT_FILE)) {
        halts.push(mv.join(HALT_FILE));
    }
    if let Some((h, t)) = halt_standing(&halts) {
        return Err(Refused(format!(
            "{} stands — a sticky stop: nothing signs until the operator reconciles and removes it:\n{t}",
            h.display()
        )));
    }
    let mut exec_tomls = vec![exec_toml.clone()];
    if default_toml != exec_toml {
        exec_tomls.push(default_toml);
    }
    exec_halt_clear(&exec_tomls).map_err(Refused)?;
    let host = HostCheck {
        conf,
        mv: mv.clone(),
        home: env.home.clone(),
        exec_tomls: exec_tomls.clone(),
        state: env.state,
        zsh: env.zsh.clone(),
    };
    host_live(&host).map_err(|e| Refused(format!("slot 11 is (or may be) live on this host: {}", e.text())))?;
    let keys = (env.load_keys)(args.pmj.as_deref()).map_err(Refused)?;
    let reviewed = guard_from_keys(&keys, [1; KEY_LEN]).map_err(Refused)?;
    dust_keys_ok(&keys).map_err(Refused)?;
    (env.self_test)().map_err(Refused)?;
    let ledger = Ledger::open(&mv.join(LEDGER_FILE)).map_err(Refused)?;
    if !args.dry_run {
        let had = ledger.battery_orders(&args.battery);
        if had + args.orders > DUST_BATTERY_MAX {
            return Err(Refused(format!(
                "battery {} holds {had} reservation(s): at most {} more (≤ {DUST_BATTERY_MAX} a battery)",
                args.battery,
                DUST_BATTERY_MAX.saturating_sub(had)
            )));
        }
        let today = utc_date(env.now_ns / NS);
        let used = ledger.day_used(&today);
        if used + DUST_DEPOSIT_MICRO > DUST_DAY_CAP_MICRO {
            return Err(Refused(format!(
                "the day cap: {} of {} already reserved on {today} UTC (the ledger {})",
                usd(used as i64),
                usd(DUST_DAY_CAP_MICRO as i64),
                ledger.path.display()
            )));
        }
    }
    let mut out = Out::open(&mv.join("research").join("pmj").join("dust"), &args.battery).map_err(Refused)?;
    // The review triggers, re-read at every run's start (not per order):
    // the Predict program's deploy slot against its pin, the vault's table
    // entry, the deposit mint, a pinned account closed — `pmj-exec
    // --key-check`'s chain half. 77 is a sticky stop.
    match (env.chain)(&keys, &reviewed) {
        Ok(words) => {
            let mut r = Rec::new();
            r.str("battery", &args.battery).u64("t_ns", env.now_ns).str("chain", &words);
            out.line(&r.done("chain")).map_err(Refused)?;
        }
        Err(KeyCheckFail::Review(m)) => {
            let why = format!("REVIEW TRIGGER (the chain half of the key check): {m}");
            let text = format!(
                "pmj-dust HALT {} battery={}\nreason: {why}\nNothing signs while this file exists. Review \
                 the change, re-pin pmj.toml, then remove this file (risk-policy PMJ J17).\n",
                utc_stamp(env.now_ns),
                args.battery
            );
            let mut werr = String::new();
            for h in &halts {
                if let Err(e) = write_halt(h, &text) {
                    werr = format!(" — AND {} could not be written ({e})", h.display());
                }
            }
            let mut r = Rec::new();
            r.str("battery", &args.battery).u64("t_ns", env.now_ns).str("stop", "sticky").str("why", &why);
            let _ = out.line(&r.done("stop"));
            return Err(Sticky(format!("{why}{werr}")));
        }
        Err(KeyCheckFail::Refused(m)) => {
            return Err(Refused(format!("the chain half of the key check: {m}")));
        }
    }
    let seed = args
        .seed
        .unwrap_or_else(|| Rng(env.now_ns ^ u64::from(std::process::id())).next());
    Ok(Pre {
        cfg: RunCfg {
            battery: args.battery.clone(),
            orders: args.orders,
            dry_run: args.dry_run,
            seed,
            min_sol: keys.min_sol_lamports,
            exec_tomls,
            halts,
            t_start: env.now_ns,
            run_max_ns: RUN_WALL_MAX_NS,
        },
        keys,
        host,
        ledger,
        out,
        lock,
    })
}

/// The arguments and the build-time dust gate (refusals 1 and 2) — before
/// the process environment is read at all.
fn args_and_gate(args: &DustArgs, gates: PmjGates) -> Result<(), DustFail> {
    if !battery_ok(&args.battery) {
        return Err(Refused(format!(
            "--battery `{}`: an id of [A-Za-z0-9][A-Za-z0-9_.-]{{0,63}} (it names the record file)",
            args.battery
        )));
    }
    if args.orders == 0 || args.orders > DUST_BATTERY_MAX {
        return Err(Refused(format!("--orders {}: 1 ..= {DUST_BATTERY_MAX}", args.orders)));
    }
    land_path(&args.path)?;
    if !gates.dust {
        return Err(Refused(String::from(
            "PMJ-DUST-GATE: this binary was built from a tree whose docs/risk-policy.md has no line \
             beginning `PMJ-DUST-GATE: ruled YYYY-MM-DD` (in the working tree AND committed) — the \
             dust battery is the dust gate's, the operator's ruling, written there and built in \
             (PMJ J16, J17). Refusing before anything else.",
        )));
    }
    Ok(())
}

// --- the verb -----------------------------------------------------------------------

/// **`multivenue-engine pmj-dust`** (module docs): the preflight; then the
/// secrets, exactly as the live boot loads them (the public key, the guard
/// for it, the 0600 keypair file, `JUP_API_KEY` — a dry run drops the
/// signer at once); then the battery on the production IO. `Ok` names the
/// orders and the records, and lists the claimable positions.
///
/// # Errors
/// [`DustFail`] — the exit code is [`DustFail::exit_code`].
pub fn run(args: &DustArgs, tls: Arc<ClientConfig>) -> Result<String, DustFail> {
    // The arguments and the gate before the environment is read at all.
    args_and_gate(args, PMJ_GATES_BUILT)?;
    let env = PreEnv::process(Arc::clone(&tls))?;
    let pre = preflight(args, &env)?;
    let pubkey = key58(&crate::pmj_live::env(ENV_PUBKEY).map_err(Refused)?)
        .ok_or_else(|| Refused(format!("{ENV_PUBKEY} is not a base58 public key of 32 bytes")))?;
    let guard = guard_from_keys(&pre.keys, pubkey).map_err(Refused)?;
    let (signer, key, _) = wallet_from_env().map_err(Refused)?;
    if key != pubkey {
        return Err(Refused(format!("the keypair file's key is not {ENV_PUBKEY}")));
    }
    let mut api = api_key_from_env().map_err(Refused)?;
    let io = match LiveIo::new(tls, pre.host.clone()) {
        Ok(io) => io,
        Err(e) => {
            api.zeroize();
            return Err(Refused(e));
        }
    };
    let r = if args.dry_run {
        drop(signer);
        run_with(io, NoSigner, api.as_bytes(), pre, &guard)
    } else {
        run_with(io, signer, api.as_bytes(), pre, &guard)
    };
    api.zeroize();
    r
}

/// The battery on any IO and signer (the tests' scripted ones), with the
/// claimable list appended to whatever it ends with.
fn run_with<I: DustIo, S: DustSigner>(io: I, signer: S, key: &[u8], pre: Pre, guard: &PmjGuard) -> Result<String, DustFail> {
    let Pre { cfg, ledger, out, lock, .. } = pre;
    let mut run = Run::new(io, signer, key, &cfg, guard, ledger, out);
    let r = run.battery();
    let claim = run.claimable_text();
    let records = run.out.path.display().to_string();
    drop(run);
    drop(lock);
    match r {
        Ok(n) => Ok(format!(
            "pmj-dust: battery {} — {n} order(s) {} this run; records {records}\n{claim}",
            cfg.battery,
            if cfg.dry_run { "built and guarded (a dry run: none signed, none landed)" } else { "placed" }
        )),
        Err(Refused(m)) => Err(Refused(format!("{m}\nrecords {records}\n{claim}"))),
        Err(Stopped(m)) => Err(Stopped(format!("{m}\nrecords {records}\n{claim}"))),
        Err(Sticky(m)) => Err(Sticky(format!("{m}\nrecords {records}\n{claim}"))),
    }
}
