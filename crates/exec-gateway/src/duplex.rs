// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The engine ⇄ gateway duplex: one bounded SPSC ring each way plus the
//! shared [`GatewayStats`].
//!
//! * **Commands** (engine → gateway, `NC` slots): the engine's submit,
//!   cancel and modify become one `C` pushed by reference — the ring-slot
//!   publish, the one copy the zero-copy law designs in. A full ring
//!   refuses the command ([`CmdTx::push`] → `false`, counted): the caller
//!   fails a GROWING order closed; a refused cancel stays the caller's to
//!   retry, its order still counted as exposure.
//! * **Events** (gateway → engine, `NE` slots): acks, rejects, order
//!   state changes and fills, read by the engine IN PLACE through the
//!   ring's [`Popped`] guard. The event ring must never fill: a full event
//!   ring is a bug — fatal in debug, and in release a counted drop that
//!   sets the halt word, because an unseen fill is an unbooked position.
//!
//! **Both rings are 64 B slots** (one cache line, the house law —
//! const-asserted), and the event ring holds at least four command
//! rings' worth (`NE ≥ 4·NC`, const-asserted). An order that fills
//! produces up to five events (ack, resting, fill, fill meta, filled), so
//! the gateway must also stop popping commands while the event ring lacks
//! room for its in-flight worst case (XH4b) — that turns a full event
//! ring into a command refusal before it can become a lost event.
//!
//! Each end is two disjoint handles with public fields, so a command
//! guard can stay lent while the same thread emits (`gw.cmd.pop()` and
//! `gw.evt.emit(..)` borrow different fields), and the engine can hold an
//! event guard while a member submits: nothing is copied out of a slot to
//! satisfy the borrow checker.

use std::sync::Arc;

use core_ring::{Consumer, Popped, Producer, Ring};

use crate::stats::{GatewayStats, HALT_EVENT_RING_FULL};

/// The engine's command sender.
pub struct CmdTx<C: Copy, const NC: usize> {
    ring: Producer<C, NC>,
    stats: Arc<GatewayStats>,
}

/// The gateway's command receiver.
pub struct CmdRx<C: Copy, const NC: usize> {
    ring: Consumer<C, NC>,
    stats: Arc<GatewayStats>,
}

/// The gateway's event sender.
pub struct EvtTx<E: Copy, const NE: usize> {
    ring: Producer<E, NE>,
    stats: Arc<GatewayStats>,
}

/// The engine's event receiver.
pub struct EvtRx<E: Copy, const NE: usize> {
    ring: Consumer<E, NE>,
}

/// The engine thread's end.
pub struct EngineEnd<C: Copy, E: Copy, const NC: usize, const NE: usize> {
    /// Commands out.
    pub cmd: CmdTx<C, NC>,
    /// Events in, read in place.
    pub evt: EvtRx<E, NE>,
    /// The shared stats block (the halt word, counters, latencies).
    pub stats: Arc<GatewayStats>,
}

/// The gateway thread's end.
pub struct GatewayEnd<C: Copy, E: Copy, const NC: usize, const NE: usize> {
    /// Commands in, read in place.
    pub cmd: CmdRx<C, NC>,
    /// Events out.
    pub evt: EvtTx<E, NE>,
    /// The shared stats block.
    pub stats: Arc<GatewayStats>,
}

/// Build one arm's duplex (boot only: allocates both rings and the stats
/// block, once).
#[must_use]
pub fn duplex<C: Copy, E: Copy, const NC: usize, const NE: usize>(
) -> (EngineEnd<C, E, NC, NE>, GatewayEnd<C, E, NC, NE>) {
    const {
        assert!(
            core::mem::size_of::<C>() == 64 && core::mem::align_of::<C>() == 64,
            "a command is one 64 B cache line"
        );
        assert!(
            core::mem::size_of::<E>() == 64 && core::mem::align_of::<E>() == 64,
            "an event is one 64 B cache line"
        );
        assert!(
            NE >= 4 * NC,
            "the event ring holds at least four command rings"
        );
    }
    let (cp, cc) = Ring::<C, NC>::new().split();
    let (ep, ec) = Ring::<E, NE>::new().split();
    // COPY: the stats block, GATEWAY_STATS_SIZE (83 456 B), moved once per
    // arm at boot from the booting thread's stack into its Arc — cold; the
    // kit forbids the `unsafe` an in-place build (`Arc::new_uninit`) needs.
    let stats = Arc::new(GatewayStats::new());
    (
        EngineEnd {
            cmd: CmdTx {
                ring: cp,
                stats: Arc::clone(&stats),
            },
            evt: EvtRx { ring: ec },
            stats: Arc::clone(&stats),
        },
        GatewayEnd {
            cmd: CmdRx {
                ring: cc,
                stats: Arc::clone(&stats),
            },
            evt: EvtTx {
                ring: ep,
                stats: Arc::clone(&stats),
            },
            stats,
        },
    )
}

impl<C: Copy, const NC: usize> CmdTx<C, NC> {
    /// Push one command. `false` = the ring is full (counted). Never
    /// blocks, never allocates, no syscall.
    #[inline]
    pub fn push(&mut self, cmd: &C) -> bool {
        if self.ring.try_push_ref(cmd) {
            self.stats.note_pushed();
            true
        } else {
            self.stats.note_cmd_ring_full();
            false
        }
    }

