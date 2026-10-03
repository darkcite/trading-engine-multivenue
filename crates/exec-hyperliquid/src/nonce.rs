// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The action nonce.
//!
//! Hyperliquid requires a millisecond timestamp inside
//! `(T − 2 days, T + 1 day)`, unique, and **greater than the smallest
//! of the 100 highest** the venue holds for that signer. A monotonic counter
//! anchored on the wall clock satisfies all three trivially: it never
//! repeats, and it only ever moves forward.
//!
//! [`Nonce`] is single-writer by construction — it lives on one signer's
//! thread and is `&mut`, so there is no atomic on the hot path and no
//! question about ordering.
//!
//! **One nonce stream per agent address in a process (XMM XH4h, S3).**
//! The venue keeps its window per SIGNER, and one process may hold
//! several signers of one agent — the Hyperliquid gateway's thread and
//! the blocking arms on the engine thread (the E3 testnet agent signs for
//! slots 0, 3 and 6). Two counters on one agent would draw the same
//! millisecond, and the venue refuses the loser — possibly a cancel. So
//! the engine's boot gives every signer of one agent the same
//! [`SharedNonce`] ([`NonceStreams`]); an own [`Nonce`]
//! ([`NonceSource::Own`]) is for a process's lone signer (exec-smoke).
//! **S3 shares within ONE process only.** Two processes signing as one
//! agent (E3 on testnet, X on mainnet) still collide: the XH4g battery,
//! exec-smoke, the arm smoke and an engine never run at once on one agent
//! (the XH4f obligation stands). The venue's window of the 100 highest
//! nonces is shared by every signer of the agent too: a post overtaken by
//! 100 later draws is refused for its nonce — never a fill, never a
//! duplicate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A monotonic millisecond nonce.
#[derive(Debug, Default, Copy, Clone)]
pub struct Nonce {
    last: u64,
}

impl Nonce {
    /// A fresh counter.
    #[inline(always)]
    #[must_use]
    pub const fn new() -> Self {
        Self { last: 0 }
    }

    /// The next nonce for `now_ms`.
    ///
    /// Takes the clock when it has moved and otherwise increments —
    /// so a burst inside one millisecond, or a clock that steps
    /// backwards, still yields a strictly increasing sequence.
    #[inline(always)]
    pub fn next(&mut self, now_ms: u64) -> u64 {
        self.last = if now_ms > self.last {
            now_ms
        } else {
            self.last.saturating_add(1)
        };
        self.last
    }

    /// The last nonce issued. Cold; the boot tell and `/state`.
    #[inline(always)]
    #[must_use]
    pub const fn last(&self) -> u64 {
        self.last
    }
}

/// **XMM XH4h S3 — one agent address's nonce stream**, shared by every
/// signer of that agent in this process: `max(now_ms, last + 1)` taken by
/// one compare-and-swap, so no two draws are equal and each is above every
/// earlier one (in the stream's modification order — the venue accepts a
/// nonce that arrives behind a later one while it beats the smallest of
/// its 100 highest). No lock, no allocation; its own 128 B granule (the
/// house rule for an atomic two threads write: the M-series line, and
/// x86's adjacent-line prefetch pair).
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct SharedNonce {
    last: AtomicU64,
}

const _: () = assert!(core::mem::size_of::<SharedNonce>() == 128);

impl SharedNonce {
    /// A fresh stream.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }

    /// The next nonce for `now_ms`, as [`Nonce::next`], across threads.
    /// `Relaxed` is enough: the stream publishes no other memory, and the
    /// read-modify-writes of one atomic are totally ordered.
    #[inline(always)]
    pub fn next(&self, now_ms: u64) -> u64 {
        let mut cur = self.last.load(Ordering::Relaxed);
        loop {
            let n = if now_ms > cur {
                now_ms
            } else {
                cur.saturating_add(1)
            };
            match self
                .last
                .compare_exchange_weak(cur, n, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return n,
                Err(seen) => cur = seen,
            }
        }
    }

    /// Raise the stream to at least `n` — a nonce a signer drew on its own
    /// before it joined: every later draw is above it.
    #[inline]
    pub fn raise_past(&self, n: u64) {
        let _ = self.last.fetch_max(n, Ordering::Relaxed);
    }

    /// The last nonce drawn. Cold; the boot tell and the tests.
    #[must_use]
    pub fn last(&self) -> u64 {
        self.last.load(Ordering::Relaxed)
    }
}

/// Where one signer draws its nonces: its own counter (a process's lone
/// signer) or its agent's [`SharedNonce`] (every engine signer, from its
/// boot). One predictable branch per signed action; no allocation; an
/// enum rather than a generic, so no type parameter rides through the
/// arms and the engine's loop shapes for a nanosecond beside a signature's
/// tens of microseconds. Not `Clone`: a cloned own counter would be a
/// second counter on one agent.
#[derive(Debug)]
pub enum NonceSource {
    /// Its own counter.
    Own(Nonce),
    /// The agent address's stream ([`NonceStreams`]).
    Shared(Arc<SharedNonce>),
}

