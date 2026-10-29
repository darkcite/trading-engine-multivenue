// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `exec-jupiter` — the slot-11 Jupiter Predict order arm (PMJ J3)
//!
//! Jupiter Predict sells the SAME Polymarket outcome tokens slot 9 trades,
//! through a Solana keeper: `POST /orders` builds an unsigned Solana
//! transaction for a deposit, we sign it, `POST /execute` lands it, and
//! Jupiter's keeper fills the order on the Polymarket CLOB with its own
//! account (project doc `claude/pmj-plan-2026-10-18.md` §3.3; the API's
//! wire facts in the vault, `docs/research/pmj/jup-api-spec-2026-10-18.md`).
//!
//! **Paper only in this pass** (scope ruling): the cores are socket-free and
//! rig-tested; a live slot 11 refuses the boot until a `PMJ-DUST-GATE` line.
//!
//! | module | responsibility |
//! |---|---|
//! | [`tx`] | the transaction parsed in place, the guard, our signature written into its slot |
//! | [`json`] | the build / execute / status / error answers, scanned in place by structure |
//! | [`render`] | the order, execute and claim bodies and the paths, in their final buffers |
//! | [`cmd`] | the 64 B intent and event records of the engine ⇄ jup-gw rings |
//! | [`client`] | the one request cycle jup-gw needs (`core_net::HttpsReq`, or the rig) |
//! | [`gw`] | jup-gw: build → guard → sign → execute → status → recon; balances, positions, claims |
//! | [`arm`] | the engine thread's `OrderDispatch`: admission, fills, the ledger and the worth |
//! | [`held`] | `pmj-held.state`: the legs held, across a restart |
//! | [`stats`] | the arm's and jup-gw's `/metrics` words |
//!
//! ## The laws
//!
//! * **Never sign a server-built transaction blind** ([`tx::guard`]); an
//!   empty allowlist refuses everything. An order is signed only through
//!   [`tx::guard_order`] (its signed create-order a Limit buy at or under
//!   the member's limit, depositing exactly what was asked — PMJ W5), a
//!   claim only through [`tx::guard_claim`].
//! * **The debit halt** — the wallet's deposit-mint balance never falls
//!   below the reference minus the deposits sent since ([`gw`]).
//! * **E-1** — a failure is a refusal, counted; never a modelled fill.
//! * **Fail closed** — an answer that does not read is "in doubt", never an
//!   acceptance.
//!
//! ## Zero allocation, zero copy
//!
//! Every parser records offsets into the caller's buffer; every renderer
//! writes into a caller's fixed buffer. A copy that cannot be avoided
//! carries a `// COPY:` line.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod arm;
pub mod client;
pub mod cmd;
pub mod gw;
pub mod held;
pub mod json;
pub mod render;
pub mod stats;
pub mod tx;

#[cfg(test)]
mod rig;
