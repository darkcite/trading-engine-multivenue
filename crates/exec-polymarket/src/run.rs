// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The two gateway threads' loops (PM15-6c): they only move time and
//! bytes into the socket-free cores ([`crate::gw`], [`crate::user`]).
//!
//! * **pm-gw** — [`run_pm_gw`]: one [`PmGateway::step`] a pass; spin while
//!   it works, then park up to the kit's `Idle` cap (the engine's push is a
//!   ring write, never a doorbell — XG-1).
//! * **pm-user** — [`run_pm_user`]: one `mio` poll over the user channel's
//!   [`WsConn`]: the subscribe on open, `PING` every [`USER_PING_NS`],
//!   every text frame to the driver; a session lost is a GAP, then the
//!   dial again on the ingress backoff.

use core::sync::atomic::{AtomicBool, Ordering};
use std::io;

use core_net::{Backoff, WsConn, WsNext, WsProgress};
use core_time::{now_ns, WallAnchor};
use exec_gateway::Idle;

use crate::clob::PmClob;
use crate::gw::PmGateway;
use crate::user::{PmUserDriver, USER_PING_NS};

/// The user channel's `mio` token.
pub const PM_USER_TOKEN: mio::Token = mio::Token(0);

/// The pm-user poll's timeout (the keepalive's resolution).
const POLL_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(50);

/// Run pm-gw until `stop` (released by the gateway's owner after the
/// engine loop returned, or on a boot abort).
pub fn run_pm_gw<W: PmClob>(gw: &mut PmGateway<W>, wall: WallAnchor, stop: &AtomicBool) {
    let mut idle = Idle::default();
    while !stop.load(Ordering::Acquire) {
        let now = now_ns();
        let worked = gw.step(now, wall.wall_of(now));
        let park = idle.timeout_ns(worked, now, None);
        if park > 0 {
            std::thread::park_timeout(core::time::Duration::from_nanos(park));
        }
    }
}

/// Run pm-user until `stop`.
///
/// # Errors
/// The `mio` poll could not be created or polled (the spawner logs it;
/// the thread then ends and the arm sees the channel's silence).
pub fn run_pm_user(
    d: &mut PmUserDriver,
    ws: &mut WsConn,
    seed: u64,
    stop: &AtomicBool,
) -> io::Result<()> {
    let mut poll = mio::Poll::new()?;
    let mut events = mio::Events::with_capacity(16);
    let mut backoff = Backoff::default_for_ingress(seed);
    let mut dial_ns = 0u64;
    let mut ping_ns = 0u64;
    while !stop.load(Ordering::Acquire) {
        poll.poll(&mut events, Some(POLL_TIMEOUT))?;
        for ev in events.iter() {
            let now = now_ns();
            match ws.on_event(ev, poll.registry(), now) {
                WsProgress::Opened => {
                    backoff.reset();
                    if !d.on_open(ws) {
                        ws.close();
                        d.on_down();
                        dial_ns = now.saturating_add(backoff.next_delay_ns());
                        continue;
                    }
                    ping_ns = now.saturating_add(USER_PING_NS);
                    if !drain(d, ws) {
                        dial_ns = now.saturating_add(backoff.next_delay_ns());
                    }
                }
                WsProgress::Readable => {
                    if !drain(d, ws) {
                        dial_ns = now.saturating_add(backoff.next_delay_ns());
                    }
                }
                WsProgress::Failed(_) => {
                    d.on_down();
                    dial_ns = now.saturating_add(backoff.next_delay_ns());
                }
                WsProgress::Waiting => {}
            }
        }
        let now = now_ns();
        if ws.is_down() && now >= dial_ns && ws.connect(poll.registry(), now).is_err() {
            d.on_down();
            dial_ns = now.saturating_add(backoff.next_delay_ns());
        }
        if let WsProgress::Failed(_) = ws.on_tick(now) {
            d.on_down();
            dial_ns = now.saturating_add(backoff.next_delay_ns());
        }
        if ws.is_open() && now >= ping_ns {
            ping_ns = now.saturating_add(USER_PING_NS);
            if ws.queue_text(&[b"PING"]).is_err() {
                ws.close();
                d.on_down();
                dial_ns = now.saturating_add(backoff.next_delay_ns());
            }
        }
        if ws.wants_flush() && ws.flush(poll.registry()).is_err() {
            d.on_down();
            dial_ns = now.saturating_add(backoff.next_delay_ns());
        }
    }
    ws.close();
    Ok(())
}

/// Every buffered frame to the driver; `false` when the session failed
/// (told to the driver as a GAP).
fn drain(d: &mut PmUserDriver, ws: &mut WsConn) -> bool {
    loop {
        match ws.next_frame() {
            WsNext::Text(span) => {
                let now = now_ns();
                d.on_text(ws.payload(span), now);
            }
            WsNext::Binary(_) => {}
            WsNext::Idle => return true,
            WsNext::Failed(_) => {
                d.on_down();
                return false;
            }
        }
    }
}
