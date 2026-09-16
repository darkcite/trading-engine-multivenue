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
    let ok = post(http, &sk, cfg, &mut nonces, &mp[..mp_n], &aj[..aj_n], "place", ItemErrors::AreFailures)?;

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
    let ok = match cancel(http, &sk, cfg, &mut nonces, spec.asset, cloid, "cancel", ItemErrors::AreFailures) {
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
    let ok = cancel(http, &sk, cfg, &mut nonces, spec.asset, cloid, "verify", ItemErrors::AreData)?;
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
    let ok = post(http, sk, cfg, nonces, &mp[..mp_n], &aj[..aj_n], "modify", ItemErrors::AreFailures)?;
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
    items: ItemErrors,
) -> Result<HlOk, SmokeErr> {
    let c = [CancelByCloidWire { asset, cloid }];
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_cancel_by_cloid(&mut mp, &c).map_err(|_| SmokeErr::Encode)?;
    let aj_n = cancel_by_cloid_json(&mut aj, &c).map_err(|_| SmokeErr::Encode)?;
    post(http, sk, cfg, nonces, &mp[..mp_n], &aj[..aj_n], stage, items)
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
    match cancel(http, sk, cfg, nonces, asset, cloid, "cleanup", ItemErrors::AreData) {
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
/// Whether a PER-ITEM error in an `ok` envelope is a failure or the
/// answer being asked for.
///
/// Hyperliquid's `status:"ok"` is an envelope verdict, not an
/// acceptance — a refusal for one order rides INSIDE it. Most stages
/// want that treated as a failure. Two do not: a cancel issued to
/// PROVE an order is gone, and a cleanup cancel for an order that may
/// already have gone, both exist precisely to read `errors > 0`.
///
/// **Passed in rather than inferred from the stage name.** This was a
/// `stage != "verify" && stage != "cleanup"` string compare with no
/// compile-time link to any caller — and phase F doubled the number of
/// callers depending on those exact spellings. A stage renamed to
/// `"verify-old"` would have the venue's "already canceled" — the
/// precise answer that probe exists to obtain — come back as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemErrors {
    /// A per-item error means the stage failed.
    AreFailures,
    /// A per-item error IS the measurement.
    AreData,
}

fn post(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    action_mp: &[u8],
    action_json: &[u8],
    stage: &'static str,
    items: ItemErrors,
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
            if ok.errors > 0 && matches!(items, ItemErrors::AreFailures) {
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
    ///
    /// With `repeat > 1` this is the FIRST id; each order gets the
    /// next one, so every fill is distinguishable in `userFills`.
    pub client_oid: u64,
    /// How many IoCs to place, back to back.
    ///
    /// E4's exit gate wants reconciliation agreeing over >= 20 fills,
    /// and one invocation per fill is twenty chances to mistype a
    /// number. Repeating here keeps the per-order ceiling doing its
    /// job — [`MAX_FILL_NOTIONAL_1E8`] is checked against ONE order,
    /// not the batch, so twenty small fills stay twenty small fills.
    ///
    /// Bounded by [`MAX_FILL_REPEAT`]: a batch is still a number typed
    /// on a command line.
    pub repeat: u32,
}

/// The most IoCs one `--fill` invocation will place.
///
/// Sized for the >= 20 the E4 gate asks for, with room to redo a run
/// that partly failed — and no more. The per-order notional ceiling
/// bounds each order; this bounds the batch, because `--fill-repeat`
/// is the one number where a slipped digit multiplies rather than
/// scales.
pub const MAX_FILL_REPEAT: u32 = 32;

/// What the trade observed. **The ACK, not the fill** (LAW E-5).
///
/// Every field describes what ACTUALLY happened, including when the
/// batch stopped early — see [`FillRun`]. A report that existed only
/// on the success path would lose the one case that matters: an order
/// the venue rested, or one that traded, sitting behind a later
/// refusal that threw the counts away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FillReport {
    /// The cloid of the last order the venue ACKED — look for it in
    /// `userFills`. The batch numbers its orders
    /// `client_oid ..< client_oid + attempted`, so a caller holding
    /// the spec can derive every one of them.
    pub cloid: [u8; 16],
    /// The oid the venue echoed for that order.
    pub oid: u64,
    /// Requests that LEFT THE PROCESS.
    ///
    /// `attempted > sent` means the last one's fate is **unknown**: a
    /// send that fails after the venue has read it still placed the
    /// order. Recovery must cover the whole `attempted` range, not the
    /// `sent` one.
    ///
    /// Conservative by one in two sub-cases: a signing or envelope
    /// failure inside `post` happens before the socket, and this is
    /// incremented before `post` is called. Sweeping one cloid that
    /// was never sent costs a lookup; missing one that was costs an
    /// order. See `in_doubt` for the bit that is actually actionable.
    pub attempted: u32,
    /// A request left the process and its fate is **UNKNOWN**.
    ///
    /// A venue that ANSWERED — even to refuse — is NOT this: it told
    /// us nothing was placed. Only a transport failure or an answer we
    /// could not parse leaves an order genuinely in doubt.
    ///
    /// The distinction is the difference between an alarm and noise. A
    /// non-crossing IoC is REFUSED by the venue, and that is the most
    /// common outcome of this command; `attempted != sent` alone is
    /// true then, so warning on it would fire "an order may be on the
    /// book" on the one run where the venue has just told us the
    /// opposite — and an alarm that cries wolf on the routine case is
    /// not guarding the stranded IoC it exists for.
    pub in_doubt: bool,
    /// Of those, how many the venue ACKED.
    pub sent: u32,
    /// Of those, how many the venue said `filled`.
    pub filled: u32,
    /// The venue said `resting` for any of them. **An IoC should never
    /// rest**; if this is true the venue did something the order type
    /// forbids — and the order is then ON THE BOOK. There is no
    /// cleanup path here (an IoC is not supposed to need one), so the
    /// caller must cancel it. The cloid is deterministic —
    /// `cloid::encode(strategy_id, client_oid + i)` — so it can be
    /// cancelled by cloid from the same numbers that placed it.
    pub any_resting: bool,
}

/// A batch and how it ended.
///
/// [`run_fill_on`] returns `Err` **only when nothing left the
/// process** — a refused spec, a missing key, the mainnet guard. Once
/// the first request goes out it always returns `Ok`, because by then
/// there is state on the venue, and an error carrying none of it would
/// hide exactly what recovery needs.
///
/// That distinction is not theoretical. The shape this replaced
/// accumulated into a local and returned it only on the happy path, so
/// a batch that had an IoC RESTED at order 4 and was refused at order
/// 5 reported the refusal and **nothing about the resting order** — on
/// the one path in this repo that has no cleanup behind it.
#[derive(Debug)]
pub struct FillRun {
    /// Everything the batch actually did.
    pub report: FillReport,
    /// Why it stopped short of `repeat`, if it did. `None` means every
    /// one of the `repeat` orders was sent and ACKED.
    pub stopped: Option<SmokeErr>,
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
    if spec.repeat == 0 || spec.repeat > MAX_FILL_REPEAT {
        return Err(SmokeErr::Lifecycle {
            stage: "fill-spec",
            msg: format!(
                "repeat must be 1..={MAX_FILL_REPEAT}; a batch is still a number typed on a \
                 command line, and this is the one where a slipped digit multiplies"
            ),
        });
    }
    // The batch numbers its orders `client_oid ..< client_oid +
    // repeat`. Unchecked, that add WRAPS in release (`overflow-checks
    // = false`), and a wrapped id is a cloid for an order nobody asked
    // for — one whose low 32 bits are the roll instance
    // `core_types::OID_INSTANCE_MASK` names, a meaning agreed between
    // two crates that cannot see each other. Refused HERE rather than
    // in the loop, so the dry run refuses exactly what the send does.
    if spec
        .client_oid
        .checked_add(u64::from(spec.repeat).saturating_sub(1))
        .is_none()
    {
        return Err(SmokeErr::Lifecycle {
            stage: "fill-spec",
            msg: format!(
                "client_oid {} + repeat {} overflows u64 — the tail of the batch would carry \
                 client ids nobody asked for",
                spec.client_oid, spec.repeat
            ),
        });
    }
    // Per ORDER, not per batch. The ceiling exists to catch a mistyped
    // price or size, and twenty small fills are twenty small fills.
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
    // The LAST id too. It is the number a cancel-by-cloid recovery
    // needs, and `check_fill_spec` has already refused the range that
    // would wrap, so the saturation below is belt.
    let last = crate::cloid::encode(
        spec.strategy_id,
        spec.client_oid
            .saturating_add(u64::from(spec.repeat).saturating_sub(1)),
    );
    let mut hex = [0u8; 34];
    let hn = crate::cloid::to_hex(&cloid, &mut hex);
    let mut lhex = [0u8; 34];
    let ln = crate::cloid::to_hex(&last, &mut lhex);
    Ok(FillPreview {
        place: String::from_utf8_lossy(&buf[..n]).to_string(),
        px: wire_str(spec.px_1e8),
        sz: wire_str(spec.sz_1e8),
        notional: wire_str(
            ((i128::from(spec.px_1e8) * i128::from(spec.sz_1e8)) / 100_000_000) as i64,
        ),
        batch_notional: wire_str(
            ((i128::from(spec.px_1e8) * i128::from(spec.sz_1e8) * i128::from(spec.repeat))
                / 100_000_000) as i64,
        ),
        repeat: spec.repeat,
        cloid: String::from_utf8_lossy(&hex[..hn]).to_string(),
        last_cloid: String::from_utf8_lossy(&lhex[..ln]).to_string(),
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
    /// `px × sz` for ONE order — the number a misplaced decimal
    /// corrupts.
    pub notional: String,
    /// `notional × repeat` — what the whole batch spends.
    pub batch_notional: String,
    /// How many orders the batch will place.
    pub repeat: u32,
    /// The FIRST cloid, hex. This is what to look for in `userFills`.
    pub cloid: String,
    /// The LAST cloid the batch will use, hex. Equal to `cloid` when
    /// `repeat` is 1. Printed because the range — not its head — is
    /// what a cancel-by-cloid recovery has to sweep.
    pub last_cloid: String,
}

/// Place `spec.repeat` IoCs that are meant to trade, and report what
/// the venue ACKED.
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
/// Not testnet, a refused spec, or the transport could not be built —
/// that is, **only cases where nothing was sent**. A refusal that
/// arrives mid-batch comes back as [`FillRun::stopped`], carrying the
/// report of everything that had already happened.
pub fn run_fill(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    spec: FillSpec,
) -> Result<FillRun, SmokeErr> {
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
) -> Result<FillRun, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    check_fill_spec(spec)?;

    let sk = cfg.secret_key().map_err(SmokeErr::Config)?;
    let mut nonces = crate::nonce::Nonce::new();
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];

    let mut run = FillRun {
        report: FillReport {
            cloid: [0u8; 16],
            oid: 0,
            attempted: 0,
            in_doubt: false,
            sent: 0,
            filled: 0,
            any_resting: false,
        },
        stopped: None,
    };
    // SEQUENTIAL, one at a time. A batch action would be one signature
    // and one nonce for twenty orders — faster, and it would make a
    // partial refusal impossible to read: LAW E-5 says the response is
    // the ACK, and an ACK covering twenty orders tells you nothing
    // about which one the venue took.
    //
    // **Nothing below this line uses `?`.** Past the first send there
    // is state on the venue, and `?` would drop the report describing
    // it — including an IoC the venue rested, on the one path here
    // that has no cleanup behind it.
    for i in 0..spec.repeat {
        // Already refused by `check_fill_spec`; belt, because the
        // failure is silent in release and the value is a client id.
        let Some(oid_i) = spec.client_oid.checked_add(u64::from(i)) else {
            run.stopped = Some(SmokeErr::Lifecycle {
                stage: "fill",
                msg: "client_oid + repeat overflowed u64".to_owned(),
            });
            break;
        };
        // OUR magic, so the echo decodes through `cloid::decode`
        // exactly as a live fill would — and a DISTINCT client id per
        // order, so every fill is distinguishable in `userFills`.
        let cloid = crate::cloid::encode(spec.strategy_id, oid_i);
        let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Ioc)
            .with_cloid(cloid);
        let Ok(mp_n) = encode_order(&mut mp, &[order], b"na") else {
            run.stopped = Some(SmokeErr::Encode);
            break;
        };
        let Ok(aj_n) = order_json(&mut aj, &[order], b"na") else {
            run.stopped = Some(SmokeErr::Encode);
            break;
        };

        // ATTEMPTED before the send, never after. A request that fails
        // on the way back was still READ by the venue, and an order
        // placed by a request whose answer we never saw is the one an
        // operator most needs to hear about.
        run.report.attempted += 1;

        // The FIRST refusal ends the batch. A venue refusing one order
        // will likely refuse the next nineteen for the same reason,
        // and nineteen more refusals is nineteen more nonces spent to
        // learn nothing. The report says what happened up to here.
        let ok = match post(http, &sk, cfg, &mut nonces, &mp[..mp_n], &aj[..aj_n], "fill", ItemErrors::AreFailures) {
            Ok(ok) => ok,
            Err(e) => {
                // The ONE site that knows which refusals are answers.
                // `Lifecycle` is the venue's own refusal, envelope- or
                // item-level: it placed nothing. `Sign` and `Encode`
                // happen before the socket. Everything else — and
                // anything added later — defaults to doubt, because an
                // alarm that a new variant silently disarms is worse
                // than one that over-fires.
                run.report.in_doubt = !matches!(
                    e,
                    SmokeErr::Lifecycle { .. } | SmokeErr::Sign | SmokeErr::Encode
                );
                run.stopped = Some(e);
                break;
            }
        };

        run.report.cloid = cloid;
        run.report.oid = ok.oid;
        run.report.sent += 1;
        run.report.filled += u32::from(ok.any_filled);
        run.report.any_resting |= ok.any_resting;
    }
    Ok(run)
}

