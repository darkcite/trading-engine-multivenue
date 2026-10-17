// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `exec-polymarket` — the slot-9 Polymarket order arm (PM15-6)
//!
//! A **FAK-only taker arm** on Polymarket's CLOB V2 for one account and
//! one slot (9, `strategy-pm15`), on the CTF 15-minute up/down markets
//! (`negRisk:false`, market `version:"v1"`, Exchange V2, domain `"2"`).
//! The plan is project doc `claude/pm15-6-exec-plan-2026-10-17.md`; every
//! wire fact is cited in `claude/pm15-clob-v2-spec-2026-10-17.md`.
//!
//! ## PM15-6a (this layer: no socket)
//!
//! | module | responsibility |
//! |---|---|
//! | [`amounts`] | the FAK BUY amounts law (D-3): pUSD to 2 dp, shares truncated to the tick's amount precision — integers only |
//! | [`l2`] | the decoded API secret and the `POLY_SIGNATURE` HMAC over the request's own spans |
//! | [`sign`] | one order's signature (types 0–2 plain, type 3 wrapped) and its `orderID` |
//! | [`render`] | the request in place: the per-account auth lines, the per-token body prefix, the per-order tail and dynamic head |
//! | [`response`] | the `POST /order` answer, read fail-closed into a 64 B record |
//! | [`userws`] | the user channel's `trade` and `order` events, scanned in place |
//! | [`selftest`] | every pinned vector, run at boot before the arm may arm |
//! | [`num`] | the decimal and hex renderers |
//!
//! ## PM15-6b (the threads' cores: socket-free, rig-tested)
//!
//! | module | thread | responsibility |
//! |---|---|---|
//! | [`cmd`] | — | the 64 B command and event records of the three rings |
//! | [`account`] | — | the key, the three addresses of the signature type, the API credentials |
//! | [`tokens`] | pm-gw | the token table the discovery tee feeds: ids, body prefixes, condition ids |
//! | [`clob`] | pm-gw | the request cycle (`core_net::HttpsReq`, or the rig) |
//! | [`gw`] | pm-gw | submit (amounts → sign → body in place → MAC → SENT → POST → answer), the retry law, recon |
//! | [`user`] | pm-user | the user channel: subscribe, trades → FILL, the session's loss → GAP |
//! | [`arm`] | engine | `OrderDispatch`: admission (D-9), the in-flight table, fills once, retirements after them; the ledger, the account's worth and the halt signal (6c) |
//! | [`run`] | pm-gw, pm-user | the threads' loops (6c) |
//! | [`venue`] | boot | the venue gate: version 2, the account open, pUSD readable, the allowance (D-8, 6c) |
//!
//! ## The laws
//!
//! * **E-1** — a failure is a refusal, counted; never a modelled fill.
//! * **E-3 / D7** — the MAC covers the bytes on the wire; nothing is
//!   rendered twice ([`render`], [`l2`]).
//! * **E-5** — the HTTP answer is the ACK; the user channel's `trade` is
//!   the FILL ([`userws`]).
//! * **Fail closed** — an answer that does not read is "in doubt", never
//!   an acceptance and never a refusal that releases the stake
//!   ([`response::Verdict::InDoubt`]).
//!
//! ## Zero allocation
//!
//! Every renderer writes into a caller's fixed buffer; every scanner reads
//! the frame where it lies and writes `repr(C)` records. A copy that
//! cannot be avoided carries a `// COPY:` line.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod account;
pub mod amounts;
pub mod arm;
pub mod clob;
pub mod cmd;
pub mod gw;
pub mod l2;
pub mod num;
pub mod render;
pub mod response;
pub mod run;
pub mod selftest;
pub mod sign;
pub mod tokens;
pub mod user;
pub mod userws;
pub mod venue;

/// The CLOB REST host (V2 since the 2026-04-28 cutover; there is no
/// testnet the arm uses — the venue gate is the self-test plus a FAK
/// priced to miss).
pub const CLOB_HOST: &str = "clob.polymarket.com";

/// The user channel's host.
pub const WS_HOST: &str = "ws-subscriptions-clob.polymarket.com";

/// The user channel's path.
pub const USER_WS_PATH: &str = "/ws/user";

/// The one slot this arm trades (`strategy-pm15`, PM15-5).
pub const PM_SLOT: u8 = core_types::STRATEGY_SLOT_PM15;

/// The order type the arm sends, the only one: fill what the book has
/// now, kill the rest.
pub const ORDER_TYPE_FAK: &str = "FAK";
