// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Phase C — the order lifecycle round-trip (plan §5.1).
//!
//! Phases A and B in [`crate::smoke`] prove the venue verifies our
//! SIGNATURE. They prove nothing about the venue's order state
//! machine, because they never create an order. Phase C does:
//!
//! ```text
//!   place  ->  the venue echoes an oid and the order RESTS
//!   modify ->  the resting order moves
//!   cancel ->  by CLOID, not by oid
//!   verify ->  cancelling it again is refused, so it is really gone
//! ```
//!
//! The last step is the one that makes the other three mean
//! something. Without it, "cancel returned ok" is a claim about a
//! response body, not about the book.
//!
//! ## Why this is not in the pre-restart gate
//!
//! It costs balance, it creates state on the account, and it needs a
//! funded testnet account with a registered agent — none of which the
//! restart lane should depend on. It is **opt-in**
//! (`exec-smoke --lifecycle`) and run by a person.
//!
//! ## The four things that keep it from costing anything real
//!
//! 1. **Testnet only**, by the same guard as [`crate::smoke::run`] —
//!    which is checked here again rather than assumed.
//! 2. **Post-only (ALO).** A post-only order cannot take liquidity: if
//!    it would cross, the venue REJECTS it rather than filling it. So
//!    the failure mode of a badly chosen price is a refusal, not a
//!    position.
//! 3. **The operator states the market and the price.** Nothing is
//!    derived — LAW E-4 forbids deriving an asset id, and a price this
//!    module guessed would be the one number capable of turning a test
//!    into a trade.
//! 4. **It cleans up after itself.** Any failure after the place
//!    attempts a cancel before returning, and says so if that cancel
//!    also failed — a probe that strands a resting order on the
//!    account is worse than one that never ran.

use std::sync::Arc;

use crate::action::{
    encode_cancel_by_cloid, encode_order, CancelByCloidWire, ModifyWire, OrderWire, Tif, MAX_ACTION,
};
use crate::config::HlConfig;
use crate::http::{HlHttp, MAX_REQ_BODY};
use crate::request::{batch_modify_json, cancel_by_cloid_json, envelope, order_json};
use crate::response::{scan, HlOk, HlResponse};
use crate::sign::{sign_action, Vault};
use crate::smoke::SmokeErr;

/// What the operator must state. Nothing here is derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleSpec {
    /// Venue asset id. **Bound by the operator, never derived**
    /// (LAW E-4).
    pub asset: u32,
    /// Resting price, 1e8-scaled. Must be far enough from the market
    /// that a post-only order rests instead of being refused.
    pub px_1e8: i64,
    /// The price to modify to, 1e8-scaled.
    pub px2_1e8: i64,
    /// Size, 1e8-scaled.
    pub sz_1e8: i64,
    /// Buy side. A resting BID below the market is the safe default;
    /// a resting ask below the market would cross.
    pub is_buy: bool,
}

/// What the round trip observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleReport {
    /// The oid the venue echoed for the placed order.
    pub placed_oid: u64,
    /// The oid after the modify (the venue may issue a new one).
    pub modified_oid: u64,
    /// The cancel-by-cloid was accepted.
    pub cancelled: bool,
    /// Cancelling the same cloid AGAIN was refused — so the first
    /// cancel really removed it, rather than merely returning `ok`.
    pub verified_gone: bool,
}

impl LifecycleReport {
    /// Every stage did what it claimed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.placed_oid != 0 && self.cancelled && self.verified_gone
    }
}

/// Run the place → modify → cancel → verify round trip.
///
/// Builds its own client from `cfg.host`, so nothing a caller passes
/// can redirect where this goes.
pub fn run(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    spec: LifecycleSpec,
) -> Result<LifecycleReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;
    run_on(cfg, &mut http, spec)
}