impl Default for NonceSource {
    fn default() -> Self {
        Self::own()
    }
}

impl NonceSource {
    /// A fresh own counter.
    #[must_use]
    pub const fn own() -> Self {
        Self::Own(Nonce::new())
    }

    /// The next nonce for `now_ms`.
    #[inline(always)]
    pub fn next(&mut self, now_ms: u64) -> u64 {
        match self {
            Self::Own(n) => n.next(now_ms),
            Self::Shared(s) => s.next(now_ms),
        }
    }

    /// The last nonce this source issued (shared: the stream's).
    #[must_use]
    pub fn last(&self) -> u64 {
        match self {
            Self::Own(n) => n.last(),
            Self::Shared(s) => s.last(),
        }
    }

    /// Draw from `stream` from now on — at boot, before the signer's first
    /// signed action (asserted in debug builds: a signer that drew on its
    /// own first could repeat a nonce ANOTHER signer of the agent drew in
    /// the same millisecond). In release the stream is still raised past
    /// every nonce drawn here, so this signer never steps back.
    pub fn share(&mut self, stream: Arc<SharedNonce>) {
        debug_assert_eq!(
            self.last(),
            0,
            "a signer joins its agent's nonce stream before its first signed action"
        );
        stream.raise_past(self.last());
        *self = Self::Shared(stream);
    }

    /// Whether this source draws from a shared stream.
    #[must_use]
    pub const fn is_shared(&self) -> bool {
        matches!(self, Self::Shared(_))
    }
}

/// The most agent addresses one process signs as: the operator's, X's,
/// slot 6's — and one spare.
pub const MAX_AGENTS: usize = 4;

/// The process's nonce streams, one per agent address (S3). Boot only:
/// each signer asks for its agent's stream right after it is built and
/// before its first signed action.
#[derive(Debug, Default)]
pub struct NonceStreams {
    agents: [[u8; 20]; MAX_AGENTS],
    streams: [Option<Arc<SharedNonce>>; MAX_AGENTS],
    n: usize,
    signers: u32,
}

impl NonceStreams {
    /// No stream yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `agent`'s stream, made on its first ask. `None` past
    /// [`MAX_AGENTS`] agents.
    pub fn stream(&mut self, agent: &[u8; 20]) -> Option<Arc<SharedNonce>> {
        let mut i = 0usize;
        while i < self.n {
            if self.agents[i] == *agent {
                self.signers += 1;
                return self.streams[i].clone();
            }
            i += 1;
        }
        if self.n == MAX_AGENTS {
            return None;
        }
        let s = Arc::new(SharedNonce::new());
        self.agents[self.n] = *agent;
        self.streams[self.n] = Some(Arc::clone(&s));
        self.n += 1;
        self.signers += 1;
        Some(s)
    }

    /// Agent addresses with a stream.
    #[must_use]
    pub const fn agents(&self) -> usize {
        self.n
    }

    /// Signers that asked for a stream.
    #[must_use]
    pub const fn signers(&self) -> u32 {
        self.signers
    }
}

