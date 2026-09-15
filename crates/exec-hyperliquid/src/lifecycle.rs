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
/// The largest notional phase D will send, 1e8-scaled: **$100**.
///
/// Not a risk limit — it is a TYPO limit. Every cap that matters lives
/// in `strategy-*` and the ruleset validator, and this path traverses
/// none of them: it is an operator typing four numbers on a command
/// line straight into an order that is DESIGNED to execute. A size off
/// by a decimal is the plausible mistake, and on this path it does not
/// bounce off post-only — it trades.
///
/// The value is deliberately small. Phase D exists to prove a cloid
/// round trip, and $100 is two orders of magnitude more than that
/// needs. Raising it is an edit here, in the open, rather than a
/// number that was never there.
///
/// **A MAINNET VARIANT MUST NOT COPY THIS CONST.** It is a single
/// global ceiling, which is the wrong shape for real caps: those are
/// per-venue and per-unit (`strategy_core::caps_for_venue`, and
/// Deribit is capped in coins rather than dollars). A second
/// order-submission path running to its own numbers is precisely the
/// hole `docs/risk-policy.md` warns about. Anything that trades for
/// real reads `caps_for_venue`; this number exists only because phase
/// D traverses none of that machinery and an unbounded `i64` on an
/// executing path is indefensible even on testnet.
pub const MAX_FILL_NOTIONAL_1E8: i128 = 100 * 100_000_000;

/// What the operator must state to make a REAL TRADE. Nothing here is
/// derived, for the same reason nothing in [`LifecycleSpec`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillSpec {
    /// Venue asset id. **Bound by the operator, never derived**
    /// (LAW E-4).
    pub asset: u32,
    /// A price that CROSSES the book, 1e8-scaled. The operator states
    /// it, having looked at the book — this code does not read the
    /// book and does not guess, because the one number able to turn a
    /// test into an expensive trade is the price.
    pub px_1e8: i64,
    /// Size, 1e8-scaled.
    pub sz_1e8: i64,
    /// Buy side.
    pub is_buy: bool,
    /// The slot the cloid will name (LAW E-9). The point of this probe
    /// is that the cloid comes back down `userFills` and decodes to
    /// this, so it is OUR magic, not the smoke's.
    pub strategy_id: u8,
    /// The member's own id for the order — what `Fill::order_id` must
    /// carry when the fill is booked.
    pub client_oid: u64,
}

/// What the trade observed. **The ACK, not the fill** (LAW E-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillReport {
    /// The cloid sent — look for this in `userFills`.
    pub cloid: [u8; 16],
    /// The oid the venue echoed.
    pub oid: u64,
    /// The venue said `filled`.
    pub any_filled: bool,
    /// The venue said `resting`. **An IoC should never rest**; if this
    /// is true the venue did something the order type forbids — and
    /// the order is then ON THE BOOK. There is no cleanup path here
    /// (an IoC is not supposed to need one), so the caller must cancel
    /// it. The cloid is deterministic — `cloid::encode(strategy_id,
    /// client_oid)` — so it can be cancelled by cloid from the same
    /// two numbers that placed it.
    pub any_resting: bool,
}

/// The spec checks, shared by [`run_fill_on`] and [`preview_fill`] so
/// a dry run cannot pass inputs the real send would refuse.
///
/// # Errors
/// Non-positive price or size, or a notional over
/// [`MAX_FILL_NOTIONAL_1E8`].
fn check_fill_spec(spec: FillSpec) -> Result<(), SmokeErr> {
    if spec.sz_1e8 <= 0 || spec.px_1e8 <= 0 {
        return Err(SmokeErr::Lifecycle {
            stage: "fill-spec",
            msg: "price and size must both be positive".to_owned(),
        });
    }
    // i128 so the multiply cannot wrap before the comparison — the
    // guard would otherwise be defeated by exactly the mistyped size
    // it exists to catch.
    let notional = i128::from(spec.px_1e8) * i128::from(spec.sz_1e8) / 100_000_000;
    if notional > MAX_FILL_NOTIONAL_1E8 {
        return Err(SmokeErr::Lifecycle {
            stage: "fill-spec",
            msg: format!(
                "notional {notional} exceeds the phase D ceiling of {MAX_FILL_NOTIONAL_1E8} \
                 (1e8-scaled). This is a TYPO limit, not a risk limit: phase D trades on \
                 purpose and nothing else on this path is capped. Raise \
                 MAX_FILL_NOTIONAL_1E8 deliberately if you mean it."
            ),
        });
    }
    Ok(())
}