/// The same round trip over a caller-supplied transport.
///
/// This is the seam the loopback test drives: a four-request sequence
/// against a scripted server is the only way to exercise the modify
/// and cleanup paths before a funded account exists, and a lifecycle
/// whose failure paths have never run is not one to point at an
/// account for the first time.
///
/// It is not a way around the guard. The `is_testnet` check runs here
/// too, and more to the point the signature is computed over
/// `cfg.network`'s source byte — so an action signed under a testnet
/// config recovers to a different address on mainnet and cannot be
/// replayed there whatever socket carried it.
pub fn run_on(
    cfg: &HlConfig,
    http: &mut HlHttp,
    spec: LifecycleSpec,
) -> Result<LifecycleReport, SmokeErr> {
    // Checked again rather than assumed: this is a public entry point.
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    if spec.sz_1e8 <= 0 || spec.px_1e8 <= 0 || spec.px2_1e8 <= 0 {
        return Err(SmokeErr::Lifecycle {
            stage: "spec",
            msg: "price and size must both be positive".to_owned(),
        });
    }

    let sk = cfg.secret_key().map_err(SmokeErr::Config)?;
    let cloid = fresh_cloid();
    // FOUR requests, back to back, easily inside one millisecond. A
    // nonce that repeated would have the venue reject the second as a
    // replay and the failure would read as a lifecycle bug.
    let mut nonces = crate::nonce::Nonce::new();

    // ---- place ------------------------------------------------------
    // ALO. Post-only is what makes a wrong price a refusal instead of
    // a fill.
    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Alo)
        .with_cloid(cloid);
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_order(&mut mp, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let aj_n = order_json(&mut aj, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let ok = post(http, &sk, cfg, &mut nonces, &mp[..mp_n], &aj[..aj_n], "place")?;

    if ok.any_filled {
        // A post-only order should be unable to trade. If one did, the
        // price was wrong in a way that cost real (testnet) money, and
        // nothing below should run on top of that.
        return Err(SmokeErr::Lifecycle {
            stage: "place",
            msg: "the post-only order FILLED — the price crosses the market. Nothing after this \
                  would mean anything; choose a price further from the book."
                .to_owned(),
        });
    }
    if !ok.any_resting || ok.oid == 0 {
        return Err(SmokeErr::Lifecycle {
            stage: "place",
            msg: "accepted but not resting, and with no oid echoed back".to_owned(),
        });
    }
    let placed_oid = ok.oid;

    // From here on, every failure path must try to take the order back
    // off the book before returning.
    let modified_oid = match modify(http, &sk, cfg, &mut nonces, spec, cloid, placed_oid) {
        Ok(oid) => oid,
        Err(e) => return Err(cleanup(http, &sk, cfg, &mut nonces, spec.asset, cloid, e)),
    };

    // ---- cancel by CLOID --------------------------------------------
    // By cloid rather than by oid deliberately: the modify may have
    // issued a NEW oid, and a client that tracked only the old one
    // would be unable to cancel what it placed. LAW E-9 puts the slot
    // in the cloid precisely so the client id is the durable handle.
    let ok = match cancel(http, &sk, cfg, &mut nonces, spec.asset, cloid, "cancel") {
        Ok(ok) => ok,
        Err(e) => return Err(cleanup(http, &sk, cfg, &mut nonces, spec.asset, cloid, e)),
    };
    let cancelled = ok.errors == 0 && ok.any_success;
    if !cancelled {
        return Err(SmokeErr::Lifecycle {
            stage: "cancel",
            msg: "the cancel was not accepted; the order may still be resting".to_owned(),
        });
    }

    // ---- verify it is really gone -----------------------------------
    // Cancelling the same cloid again must be REFUSED. A second `ok`
    // here would mean the venue accepts cancels for orders it does not
    // have, and the first `ok` would have proved nothing.
    let ok = cancel(http, &sk, cfg, &mut nonces, spec.asset, cloid, "verify")?;
    let verified_gone = ok.errors > 0;

    Ok(LifecycleReport {
        placed_oid,
        modified_oid,
        cancelled,
        verified_gone,
    })
}

#[allow(clippy::too_many_arguments)]
/// The three actions a run would send, built and shown but not sent.
///
/// Phase C is the one thing in this lane that can create state on a
/// funded account, and its inputs are four numbers typed on a command
/// line. A transposed price or a size off by a decimal is a plausible
/// mistake and an expensive one, so there is a way to see exactly what
/// would go on the wire first.
///
/// Needs no key, no network and no account — so it can be run before
/// any of those exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    /// The `order` action, as JSON.
    pub place: String,
    /// The `batchModify` action, as JSON.
    pub modify: String,
    /// The `cancelByCloid` action, as JSON.
    pub cancel: String,
    /// The price as the VENUE will read it, after wire rendering.
    pub px: String,
    /// The modified price, likewise.
    pub px2: String,
    /// The size as the venue will read it.
    pub sz: String,
}

