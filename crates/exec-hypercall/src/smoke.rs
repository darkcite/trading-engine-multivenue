// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: a cold operator module — the `hypercall-live` verbs.
//! It runs once per invocation, never on the engine loop, and allocates
//! and copies freely (report lines, the venue's answers echoed back).
//!
//! # The operator's verbs (plan `hc9-hc11` §2; ruling O-HC19)
//!
//! There is no Hypercall testnet, so every live proof is a mainnet
//! micro-step. The verbs drive the SAME [`HcExchange`] the engine would
//! — the dust order is a proof of the arm, not of a side path.
//!
//! | verb | writes? | what |
//! |---|---|---|
//! | `status` | no | who signs for which wallet; the private socket authenticates; balance, positions, open orders |
//! | `simulate` | no | `POST /risk/simulate/orders` for the dust leg (never mutates) |
//! | `recon` | no | one reconciliation, reported |
//! | `dust` | **yes** | one `book_only` GTC bid of 0.000001 contracts at $0.0005, then its cancel by client id, then a reconcile: PASS is "the venue took the signature, the order is gone, nothing filled" |
//! | `cancel-all` | **yes** | cancel every order of ours the venue lists |
//!
//! A write needs `--confirm` (the `MainnetAuthority` precedent of the
//! HYPARB verbs); without it the verb prints what it would do and exits
//! 2. Any HTTP answer to the dust order other than 401/403 proves the
//! signature was accepted; a `REJECTED` is reported with the venue's
//! reason (a tick or minimum-size rule is information, not a failure of
//! the arm).

use std::io::Write;
use std::time::{Duration, Instant};

use core_fill::ORDER_KIND_IOC;
use core_types::{Order, Price, Qty, Side, VenueId};

use crate::exchange::{HcExchange, ReconReport};
use crate::response::{Status, Why};

/// The dust order's price, USD ×1e6 ($0.0005).
pub const DUST_PX_1E6: i64 = 500;
/// The dust order's size, contracts ×1e6 (0.000001).
pub const DUST_QTY_1E6: i64 = 1;

/// The verb's verdict → process exit code.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 0.
    Pass,
    /// 1 — the venue or the arm disagreed with the expectation.
    Fail,
    /// 2 — a write without `--confirm`.
    Unconfirmed,
}

impl Verdict {
    /// The exit code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Pass => 0,
            Self::Fail => 1,
            Self::Unconfirmed => 2,
        }
    }
}

fn say<W: Write>(out: &mut W, line: &str) {
    let _ = writeln!(out, "{line}");
}

fn clip(b: &[u8]) -> String {
    let n = b.len().min(600);
    let mut s = String::from_utf8_lossy(&b[..n]).into_owned();
    if b.len() > n {
        s.push_str(" …");
    }
    s
}

fn report<W: Write>(out: &mut W, r: &ReconReport) {
    say(
        out,
        &format!(
            "recon: positions {} · drift legs {} · unseen legs {} · worst drift ${:.6} · open orders {} (ours unknown to this run {}, not ours {}) · available ${:.6} · agreed {}",
            r.positions,
            r.drift_legs,
            r.unseen_legs,
            r.drift_usd_1e6 as f64 / 1e6,
            r.open_orders,
            r.orphans,
            r.foreign,
            r.available_1e6 as f64 / 1e6,
            r.agreed
        ),
    );
}