/// What phase D WOULD send, and sends nothing.
///
/// The same rationale `--lifecycle` has for a dry run, and more of it:
/// there a mistyped price is REFUSED by post-only and the venue's
/// price band, so the dry run guards the weaker case. Here the same
/// typo **executes**. The notional is rendered because that is the
/// number a decimal slip corrupts, and the action goes through the
/// same `order_json` the signed send uses, so what is printed is what
/// the venue would read.
///
/// # Errors
/// As [`check_fill_spec`].
pub fn preview_fill(spec: FillSpec) -> Result<FillPreview, SmokeErr> {
    check_fill_spec(spec)?;
    let cloid = crate::cloid::encode(spec.strategy_id, spec.client_oid);
    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Ioc)
        .with_cloid(cloid);
    let mut buf = [0u8; MAX_ACTION];
    let n = order_json(&mut buf, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let mut hex = [0u8; 34];
    let hn = crate::cloid::to_hex(&cloid, &mut hex);
    Ok(FillPreview {
        place: String::from_utf8_lossy(&buf[..n]).to_string(),
        px: wire_str(spec.px_1e8),
        sz: wire_str(spec.sz_1e8),
        notional: wire_str(
            ((i128::from(spec.px_1e8) * i128::from(spec.sz_1e8)) / 100_000_000) as i64,
        ),
        cloid: String::from_utf8_lossy(&hex[..hn]).to_string(),
    })
}

/// What [`preview_fill`] renders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillPreview {
    /// The `order` action, as JSON.
    pub place: String,
    /// The price as the VENUE will read it.
    pub px: String,
    /// The size as the venue will read it.
    pub sz: String,
    /// `px × sz` — the number a misplaced decimal corrupts.
    pub notional: String,
    /// The cloid, hex. This is what to look for in `userFills`.
    pub cloid: String,
}

/// Place ONE IoC that is meant to trade, and report the venue's ACK.
///
/// Separate from [`run`] rather than a flag on it, because they are
/// opposite in intent. `run` places a POST-ONLY order and treats a
/// fill as a failure — it tests the lifecycle without trading. This
/// deliberately trades, and exists for one reason: **nothing else
/// proves that our own cloid survives the round trip and comes back
/// down `userFills`** (LAW E-9). The fill lane's attribution rests
/// entirely on that, and until this ran it rested on source code.
///
/// Why an IoC rather than a crossing limit: an IoC either trades or is
/// gone, so this probe — unlike the post-only one — carries no cleanup
/// path. **That is a property of the ORDER TYPE, not a guarantee this
/// code enforces.** A venue that rested one anyway would leave the
/// order on the book with nothing behind it; [`FillReport::any_resting`]
/// is how the caller finds out, and the cloid is deterministic from
/// `(strategy_id, client_oid)` so it can still be cancelled by cloid.
///
/// **This is the ACK.** Per LAW E-5 the fill itself is whatever
/// `userFills` says; a caller that wants proof reads the stream.
///
/// # Errors
/// Not testnet, a non-positive price or size, or the venue refused the
/// action.
pub fn run_fill(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    spec: FillSpec,
) -> Result<FillReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;
    run_fill_on(cfg, &mut http, spec)
}

/// The same trade over a caller-supplied transport — the seam the
/// loopback test drives (`tests/hl_lifecycle_loopback.rs`). The
/// `is_testnet` guard runs here too.
///
/// # Errors
/// As [`run_fill`].
pub fn run_fill_on(
    cfg: &HlConfig,
    http: &mut HlHttp,
    spec: FillSpec,
) -> Result<FillReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    check_fill_spec(spec)?;

    let sk = cfg.secret_key().map_err(SmokeErr::Config)?;
    // OUR magic, so the echo decodes through `cloid::decode` exactly
    // as a live fill would.
    let cloid = crate::cloid::encode(spec.strategy_id, spec.client_oid);
    let mut nonces = crate::nonce::Nonce::new();

    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Ioc)
        .with_cloid(cloid);
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_order(&mut mp, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let aj_n = order_json(&mut aj, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let ok = post(http, &sk, cfg, &mut nonces, &mp[..mp_n], &aj[..aj_n], "fill")?;

    Ok(FillReport {
        cloid,
        oid: ok.oid,
        any_filled: ok.any_filled,
        any_resting: ok.any_resting,
    })
}

/// A cloid for the smoke's own probes. **Deliberately NOT our magic**
/// — this is not a slot, so the high bytes are a fixed marker and the
/// low ones a timestamp, and `cloid::decode` reads it as `Foreign`.
/// [`FillSpec`] is the opposite case: it carries the REAL magic
/// precisely so the echo decodes to a slot.
fn fresh_cloid() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[0] = 0xE3; // the phase that placed it
    c[1] = 0x5C; // "smoke"
    let ms = crate::smoke::now_ms().to_be_bytes();
    c[8..16].copy_from_slice(&ms);
    c
}