/// Build the three actions without sending or signing anything.
pub fn preview(spec: LifecycleSpec) -> Result<Preview, SmokeErr> {
    if spec.sz_1e8 <= 0 || spec.px_1e8 <= 0 || spec.px2_1e8 <= 0 {
        return Err(SmokeErr::Lifecycle {
            stage: "spec",
            msg: "price and size must both be positive".to_owned(),
        });
    }
    // A FIXED cloid, not a fresh one: a preview that changed on every
    // run would be useless to diff, and nothing here reaches a venue.
    let cloid = [0u8; 16];
    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Alo)
        .with_cloid(cloid);
    let replacement =
        OrderWire::new(spec.asset, spec.is_buy, spec.px2_1e8, spec.sz_1e8, Tif::Alo)
            .with_cloid(cloid);

    let mut buf = [0u8; MAX_ACTION];
    let n = order_json(&mut buf, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let place = String::from_utf8_lossy(&buf[..n]).to_string();

    let m = [ModifyWire {
        order: replacement,
        oid: 0,
        oid_cloid: [0u8; 16],
        oid_is_cloid: false,
    }];
    let n = batch_modify_json(&mut buf, &m).map_err(|_| SmokeErr::Encode)?;
    let modify = String::from_utf8_lossy(&buf[..n]).to_string();

    let c = [CancelByCloidWire {
        asset: spec.asset,
        cloid,
    }];
    let n = cancel_by_cloid_json(&mut buf, &c).map_err(|_| SmokeErr::Encode)?;
    let cancel = String::from_utf8_lossy(&buf[..n]).to_string();

    // Rendered through the SAME WireNum the signed action uses, so
    // what is shown is what the venue reads — not a re-derivation that
    // could round differently and reassure about the wrong number.
    Ok(Preview {
        place,
        modify,
        cancel,
        px: wire_str(spec.px_1e8),
        px2: wire_str(spec.px2_1e8),
        sz: wire_str(spec.sz_1e8),
    })
}

fn wire_str(v_1e8: i64) -> String {
    let w = crate::wire::WireNum::from_1e8(v_1e8);
    String::from_utf8_lossy(w.as_bytes()).to_string()
}

fn modify(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    spec: LifecycleSpec,
    cloid: [u8; 16],
    oid: u64,
) -> Result<u64, SmokeErr> {
    let replacement =
        OrderWire::new(spec.asset, spec.is_buy, spec.px2_1e8, spec.sz_1e8, Tif::Alo)
            .with_cloid(cloid);
    let m = [ModifyWire {
        order: replacement,
        oid,
        oid_cloid: [0u8; 16],
        oid_is_cloid: false,
    }];
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = crate::action::encode_batch_modify(&mut mp, &m).map_err(|_| SmokeErr::Encode)?;
    let aj_n = batch_modify_json(&mut aj, &m).map_err(|_| SmokeErr::Encode)?;
    let ok = post(http, sk, cfg, nonces, &mp[..mp_n], &aj[..aj_n], "modify")?;
    if ok.any_filled {
        return Err(SmokeErr::Lifecycle {
            stage: "modify",
            msg: "the modified post-only order FILLED — the second price crosses the market"
                .to_owned(),
        });
    }
    if !ok.any_resting {
        return Err(SmokeErr::Lifecycle {
            stage: "modify",
            msg: "accepted but the order is no longer resting".to_owned(),
        });
    }
    // The venue may reuse the oid or issue a new one; either is fine,
    // and which it does is worth recording.
    Ok(if ok.oid == 0 { oid } else { ok.oid })
}

