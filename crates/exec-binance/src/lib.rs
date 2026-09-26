// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `exec-binance` — the Binance execution arm (BX6)
//!
//! Two halves, two threads, two SPSC rings (plan §3.1):
//!
//! ```text
//! ENGINE THREAD                                   bn-gateway THREAD
//! BnArm: OrderDispatch  ── BnCmd (64 B) ──▶ cmd ring ──▶  every Binance socket and table
//!   row · owned? · quantize · governor · push             render · ONE flush per connection
//!   on_idle: drain ≤ 64 ◀── BnEvt (64 B) ── evt ring ◀──   acks · rejects · retirements · status
//!   engine fill lane 4  ◀── Fill (64 B) ──────────────────  fills (pushed BEFORE the retirement)
//! ```
//!
//! Module map:
//!
//! | module | responsibility |
//! |---|---|
//! | [`cmd`] | [`cmd::BnCmd`], [`cmd::BnEvt`]; size asserts |
//! | [`arm`] | **[`arm::BnArm`]`: OrderDispatch`**, the BX-17 / BX-18 refusals, obligations 1–3 |
//! | [`gateway`] | the thread: boot, the loop, burst render + one flush, timers, rotation |
//! | [`cid`] | the 32-character client order id; the foreign-prefix classifier |
//! | [`num`] | BX-5 quantization by magic reciprocals; the fixed-decimal renderer |
//! | [`inst`] | the instrument tables; the alias-first lookup; the live-tradable predicate |
//! | [`config`] | keys, hosts per scope, the BX-16 network interlock, the one signer |
//! | [`mode`] | account mode and scope; the BX-17 maker list; what is built; the BX-19 judges |
//! | [`gov`] | the order governors (§3.9) and the venue-code law |
//! | [`margin`] | the margin ratio and the O-BX18 per-slot side table (BX-20) |
//! | [`oot`] | the open-order table (1 024, keyed from submit time, in-doubt set) |
//! | [`ttl`] | the maker TTL wheel (BX-13) |
//! | [`clock`] | the venue clock offset, minimum-RTT (BX-6) |
//! | `json` | the one in-place JSON walker the scanners share (crate-private) |
//! | [`userstream`] | the user-data event scanner (BX-2, BX-15); the listenKey lifecycle |
//! | [`wsapi`] | the WS API session: `session.logon`, the order verbs, the answer scanner |
//! | [`rest`] | the signed REST builder (rendered, then signed in place) and its scanners |
//! | [`recon`] | the comparison law (shared-scope filtering), the account scan, the day's spend |
//! | [`journal`] | the `binance-exec.pmlr` writer (cold thread, SPSC ring) |
//! | [`selftest`] | the boot vectors (BX-3) |
//!
//! ## Doctrine
//!
//! * **No allocation of ours after boot**, on either thread. Every table
//!   is fixed and preallocated; every request is rendered into a
//!   boot-owned window. The one allocator on the gateway thread is rustls'
//!   buffered API — its record buffers, per record (gates 72/74b/74c pin
//!   that residue) — and a reconnect's new TLS session.
//! * **Zero-copy.** A request's numbers and client id are rendered in the
//!   frame itself ([`wsapi::Part`]); a copy that cannot be avoided carries
//!   a `// COPY:` line.
//! * **No `dyn`, no closures, no iterator chains on hot paths.** Records are
//!   `repr(C)`, `Copy`, cache-line sized, with no destructor.
//! * **Fail fast, fail closed.** A frame that does not scan is a halt
//!   observation (BX-15), never a skip; a lost ACK is never resent (BX-11).
//!   No panic on a release hot path: invariants are `debug_assert!`.
//! * **Single writer.** The engine thread writes commands; the gateway
//!   thread writes events and fills. No lock anywhere.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod arm;
pub mod cid;
pub mod clock;
pub mod cmd;
pub mod config;
pub mod gateway;
pub mod gov;
pub mod inst;
pub mod journal;
pub(crate) mod json;
pub mod margin;
pub mod mode;
pub mod num;
pub mod oot;
pub mod recon;
pub mod rest;
pub mod selftest;
pub mod ttl;
pub mod userstream;
pub mod wsapi;