// ---- Phase F: a requote is a MODIFY, and it changes the cloid ------

/// What the requote round trip observed.
///
/// **Carries both client ids.** They come from a millisecond timestamp
/// the operator never typed and nothing else prints, so an order left
/// on the book under one of them is recoverable only by listing open
/// orders in the venue UI and cancelling by oid. A report that named
/// neither — which is what this was — made every failure path
/// unrecoverable by hand, on the first probe in this lane to hold two
/// ids at once.
#[derive(Debug)]
pub struct RequoteReport {
    /// The id the original quote carried.
    pub old_cloid: [u8; 16],
    /// The id the replacement carried.
    pub new_cloid: [u8; 16],
    /// The oid the venue gave the original quote.
    pub placed_oid: u64,
    /// The oid after the modify. The venue issues a NEW one — which is
    /// why cancel-by-cloid is the durable handle and an oid captured at
    /// placement is not.
    pub modified_oid: u64,
    /// **Cancelling the OLD id came back REFUSED**, which is what the
    /// modify having consumed it looks like from here.
    ///
    /// Named for what it MEASURES, not for what it is taken to mean.
    /// The two differ in one case: a per-item refusal for a reason
    /// other than the order's non-existence would read the same. That
    /// case is narrow rather than absent — an envelope-level failure
    /// (a rate limit, a rejected signature) is turned into `stopped` by
    /// `post` whatever the stage, so what remains is a per-item refusal
    /// of a well-formed cancel for a well-formed cloid this probe
    /// built itself. Narrow is not the same as impossible, and a field
    /// called `old_cloid_gone` would have hidden the difference.
    pub old_cancel_refused: bool,
    /// **Cancelling the NEW id SUCCEEDED**: there really was an order
    /// under it. Positive evidence — `errors == 0` alone would also be
    /// true of an empty or unreadable `statuses` array.
    pub new_cancel_succeeded: bool,
    /// The OLD id's cancel never got an answer. **That order may be on
    /// the book**, under the id above.
    pub unswept_old: bool,
    /// The NEW id's cancel never got an answer. Same.
    pub unswept_new: bool,
    /// Why the run stopped short, if it did. `Err` from
    /// [`run_requote_on`] means **nothing was placed**; once there is an
    /// order on the book the failure rides here, with the report that
    /// says which ids to go and look for.
    pub stopped: Option<SmokeErr>,
}