fn cancel(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    asset: u32,
    cloid: [u8; 16],
    stage: &'static str,
) -> Result<HlOk, SmokeErr> {
    let c = [CancelByCloidWire { asset, cloid }];
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_cancel_by_cloid(&mut mp, &c).map_err(|_| SmokeErr::Encode)?;
    let aj_n = cancel_by_cloid_json(&mut aj, &c).map_err(|_| SmokeErr::Encode)?;
    post(http, sk, cfg, nonces, &mp[..mp_n], &aj[..aj_n], stage)
}

/// Best-effort removal of a resting order after a failure, folding the
/// outcome into the error the caller is already returning.
fn cleanup(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    asset: u32,
    cloid: [u8; 16],
    original: SmokeErr,
) -> SmokeErr {
    match cancel(http, sk, cfg, nonces, asset, cloid, "cleanup") {
        Ok(ok) if ok.errors == 0 => original,
        _ => SmokeErr::Lifecycle {
            stage: "cleanup",
            msg: format!(
                "{original} — AND the order could not be cancelled afterwards. An order placed \
                 by this probe may still be RESTING on the testnet account; cancel it by hand."
            ),
        },
    }
}

/// Sign, send, scan. Every stage goes through here so that the
/// fail-closed reading of the venue's answer is stated once.
#[allow(clippy::too_many_arguments)]
fn post(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    action_mp: &[u8],
    action_json: &[u8],
    stage: &'static str,
) -> Result<HlOk, SmokeErr> {
    let nonce = nonces.next(crate::smoke::now_ms());
    let sig = sign_action(sk, action_mp, nonce, Vault::None, None, cfg.network)
        .map_err(|_| SmokeErr::Sign)?;
    let mut body = [0u8; MAX_REQ_BODY];
    let n = envelope(&mut body, action_json, nonce, &sig, None, None)
        .map_err(|_| SmokeErr::Encode)?;
    let (_status, range) = http.post(&body[..n]).map_err(SmokeErr::Http)?;
    let resp = http.resp();
    let slice = &resp[range];
    match scan(slice).map_err(|_| SmokeErr::Unreadable)? {
        HlResponse::Ok(ok) => {
            // `status:"ok"` is an envelope verdict, not an acceptance —
            // a per-item error rides inside it. The `verify` stage is
            // the one place that WANTS an item error, so it reads
            // `ok.errors` itself.
            if ok.errors > 0 && stage != "verify" && stage != "cleanup" {
                return Err(SmokeErr::Lifecycle {
                    stage,
                    msg: String::from_utf8_lossy(ok.first_error.of(slice)).to_string(),
                });
            }
            Ok(ok)
        }
        HlResponse::Err { msg } => Err(SmokeErr::Lifecycle {
            stage,
            msg: String::from_utf8_lossy(msg.of(slice)).to_string(),
        }),
    }
}