    /// Commands still queued for the gateway.
    #[inline]
    pub fn queued(&self) -> usize {
        self.ring.len()
    }
}

impl<C: Copy, const NC: usize> CmdRx<C, NC> {
    /// The next command, read in place; the slot is released when the
    /// guard drops.
    #[inline]
    pub fn pop(&mut self) -> Option<Popped<'_, C, NC>> {
        let c = self.ring.try_pop_ref();
        if c.is_some() {
            self.stats.note_popped();
        }
        c
    }

    /// Commands waiting.
    #[inline]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether no command waits.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

impl<E: Copy, const NE: usize> EvtTx<E, NE> {
    /// Emit one event. `false` only when the event ring is full — a bug:
    /// counted and the arm halted ([`HALT_EVENT_RING_FULL`], set first),
    /// then a `debug_assert!` failure in debug.
    #[inline]
    pub fn emit(&mut self, ev: &E) -> bool {
        if self.ring.try_push_ref(ev) {
            return true;
        }
        self.stats.note_evt_ring_full();
        self.stats.halt(HALT_EVENT_RING_FULL);
        debug_assert!(
            false,
            "exec-gateway: the event ring is full — an event was lost"
        );
        false
    }

    /// Free event slots (the gateway's back-pressure check before it pops
    /// more commands).
    #[inline]
    pub fn room(&self) -> usize {
        NE - self.ring.len()
    }
}

impl<E: Copy, const NE: usize> EvtRx<E, NE> {
    /// The next event, read in place; the slot is released when the guard
    /// drops.
    #[inline]
    pub fn pop(&mut self) -> Option<Popped<'_, E, NE>> {
        self.ring.try_pop_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    #[repr(C, align(64))]
    struct Msg {
        oid: u64,
        _pad: [u8; 56],
    }

    fn msg(oid: u64) -> Msg {
        Msg { oid, _pad: [0; 56] }
    }

    #[test]
    fn a_command_guard_stays_lent_while_the_same_thread_emits() {
        let (mut eng, mut gw) = duplex::<Msg, Msg, 8, 32>();
        let mut i = 1u64;
        while i <= 5 {
            assert!(eng.cmd.push(&msg(i)));
            i += 1;
        }
        assert_eq!(eng.cmd.queued(), 5);
        let mut want = 1u64;
        while let Some(c) = gw.cmd.pop() {
            assert_eq!(c.oid, want);
            // Disjoint fields: the guard on `gw.cmd` and the emit on `gw.evt`.
            assert!(gw.evt.emit(&msg(c.oid * 10)));
            want += 1;
        }
        assert_eq!(want, 6);
        let mut got = 0u64;
        while let Some(e) = eng.evt.pop() {
            got += e.oid;
        }
        assert_eq!(got, 150);
        assert_eq!(eng.stats.cmds(), (5, 0));
        assert_eq!(gw.stats.gateway_ring(), (5, 0));
        assert_eq!(eng.stats.halt_reason(), crate::HALT_NONE);
        assert_eq!(gw.evt.room(), 32);
    }

    #[test]
    fn a_full_command_ring_refuses_and_counts_never_blocks() {
        let (mut eng, _gw) = duplex::<Msg, Msg, 4, 16>();
        let mut ok = 0;
        let mut i = 0u64;
        while i < 6 {
            ok += usize::from(eng.cmd.push(&msg(i)));
            i += 1;
        }
        assert_eq!(ok, 4);
        assert_eq!(eng.stats.cmds(), (4, 2));
    }

    /// The production law, checked in every build: the lost event is
    /// counted and the halt word is set BEFORE the debug assertion fires.
    #[test]
    fn a_full_event_ring_counts_and_halts_before_it_asserts() {
        let (eng, mut gw) = duplex::<Msg, Msg, 2, 8>();
        let mut i = 1u64;
        while i <= 8 {
            assert!(gw.evt.emit(&msg(i)));
            i += 1;
        }
        assert_eq!(gw.evt.room(), 0);
        let lost = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gw.evt.emit(&msg(9))));
        if cfg!(debug_assertions) {
            assert!(lost.is_err(), "debug: a lost event is fatal");
        } else {
            assert_eq!(lost.ok(), Some(false), "release: refused, counted, halted");
        }
        assert_eq!(eng.stats.halt_reason(), HALT_EVENT_RING_FULL);
        assert_eq!(eng.stats.gateway_ring().1, 1);
    }

    #[test]
    fn the_ends_cross_threads() {
        let (mut eng, mut gw) = duplex::<Msg, Msg, 64, 256>();
        let t = std::thread::spawn(move || {
            let mut seen = 0u64;
            while seen < 1_000 {
                if gw.evt.room() == 0 {
                    std::hint::spin_loop();
                    continue;
                }
                match gw.cmd.pop() {
                    Some(c) => {
                        assert!(gw.evt.emit(&msg(c.oid)));
                        seen += 1;
                    }
                    None => std::hint::spin_loop(),
                }
            }
        });
        let mut sent = 0u64;
        let mut sum = 0u64;
        let mut got = 0u64;
        while got < 1_000 {
            if sent < 1_000 && eng.cmd.push(&msg(sent + 1)) {
                sent += 1;
            }
            while let Some(e) = eng.evt.pop() {
                sum += e.oid;
                got += 1;
            }
        }
        t.join().expect("gateway thread");
        assert_eq!(sum, 500_500);
    }
}