/// Connect the private socket and pump it for `dur` (answers pings,
/// books whatever arrives).
fn pump_for(arm: &mut HcExchange, dur: Duration) {
    let end = Instant::now() + dur;
    while Instant::now() < end {
        if !arm.pump() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// `status` (read-only). PASS: the socket authenticates and both reads
/// answer 200.
pub fn status<W: Write>(arm: &mut HcExchange, out: &mut W) -> Verdict {
    say(out, &format!("private socket: connecting (slot {})", arm.slot()));
    pump_for(arm, Duration::from_millis(200));
    if arm.ws_connected() {
        say(out, "private socket: Authenticated; fills + order_updates confirmed");
    } else {
        say(out, "private socket: NOT connected (see ws_connect_failures)");
    }
    let mut ok = arm.ws_connected();
    for (path, tail, label) in [
        (&b"/portfolio"[..], &b""[..], "GET /portfolio"),
        (&b"/orders"[..], &b"&status=open"[..], "GET /orders?status=open"),
    ] {
        match arm.get_wallet(path, tail) {
            Ok((http, body)) => {
                ok &= http == 200;
                say(out, &format!("{label} → {http}: {}", clip(body)));
            }
            Err(e) => {
                ok = false;
                say(out, &format!("{label} failed: {e:?}"));
            }
        }
    }
    if ok {
        Verdict::Pass
    } else {
        Verdict::Fail
    }
}

/// `simulate` (read-only): a bid of `qty_1e6` at `px_1e6` on the
/// table's first row. PASS: HTTP 200, `"success":true` and the venue's
/// `data.admissible` true — its own verdict on whether it would take the
/// order (a wallet with no funds reads `false` with its reason).
pub fn simulate<W: Write>(arm: &mut HcExchange, px_1e6: i64, qty_1e6: i64, out: &mut W) -> Verdict {
    let Some((sym, name)) = arm.table().get(0) else {
        say(out, "no instrument (--symbol)");
        return Verdict::Fail;
    };
    let name = String::from_utf8_lossy(name).into_owned();
    match arm.simulate(sym, true, px_1e6, qty_1e6) {
        Ok((http, body)) => {
            let root = crate::json::root(body);
            let success = root
                .as_ref()
                .and_then(|r| crate::json::field_in(body, r, b"success"))
                .and_then(|v| v.as_bool(body))
                == Some(true);
            let data = root.as_ref().and_then(|r| crate::json::field_in(body, r, b"data"));
            let admissible = data
                .as_ref()
                .and_then(|d| crate::json::field_in(body, d, b"admissible"))
                .and_then(|v| v.as_bool(body))
                == Some(true);
            let reason = data
                .as_ref()
                .and_then(|d| crate::json::field_in(body, d, b"rejection_reason"))
                .map(|v| String::from_utf8_lossy(v.bytes(body)).into_owned())
                .unwrap_or_default();
            say(out, &format!("POST /risk/simulate/orders ({name}, buy {:.6} @ {:.6}) → {http}: {}", qty_1e6 as f64 / 1e6, px_1e6 as f64 / 1e6, clip(body)));
            say(out, &format!("simulate: admissible {admissible}{}", if reason.is_empty() { String::new() } else { format!(" — {reason}") }));
            if http == 200 && success && admissible {
                Verdict::Pass
            } else {
                Verdict::Fail
            }
        }
        Err(e) => {
            say(out, &format!("simulate failed: {e:?}"));
            Verdict::Fail
        }
    }
}

/// `recon` (read-only).
pub fn recon<W: Write>(arm: &mut HcExchange, out: &mut W) -> Verdict {
    match arm.reconcile() {
        Ok(r) => {
            report(out, &r);
            Verdict::Pass
        }
        Err(e) => {
            say(out, &format!("recon failed: {e:?}"));
            Verdict::Fail
        }
    }
}

/// After a place that may have reached the book without an answer we
/// could read: take it back by its client id, then reconcile.
fn take_back<W: Write>(arm: &mut HcExchange, oid: u64, out: &mut W) {
    let c = arm.cancel_oid(oid);
    say(out, &format!("dust: taking it back by client id → {c:?} (reason {:?})", String::from_utf8_lossy(arm.last_reason())));
    if let Ok(r) = arm.reconcile() {
        report(out, &r);
    }
}

/// `dust` — WRITES on mainnet (module doc): a resting bid of `qty_1e6`
/// at `px_1e6` (the ruling's 0.000001 @ $0.0005 by default — the venue's
/// tick and minimum-size rules may refuse it, and then the verb says so
/// and a legal pair is passed). `confirm` is `--confirm`.
///
/// PASS needs ALL of: the place accepted; its cancel answered CANCELED
/// with nothing filled; the socket booked no fill; the reconcile after
/// agrees on every leg, finds nothing of ours open, and the position on
/// the symbol is what it was before.
pub fn dust<W: Write>(
    arm: &mut HcExchange,
    px_1e6: i64,
    qty_1e6: i64,
    confirm: bool,
    out: &mut W,
) -> Verdict {
    let Some((sym, name)) = arm.table().get(0) else {
        say(out, "no instrument (--symbol)");
        return Verdict::Fail;
    };
    let name = String::from_utf8_lossy(name).into_owned();
    say(out, &format!("dust: BUY {:.6} {name} @ ${:.6}, book_only GTC, then cancel by client id, then reconcile", qty_1e6 as f64 / 1e6, px_1e6 as f64 / 1e6));
    if !confirm {
        say(out, "dust: a mainnet order — refused without --confirm");
        return Verdict::Unconfirmed;
    }
    // 1. The venue's view first: positions seeded, orphans named.
    let pre = match arm.reconcile() {
        Ok(r) => r,
        Err(e) => {
            say(out, &format!("dust: the pre-reconcile failed ({e:?}) — nothing sent"));
            return Verdict::Fail;
        }
    };
    report(out, &pre);
    if pre.orphans > 0 {
        say(out, "dust: orders of ours are already open — run `cancel-all --confirm` first; nothing sent");
        return Verdict::Fail;
    }
    let pos_before = arm.position(sym);
    // 2. The fills socket up (and confirmed) before the order (E-5).
    pump_for(arm, Duration::from_millis(500));
    if !arm.ws_connected() {
        say(out, "dust: the private socket did not connect — nothing sent");
        return Verdict::Fail;
    }
    // 3. The order: a client id of its own per run (the wall clock).
    let oid = crate::nonce::now_ms();
    let mut o = Order::new(
        core_time::now_ns(),
        VenueId::Hypercall,
        sym,
        Side::Bid,
        ORDER_KIND_IOC,
        Price::from_raw(px_1e6),
        Qty::from_raw(qty_1e6),
        oid,
    );
    o.strategy_id = arm.slot();
    let before = *arm.counters();
    if let Err(e) = arm.place_resting(&o) {
        let c = *arm.counters();
        let reason = String::from_utf8_lossy(arm.last_reason()).into_owned();
        say(out, &format!("dust: not accepted: {e:?} · venue verdict {:?} · reason {reason:?}", arm.last_refusal()));
        match arm.last_refusal() {
            Some(Why::Auth) => say(out, "dust: FAIL — the venue refused the signature or the agent; nothing rests"),
            Some(Why::Rejected | Why::IocMissed) => say(out, "dust: FAIL — the venue ACCEPTED THE SIGNATURE and refused the order on its own rules (the reason above); nothing rests — rerun with a legal --price/--size"),
            _ if c.submitted == before.submitted => say(out, "dust: FAIL — refused before sending (see the counters); nothing sent"),
            // A 5xx, a 429, an answer that did not read, or no answer:
            // the order may be RESTING. Take it back, then look.
            _ => {
                say(out, "dust: FAIL — the order's fate is unknown; taking it back");
                take_back(arm, oid, out);
            }
        }
        return Verdict::Fail;
    }
    say(out, &format!("dust: ACCEPTED — the arm tracks {} working order(s)", arm.live_orders()));
    pump_for(arm, Duration::from_secs(2));
    // 4. Its cancel, by client id — the status the venue answers is
    // part of the verdict (a FILLED here is a fill).
    let cancelled = arm.cancel_oid(oid);
    say(out, &format!("dust: cancel → {cancelled:?} (reason {:?})", String::from_utf8_lossy(arm.last_reason())));
    let clean_cancel = matches!(&cancelled, Ok(a) if a.status == Status::Canceled && a.filled_1e6 == 0);
    if cancelled.is_err() {
        take_back(arm, oid, out);
    }
    pump_for(arm, Duration::from_secs(2));
    // 5. The verdict: gone, and nothing filled — by every account.
    let r = match arm.reconcile() {
        Ok(r) => r,
        Err(e) => {
            say(out, &format!("dust: the post-reconcile failed ({e:?})"));
            return Verdict::Fail;
        }
    };
    report(out, &r);
    let c = *arm.counters();
    let pos = arm.position(sym);
    say(out, &format!("dust: fills booked {} · position {} (before {}) · working {} · orphans {}", c.fills_booked, pos, pos_before, arm.live_orders(), r.orphans));
    if clean_cancel && r.agreed && r.drift_legs == 0 && r.orphans == 0 && c.fills_booked == 0 && pos == pos_before && arm.live_orders() == 0 {
        say(out, "dust: PASS — signed on mainnet, cancelled, nothing filled");
        Verdict::Pass
    } else {
        say(out, "dust: FAIL — see above; `cancel-all --confirm` takes back anything of ours");
        Verdict::Fail
    }
}

/// `cancel-all` — WRITES on mainnet: every order of ours the venue
/// lists. PASS: a reconcile AFTER the sweep finds none of ours open.
pub fn cancel_all<W: Write>(arm: &mut HcExchange, confirm: bool, out: &mut W) -> Verdict {
    let r = match arm.reconcile() {
        Ok(r) => r,
        Err(e) => {
            say(out, &format!("cancel-all: reconcile failed ({e:?})"));
            return Verdict::Fail;
        }
    };
    report(out, &r);
    if r.orphans == 0 {
        say(out, "cancel-all: nothing of ours is open");
        return Verdict::Pass;
    }
    if !confirm {
        say(out, &format!("cancel-all: would cancel {} order(s) — refused without --confirm", r.orphans));
        return Verdict::Unconfirmed;
    }
    let done = arm.sweep_orphans();
    say(out, &format!("cancel-all: the venue confirmed {done} of {}", r.orphans));
    match arm.reconcile() {
        Ok(after) => {
            report(out, &after);
            if after.orphans == 0 {
                Verdict::Pass
            } else {
                say(out, "cancel-all: FAIL — orders of ours are still open");
                Verdict::Fail
            }
        }
        Err(e) => {
            say(out, &format!("cancel-all: the reconcile after failed ({e:?})"));
            Verdict::Fail
        }
    }
}