impl RequoteReport {
    /// Both halves of the asymmetry, nothing left unswept, and nothing
    /// stopped it. See [`run_requote_on`] for why neither half proves
    /// the requote alone.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.old_cancel_refused
            && self.new_cancel_succeeded
            && !self.unswept_old
            && !self.unswept_new
            && self.stopped.is_none()
    }

    /// An id whose cancel went unanswered — go and look for it.
    #[must_use]
    pub const fn has_unswept(&self) -> bool {
        self.unswept_old || self.unswept_new
    }
}

/// **PHASE F — can a modify addressed BY CLOID also CHANGE the cloid?**
///
/// LAW E-7 says a live requote is a modify rather than a cancel plus a
/// place: two requests instead of one, at 333 reprices per instance, is
/// the difference between fitting inside the address budget and not.
/// The operator ruling of 2026-09-16 added the second half — the
/// replacement carries a **fresh** client id, so every `userFills` row
/// maps to exactly one quote instead of to a cloid that has meant
/// several different prices.
///
/// **Neither half had been measured.** Phase C's modify targets the
/// resting order by its VENUE oid and hands the replacement the SAME
/// cloid, so nothing in this repo had ever asked the venue the question
/// this ruling depends on. If the answer were no, the ruling could not
/// be implemented as written — and that is a thing to learn from one
/// testnet probe rather than from a live requote lane running at 333
/// per instance.
///
/// Four requests, and **the last two ARE the assertion**:
///
/// ```text
///   place  cloid A, post-only     -> rests
///   modify BY cloid A -> cloid B  -> ok
///   cancel cloid A                -> must be REFUSED  (A is gone)
///   cancel cloid B                -> must SUCCEED     (B is real)
/// ```
///
/// The asymmetry is the evidence, and neither half carries it alone: a
/// refusal on A is equally explained by a modify that killed A and
/// created nothing, and a success on B is equally explained by a modify
/// that left BOTH resting — which would be a leak, not a requote. Only
/// the pair says the order MOVED.
///
/// **Both cancels always run, and nothing past the place uses `?`.**
/// The first version returned on the first cancel's transport failure,
/// which left the NEW id — the one the probe's own hypothesis says is
/// resting — unswept and unnamed. Phase C gets away with the same shape
/// only because its `?` sits AFTER its successful cancel; this one held
/// two ids and swept one. A failure now comes back as `Ok` with
/// `stopped` set and both ids printed.
///
/// Post-only throughout. A requote that traded would be measuring the
/// market rather than the venue's modify semantics, and it would cost
/// balance to learn nothing.
///
/// # Errors
/// Not testnet, a non-positive price or size, the two probe cloids
/// collided, or an encode overflowed — that is, **only cases where
/// nothing has been sent**. Every failure from the first request
/// onwards, the place included, comes back as `Ok` with `stopped` set
/// and both ids named, because past that point an order may be on the
/// book.
pub fn run_requote_on(
    cfg: &HlConfig,
    http: &mut HlHttp,
    spec: LifecycleSpec,
) -> Result<RequoteReport, SmokeErr> {
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
    let old = fresh_cloid();
    // DERIVED from the first rather than drawn again. `fresh_cloid` is
    // a marker plus a millisecond timestamp, and these two calls are
    // microseconds apart — drawing twice would collide most of the
    // time, and a probe whose two ids are equal proves the opposite of
    // what it claims while looking like a pass.
    //
    // Byte 15 is that timestamp's LSB, so two runs exactly one
    // millisecond apart can produce swapped pairs (run 1's B is run 2's
    // A). Harmless for a four-round-trip probe that sweeps both ids,
    // and recorded here rather than rediscovered.
    let mut new = old;
    new[15] ^= 0x01;
    debug_assert_ne!(old, new);
    if old == new {
        return Err(SmokeErr::Lifecycle {
            stage: "spec",
            msg: "the two probe cloids are identical".to_owned(),
        });
    }

    let mut nonces = crate::nonce::Nonce::new();

    // The report exists BEFORE the first send. Everything below writes
    // into it and nothing below returns `Err`: past this point a
    // request may have reached the venue, and a failure carrying no
    // report leaves an order on the book under an id nothing prints.
    let mut rep = RequoteReport {
        old_cloid: old,
        new_cloid: new,
        placed_oid: 0,
        modified_oid: 0,
        old_cancel_refused: false,
        new_cancel_succeeded: false,
        unswept_old: false,
        unswept_new: false,
        stopped: None,
    };

    // ---- place ------------------------------------------------------
    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Alo)
        .with_cloid(old);
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_order(&mut mp, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let aj_n = order_json(&mut aj, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    match post(
        http,
        &sk,
        cfg,
        &mut nonces,
        &mp[..mp_n],
        &aj[..aj_n],
        "place",
        ItemErrors::AreFailures,
    ) {
        Ok(ok) => {
            rep.placed_oid = ok.oid;
            if ok.any_filled {
                rep.stopped = Some(SmokeErr::Lifecycle {
                    stage: "place",
                    msg: "the post-only order FILLED — the price crosses the market. Nothing \
                          after this would mean anything; choose a price further from the book."
                        .to_owned(),
                });
            } else if !ok.any_resting {
                rep.stopped = Some(SmokeErr::Lifecycle {
                    stage: "place",
                    msg: "accepted, but the venue says it is not resting".to_owned(),
                });
            } else if ok.oid == 0 {
                // RESTING with no oid echoed. Split from the case above
                // because they need opposite things said about them: an
                // order that is not resting strands nothing, and this
                // one IS on the book. It is still recoverable — cancel
                // by cloid needs no oid, which is the whole reason this
                // probe addresses orders that way — so the sweep below
                // takes it back rather than the run walking away.
                rep.stopped = Some(SmokeErr::Lifecycle {
                    stage: "place",
                    msg: "RESTING, but with no oid echoed back".to_owned(),
                });
            }
        }
        Err(e) => {
            // The request may have reached the venue with the answer
            // lost. We cannot tell — but we hold the cloid, and the
            // sweep below is the only thing that can take the order
            // back if it is there.
            rep.stopped = Some(e);
        }
    }

    // ---- modify BY CLOID, to a DIFFERENT cloid ----------------------
    // Skipped when the place already went wrong: modifying an order
    // that filled, or that may not exist, would place a SECOND one.
    //
    // A failure here does NOT return either. WHICH id survives is
    // exactly what is unknown: the modify may have reached the venue
    // with the answer lost, in which case A is consumed and B is
    // resting. The sweep tries BOTH.
    if rep.stopped.is_none() {
        match modify_to_cloid(http, &sk, cfg, &mut nonces, spec, old, new) {
            Ok(oid) => rep.modified_oid = oid,
            Err(e) => rep.stopped = Some(e),
        }
    }

    // ---- the assertion, which is also the sweep ---------------------
    match cancel(
        http,
        &sk,
        cfg,
        &mut nonces,
        spec.asset,
        old,
        "verify",
        ItemErrors::AreData,
    ) {
        Ok(a) => rep.old_cancel_refused = a.errors > 0,
        Err(e) => {
            rep.unswept_old = true;
            if rep.stopped.is_none() {
                rep.stopped = Some(e);
            }
        }
    }
    match cancel(
        http,
        &sk,
        cfg,
        &mut nonces,
        spec.asset,
        new,
        "cleanup",
        ItemErrors::AreData,
    ) {
        // Positive evidence, not the absence of an error.
        Ok(b) => rep.new_cancel_succeeded = b.errors == 0 && b.any_success,
        Err(e) => {
            rep.unswept_new = true;
            if rep.stopped.is_none() {
                rep.stopped = Some(e);
            }
        }
    }
    Ok(rep)
}

/// A `batchModify` that addresses the resting order **by cloid** and
/// gives the replacement a **different** cloid.
///
/// Both halves differ from phase C's `modify`, which targets by venue
/// oid and reuses the id. Split out rather than parameterised onto that
/// one because phase C is a proven surface whose meaning should not
/// shift under a new flag.
fn modify_to_cloid(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    spec: LifecycleSpec,
    old: [u8; 16],
    new: [u8; 16],
) -> Result<u64, SmokeErr> {
    let replacement = OrderWire::new(spec.asset, spec.is_buy, spec.px2_1e8, spec.sz_1e8, Tif::Alo)
        .with_cloid(new);
    let m = [ModifyWire {
        order: replacement,
        oid: 0,
        oid_cloid: old,
        oid_is_cloid: true,
    }];
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = crate::action::encode_batch_modify(&mut mp, &m).map_err(|_| SmokeErr::Encode)?;
    let aj_n = batch_modify_json(&mut aj, &m).map_err(|_| SmokeErr::Encode)?;
    let ok = post(http, sk, cfg, nonces, &mp[..mp_n], &aj[..aj_n], "modify", ItemErrors::AreFailures)?;
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
    // Not `if ok.oid == 0 { 0 }` — that was a copy artefact from phase
    // C, where the fallback is the INPUT oid and means something. There
    // is no input oid here, so the conditional was a no-op dressed up
    // as a preservation.
    Ok(ok.oid)
}

/// [`run_requote_on`] over a transport this builds. The `is_testnet`
/// guard runs here too.
///
/// # Errors
/// As [`run_requote_on`].
pub fn run_requote(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    spec: LifecycleSpec,
) -> Result<RequoteReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;
    run_requote_on(cfg, &mut http, spec)
}