/// The wall clock in milliseconds since the Unix epoch — the value the
/// venue window `(T − 2 days, T + 1 day)` is measured against.
///
/// Returns **0 when the clock is before the epoch**, which no caller
/// may feed to [`Nonce::next`] as a real time: `exchange::seal`
/// refuses to sign on a zero clock rather than emit a nonce the venue
/// would reject as ancient. One definition for the arm, the smoke and
/// the lifecycle gate, so all three read the same clock.
#[inline]
#[must_use]
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wall_clock_is_after_the_epoch_and_in_milliseconds() {
        let ms = now_ms();
        // 2020-01-01T00:00:00Z — any real clock on a build host is past it.
        assert!(ms > 1_577_836_800_000, "clock reads {ms} ms");
        // and not in nanoseconds / seconds by mistake (year 2100 bound).
        assert!(ms < 4_102_444_800_000, "clock reads {ms} ms");
    }

    #[test]
    fn it_takes_the_clock_when_the_clock_moves() {
        let mut n = Nonce::new();
        assert_eq!(n.next(1_000), 1_000);
        assert_eq!(n.next(1_001), 1_001);
        assert_eq!(n.next(2_000), 2_000);
    }

    #[test]
    fn a_burst_inside_one_millisecond_still_increases() {
        let mut n = Nonce::new();
        assert_eq!(n.next(1_000), 1_000);
        assert_eq!(n.next(1_000), 1_001);
        assert_eq!(n.next(1_000), 1_002);
        assert_eq!(n.next(1_000), 1_003);
    }

    /// NTP steps the clock backwards. A nonce that went backwards with
    /// it would be rejected by the venue and, worse, could repeat one
    /// the venue has already seen.
    #[test]
    fn a_backwards_clock_never_moves_the_nonce_backwards() {
        let mut n = Nonce::new();
        assert_eq!(n.next(5_000), 5_000);
        assert_eq!(n.next(1_000), 5_001, "clock stepped back; nonce did not");
        assert_eq!(n.next(1), 5_002);
        assert_eq!(n.next(5_003), 5_003, "and it recovers when the clock passes");
    }

    #[test]
    fn it_is_strictly_increasing_over_a_hostile_clock() {
        let mut n = Nonce::new();
        let mut prev = 0u64;
        let clocks = [10u64, 10, 9, 11, 11, 11, 1, 100, 100, 99];
        for c in clocks {
            let v = n.next(c);
            assert!(v > prev, "{v} !> {prev}");
            prev = v;
        }
        assert_eq!(n.last(), prev);
    }

    /// S3: the shared stream is [`Nonce`] across threads — the clock when
    /// it moves, else one more, never backwards.
    #[test]
    fn the_shared_stream_follows_the_counter_law() {
        let s = SharedNonce::new();
        assert_eq!(s.next(1_000), 1_000);
        assert_eq!(s.next(1_000), 1_001);
        assert_eq!(s.next(5), 1_002, "a backwards clock");
        assert_eq!(s.next(2_000), 2_000);
        s.raise_past(9_000);
        assert_eq!(s.next(2_001), 9_001, "raised past a nonce drawn alone");
        s.raise_past(10);
        assert_eq!(s.last(), 9_001, "raising never lowers");
    }

    /// S3: signers of one agent on several threads never draw the same
    /// nonce, and each thread's draws increase.
    #[test]
    fn threads_on_one_stream_never_draw_the_same_nonce() {
        const T: usize = 4;
        const N: usize = 50_000;
        let s = Arc::new(SharedNonce::new());
        let mut all: Vec<u64> = Vec::with_capacity(T * N);
        std::thread::scope(|sc| {
            let mut hs = Vec::with_capacity(T);
            for t in 0..T {
                let s = Arc::clone(&s);
                hs.push(sc.spawn(move || {
                    let mut v = Vec::with_capacity(N);
                    for i in 0..N {
                        // A clock that stalls, steps and jumps per thread.
                        v.push(s.next(1_000 + ((i / 7) as u64) * (t as u64 + 1)));
                    }
                    v
                }));
            }
            for h in hs {
                let v = h.join().expect("thread");
                assert!(
                    v.windows(2).all(|w| w[1] > w[0]),
                    "a thread's draws increase"
                );
                all.extend(v);
            }
        });
        all.sort_unstable();
        let n = all.len();
        all.dedup();
        assert_eq!(all.len(), n, "no nonce drawn twice");
    }

    /// S3: two signers that join one stream interleave uniquely, and the
    /// stream may already be ahead of the clock (raised past a draw).
    #[test]
    fn signers_joined_to_one_stream_interleave_uniquely() {
        let s = Arc::new(SharedNonce::new());
        s.raise_past(7_001);
        let mut a = NonceSource::own();
        let mut b = NonceSource::own();
        assert!(!a.is_shared());
        a.share(Arc::clone(&s));
        b.share(Arc::clone(&s));
        assert!(a.is_shared() && b.is_shared());
        assert_eq!(b.next(7_000), 7_002, "above the raise");
        assert_eq!(a.next(7_000), 7_003);
        assert_eq!(b.next(8_000), 8_000);
        assert_eq!(a.last(), 8_000, "one stream");
    }

    /// S3, debug builds: a signer that already drew cannot join a stream
    /// (the boot's order is wrong).
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "before its first signed action")]
    fn a_signer_that_drew_cannot_join_late() {
        let mut a = NonceSource::own();
        let _ = a.next(7_000);
        a.share(Arc::new(SharedNonce::new()));
    }

    /// S3: one stream per agent address, made on its first ask.
    #[test]
    fn the_streams_are_one_per_agent_address() {
        let mut r = NonceStreams::new();
        let e3 = [0xE3; 20];
        let x = [0x0A; 20];
        let a = r.stream(&e3).expect("first");
        let b = r.stream(&e3).expect("same agent");
        let c = r.stream(&x).expect("another agent");
        assert!(Arc::ptr_eq(&a, &b), "one agent, one stream");
        assert!(!Arc::ptr_eq(&a, &c), "two agents, two streams");
        assert_eq!((r.agents(), r.signers()), (2, 3));
        assert!(r.stream(&[1; 20]).is_some() && r.stream(&[2; 20]).is_some());
        assert!(r.stream(&[3; 20]).is_none(), "past MAX_AGENTS");
        assert!(r.stream(&e3).is_some(), "a known agent still resolves");
    }
}