#[cfg(test)]
mod fill_tests {
    use super::*;
    use crate::config::{Scope, HOST_MAINNET};

    const KEY: [u8; 32] = [0x55; 32];
    const ADDR: [u8; 20] = [0x66; 20];

    fn spec() -> FillSpec {
        FillSpec {
            asset: 100_194_180,
            px_1e8: 68_000_000,
            sz_1e8: 200_000_000,
            is_buy: true,
            strategy_id: 3,
            client_oid: 1,
        }
    }

    /// The trading path gets the SAME mainnet refusal the post-only
    /// path has — checked here rather than assumed from the fact that
    /// the two functions look alike.
    #[test]
    fn mainnet_is_refused_on_the_trading_path_too() {
        let tls = core_net::TlsTransport::default_client_config();
        let m = HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR).expect("cfg");
        let e = run_fill(&m, tls, spec()).expect_err("mainnet must be refused");
        assert!(
            matches!(e, SmokeErr::NotTestnet(_)),
            "phase D must be as unreachable from mainnet as phase C: {e:?}"
        );
    }

    /// A notional over the ceiling is refused BEFORE anything is
    /// signed — the point of a typo limit is that it costs nothing.
    #[test]
    fn an_oversized_notional_is_refused_before_anything_is_signed() {
        let mut s = spec();
        // A size off by three decimals: $1.36 becomes $1,360.
        s.sz_1e8 = 200_000_000_000;
        match check_fill_spec(s) {
            Err(SmokeErr::Lifecycle { stage, msg }) => {
                assert_eq!(stage, "fill-spec");
                assert!(msg.contains("ceiling"), "the message must say what it refused: {msg}");
            }
            other => panic!("a 1,360 USDC order was not refused: {other:?}"),
        }
        // And the guard cannot be defeated by an overflow.
        let mut s = spec();
        s.sz_1e8 = i64::MAX;
        s.px_1e8 = i64::MAX;
        assert!(check_fill_spec(s).is_err(), "i64::MAX squared must not wrap past the ceiling");
    }

    #[test]
    fn a_nonsense_fill_spec_is_refused() {
        for (px, sz) in [(0, 1), (1, 0), (-1, 1), (1, -1)] {
            let mut s = spec();
            s.px_1e8 = px;
            s.sz_1e8 = sz;
            assert!(check_fill_spec(s).is_err(), "px={px} sz={sz} was accepted");
        }
    }

    /// The dry run must refuse exactly what the real send refuses, or
    /// it is a rehearsal of a different action.
    #[test]
    fn the_dry_run_refuses_what_the_send_refuses() {
        let mut s = spec();
        s.sz_1e8 = 200_000_000_000;
        assert!(preview_fill(s).is_err());
        let p = preview_fill(spec()).expect("a valid spec previews");
        assert_eq!(p.notional, "1.36", "the number a decimal slip corrupts");
        assert_eq!(p.px, "0.68");
        assert_eq!(p.sz, "2");
        assert_eq!(p.cloid, "0x4d560300000000000000000000000001");
        assert!(p.place.contains("\"Ioc\""), "phase D is an IoC: {}", p.place);
        assert!(!p.place.contains("Alo"), "a post-only preview would be the wrong rehearsal");
    }

    /// The preview renders the REAL magic, so the echo it tells the
    /// operator to look for is the one the venue will send back.
    #[test]
    fn the_previewed_cloid_decodes_to_the_slot_it_names() {
        let c = crate::cloid::encode(3, 1);
        assert_eq!(
            crate::cloid::decode(&c),
            crate::cloid::Owner::Ours {
                strategy_id: 3,
                client_oid: 1
            }
        );
        // And it is NOT the smoke's foreign marker.
        assert!(matches!(
            crate::cloid::decode(&fresh_cloid()),
            crate::cloid::Owner::Foreign
        ));
    }

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