// ---- Phase G: the roll sweep, run by hand --------------------------

/// What the sweep probe needs. Post-only, so it rests rather than
/// trades — the sweep is about taking orders BACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepSpec {
    /// Venue asset id. **Bound by the operator, never derived**
    /// (LAW E-4).
    pub asset: u32,
    /// Resting price, 1e8-scaled. Far enough from the market that a
    /// post-only order rests instead of being refused.
    pub px_1e8: i64,
    /// Size, 1e8-scaled.
    pub sz_1e8: i64,
    /// Buy side. A resting BID below the market is the safe default.
    pub is_buy: bool,
    /// The slot the cloid will carry — **our magic**, because the
    /// sweep selects on exactly that.
    pub strategy_id: u8,
    /// The member's own id for the order.
    pub client_oid: u64,
}

/// What the sweep probe observed.
#[derive(Debug)]
pub struct SweepReport {
    /// The cloid placed and then swept.
    pub cloid: [u8; 16],
    /// The oid the venue gave it.
    pub placed_oid: u64,
    /// The venue's OWN name for the leg — taken from the open-orders
    /// row that carried our cloid, never derived from the asset id.
    pub coin: [u8; crate::asset::COIN_MAX],
    /// How much of `coin` is real.
    pub coin_len: u8,
    /// `frontendOpenOrders` listed the order, with our cloid on it.
    /// **This is the half a plain `openOrders` cannot prove**: that
    /// variant omits the cloid, and without it a sweep cannot tell our
    /// order from a stranger's.
    pub listed: bool,
    /// [`crate::recon::ours_on_leg`] — the arm's own selection — picked
    /// it out.
    pub selected: bool,
    /// The cancel the sweep would send was accepted.
    pub cancelled: bool,
    /// A SECOND enumerate no longer lists it. The proof the sweep
    /// actually took it off the book rather than merely being told so.
    pub gone_after: bool,
    /// The order may still be resting: something went unanswered.
    pub unswept: bool,
    /// Why it stopped short, if it did.
    pub stopped: Option<SmokeErr>,
}

impl SweepReport {
    /// Every step, and nothing left behind.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.listed
            && self.selected
            && self.cancelled
            && self.gone_after
            && !self.unswept
            && self.stopped.is_none()
    }
}

/// **PHASE G — prove the roll sweep against the venue (LAW E-8).**
///
/// The sweep takes back every order this engine placed on a leg the
/// venue has retired. It asks the VENUE what is resting rather than
/// trusting a table this arm keeps, because a local record disagrees
/// invisibly — and invisibly is exactly how a restart or a missed ACK
/// leaves a quote on a dead instance.
///
/// That path had never run against a socket. `HlExchange` is where it
/// lives and E7 gates constructing one, so this probe drives the same
/// pieces directly: `frontendOpenOrders`, the venue's answer, and
/// [`crate::recon::ours_on_leg`] — **the arm's own selection function,
/// not a copy of it**. A selection exercised only behind a socket the
/// tests cannot reach is a claim about source code.
///
/// Five requests:
///
/// ```text
///   place      post-only, OUR magic cloid  -> rests
///   enumerate  frontendOpenOrders          -> must LIST it, with the cloid
///              ours_on_leg                 -> must SELECT it
///   cancel     by oid, as the sweep does   -> accepted
///   enumerate  again                       -> must NOT list it
/// ```
///
/// **The leg's name comes from the venue's own row**, the one carrying
/// our cloid — never derived from the asset id. That is what a name
/// derived from an id would be: the same class of guess LAW E-4
/// refuses in the other direction. It also makes the probe stronger,
/// because it proves the coin the venue prints is the coin the
/// selection matches on.
///
/// # Errors
/// Not testnet, a non-positive price or size, or an encode overflowed
/// — **only cases where nothing has been sent**. Everything after the
/// place rides in `stopped`, with the cloid named, because past that
/// point an order is on the book.
pub fn run_sweep_on(
    cfg: &HlConfig,
    http: &mut HlHttp,
    spec: SweepSpec,
) -> Result<SweepReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    if spec.sz_1e8 <= 0 || spec.px_1e8 <= 0 {
        return Err(SmokeErr::Lifecycle {
            stage: "spec",
            msg: "price and size must both be positive".to_owned(),
        });
    }
    let sk = cfg.secret_key().map_err(SmokeErr::Config)?;
    // OUR magic, because that is exactly what the sweep selects on.
    let cloid = crate::cloid::encode(spec.strategy_id, spec.client_oid);
    let mut nonces = crate::nonce::Nonce::new();

    let mut rep = SweepReport {
        cloid,
        placed_oid: 0,
        coin: [0u8; crate::asset::COIN_MAX],
        coin_len: 0,
        listed: false,
        selected: false,
        cancelled: false,
        gone_after: false,
        unswept: false,
        stopped: None,
    };

    // ---- place ------------------------------------------------------
    let order = OrderWire::new(spec.asset, spec.is_buy, spec.px_1e8, spec.sz_1e8, Tif::Alo)
        .with_cloid(cloid);
    let mut mp = [0u8; MAX_ACTION];
    let mut aj = [0u8; MAX_ACTION];
    let mp_n = encode_order(&mut mp, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    let aj_n = order_json(&mut aj, &[order], b"na").map_err(|_| SmokeErr::Encode)?;
    match post(
        http,
        &sk,
        cfg,
        &mut nonces,
        &mp[..mp_n],
        &aj[..aj_n],
        "place",
        ItemErrors::AreFailures,
    ) {
        Ok(ok) => {
            rep.placed_oid = ok.oid;
            if ok.any_filled {
                rep.stopped = Some(SmokeErr::Lifecycle {
                    stage: "place",
                    msg: "the post-only order FILLED — choose a price further from the book"
                        .to_owned(),
                });
            } else if !ok.any_resting {
                rep.stopped = Some(SmokeErr::Lifecycle {
                    stage: "place",
                    msg: "accepted, but the venue says it is not resting".to_owned(),
                });
            }
        }
        Err(e) => {
            rep.unswept = true;
            rep.stopped = Some(e);
        }
    }
    if rep.stopped.is_some() {
        // Nothing to enumerate, but the order may be out there. Try the
        // cancel-by-cloid anyway — it needs no oid, which is the whole
        // reason the durable handle is the client id.
        sweep_cleanup(http, &sk, cfg, &mut nonces, spec.asset, cloid, &mut rep);
        return Ok(rep);
    }

    // ---- enumerate, and let the ARM'S OWN selection decide ----------
    let mut rows = vec![crate::recon::OpenOrder::default(); crate::recon::MAX_OPEN_ORDERS];
    match enumerate(http, cfg, &mut rows) {
        Ok((n, body)) => {
            // The row carrying OUR cloid names the leg. Taking the name
            // from the venue rather than deriving it from the asset id
            // is the point.
            let mut i = 0usize;
            while i < n {
                let r = rows[i];
                i += 1;
                if r.cloid != Some(cloid) {
                    continue;
                }
                rep.listed = true;
                let c = r.coin.of(&body);
                let k = c.len().min(crate::asset::COIN_MAX);
                rep.coin[..k].copy_from_slice(&c[..k]);
                rep.coin_len = u8::try_from(k).unwrap_or(0);
                break;
            }
            if rep.listed {
                let mut oids = [0u64; crate::recon::MAX_OPEN_ORDERS];
                let picked = crate::recon::ours_on_leg(
                    &rows[..n],
                    &body,
                    &rep.coin[..rep.coin_len as usize],
                    &mut oids,
                );
                rep.selected = oids[..picked].contains(&rep.placed_oid);
            }
        }
        Err(e) => rep.stopped = Some(e),
    }

    // ---- cancel, the way the sweep does: BY OID --------------------
    if rep.selected {
        let c = [crate::action::CancelWire {
            asset: spec.asset,
            oid: rep.placed_oid,
        }];
        let mut cmp = [0u8; MAX_ACTION];
        let mut caj = [0u8; MAX_ACTION];
        match (
            crate::action::encode_cancel(&mut cmp, &c),
            crate::request::cancel_json(&mut caj, &c),
        ) {
            (Ok(a), Ok(b)) => match post(
                http,
                &sk,
                cfg,
                &mut nonces,
                &cmp[..a],
                &caj[..b],
                "cancel",
                ItemErrors::AreFailures,
            ) {
                Ok(ok) => rep.cancelled = ok.errors == 0 && ok.any_success,
                Err(e) => {
                    rep.unswept = true;
                    if rep.stopped.is_none() {
                        rep.stopped = Some(e);
                    }
                }
            },
            _ => {
                if rep.stopped.is_none() {
                    rep.stopped = Some(SmokeErr::Encode);
                }
            }
        }
    }

    // ---- enumerate again: it must be GONE --------------------------
    if rep.cancelled {
        match enumerate(http, cfg, &mut rows) {
            Ok((n, body)) => {
                let _ = &body;
                let mut still = false;
                let mut i = 0usize;
                while i < n {
                    still |= rows[i].cloid == Some(cloid);
                    i += 1;
                }
                rep.gone_after = !still;
            }
            Err(e) => {
                // We cannot confirm. The order was ACKED as cancelled,
                // but "acked" and "gone" are different claims and this
                // probe exists to tell them apart.
                rep.unswept = true;
                if rep.stopped.is_none() {
                    rep.stopped = Some(e);
                }
            }
        }
    }
    // Whatever happened, do not leave a post-only order behind.
    if !rep.gone_after {
        sweep_cleanup(http, &sk, cfg, &mut nonces, spec.asset, cloid, &mut rep);
    }
    Ok(rep)
}

