// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # drain — the I-3 inner loop every ingress run loop shares
//!
//! A poll wakes an ingress run loop; the loop then drives each live
//! connection — `drive_one`: flush tx, read rx ([`fill_rx`]) until it is
//! full or the transport says `WouldBlock`, advance the state machine —
//! again, until a step makes no progress ([`drain_until_idle!`], one body for
//! every crate). An edge-triggered poller (kqueue `EV_CLEAR`, epoll
//! `EPOLLET`) reports new arrivals only, so what a step leaves behind is the
//! loop's to finish. A step made progress, and the loop drives again, when:
//!
//! * **its read stopped on a full rx** ([`RxFill::Full`]) — not on
//!   `WouldBlock`, so input may still wait below it, in the kernel or in
//!   rustls, that no readiness edge will announce again. A read that finds
//!   rx full before taking a byte is not this: re-driving cannot help a
//!   frame larger than rx.
//! * **its state moved** — the WebSocket upgrade completing with frames
//!   already behind it in rx (HyperEVM also counts its phase machine).
//! * **it published** — kept from the loop's first form: a producer that
//!   emits as the engine frees ring room (HyperEVM's snapshot phases) keeps
//!   going, and input that arrived meanwhile is read without a poll round
//!   trip.
//!
//! A backlog stays bounded: after [`DRAIN_STEP_CAP`] steps a connection's
//! drain ends [`Drained::Capped`], and the loop polls again at once
//! ([`poll_timeout`]) rather than sleeping — its other connections,
//! keepalive, capture flush and stop flag get their turn — and drives the
//! capped connection again on the next iteration: every run loop drives
//! each live connection every iteration, readiness or not.

use core::time::Duration;
use std::io;

use crate::iobuf::IoBuf;
use crate::transport::Transport;

/// Most drive steps one connection gets per poll iteration while its steps
/// keep making progress. A step can fill a whole rx buffer (64 KiB – 4 MiB
/// by venue), so eight bound one connection's turn at 0.5–32 MiB of
/// backlog before the loop serves anything else.
pub const DRAIN_STEP_CAP: u32 = 8;

// The cap bounds re-drives; it must not forbid them (one step is the
// loop's first form, which left a full rx's backlog to the next edge).
const _: () = assert!(DRAIN_STEP_CAP > 1);

/// A run loop's poll timeout when nothing is left to drive: the cadence of
/// its capture flush and keepalive checks on a quiet feed.
pub const POLL_IDLE: Duration = Duration::from_millis(50);

/// How one connection's drain ended.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Drained {
    /// A step made no progress: poll with [`POLL_IDLE`].
    Idle,
    /// [`DRAIN_STEP_CAP`] steps, still making progress: poll again at once.
    Capped,
    /// The state machine reached `Closed` (EOF, a WebSocket Close, the
    /// transport's close).
    Closed,
    /// A step failed with this I/O error kind (transport or protocol).
    Failed(io::ErrorKind),
}

/// The next poll's timeout: zero once any connection's drain ended
/// [`Drained::Capped`] this iteration, [`POLL_IDLE`] otherwise.
#[inline]
pub const fn poll_timeout(repoll_now: bool) -> Duration {
    if repoll_now {
        Duration::ZERO
    } else {
        POLL_IDLE
    }
}

/// What [`fill_rx`] left behind.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RxFill {
    /// Nothing more to take now: the transport would block — everything
    /// that had arrived is in rx — or rx was full before this read took a
    /// byte (a frame larger than rx, which driving again cannot shrink).
    Drained,
    /// The read took bytes and stopped on a full rx: input may still wait
    /// below it. Drive again once the frames in rx are consumed.
    Full,
    /// The peer closed the stream (a read returned 0).
    Eof,
}

