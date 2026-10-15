// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `LiveDispatcher` — the real Polymarket CLOB POST path.
//!
//! Synchronous: each `submit` call signs the order, encodes its JSON
//! straight into the connection's request window, POSTs it, reads the
//! answer, and increments counters. It runs on the `QueuedDispatcher`
//! worker thread (`queued.rs`), never the engine loop: one order may
//! block up to `core_net::https_post::REQ_DEADLINE` (5 s), plus a
//! blocking DNS re-resolve after every third failed dial.
//!
//! The dispatcher holds:
//! * a [`core_net::HttpsPost`] (HD3: core-net's one keep-alive HTTPS
//!   engine, `core_net::HttpsConn`, in its blocking form) — `POST
//!   <path>` rendered once at boot, the reuse probe before a kept-alive
//!   connection is written to, the retire rules, `Content-Length` and
//!   `chunked` framing, and one 5 s budget per order;
//! * the maker's 32-byte signing key (caller-owned; we never log it),
//! * a monotonic `next_salt`/`next_nonce` counter,
//! * dispatch stats.
//!
//! Every transport failure closes the connection, and so does a non-2xx
//! answer, so the next submit redials. Whether a failed order may have
//! reached the CLOB (`PostErr::left_host`) has no `DispatchError`
//! variant and is dropped at this boundary, as before — so a caller that
//! retried after `Disconnected` (which includes a timeout once the order
//! was written) could send it twice. The queued worker does not retry.
//!
//! **Not expected to place a real order** (Polymarket's documentation,
//! read 2026-10-05; not tested against the venue). Its `POST /order`
//! requires the L2 authentication headers (`POLY_API_KEY`,
//! `POLY_ADDRESS`, `POLY_SIGNATURE`, `POLY_PASSPHRASE`, `POLY_TIMESTAMP`)
//! and an `owner` that is the API key owner's UUID, and since CLOB V2
//! (2026-04-28) it refuses V1-signed orders. This dispatcher sends none
//! of the headers, puts the maker address in `owner`, and signs V1
//! (risk-policy "HD3").

use std::sync::Arc;

use core_net::{HttpsPost, PostErr, PostErrKind};
use core_time::now_ns;
use core_types::{CancelReq, Fill, ModifyReq, Order, SymbolId};
use rustls::ClientConfig;
use signer_eip712::{address_from_private_key, parse_secret_key, sign_order_with_key, OrderToSign};

use crate::json_encoder::{encode_signed_order, ORDER_TYPE_GTC};
use crate::response::{parse_clob_response, ClobResponse};
use crate::{DispatchError, DispatchStats, OrderDispatch};

/// Preallocated request-body buffer size. 8 KiB is generous —
/// Polymarket order bodies max out around ~800 bytes including
/// the signature and the address fields.
pub const MAX_REQ_BODY: usize = 8 * 1024;

/// Preallocated response-body buffer size. CLOB responses are small
/// (order id + flags); 8 KiB is generous.
pub const MAX_RESP_BUF: usize = 8 * 1024;

/// Live dispatcher errors surfaced via [`OrderDispatch::submit`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum LiveDispatcherErr {
    /// DNS resolution failed at construction, or the connection's poll
    /// could not be created.
    DnsResolution,
    /// rustls rejected the host as a server name, or the host or the
    /// path was refused at construction (`core_net::HttpsPost::new`).
    BadServerName,
    /// Signer-key bytes failed the secp256k1 scalar check.
    InvalidKey,
}

/// Capacity of the per-dispatcher token-id table. Each Polymarket
/// outcome (e.g. "Yes" / "No" on a binary market) has its own
/// 256-bit on-chain `tokenId`. The cli registers up to this many
/// at boot via [`LiveDispatcher::register_token_id`].
pub const TOKEN_ID_TABLE_CAP: usize = 64;