/// One `frontendOpenOrders` round trip, scanned. Returns the row count
/// and an OWNED copy of the body, because the caller reads spans out of
/// it across a later borrow of `http`.
fn enumerate(
    http: &mut HlHttp,
    cfg: &HlConfig,
    rows: &mut [crate::recon::OpenOrder],
) -> Result<(usize, Vec<u8>), SmokeErr> {
    let mut req = [0u8; crate::recon::MAX_OPEN_ORDERS_REQ];
    let n = crate::recon::open_orders_request(&mut req, &cfg.master_addr)
        .map_err(|_| SmokeErr::Encode)?;
    let (_status, range) = http
        .post_to(crate::http::INFO_PATH, &req[..n])
        .map_err(SmokeErr::Http)?;
    let body = http.resp()[range].to_vec();
    // Fail-closed: an unreadable answer is NOT "nothing is resting".
    let k = crate::recon::scan_open_orders(&body, rows).map_err(|_| SmokeErr::Unreadable)?;
    Ok((k, body))
}

/// Best-effort cancel-by-cloid so a failed probe leaves nothing on the
/// book. Needs no oid, which is why the client id is the durable
/// handle.
fn sweep_cleanup(
    http: &mut HlHttp,
    sk: &secp256k1::SecretKey,
    cfg: &HlConfig,
    nonces: &mut crate::nonce::Nonce,
    asset: u32,
    cloid: [u8; 16],
    rep: &mut SweepReport,
) {
    match cancel(http, sk, cfg, nonces, asset, cloid, "cleanup", ItemErrors::AreData) {
        // `errors > 0` here means the venue had nothing under that id,
        // which is the answer we want. Unlike phase F's
        // `new_cancel_succeeded` this is a CLEANUP rather than an
        // assertion, so "cancelled it" and "nothing there" are both
        // clean and neither needs `any_success`. The same narrow
        // residual applies — a per-item error for an unrelated reason
        // would read as clean while the order rests — and `gone_after`
        // is the independent assertion that does not rest on it.
        Ok(_) => rep.unswept = false,
        Err(e) => {
            rep.unswept = true;
            if rep.stopped.is_none() {
                rep.stopped = Some(e);
            }
        }
    }
}

/// [`run_sweep_on`] over a transport this builds.
///
/// # Errors
/// As [`run_sweep_on`].
pub fn run_sweep(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    spec: SweepSpec,
) -> Result<SweepReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;
    run_sweep_on(cfg, &mut http, spec)
}

// ---- Phase E: the reconciliation, run by hand ----------------------

/// The first synthetic symbol id the phase E ledger hands out.
///
/// **Synthetic on purpose.** A real symbol id comes from a roll event
/// and the boot ordinal law; the smoke has neither, and inventing one
/// that LOOKED real is how a number gets believed later. Only the COIN
/// is real here — and the coin is the only thing
/// [`crate::recon::compare_booked`] matches on, so the symbol is a
/// table key and nothing more.
const SMOKE_SYM_BASE: u32 = 4096;

/// What rebuilding the ledger out of `userFills` observed.
///
/// Every row is accounted for: `ours + foreign + settlements == rows`.
/// A row this cannot classify is `refused`, never dropped — an
/// unreadable row that read as "no position" would make the
/// reconciliation agree by having nothing to disagree with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LedgerStats {
    /// Rows in the snapshot.
    pub rows: u32,
    /// Rows whose cloid decodes to OURS (LAW E-9).
    pub ours: u32,
    /// Rows carrying a cloid that is not ours. **Counted, never
    /// booked** — a fill the engine did not order is the evidence
    /// reconciliation exists to catch.
    pub foreign: u32,
    /// Settlement rows. They carry NO cloid, so they are attributed by
    /// symbol, exactly as the live arm does.
    pub settlements: u32,
    /// Settlements on a leg no fill of ours ever bound. Counted, never
    /// booked.
    pub settlements_unowned: u32,
    /// Distinct legs bound.
    pub legs: u32,
    /// Rows that could not be placed: the table was full, or the coin
    /// was unusable.
    pub refused: u32,
}

/// A fill's SIGNED contribution to a position, 1e6.
#[inline]
fn signed_1e6(f: &crate::userws::UserFill) -> i64 {
    let q = f.sz_1e8 / crate::recon::WIRE_TO_ENGINE_QTY;
    if f.is_buy {
        q
    } else {
        -q
    }
}