/// A cloid unique to this run.
///
/// LAW E-9 puts the slot in the cloid; this probe is not a slot, so
/// the high bytes are a fixed marker that says "exec-smoke placed
/// this" — an order found resting on the account can be traced back
/// here rather than guessed at.
fn fresh_cloid() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = 0xE3; // the phase that placed it
    c[1] = 0x5C; // "smoke"
    let ms = crate::smoke::now_ms().to_be_bytes();
    c[8..16].copy_from_slice(&ms);
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Scope, HOST_MAINNET};

    const KEY: [u8; 32] = [0x55; 32];
    const ADDR: [u8; 20] = [0x66; 20];

    fn spec() -> LifecycleSpec {
        LifecycleSpec {
            asset: 100_000_000 + 10 * 3253,
            px_1e8: 1_000_000,
            px2_1e8: 2_000_000,
            sz_1e8: 1_000_000_000,
            is_buy: true,
        }
    }

    /// The same guard as phase A/B, restated because this module is
    /// public and reachable without going through `smoke::run`.
    #[test]
    fn mainnet_is_refused_here_too() {
        let tls = core_net::TlsTransport::default_client_config();
        let m = HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR).expect("cfg");
        let e = run(&m, tls, spec()).expect_err("mainnet must be refused");
        assert!(matches!(e, SmokeErr::NotTestnet(_)), "{e:?}");
    }

    /// A zero or negative size would be an order the venue reads very
    /// differently from what was meant. Refuse before signing.
    #[test]
    fn a_nonsense_spec_is_refused_before_anything_is_signed() {
        let tls = core_net::TlsTransport::default_client_config();
        let c = HlConfig::new(Scope::Testnet, crate::config::HOST_TESTNET, 'b', KEY, ADDR)
            .expect("cfg");
        for bad in [
            LifecycleSpec { sz_1e8: 0, ..spec() },
            LifecycleSpec { sz_1e8: -1, ..spec() },
            LifecycleSpec { px_1e8: 0, ..spec() },
            LifecycleSpec { px2_1e8: -5, ..spec() },
        ] {
            let e = run(&c, tls.clone(), bad).expect_err("must refuse");
            assert!(matches!(e, SmokeErr::Lifecycle { stage: "spec", .. }), "{e:?}");
        }
    }

    /// The preview must show the numbers the VENUE will read, not the
    /// integers that were typed — that is the whole point of looking.
    #[test]
    fn the_preview_shows_the_wire_form_of_every_number() {
        let p = preview(spec()).expect("preview");
        assert_eq!(p.px, "0.01");
        assert_eq!(p.px2, "0.02");
        assert_eq!(p.sz, "10");
        // And those same strings are what the action carries.
        assert!(p.place.contains(r#""p":"0.01""#), "{}", p.place);
        assert!(p.place.contains(r#""s":"10""#), "{}", p.place);
        assert!(p.modify.contains(r#""p":"0.02""#), "{}", p.modify);
        // Post-only, always.
        assert!(p.place.contains("Alo"), "{}", p.place);
        assert!(p.modify.contains("Alo"), "{}", p.modify);
        // The asset the operator stated, in all three — but note the
        // KEY is not the same in all three. `order` and `batchModify`
        // spell it "a"; `cancelByCloid` spells it "asset". That is the
        // venue SDK's own inconsistency, reproduced deliberately
        // because the msgpack of these actions is what the signature
        // covers (LAW E-3) — so both spellings are pinned here rather
        // than tidied into one.
        let short = format!(r#""a":{}"#, spec().asset);
        assert!(p.place.contains(&short), "{}", p.place);
        assert!(p.modify.contains(&short), "{}", p.modify);
        let long = format!(r#""asset":{}"#, spec().asset);
        assert!(p.cancel.contains(&long), "{}", p.cancel);
        assert!(
            !p.cancel.contains(&short),
            "cancelByCloid must NOT use the short key: {}",
            p.cancel
        );
    }

    /// A preview is worthless if it can differ from what would be
    /// sent, so it refuses the same specs the real run refuses.
    #[test]
    fn the_preview_refuses_what_the_run_refuses() {
        for bad in [
            LifecycleSpec { sz_1e8: 0, ..spec() },
            LifecycleSpec { px_1e8: -1, ..spec() },
        ] {
            assert!(matches!(
                preview(bad),
                Err(SmokeErr::Lifecycle { stage: "spec", .. })
            ));
        }
    }

    #[test]
    fn a_report_passes_only_when_the_order_was_really_removed() {
        let base = LifecycleReport {
            placed_oid: 7,
            modified_oid: 8,
            cancelled: true,
            verified_gone: true,
        };
        assert!(base.passed());
        assert!(!LifecycleReport { verified_gone: false, ..base }.passed());
        assert!(!LifecycleReport { cancelled: false, ..base }.passed());
        assert!(!LifecycleReport { placed_oid: 0, ..base }.passed());
    }

    /// Two runs must not collide on the account.
    #[test]
    fn each_run_gets_its_own_traceable_cloid() {
        let a = fresh_cloid();
        assert_eq!(a[0], 0xE3, "the marker says which probe placed it");
        assert_eq!(a[1], 0x5C);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = fresh_cloid();
        assert_ne!(a, b, "two runs collided");
    }
}
