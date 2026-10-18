// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # cmd — the 64 B records between the engine's arm and jup-gw (PMJ J3)
//!
//! The arm (engine thread) pushes a [`JupIntent`] onto the duplex's
//! command ring (`exec_gateway::duplex`, a core-ring SPSC); jup-gw answers
//! with [`JupEvent`]s on its event ring. Both are `#[repr(C, align(64))]`
//! PODs of exactly one cache line, copied once into their ring slot and
//! read there in place.

/// The command ring's capacity (orders are few: one per family a window).
pub const JUP_CMD_RING: usize = 16;
/// The event ring's capacity (≥ 4 command rings, XG-2).
pub const JUP_EVT_RING: usize = 64;

/// [`JupIntent::kind`]: buy `deposit_micro` of the outcome, capped at
/// `limit_px_1e6` (jup-gw refuses a build whose `maxBuyPriceUsd` is above
/// it).
pub const INTENT_BUY: u8 = 1;
/// [`JupIntent::kind`]: read the positions now and claim what is
/// claimable (the arm's ask at a won leg's settlement).
pub const INTENT_CLAIM: u8 = 2;

/// One intent (engine → jup-gw).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct JupIntent {
    /// The order's client id (the cloid law: its bits encode the slot,
    /// E-9; its low 32 bits the window's instance).
    pub client_oid: u64,
    /// The deposit, USDC micro units (≥ the venue's $5 minimum).
    pub deposit_micro: u64,
    /// The highest price the member accepts, ×1e6 (`p − 2c`).
    pub limit_px_1e6: i64,
    /// The member's decision instant, engine ns (the push).
    pub decision_ns: u64,
    /// The market window's end, Unix ns: no build at or after it.
    pub window_end_ns: u64,
    /// Reserved — always zero.
    pub _rsv: u64,
    /// The window's instance id (`pm5_instance_id`): jup-gw resolves the
    /// Gamma market id from its window table by it.
    pub instance: u32,
    /// The outcome leg's engine sym.
    pub sym: u32,
    /// 0 = Up, 1 = Down (`render::Outcome`).
    pub outcome: u8,
    /// [`INTENT_BUY`] / [`INTENT_CLAIM`].
    pub kind: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
}

/// [`JupEvent::kind`]: `/execute` answered `Success`: landed, the keeper
/// to fill (`fee_micro` / `cost_micro`: the build's estimates; `aux`: the
/// execute round trip, µs).
pub const EVT_LANDED: u8 = 1;
/// [`JupEvent::kind`]: the order filled `filled_micro` contracts IN ALL
/// (cumulative) at `avg_px_micro` — the arm books the difference.
pub const EVT_FILL: u8 = 2;
/// [`JupEvent::kind`]: refused before anything could trade
/// ([`JupEvent::reason`]); the stake is released.
pub const EVT_REFUSED: u8 = 3;
/// [`JupEvent::kind`]: terminal — nothing more fills (the program refunds
/// an unfilled deposit). `aux`: 1 = the order was never found (an
/// in-doubt execute that never landed).
pub const EVT_DONE: u8 = 4;
/// [`JupEvent::kind`]: `/execute` was sent and nothing that reads came
/// back — in doubt until the status poll or recon says.
pub const EVT_IN_DOUBT: u8 = 5;
/// [`JupEvent::kind`]: a claim was built, guarded and signed
/// (`cost_micro`: the payout the venue states).
pub const EVT_CLAIM_READY: u8 = 6;
/// [`JupEvent::kind`]: an account read — `aux` the wallet's USDC (micro),
/// `cost_micro` its SOL (lamports); `reason` 1 = the read failed.
pub const EVT_BALANCE: u8 = 7;
/// [`JupEvent::kind`]: recon gave up on the order (the gateway halts;
/// the operator reconciles).
pub const EVT_LOST: u8 = 8;
/// [`JupEvent::kind`]: the positions read shows a held leg claimed (or
/// settled by the venue on its own): it leaves the ledger — its payout is
/// in the wallet's USDC. `aux` the leg's instance, `reason` its outcome,
/// `cost_micro` the leg's cost.
pub const EVT_CLAIMED: u8 = 9;