/// Rebuild this arm's position ledger from a `userFills` snapshot.
///
/// **Two passes, and the reason is the `sym_of_coin` lesson.** A
/// settlement carries no cloid, so it can only be attributed by a
/// symbol some EARLIER fill bound — and `userFills` arrives newest
/// first, so a single pass would meet the settlement before the trades
/// that created the position it settles. Binding is therefore its own
/// pass over our own rows, and booking is a second pass over
/// everything.
///
/// The asset id is left at ZERO rather than derived from the coin.
/// LAW E-4 forbids deriving one, and this table exists to be compared
/// against, never to route an order; a zero makes that structural
/// rather than a promise, because `lookup` would refuse it.
pub fn build_ledger(
    fills: &[crate::userws::UserFill],
    body: &[u8],
    assets: &mut crate::asset::AssetTable,
) -> LedgerStats {
    let mut st = LedgerStats {
        rows: u32::try_from(fills.len()).unwrap_or(u32::MAX),
        ..LedgerStats::default()
    };

    // Pass 1 — bind every leg WE TRADED, and only those. A settlement
    // is a payout, not a trade: binding from one would create a leg
    // out of somebody else's position.
    //
    // Nothing is counted here. A bind that fails leaves the coin
    // unresolvable, so pass 2 counts every row on it as `refused` —
    // once each, which is what makes `refused` a ROW count rather than
    // a number that means neither rows nor legs.
    let mut i = 0usize;
    while i < fills.len() {
        let f = &fills[i];
        i += 1;
        if f.is_settlement
            || !matches!(crate::userws::owner_of(f), crate::cloid::Owner::Ours { .. })
        {
            continue;
        }
        let coin = f.coin.of(body);
        if assets.sym_of_coin(coin).is_some() {
            continue;
        }
        let sym = SMOKE_SYM_BASE.saturating_add(st.legs.saturating_mul(2));
        if assets.bind(sym, 0, 1, coin).is_ok() {
            st.legs = st.legs.saturating_add(1);
        }
    }

    // Pass 2 — book. Nothing binds here, so a settlement on a leg we
    // never traded stays unowned rather than inventing a position.
    let mut i = 0usize;
    while i < fills.len() {
        let f = &fills[i];
        i += 1;
        let coin = f.coin.of(body);
        // SETTLEMENT FIRST, before the cloid is even looked at —
        // the order `strategy_bin15::book_fill` uses. Settlements
        // measured on testnet carry no cloid, so the two orders agree
        // today; the smoke exists to validate the live arm, and a
        // classification that only agrees by coincidence is one that
        // stops agreeing the day the venue adds one.
        if f.is_settlement {
            st.settlements = st.settlements.saturating_add(1);
            match assets.sym_of_coin(coin) {
                Some(sym) => assets.book_qty(sym, signed_1e6(f)),
                None => st.settlements_unowned = st.settlements_unowned.saturating_add(1),
            }
            continue;
        }
        match crate::userws::owner_of(f) {
            crate::cloid::Owner::Ours { .. } => {
                st.ours = st.ours.saturating_add(1);
                match assets.sym_of_coin(coin) {
                    Some(sym) => assets.book_qty(sym, signed_1e6(f)),
                    None => st.refused = st.refused.saturating_add(1),
                }
            }
            crate::cloid::Owner::Foreign => st.foreign = st.foreign.saturating_add(1),
        }
    }
    st
}

/// What a reconciliation observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconReport {
    /// How the ledger was rebuilt.
    pub ledger: LedgerStats,
    /// Rows on the venue's balance sheet.
    pub balances: u32,
    /// Legs where the two disagreed.
    pub drift_legs: u64,
    /// Of the legs compared, how many this arm booked a **non-zero**
    /// position in.
    ///
    /// A leg that nets to zero against a venue that does not mention it
    /// still agrees, and that agreement is real — it is exactly what a
    /// mis-booked settlement breaks. But it is weaker evidence than a
    /// live position matching, and a run where every leg netted to zero
    /// would pass on arithmetic alone. This says how much of the
    /// agreement is carrying weight.
    pub legs_nonzero: u32,
    /// Outcome legs the VENUE holds that this run never compared.
    ///
    /// Phase E's reach is the intersection of the `userFills` snapshot
    /// window and our own fills, so a position we still hold whose
    /// trades aged out of that window is never bound and reads as
    /// agreement **by absence**. This is that blind spot measured from
    /// the side that can see it — see
    /// [`crate::recon::unreconciled_venue_legs`].
    ///
    /// Non-zero does not mean the ledger is wrong. It means this run
    /// did not reconcile everything the account holds, which is a
    /// different sentence from "agreed".
    pub venue_legs_unreconciled: u32,
    /// Worst single-leg disagreement, as a CONTRACT QUANTITY, 1e6.
    pub worst_qty_1e6: i64,
    /// The same number in the unit a halt rule is written in, through
    /// [`crate::recon::drift_qty_to_usd_1e6`]. E6's threshold is USD;
    /// this is what it must be compared against.
    pub worst_usd_1e6: i64,
}

impl ReconReport {
    /// Did the venue and the ledger agree, over something, **about
    /// everything the account holds**?
    ///
    /// `drift_legs == 0` alone answers a different question. It is
    /// accumulated inside `for_each_live`, which does not run at all
    /// when no leg is bound — so a snapshot with no fills of ours
    /// produces zero drift over zero legs, and reading that as
    /// agreement is reading "nothing disagreed" as "the things agreed".
    /// Those are the same sentence only when the set is non-empty.
    ///
    /// Reachable, not theoretical: a wrong master address, an account
    /// nothing has traded, or a snapshot of nothing but settlements all
    /// land there — and this is the E4 exit gate's evidence.
    /// The third clause is the coverage one: a run that agreed on every
    /// leg it looked at, while the venue holds a leg it never looked
    /// at, has not reconciled the account — and E4's exit gate is about
    /// the account, not about the subset that happened to fit in a
    /// snapshot.
    #[must_use]
    pub const fn agreed(&self) -> bool {
        self.ledger.legs > 0 && self.drift_legs == 0 && self.venue_legs_unreconciled == 0
    }
}

