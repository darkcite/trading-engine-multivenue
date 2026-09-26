// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `exec-hypercall` — the Hypercall order arm (HC9, ruling O-HC19)
//!
//! The `exec-hyperliquid` shape (E2–E6) adapted to a REST venue that
//! answers a refused order with HTTP 200: an [`OrderDispatch`] that
//! renders each request body IN PLACE, signs the EIP-712 struct over
//! that body's own spans (HC8, LAW E-3 / D7), posts it on a keep-alive
//! connection, and reads the answer fail-closed. Fills come from the
//! private `fills` stream alone (LAW E-5); reconciliation reads the
//! venue's orders, fills and portfolio on the idle path.
//!
//! ## Where it runs
//!
//! **Only in the operator's verbs until slot 7's live-arming ruling**
//! (`multivenue-engine hypercall-live`, plan `hc9-hc11` §1). The
//! engine's `--exec` interlock does not list Hypercall among the venues
//! it arms: the E6 exposure clamp values HIP-4 binaries and cannot see
//! a short option, and slot 7's HL hedges need an account whose perps
//! are reconciled. The verbs drive this very type — the mainnet dust
//! smoke is a proof of the arm, not of a side path.
//!
//! ## The laws
//!
//! * **E-1** — a failure is a refusal, counted; never a modelled fill.
//! * **E-3 / D7** — the signed strings are the body's own bytes
//!   ([`render`]); nothing is rendered twice.
//! * **E-5** — the HTTP answer is the ACK; the `fills` channel is the
//!   FILL ([`events`]). An ACK saying `FILLED` books nothing.
//! * **E-9** — the client id carries the slot ([`cloid`]).
//! * **Fail closed** — only `ACKED`/`OPEN`/`PARTIALLY_FILLED`/`FILLED`
//!   at HTTP 200 is an acceptance ([`response`]); an unreadable answer
//!   is a refusal, never an acceptance.
//! * **One slot, one wallet** — the arm trades exactly one slot; every
//!   fill on its wallet is that slot's.
//!
//! ## Zero allocation
//!
//! Boot allocates the buffers (request head/body windows, the 1 MiB WS
//! receive buffer, the instrument table). A submit, a cancel, a pump
//! and a reconcile allocate nothing of their own; rustls' buffered API
//! allocates one sealed record out and one per decrypted record in
//! (bench gate 77b pins `HttpsReq` at exactly 2 per request).

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod budget;
pub mod cloid;
pub mod config;
pub mod events;
pub mod exchange;
pub mod json;
pub mod nonce;
pub mod num;
pub mod recon;
pub mod render;
pub mod response;
pub mod smoke;
pub mod table;
pub mod ws;

pub use clob_dispatcher::OrderDispatch;
pub use config::{HcConfigErr, HcExecConfig};
pub use exchange::{HcCounters, HcExchange};
pub use table::HcInstruments;

/// The venue's REST and WS host (there is no testnet: plan §1.1).
pub const HOST_MAINNET: &str = "api.hypercall.xyz";

/// The one slot Hypercall is traded by — slot 7, the S1 member's (plan
/// `hc9-hc11` §4). The arm's orders and fills carry it (LAW E-9).
pub const HC_SLOT: u8 = 7;