/// One slot in the token-id lookup table.
#[derive(Copy, Clone, Debug, Default)]
struct TokenIdSlot {
    sym: SymbolId,
    token_id: [u8; 32],
    populated: bool,
}

/// The HTTP/1.1-over-TLS dispatcher.
pub struct LiveDispatcher {
    /// The CLOB connection (HD3): `POST <path>` rendered at boot, each
    /// order's JSON encoded in place in its body window
    /// ([`MAX_REQ_BODY`]), the answer read into its [`MAX_RESP_BUF`]
    /// response buffer — every buffer allocated at boot.
    http: HttpsPost,

    /// Pre-parsed signing key. Building this once at boot saves
    /// the ~1–2 µs `SecretKey::from_slice` scalar-validity check
    /// every `submit_inline` would otherwise pay. The original
    /// raw bytes are zeroized on drop via the inner `SecretKey`'s
    /// own Drop.
    signer_key: signer_eip712::SecretKey,
    maker_address: [u8; 20],

    /// Monotonic counters — every order gets a fresh salt + nonce.
    next_salt: u64,
    next_nonce: u64,

    /// Fixed-capacity SymbolId → 32-byte tokenId map. Populated at
    /// boot via [`register_token_id`]; missing entries fall back to
    /// the left-padded-`sym` encoding (correct for development /
    /// paper; **wrong** for production CLOB orders against real
    /// Polymarket markets, where every outcome has its own
    /// hash-derived tokenId).
    token_id_table: [TokenIdSlot; TOKEN_ID_TABLE_CAP],
    token_id_len: u32,

    stats: DispatchStats,
}

impl LiveDispatcher {
    /// Resolve `host`, render the `POST <path>` head once, and stash
    /// the signing key. Defers the TCP+TLS handshake until the first
    /// `submit`.
    pub fn connect(
        host: &str,
        path: &str,
        port: u16,
        signer_key: [u8; 32],
        tls_config: Arc<ClientConfig>,
    ) -> Result<Self, LiveDispatcherErr> {
        let http = HttpsPost::new(host, port, path, tls_config, MAX_REQ_BODY, MAX_RESP_BUF)
            .map_err(connect_err)?;
        let maker_address =
            address_from_private_key(&signer_key).map_err(|_| LiveDispatcherErr::InvalidKey)?;
        // Pre-parse the secp256k1 scalar exactly once. Per-submit
        // signing reuses this rather than re-running the validity
        // check on every order (~1–2 µs/submit saving).
        let parsed_key =
            parse_secret_key(&signer_key).map_err(|_| LiveDispatcherErr::InvalidKey)?;
        Ok(Self {
            http,
            signer_key: parsed_key,
            maker_address,
            next_salt: now_ns(),
            next_nonce: 0,
            token_id_table: [TokenIdSlot::default(); TOKEN_ID_TABLE_CAP],
            token_id_len: 0,
            stats: DispatchStats::default(),
        })
    }

    /// Register the 32-byte on-chain `tokenId` for a Polymarket
    /// `SymbolId`. Boot-only; the table is fixed-capacity and
    /// linear-scanned on each submit (N=64 is fine — sub-100 ns).
    ///
    /// Without this call, `submit` falls back to a left-padded
    /// `SymbolId` encoding that the live CLOB will reject. The cli
    /// MUST call this for every market it intends to trade against
    /// in `--live` mode.
    pub fn register_token_id(
        &mut self,
        sym: SymbolId,
        token_id: [u8; 32],
    ) -> Result<(), LiveDispatcherErr> {
        let n = self.token_id_len as usize;
        if n >= TOKEN_ID_TABLE_CAP {
            return Err(LiveDispatcherErr::DnsResolution); // reuse a coarse boot-time error variant
        }
        // Reject duplicates so a misconfigured registration loop
        // doesn't silently override an earlier entry.
        for i in 0..n {
            if self.token_id_table[i].sym == sym {
                return Err(LiveDispatcherErr::InvalidKey); // sym already registered
            }
        }
        self.token_id_table[n] = TokenIdSlot {
            sym,
            token_id,
            populated: true,
        };
        self.token_id_len = self.token_id_len.wrapping_add(1);
        Ok(())
    }

