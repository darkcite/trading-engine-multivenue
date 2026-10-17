// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The three rings' records (PM15-6b): one 64 B command from the engine to
//! pm-gw, one 64 B event from pm-gw or pm-user to the engine.
//!
//! ```text
//! engine ── PmCmd ──▶ [cmd ring, PM_CMD_RING]  ──▶ pm-gw
//! engine ◀── PmEvt ── [gw ring,  PM_EVT_RING]  ◀── pm-gw   SENT · ACK · REFUSED · IN_DOUBT · FILL (recon) · DONE
//! engine ◀── PmEvt ── [user ring, PM_USER_RING] ◀── pm-user FILL · GAP
//! ```
//!
//! An event names its order by the member's `client_oid` (from pm-gw) or
//! by the venue's `orderID` (a channel `trade`: its `taker_order_id`); the
//! arm learns the pair from SENT, which pm-gw emits before the request's
//! first byte (plan §5, "Ordering").

/// Command ring slots.
pub const PM_CMD_RING: usize = 16;
/// The gw event ring's slots (`≥ 4 ·` [`PM_CMD_RING`], the kit's law).
pub const PM_EVT_RING: usize = 128;
/// The user event ring's slots: a burst of channel events while the
/// engine thread is busy (a fill is at most a few per order).
pub const PM_USER_RING: usize = 256;

/// `PmCmd.kind`: send one FAK BUY.
pub const CMD_SUBMIT: u8 = 1;
/// `PmCmd.kind`: read the venue's trades of an order past its deadline.
pub const CMD_RECON: u8 = 2;
/// `PmCmd.kind` (PM15-6c): watch a token the arm holds — pm-gw reads its
/// CONDITIONAL balance at every account read (`instance`, `leg`).
pub const CMD_WATCH: u8 = 3;

/// One command (engine → pm-gw).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmCmd {
    /// RECON: the order's `orderID` (from SENT); zero for SUBMIT.
    pub order_id: [u8; 32],
    /// The member's client id.
    pub client_oid: u64,
    /// SUBMIT: the limit ×1e6 (< 1 000 000).
    pub px_1e6: u32,
    /// SUBMIT: the dollars to spend ×1e6 (the member's shares at the
    /// limit, down to the cent — the FAK's `makerAmount`; ≤ $4 294).
    pub usd_1e6: u32,
    /// The market's instance id (`core_types::pm_roll`).
    pub instance: u32,
    /// SUBMIT: the leg's tick in force ×1e6 (the order law the member
    /// priced on).
    pub tick_1e6: u32,
    /// [`CMD_SUBMIT`] or [`CMD_RECON`].
    pub kind: u8,
    /// The leg (0 Up, 1 Down).
    pub leg: u8,
    /// RECON: the attempt (pm-gw's own count; 0 from the arm).
    pub tries: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 5],
}

const _: () = assert!(core::mem::size_of::<PmCmd>() == 64);

impl PmCmd {
    /// A command of `kind` for `client_oid`, every other field zero.
    #[must_use]
    pub const fn new(kind: u8, client_oid: u64) -> Self {
        Self {
            order_id: [0; 32],
            client_oid,
            px_1e6: 0,
            usd_1e6: 0,
            instance: 0,
            tick_1e6: 0,
            kind,
            leg: 0,
            tries: 0,
            _pad: [0; 5],
        }
    }
}

/// `PmEvt.kind`: the request is about to be written (`order_id` its
/// digest).
pub const EVT_SENT: u8 = 1;
/// `PmEvt.kind`: accepted; `b` = the answer's trade count, `why` = its
/// `response::Status`.
pub const EVT_ACK: u8 = 2;
/// `PmEvt.kind`: refused; `why` = its `response::Refusal`, `b` = the HTTP
/// status (0: refused before the wire), `c` = `retry_after_seconds`.
pub const EVT_REFUSED: u8 = 3;
/// `PmEvt.kind`: the answer did not read or never came — the stake is
/// held until recon decides.
pub const EVT_IN_DOUBT: u8 = 4;
/// `PmEvt.kind`: a taker fill of `order_id`: `a` = the trade key, `b` =
/// price ×1e6, `c` = shares ×1e6, `why` = the trade status
/// (`userws::TS_*`).
pub const EVT_FILL: u8 = 5;
/// `PmEvt.kind`: recon finished the order (every fill it found was sent
/// before this); `b` = the trades it found.
pub const EVT_DONE: u8 = 6;
/// `PmEvt.kind`: the user channel's session was lost (no replay: every
/// open order needs recon).
pub const EVT_GAP: u8 = 7;
/// `PmEvt.kind` (PM15-6c): the account's pUSD read — `b` = the balance
/// ×1e6, `c` = the CTF Exchange V2 allowance ×1e6 (capped at `i64::MAX`);
/// `why` = 1 when the read failed (then `b`, `c` are 0).
pub const EVT_BALANCE: u8 = 8;
/// `PmEvt.kind` (PM15-6c): a watched token's CONDITIONAL balance — `a` =
/// `instance << 8 | leg`, `b` = shares ×1e6; `why` = 1 when the read
/// failed.
pub const EVT_POSITION: u8 = 9;
/// `PmEvt.kind` (PM15-6c): one account read is complete (every watched
/// token reported after the balance).
pub const EVT_READ_DONE: u8 = 10;

/// `PmEvt.why` of a refusal made before the wire: no token for the
/// instance.
pub const LOCAL_NO_TOKEN: u8 = 200;
/// `PmEvt.why` of a refusal made before the wire: the amounts law.
pub const LOCAL_AMOUNTS: u8 = 201;
/// `PmEvt.why` of a refusal made before the wire: under the market's
/// floors.
pub const LOCAL_FLOOR: u8 = 202;
/// `PmEvt.why` of a refusal made before the wire: the signer failed.
pub const LOCAL_SIGN: u8 = 203;
/// `PmEvt.why` of a refusal made before the wire: the request did not
/// fit or the gateway is halted.
pub const LOCAL_REQUEST: u8 = 204;

/// One event (pm-gw or pm-user → engine).
#[repr(C, align(64))]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmEvt {
    /// The venue's `orderID` (SENT, ACK, FILL; zero otherwise).
    pub order_id: [u8; 32],
    /// FILL: the trade key (FNV-1a 64 of the trade id); else the member's
    /// client id.
    pub a: u64,
    /// See each kind.
    pub b: i64,
    /// See each kind.
    pub c: i64,
    /// `EVT_*`.
    pub kind: u8,
    /// See each kind.
    pub why: u8,
    /// Explicit padding — always zero.
    pub _pad: [u8; 6],
}

const _: () = assert!(core::mem::size_of::<PmEvt>() == 64);

impl PmEvt {
    /// An event of `kind` about `client_oid`.
    #[must_use]
    pub const fn about(kind: u8, client_oid: u64) -> Self {
        Self {
            order_id: [0; 32],
            a: client_oid,
            b: 0,
            c: 0,
            kind,
            why: 0,
            _pad: [0; 6],
        }
    }

    /// A fill of `order_id`: trade `key`, `px_1e6`, `qty_1e6`, `status`.
    #[must_use]
    pub const fn fill(order_id: [u8; 32], key: u64, px_1e6: i64, qty_1e6: i64, status: u8) -> Self {
        Self {
            order_id,
            a: key,
            b: px_1e6,
            c: qty_1e6,
            kind: EVT_FILL,
            why: status,
            _pad: [0; 6],
        }
    }
}