/// [`JupEvent::reason`] for [`EVT_REFUSED`].
pub mod reason {
    /// The build answer did not read.
    pub const BUILD_UNREADABLE: u8 = 1;
    /// The transaction did not parse or the guard refused it (`aux` = the
    /// `TxErr` discriminant + 1).
    pub const GUARD: u8 = 2;
    /// The server's price cap was above the member's limit (`aux` = the
    /// cap, micro USD).
    pub const PRICE_CAP: u8 = 3;
    /// At or past the window's end, or inside the lead before it.
    pub const LATE: u8 = 4;
    /// The gateway is halted (`exec.HALT` on slot 11, or its own halt):
    /// no new builds — a landed order is not pulled (it cannot be).
    pub const HALTED: u8 = 5;
    /// The venue refused (`aux` = the `ErrKind` discriminant, or the HTTP
    /// status when the body did not read).
    pub const VENUE: u8 = 6;
    /// Region refusal: this account cannot trade from here (also halts).
    pub const REGION: u8 = 7;
    /// `/execute` answered `Failed`: the transaction did not land.
    pub const EXECUTE_FAILED: u8 = 8;
    /// Not a keeper order (`execution.context.type` not `create_order`).
    pub const NOT_KEEPER: u8 = 9;
    /// The window is not in jup-gw's table (no Gamma market id).
    pub const NO_MARKET: u8 = 10;
    /// The deposit is under the $5 minimum or over the per-order maximum.
    pub const SIZE: u8 = 11;
    /// No free row for another order.
    pub const FULL: u8 = 12;
    /// `requiredSigners` is not exactly our wallet.
    pub const SIGNERS: u8 = 13;
    /// The request did not fit its buffer (a sizing bug; also counted).
    pub const LOCAL: u8 = 14;
    /// The build or the execute failed before any byte could reach the
    /// venue, or the build failed at all (a build creates nothing).
    pub const NETWORK: u8 = 15;
    /// The build answered another order than the one asked (market,
    /// outcome side, buy, owner, or a cost above the deposit).
    pub const MISMATCH: u8 = 16;
}

/// One gateway event (jup-gw → engine).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct JupEvent {
    /// The order's client id (0 for an account read).
    pub client_oid: u64,
    /// `EVT_*`.
    pub kind: u8,
    /// `reason::*` for a refusal; 0 otherwise.
    pub reason: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
    /// Contracts filled so far, micro units (cumulative).
    pub filled_micro: u64,
    /// Their average price, micro USD.
    pub avg_px_micro: u64,
    /// Fees, micro USD (the build's estimate: the venue reports no fee
    /// per fill — the position's `feesPaidUsd` is J5's measure).
    pub fee_micro: u64,
    /// The order's cost, micro USD (or the event's own amount).
    pub cost_micro: u64,
    /// When the gateway wrote the event, engine ns.
    pub ts_ns: u64,
    /// A detail: a discriminant, an amount, or a latency in µs.
    pub aux: u64,
}

impl JupEvent {
    /// An event of `kind` for `client_oid`, stamped `ts_ns`.
    #[inline]
    #[must_use]
    pub const fn new(kind: u8, client_oid: u64, ts_ns: u64) -> Self {
        Self {
            client_oid,
            kind,
            reason: 0,
            _pad: [0; 6],
            filled_micro: 0,
            avg_px_micro: 0,
            fee_micro: 0,
            cost_micro: 0,
            ts_ns,
            aux: 0,
        }
    }
}

const _: () = assert!(core::mem::size_of::<JupIntent>() == 64);
const _: () = assert!(core::mem::size_of::<JupEvent>() == 64);
const _: () = assert!(core::mem::align_of::<JupIntent>() == 64);
const _: () = assert!(core::mem::align_of::<JupEvent>() == 64);
const _: () = assert!(JUP_EVT_RING >= 4 * JUP_CMD_RING);