    /// O(N) lookup over the populated prefix. Returns `None` when
    /// `sym` was never registered.
    #[inline]
    fn lookup_token_id(&self, sym: SymbolId) -> Option<[u8; 32]> {
        let n = self.token_id_len as usize;
        for i in 0..n {
            let s = &self.token_id_table[i];
            if s.populated && s.sym == sym {
                return Some(s.token_id);
            }
        }
        None
    }

    /// 20-byte maker address derived from the signing key. Public
    /// so the cli can log it at boot.
    #[inline]
    pub fn maker_address(&self) -> &[u8; 20] {
        &self.maker_address
    }

    /// Force a reconnect on the next submit. Cheap; just drops the
    /// connection.
    pub fn close(&mut self) {
        self.http.close();
    }

    /// Inline submit: sign + encode + POST + read response. Returns
    /// the dispatcher-level error on any failure; the caller may
    /// observe `DispatchError::Disconnected` and retry.
    ///
    /// Every error path routes through `DispatchStats::record_rejection`
    /// so the per-category counters (`rejected_queue_full`,
    /// `rejected_network`, `rejected_http_4xx`, etc.) stay consistent
    /// with the aggregate `rejected` counter. The wrapper makes it
    /// impossible to forget a category bump on a future code path.
    pub fn submit_inline(&mut self, order: &Order) -> Result<(), DispatchError> {
        match self.submit_inline_inner(order) {
            Ok(()) => {
                self.stats.accepted = self.stats.accepted.wrapping_add(1);
                Ok(())
            }
            Err(e) => {
                self.stats.record_rejection(e);
                Err(e)
            }
        }
    }

    fn submit_inline_inner(&mut self, order: &Order) -> Result<(), DispatchError> {
        // 1. Translate `Order` → `OrderToSign`.
        let to_sign = self.build_order_to_sign(order);

        // 2. Sign.
        let sig = sign_order_with_key(&to_sign, &self.signer_key)
            .map_err(|_| DispatchError::SignerRejected)?;

        // 3. Encode the JSON straight into the connection's body window:
        //    the bytes the POST sends (HD3).
        let body_len = encode_signed_order(
            self.http.body_mut(),
            &to_sign,
            &sig,
            &self.maker_address,
            ORDER_TYPE_GTC,
        )
        .map_err(|_| DispatchError::EncodeOverflow)?;

        // 4. POST it — dialing first, or afresh when the kept-alive
        //    connection died idle — and read the whole answer.
        let (status, body) = self.http.post(body_len).map_err(post_err)?;
        if !(200..300).contains(&status) {
            self.close();
            return Err(DispatchError::Http(status));
        }

        // 5. Parse the body.
        match parse_clob_response(&self.http.resp()[body]) {
            Ok(ClobResponse::Ok { .. }) => Ok(()),
            Ok(ClobResponse::Err { .. }) => Err(DispatchError::Http(status)),
            Err(_) => Err(DispatchError::JsonMalformed),
        }
    }