/// **PHASE E — run the reconciliation by hand, against the real venue.**
///
/// §6.2 calls reconciliation "the single most valuable safety net in
/// the plan", and until this existed it could only run inside a live
/// `HlExchange` — which E7 gates. So the check that is supposed to
/// catch a lost fill, a double-counted fill and a wrong asset id had
/// never once run against a socket. This is that run, and it places no
/// order and signs nothing: it READS `userFills` and
/// `spotClearinghouseState` and compares them.
///
/// The ledger is rebuilt from our own cloids rather than from anything
/// the engine believes, which is the whole point — a reconciler that
/// asked a member would be agreeing with itself.
///
/// # Errors
/// Not testnet, the socket or the info endpoint failed, or **no
/// snapshot arrived**. That last one is an error rather than an empty
/// ledger on purpose: an absent snapshot read as "no fills" would make
/// the comparison pass by having nothing to compare.
pub fn run_recon(
    cfg: &HlConfig,
    tls: Arc<rustls::ClientConfig>,
    wait: core::time::Duration,
) -> Result<ReconReport, SmokeErr> {
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }
    let stage = "recon";

    let mut ws = crate::UserWs::new(&cfg.host, 443, Arc::clone(&tls), &cfg.master_addr).map_err(
        |e| SmokeErr::Lifecycle {
            stage,
            msg: format!("user-event socket: {e:?}"),
        },
    )?;
    ws.connect().map_err(|e| SmokeErr::Lifecycle {
        stage,
        msg: format!("subscribe: {e:?}"),
    })?;

    let mut fills = vec![crate::userws::UserFill::default(); crate::userws::SNAPSHOT_RING];
    let mut assets = crate::asset::AssetTable::new();
    let mut ledger = LedgerStats::default();
    let mut seen = false;
    let mut bad: Option<String> = None;

    let end = std::time::Instant::now() + wait;
    while !seen && std::time::Instant::now() < end {
        let pumped = ws.pump(core::time::Duration::from_millis(500), |payload| {
            // `pump` drains EVERY completed frame from one read, so the
            // `while !seen` above guards the next pump, not the next
            // frame. Two snapshots in one read would call `build_ledger`
            // twice against the same table: pass 1 would skip every
            // already-bound coin and pass 2 would book every row a
            // second time — every position doubled, reported next to a
            // `legs` of zero.
            if !crate::userws::is_user_fills(payload) {
                return;
            }
            // The SCAN always runs — `seen` gates only the BUILD below.
            // Skipping the scan outright would have made this guard
            // quietly drop an unreadable venue frame as well as a
            // duplicate snapshot, which is a step back from "an
            // unreadable answer is never ignored" in exchange for a
            // property that has nothing to do with it.
            match crate::userws::scan_user_fills(payload, &mut fills) {
                // Only the SNAPSHOT, and only the FIRST one. `pump`
                // drains EVERY completed frame from one read, so
                // `while !seen` guards the next pump and not the next
                // frame: two snapshots in one read would book every row
                // twice against the same table.
                //
                // A live row is skipped for a different reason — it is
                // a position the balance sheet may or may not have
                // caught up with, and a reconciliation racing its own
                // inputs proves nothing.
                Ok((n, true)) if !seen => {
                    ledger = build_ledger(&fills[..n], payload, &mut assets);
                    seen = true;
                }
                Ok(_) => {}
                // Fail-closed: a userFills frame that did not scan is
                // NOT zero fills.
                Err(e) => bad = Some(format!("userFills did not scan: {e:?}")),
            }
        });
        if let Err(e) = pumped {
            return Err(SmokeErr::Lifecycle {
                stage,
                msg: format!("the user-event stream dropped before the snapshot: {e:?}"),
            });
        }
        if let Some(msg) = bad.take() {
            return Err(SmokeErr::Lifecycle { stage, msg });
        }
    }
    // `rows == 0` as well as `!seen`: the guard's whole point is the
    // empty LEDGER, and an empty snapshot frame is a snapshot that
    // arrived. Testing only arrival would have been a condition whose
    // message described a stronger property than it checked.
    if !seen || ledger.rows == 0 {
        return Err(SmokeErr::Lifecycle {
            stage,
            msg: format!(
                "no userFills snapshot with any rows in it (arrived: {seen}, rows: {}). \
                 Refusing to reconcile against an empty ledger — it would agree by having \
                 nothing to compare",
                ledger.rows
            ),
        });
    }

    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;
    let mut req = [0u8; crate::recon::MAX_STATE_REQ];
    let n = crate::recon::spot_state_request(&mut req, &cfg.master_addr)
        .map_err(|_| SmokeErr::Encode)?;
    let (_status, range) = http
        .post_to(crate::http::INFO_PATH, &req[..n])
        .map_err(SmokeErr::Http)?;
    let body = &http.resp()[range];
    let mut bal = vec![crate::recon::SpotBalance::default(); crate::recon::MAX_SPOT_BALANCES];
    let rows = crate::recon::scan_spot_state(body, &mut bal).map_err(|_| SmokeErr::Unreadable)?;

    let (drift_legs, worst) = crate::recon::compare_booked(&assets, &bal[..rows], body);
    let venue_legs_unreconciled =
        crate::recon::unreconciled_venue_legs(&assets, &bal[..rows], body);
    let mut legs_nonzero = 0u32;
    assets.for_each_live(|_sym, _coin, booked_1e6| {
        if booked_1e6 != 0 {
            legs_nonzero = legs_nonzero.saturating_add(1);
        }
    });
    Ok(ReconReport {
        ledger,
        balances: u32::try_from(rows).unwrap_or(u32::MAX),
        drift_legs,
        legs_nonzero,
        venue_legs_unreconciled,
        worst_qty_1e6: worst,
        worst_usd_1e6: crate::recon::drift_qty_to_usd_1e6(worst),
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
mod recon_tests {
    use super::{build_ledger, signed_1e6, SMOKE_SYM_BASE};
    use crate::asset::AssetTable;
    use crate::response::Span;
    use crate::userws::UserFill;

    /// `#195641` at 0, `#195640` at 8 — two legs in one buffer.
    const BODY: &[u8] = b"#195641 #195640 ";

    fn span(start: u32, len: u32) -> Span {
        Span {
            start,
            end: start + len,
        }
    }

    fn row(coin: Span, cloid: Option<[u8; 16]>, sz_1e8: i64, is_buy: bool) -> UserFill {
        UserFill {
            coin,
            cloid,
            sz_1e8,
            is_buy,
            ..UserFill::default()
        }
    }

    fn ours(n: u64) -> Option<[u8; 16]> {
        Some(crate::cloid::encode(3, n))
    }

    /// **The two-pass property, and the whole reason for it.**
    ///
    /// `userFills` arrives NEWEST FIRST, so a settlement sits ahead of
    /// the trades whose position it settles. A settlement carries no
    /// cloid and can only be attributed by a symbol an earlier fill
    /// bound — so a single forward pass would meet it before the leg
    /// exists and count it unowned, and the ledger would then claim a
    /// position the venue has already paid out. Binding is its own
    /// pass precisely so the ORDER OF THE SNAPSHOT cannot decide this.
    #[test]
    fn a_settlement_ahead_of_its_own_trades_is_still_booked() {
        let leg = span(0, 7);
        let mut set = row(leg, None, 400_000_000, false); // sells 4 back
        set.is_settlement = true;
        let fills = [
            set,                                    // newest: the settlement
            row(leg, ours(2), 200_000_000, true),   // +2
            row(leg, ours(1), 200_000_000, true),   // +2
        ];

        let mut a = AssetTable::new();
        let st = build_ledger(&fills, BODY, &mut a);

        assert_eq!(st.legs, 1);
        assert_eq!(st.ours, 2);
        assert_eq!(st.settlements, 1);
        assert_eq!(
            st.settlements_unowned, 0,
            "the leg was bound in pass 1, before pass 2 ever saw the settlement"
        );
        assert_eq!(
            a.booked_qty(SMOKE_SYM_BASE),
            Some(0),
            "+2 +2 -4: the venue paid it out and the ledger agrees"
        );
    }

    /// A cloid that is not ours is COUNTED and never booked. Booking it
    /// would destroy the evidence reconciliation exists to surface.
    #[test]
    fn a_foreign_fill_is_counted_and_never_booked() {
        let leg = span(0, 7);
        let fills = [
            row(leg, ours(1), 200_000_000, true),
            row(leg, Some([0xAB; 16]), 900_000_000, true),
            row(leg, None, 900_000_000, true), // no cloid, not a settlement
        ];
        let mut a = AssetTable::new();
        let st = build_ledger(&fills, BODY, &mut a);

        assert_eq!(st.ours, 1);
        assert_eq!(st.foreign, 2);
        assert_eq!(st.settlements, 0);
        assert_eq!(
            a.booked_qty(SMOKE_SYM_BASE),
            Some(2_000_000),
            "only OUR 2 contracts are in the ledger"
        );
        assert_eq!(st.rows, 3, "and every row is accounted for");
        assert_eq!(st.ours + st.foreign + st.settlements, st.rows);
    }

    /// A settlement on a leg we never traded has no owner to attribute
    /// it to. Counted, never booked — the alternative is inventing a
    /// position out of somebody else's payout.
    #[test]
    fn a_settlement_on_a_leg_we_never_traded_stays_unowned() {
        let mine = span(0, 7);
        let theirs = span(8, 7);
        let mut set = row(theirs, None, 500_000_000, false);
        set.is_settlement = true;
        let fills = [row(mine, ours(1), 200_000_000, true), set];

        let mut a = AssetTable::new();
        let st = build_ledger(&fills, BODY, &mut a);

        assert_eq!(st.legs, 1, "only the leg WE traded is bound");
        assert_eq!(st.settlements, 1);
        assert_eq!(st.settlements_unowned, 1);
        assert_eq!(a.booked_qty(SMOKE_SYM_BASE), Some(2_000_000));
        assert!(a.sym_of_coin(theirs.of(BODY)).is_none());
    }

    /// Sign and scale, which are the two ways a ledger silently
    /// disagrees with a venue while looking right.
    #[test]
    fn a_sell_books_negative_and_the_scale_is_1e6() {
        assert_eq!(signed_1e6(&row(span(0, 7), None, 200_000_000, true)), 2_000_000);
        assert_eq!(
            signed_1e6(&row(span(0, 7), None, 200_000_000, false)),
            -2_000_000,
            "a sell REDUCES the position"
        );

        let leg = span(0, 7);
        let fills = [
            row(leg, ours(1), 500_000_000, true),
            row(leg, ours(2), 200_000_000, false),
        ];
        let mut a = AssetTable::new();
        build_ledger(&fills, BODY, &mut a);
        assert_eq!(a.booked_qty(SMOKE_SYM_BASE), Some(3_000_000), "5 - 2");
    }

    /// **An agreement over NOTHING is not an agreement.** `drift_legs`
    /// is accumulated inside `for_each_live`, which does not run when
    /// no leg is bound — so every one of these produces zero drift over
    /// zero legs, and a verdict reading only `drift_legs == 0` calls
    /// them all a pass. This is the E4 exit gate's evidence, and each
    /// case below is reachable: an empty snapshot, the wrong master
    /// address, and an account whose only rows are somebody's payout.
    #[test]
    fn a_verdict_over_zero_legs_is_never_an_agreement() {
        use super::{ReconReport, LedgerStats};

        let verdict = |ledger: LedgerStats| {
            ReconReport {
                ledger,
                balances: 18,
                drift_legs: 0,
                legs_nonzero: ledger.legs,
                venue_legs_unreconciled: 0,
                worst_qty_1e6: 0,
                worst_usd_1e6: 0,
            }
            .agreed()
        };

        // (a) nothing at all
        let mut a = AssetTable::new();
        let empty = build_ledger(&[], BODY, &mut a);
        assert_eq!(empty.legs, 0);
        assert!(!verdict(empty), "an empty snapshot compared nothing");

        // (b) rows, none of them ours — the wrong master address
        let mut a = AssetTable::new();
        let st = build_ledger(
            &[
                row(span(0, 7), Some([0xAB; 16]), 900_000_000, true),
                row(span(8, 7), None, 100_000_000, true),
            ],
            BODY,
            &mut a,
        );
        assert_eq!(st.foreign, 2);
        assert_eq!(st.legs, 0, "nothing of ours bound a leg");
        assert!(!verdict(st), "somebody else's fills are not our agreement");

        // (c) settlements only — a payout on a leg we never traded
        let mut set = row(span(0, 7), None, 500_000_000, false);
        set.is_settlement = true;
        let mut a = AssetTable::new();
        let st = build_ledger(&[set], BODY, &mut a);
        assert_eq!(st.settlements_unowned, 1);
        assert_eq!(st.legs, 0);
        assert!(!verdict(st));

        // And the control: one real leg, zero drift, IS an agreement.
        let mut a = AssetTable::new();
        let st = build_ledger(&[row(span(0, 7), ours(1), 200_000_000, true)], BODY, &mut a);
        assert_eq!(st.legs, 1);
        assert!(verdict(st), "a leg that agreed must still read as agreement");
    }

    /// A settlement is classified BEFORE its cloid is looked at, which
    /// is the order the live arm uses. Today every measured settlement
    /// arrives without a cloid, so the two orders agree by coincidence
    /// — this pins the order itself, against the day one arrives with
    /// a cloid and the smoke silently reports it as a trade.
    #[test]
    fn a_settlement_is_a_settlement_even_carrying_our_cloid() {
        let leg = span(0, 7);
        let mut set = row(leg, ours(9), 400_000_000, false);
        set.is_settlement = true;
        let fills = [row(leg, ours(1), 400_000_000, true), set];

        let mut a = AssetTable::new();
        let st = build_ledger(&fills, BODY, &mut a);

        assert_eq!(st.settlements, 1, "the flag decides, not the cloid");
        assert_eq!(st.ours, 1, "and it is NOT also counted as a trade");
        assert_eq!(st.ours + st.foreign + st.settlements, st.rows);
        assert_eq!(a.booked_qty(SMOKE_SYM_BASE), Some(0), "+4 then -4");
    }

    /// `refused` is a ROW count. A coin that could not bind makes every
    /// row on it refused, once each — not once in pass 1 and again in
    /// pass 2 for the same row.
    #[test]
    fn refused_counts_rows_once() {
        // 32 slots; bind 32 distinct legs, then a 33rd that cannot fit.
        let mut body = Vec::new();
        let mut spans = Vec::new();
        for i in 0..33u32 {
            let start = u32::try_from(body.len()).expect("small");
            body.extend_from_slice(format!("#{:06}", 100_000 + i).as_bytes());
            body.push(b' ');
            spans.push(span(start, 7));
        }
        let mut fills = Vec::new();
        for (i, sp) in spans.iter().enumerate() {
            fills.push(row(*sp, ours(i as u64 + 1), 100_000_000, true));
        }
        // two rows on the leg that will not fit
        fills.push(row(spans[32], ours(99), 100_000_000, true));

        let mut a = AssetTable::new();
        let st = build_ledger(&fills, &body, &mut a);

        assert_eq!(st.legs, 32, "the table holds exactly ASSET_SLOTS");
        assert_eq!(st.ours, 34);
        assert_eq!(st.refused, 2, "both rows on the unbindable leg, once each");
    }

    /// Two legs, two slots, and the ledgers do not bleed into each
    /// other — the failure that would make every drift number wrong at
    /// once.
    #[test]
    fn two_legs_keep_separate_ledgers() {
        let a_leg = span(0, 7);
        let b_leg = span(8, 7);
        let fills = [
            row(a_leg, ours(1), 200_000_000, true),
            row(b_leg, ours(2), 700_000_000, true),
            row(a_leg, ours(3), 100_000_000, true),
        ];
        let mut a = AssetTable::new();
        let st = build_ledger(&fills, BODY, &mut a);

        assert_eq!(st.legs, 2);
        assert_eq!(a.booked_qty(SMOKE_SYM_BASE), Some(3_000_000));
        assert_eq!(a.booked_qty(SMOKE_SYM_BASE + 2), Some(7_000_000));
    }
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
            repeat: 1,
        }
    }

    /// The ceiling is PER ORDER, so a repeat does not scale it — and
    /// the repeat has its own bound, because it is the one number
    /// where a slipped digit multiplies rather than scales.
    #[test]
    fn the_batch_size_is_bounded_on_its_own() {
        let mut s = spec();
        s.repeat = MAX_FILL_REPEAT;
        assert!(check_fill_spec(s).is_ok(), "the bound itself is usable");
        s.repeat = MAX_FILL_REPEAT + 1;
        assert!(check_fill_spec(s).is_err());
        s.repeat = 0;
        assert!(check_fill_spec(s).is_err(), "a batch of nothing is a typo");

        let mut s = spec();
        s.repeat = MAX_FILL_REPEAT;
        let p = preview_fill(s).expect("previews");
        assert_eq!(p.notional, "1.36", "one order");
        assert_eq!(p.batch_notional, "43.52", "and what the batch spends");
        assert_eq!(p.repeat, MAX_FILL_REPEAT);
    }

    /// The preview discloses the whole cloid RANGE. Its head alone is
    /// not enough: a cancel-by-cloid recovery has to sweep to the tail,
    /// and an operator who cannot see the tail cannot re-run without
    /// colliding with ids the venue has already seen.
    #[test]
    fn the_preview_names_both_ends_of_the_cloid_range() {
        let one = preview_fill(spec()).expect("previews");
        assert_eq!(one.cloid, one.last_cloid, "one order has one cloid");

        let mut s = spec();
        s.repeat = 20;
        let p = preview_fill(s).expect("previews");
        assert_ne!(p.cloid, p.last_cloid);
        let mut hex = [0u8; 34];
        let n = crate::cloid::to_hex(&crate::cloid::encode(3, 20), &mut hex);
        assert_eq!(
            p.last_cloid,
            String::from_utf8_lossy(&hex[..n]),
            "client_oid 1 + 20 orders ends at 20"
        );
    }

    /// The batch numbers its orders from `client_oid`, and that add is
    /// UNCHECKED in release (`overflow-checks = false`). Refused in the
    /// spec check, so the rehearsal refuses what the send would — the
    /// same shape `asset_id` and `event.sym + 1` were given after both
    /// shipped able to wrap.
    #[test]
    fn a_batch_that_would_wrap_the_client_id_is_refused() {
        let mut s = spec();
        s.client_oid = u64::MAX;
        s.repeat = 1;
        assert!(
            check_fill_spec(s).is_ok(),
            "a single order needs no room above it"
        );

        s.repeat = 2;
        assert!(
            check_fill_spec(s).is_err(),
            "the second order's id would wrap to 0 — a cloid nobody asked for"
        );
        assert!(
            preview_fill(s).is_err(),
            "and the rehearsal must refuse exactly what the send does"
        );
    }

    /// Every order in a batch gets a DISTINCT cloid, or the fills are
    /// indistinguishable in `userFills` and the >= 20-fill evidence is
    /// twenty copies of one row.
    #[test]
    fn each_order_in_a_batch_carries_its_own_client_id() {
        let a = crate::cloid::encode(3, 1);
        let b = crate::cloid::encode(3, 2);
        assert_ne!(a, b);
        assert_eq!(
            crate::cloid::decode(&b),
            crate::cloid::Owner::Ours {
                strategy_id: 3,
                client_oid: 2
            }
        );
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