/// Read the plaintext `transport` holds into `rx` until rx is full or the
/// transport would block — the rx half of every ingress `drive_one`.
///
/// A read that finds rx full before taking a byte reports
/// [`RxFill::Drained`]: after a drain only a frame larger than rx leaves rx
/// full, and driving again cannot shrink it. A read that fills rx exactly
/// reports [`RxFill::Full`]; the next step's read decides.
///
/// # Errors
/// Any transport error but `WouldBlock`.
#[inline]
pub fn fill_rx<T: Transport>(transport: &mut T, rx: &mut IoBuf) -> io::Result<RxFill> {
    let mut took = false;
    loop {
        let free = rx.free_mut();
        if free.is_empty() {
            return Ok(if took { RxFill::Full } else { RxFill::Drained });
        }
        match transport.read(free) {
            Ok(0) => return Ok(RxFill::Eof),
            Ok(n) => {
                rx.advance(n);
                took = true;
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(RxFill::Drained),
            Err(e) => return Err(e),
        }
    }
}

/// The I-3 drain ([module docs](crate::drain)), one body for every ingress
/// crate: evaluate `step` — a `drive_one` call, `io::Result<bool>`, `true`
/// when its read stopped on a full rx — until a step makes no progress
/// (`step` said `false`, `published` did not move, `key`, the state
/// machine's position, held) or [`DRAIN_STEP_CAP`] steps have run.
/// `closed` is checked after every step. Evaluates to a [`Drained`].
///
/// A macro rather than a function: each crate's `drive_one` takes its own
/// lanes, and the loop runs on every poll wake, where a closure is off the
/// table (the hot-path rules). `published` and `key` are evaluated before
/// and after each step; both must be cheap reads.
#[macro_export]
macro_rules! drain_until_idle {
    (step: $step:expr, published: $published:expr, key: $key:expr, closed: $closed:expr $(,)?) => {{
        let mut steps: u32 = 0;
        loop {
            let published_before = $published;
            let key_before = $key;
            let rx_full = match $step {
                Ok(full) => full,
                Err(e) => break $crate::Drained::Failed(e.kind()),
            };
            if $closed {
                break $crate::Drained::Closed;
            }
            if !rx_full && $published == published_before && $key == key_before {
                break $crate::Drained::Idle;
            }
            steps += 1;
            if steps == $crate::DRAIN_STEP_CAP {
                break $crate::Drained::Capped;
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::TestTransport;

    #[test]
    fn a_capped_drain_polls_at_once_an_idle_one_waits() {
        assert_eq!(poll_timeout(true), Duration::ZERO);
        assert_eq!(poll_timeout(false), POLL_IDLE);
        assert_eq!(POLL_IDLE, Duration::from_millis(50));
    }

    /// A scripted connection for the drain macro. Step `n` (from 1) reads
    /// to a full rx while `n <= full_until`, publishes while
    /// `n <= publish_until`, moves its state while `n <= move_until`,
    /// closes at `closed_at` and fails at `fail_at` (0: never).
    struct Script {
        steps: u32,
        full_until: u32,
        publish_until: u32,
        move_until: u32,
        closed_at: u32,
        fail_at: u32,
        published: usize,
        state: u32,
    }

    impl Script {
        fn new() -> Self {
            Self {
                steps: 0,
                full_until: 0,
                publish_until: 0,
                move_until: 0,
                closed_at: 0,
                fail_at: 0,
                published: 0,
                state: 0,
            }
        }

        fn step(&mut self) -> io::Result<bool> {
            self.steps += 1;
            if self.steps == self.fail_at {
                return Err(io::Error::from(io::ErrorKind::ConnectionReset));
            }
            self.published += usize::from(self.steps <= self.publish_until);
            self.state += u32::from(self.steps <= self.move_until);
            Ok(self.steps <= self.full_until)
        }

        fn drain(&mut self) -> Drained {
            drain_until_idle!(
                step: self.step(),
                published: self.published,
                key: self.state,
                closed: self.steps == self.closed_at,
            )
        }
    }

    #[test]
    fn a_drain_ends_at_the_first_step_without_progress() {
        let mut s = Script { full_until: 3, ..Script::new() };
        assert_eq!(s.drain(), Drained::Idle);
        assert_eq!(s.steps, 4, "three reads to a full rx, then one that drained");
        let mut s = Script { publish_until: 2, ..Script::new() };
        assert_eq!(s.drain(), Drained::Idle);
        assert_eq!(s.steps, 3, "a publish is progress");
        let mut s = Script { move_until: 1, ..Script::new() };
        assert_eq!(s.drain(), Drained::Idle);
        assert_eq!(s.steps, 2, "a state change is progress");
        let mut s = Script::new();
        assert_eq!(s.drain(), Drained::Idle);
        assert_eq!(s.steps, 1, "a step without progress ends the drain at once");
    }

    #[test]
    fn a_drain_that_keeps_progressing_is_capped_at_exactly_the_cap() {
        let mut s = Script { full_until: u32::MAX, ..Script::new() };
        assert_eq!(s.drain(), Drained::Capped);
        assert_eq!(s.steps, DRAIN_STEP_CAP);
        // The next drain picks up where the cap cut this one short.
        s.full_until = DRAIN_STEP_CAP + 2;
        assert_eq!(s.drain(), Drained::Idle);
        assert_eq!(s.steps, DRAIN_STEP_CAP + 3);
    }

    #[test]
    fn a_drain_reports_a_close_or_a_failure_at_the_step_that_saw_it() {
        let mut s = Script { full_until: u32::MAX, closed_at: 2, ..Script::new() };
        assert_eq!(s.drain(), Drained::Closed);
        assert_eq!(s.steps, 2);
        let mut s = Script { full_until: u32::MAX, fail_at: 3, ..Script::new() };
        assert_eq!(s.drain(), Drained::Failed(io::ErrorKind::ConnectionReset));
        assert_eq!(s.steps, 3);
    }

    #[test]
    fn fill_rx_reads_until_the_transport_would_block() {
        let mut t = TestTransport::with_capacity(256);
        let mut rx = IoBuf::with_capacity(64);
        t.inject_incoming(&[7u8; 10]);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Drained);
        assert_eq!(rx.len(), 10);
        assert_eq!(t.incoming_len(), 0);
    }

    #[test]
    fn fill_rx_reports_full_when_it_filled_rx_with_input_left_below() {
        let mut t = TestTransport::with_capacity(256);
        let mut rx = IoBuf::with_capacity(64);
        t.inject_incoming(&[7u8; 100]);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Full);
        assert_eq!(rx.len(), 64);
        assert_eq!(t.incoming_len(), 36, "the rest waits below rx");
        // Once the frames in rx are consumed, the next read takes the rest.
        rx.consume(64);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Drained);
        assert_eq!(rx.len(), 36);
    }

    #[test]
    fn fill_rx_that_fills_rx_exactly_reports_full_and_the_next_read_decides() {
        let mut t = TestTransport::with_capacity(256);
        let mut rx = IoBuf::with_capacity(64);
        t.inject_incoming(&[7u8; 64]);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Full);
        rx.consume(64);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Drained);
        assert!(rx.is_empty());
    }

    #[test]
    fn fill_rx_that_finds_rx_full_took_nothing_and_is_not_progress() {
        // A frame larger than rx leaves rx full after a drain: re-driving
        // cannot shrink it, so this must not read as "input below".
        let mut t = TestTransport::with_capacity(256);
        let mut rx = IoBuf::with_capacity(64);
        rx.advance(64);
        t.inject_incoming(&[7u8; 10]);
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Drained);
        assert_eq!(t.incoming_len(), 10, "nothing was read");
    }

    #[test]
    fn fill_rx_reports_eof_when_the_peer_closed() {
        let mut t = TestTransport::with_capacity(256);
        let mut rx = IoBuf::with_capacity(64);
        t.inject_incoming(&[7u8; 5]);
        t.mark_closed();
        assert_eq!(fill_rx(&mut t, &mut rx).unwrap(), RxFill::Eof);
        assert_eq!(rx.len(), 5, "bytes before the close stay in rx");
    }
}