    /// Build the [`OrderToSign`] from a strategy-emitted [`Order`]
    /// plus per-dispatcher monotonic counters. Phase 3 v1 wires
    /// only the maker/signer/taker/amount fields; token id and
    /// expiration are caller-driven via TODO future config layer.
    fn build_order_to_sign(&mut self, order: &Order) -> OrderToSign {
        let salt = self.next_salt;
        self.next_salt = self.next_salt.wrapping_add(1);
        let nonce = self.next_nonce;
        self.next_nonce = self.next_nonce.wrapping_add(1);

        // Token id lookup. Operator must call
        // `LiveDispatcher::register_token_id(sym, bytes)` at boot
        // for every market traded in `--live`. If the operator
        // forgets, we fall back to the left-padded-`sym` encoding
        // for development; the CLOB will reject it as
        // `Http(4xx)` so the bad state surfaces immediately on
        // the first real submit. Phase 8 will gate `--live` on a
        // non-empty token-id table.
        let token_id = self
            .lookup_token_id(order.sym)
            .unwrap_or_else(|| symbol_to_token_id(order.sym));

        // Side: Polymarket convention is 0 = Buy, 1 = Sell.
        let side = match order.side {
            core_types::Side::Bid => 0u8,
            core_types::Side::Ask => 1u8,
        };

        // Amounts: use the strategy-chosen qty for one leg; the
        // other leg is implied by `px`. We send `makerAmount =
        // qty`, `takerAmount = qty * px` in fixed-point 1e6. The
        // CLOB verifies the price internally; this is one of
        // many valid encodings.
        let qty = order.qty.raw().max(0) as u128;
        let px = order.px.raw().max(0) as u128;
        let (maker_amount, taker_amount) = match order.side {
            core_types::Side::Bid => (qty.saturating_mul(px), qty),
            core_types::Side::Ask => (qty, qty.saturating_mul(px)),
        };

        OrderToSign::new(
            salt,
            self.maker_address,
            self.maker_address,
            [0u8; 20], // open order
            token_id,
            maker_amount,
            taker_amount,
            0, // expiration: GTC for v1
            nonce,
            0, // feeRateBps — Polymarket sets the fee server-side
            side,
            0, // signatureType = EOA
        )
    }
}

impl OrderDispatch for LiveDispatcher {
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.submit_inline(order)
    }

    /// The legacy Polymarket path places only: no cancel endpoint is
    /// wired, so the verb is refused, in writing (BX3, O-BX19).
    fn cancel(&mut self, _req: &CancelReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    /// As [`Self::cancel`].
    fn modify(&mut self, _req: &ModifyReq) -> Result<(), DispatchError> {
        Err(DispatchError::Unsupported)
    }

    fn try_next_fill(&mut self) -> Option<Fill> {
        // Phase 3 v1 doesn't wire the fill feed. Polymarket's WS
        // `order` channel arrives in Phase 4.
        None
    }

    fn stats(&self) -> DispatchStats {
        self.stats
    }
}

// -----------------------------------------------------------------
// helpers
// -----------------------------------------------------------------

/// A construction refusal of [`HttpsPost::new`] as this dispatcher's
/// boot error: a refused host or path is `BadServerName`; DNS, or a poll
/// that could not be created, is `DnsResolution`. The per-request kinds
/// never come out of a constructor.
#[inline]
const fn connect_err(e: PostErrKind) -> LiveDispatcherErr {
    match e {
        PostErrKind::BadEndpoint => LiveDispatcherErr::BadServerName,
        _ => LiveDispatcherErr::DnsResolution,
    }
}

/// A failed POST as the dispatcher's error. An answer that was not
/// bounded HTTP/1.1 is `JsonMalformed`, as before HD3; every other
/// failure is `Disconnected` — the connection is closed and the next
/// submit redials. `left_host` has no `DispatchError` variant and is
/// dropped here.
#[inline]
const fn post_err(e: PostErr) -> DispatchError {
    match e.err {
        PostErrKind::BadHttp => DispatchError::JsonMalformed,
        _ => DispatchError::Disconnected,
    }
}

/// Pack a `SymbolId` into a 32-byte token id, big-endian, left-
/// padded. Phase 3 v1 only — Phase 4 replaces this with a real
/// SymbolId → on-chain tokenId table.
#[inline]
fn symbol_to_token_id(sym: SymbolId) -> [u8; 32] {
    let mut out = [0u8; 32];
    let be = (sym as u128).to_be_bytes();
    // COPY: 16 B on the stack — the id's big-endian bytes into the low
    // half of the 32 B token id, only for a symbol with no registered id
    // (the fallback the live CLOB refuses) — rejected: nothing to reject;
    // the copy is the value being built.
    out[16..32].copy_from_slice(&be);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_to_token_id_left_pads() {
        let t = symbol_to_token_id(0xABCD);
        assert_eq!(t[0..30], [0u8; 30]);
        assert_eq!(t[30], 0xAB);
        assert_eq!(t[31], 0xCD);
    }

    fn webpki_cfg() -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder()
                .with_root_certificates({
                    let mut s = rustls::RootCertStore::empty();
                    s.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                    s
                })
                .with_no_client_auth(),
        )
    }

    /// Boot-path test: `LiveDispatcher::connect` accepts a
    /// well-formed key, builds its connection (no socket is opened
    /// until the first submit), and stashes the pre-parsed
    /// `SecretKey` + maker address. We can't reach the
    /// real CLOB from a unit test; the DNS-resolution branch is
    /// covered by the TLS-loopback integration test.
    /// We instead exercise `register_token_id` round-trips against
    /// a constructed dispatcher.
    fn fresh_dispatcher() -> LiveDispatcher {
        // 127.0.0.1 always resolves; we never actually connect.
        let mut key = [0u8; 32];
        key[31] = 1;
        LiveDispatcher::connect("127.0.0.1", "/order", 443, key, webpki_cfg())
            .expect("LiveDispatcher::connect should succeed for 127.0.0.1")
    }

    /// HD3: the transport's errors keep their pre-HD3 meaning at the
    /// `DispatchError` / `LiveDispatcherErr` boundary.
    #[test]
    fn transport_errors_keep_their_meaning() {
        let cases = [
            (PostErrKind::BadHttp, DispatchError::JsonMalformed),
            (PostErrKind::Timeout, DispatchError::Disconnected),
            (PostErrKind::Overflow, DispatchError::Disconnected),
            (PostErrKind::Disconnected, DispatchError::Disconnected),
        ];
        for (err, want) in cases {
            let e = PostErr {
                err,
                left_host: true,
            };
            assert_eq!(post_err(e), want, "{err:?}");
        }
        assert_eq!(
            connect_err(PostErrKind::Dns),
            LiveDispatcherErr::DnsResolution
        );
        assert_eq!(
            connect_err(PostErrKind::BadEndpoint),
            LiveDispatcherErr::BadServerName
        );
        let mut key = [0u8; 32];
        key[31] = 1;
        assert!(
            matches!(
                LiveDispatcher::connect("127.0.0.1", "order", 443, key, webpki_cfg()),
                Err(LiveDispatcherErr::BadServerName)
            ),
            "a relative path is refused at boot"
        );
    }

    #[test]
    fn register_token_id_round_trips() {
        let mut d = fresh_dispatcher();
        let mut tid = [0u8; 32];
        tid[31] = 0xAB;
        d.register_token_id(42, tid).expect("register");
        // Internal lookup — we can reach it via the encoder seam
        // through `build_order_to_sign`, but the simpler invariant
        // is "registering same sym again errors".
        let r2 = d.register_token_id(42, tid);
        assert!(r2.is_err(), "duplicate registration must reject");
    }

    #[test]
    fn register_token_id_capacity_bound() {
        let mut d = fresh_dispatcher();
        let tid = [0u8; 32];
        // First TOKEN_ID_TABLE_CAP registrations fit; the next
        // returns an error.
        for sym in 0..TOKEN_ID_TABLE_CAP as u32 {
            d.register_token_id(sym, tid)
                .expect("register up to capacity");
        }
        let overflow = d.register_token_id(TOKEN_ID_TABLE_CAP as u32, tid);
        assert!(overflow.is_err(), "overflow registration must reject");
    }

    #[test]
    fn maker_address_is_stable_for_fixed_key() {
        let d = fresh_dispatcher();
        let a1 = *d.maker_address();
        let d2 = fresh_dispatcher();
        let a2 = *d2.maker_address();
        assert_eq!(a1, a2, "same key → same maker address");
    }
}
