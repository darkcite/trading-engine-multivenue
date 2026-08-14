//! # Queued dispatcher
//!
//! Decouples the engine's hot path from network I/O. The strategy
//! calls `QueuedDispatcher::submit(&order)`, which is a single SPSC
//! ring push — never blocks. A dedicated worker thread pops from
//! the ring and calls the real inner [`OrderDispatch::submit`].
//!
//! ## Why
//!
//! `LiveDispatcher::submit_inline` blocks for one TCP+TLS POST
//! round-trip (~50–100 ms over WAN). With an inline dispatcher
//! that means the entire engine loop stalls per emitted order —
//! every other strategy callback, every other tick drain, all
//! waiting on a network response that has nothing to do with them.
//!
//! Decoupling via this ring lets the engine fire-and-forget:
//! orders go in a queue, the engine moves on, and the worker
//! drains the queue under its own thread budget.
//!
//! ## Layout
//!
//! ```text
//!   engine thread             worker thread
//!   ─────────────             ─────────────
//!   QueuedDispatcher    ──►   DispatcherWorker<D>
//!     ├ producer  ────────────► consumer (SPSC ring)
//!     └ stats (read)  ◄────────  stats (write, Mutex)
//! ```
//!
//! ## Stats coherence
//!
//! The worker mirrors the inner dispatcher's [`DispatchStats`] into
//! a shared `Mutex<DispatchStats>` after each submit. The engine
//! reads via [`QueuedDispatcher::stats`] — bounded contention
//! because the strategy cooldown caps submit rate at ~10 Hz max in
//! v1, and reads only happen on the 5 s publish tick.
//!
//! ## Backpressure
//!
//! If the worker can't keep up, the ring fills and
//! [`QueuedDispatcher::submit`] returns
//! [`DispatchError::QueueFull`]. The strategy already treats
//! `RingFull` as a transient ring-full drop — same code path as
//! the inline dispatcher's QueueFull bucket.

use core_ring::{Consumer, Producer, Ring};
use core_types::{Fill, Order};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::{DispatchError, DispatchStats, DispatchStatsAtomic, OrderDispatch};

/// SPSC order ring capacity. Power of two; sized to absorb a burst
/// of cross-arb fan-out (up to `M` legs per group × number of
/// concurrent groups) while the worker drains.
pub const ORDER_RING_CAP: usize = 1024;

/// Backoff between empty pops in the worker. 50 µs balances
/// freshness (worst-case latency added) against CPU burn (worker
/// runs on its own core). Tune in Phase 7 once we have real WAN
/// latency numbers.
const WORKER_IDLE_BACKOFF: Duration = Duration::from_micros(50);

/// Engine-side handle. Holds the SPSC producer + a snapshot view
/// of the inner dispatcher's [`DispatchStats`], mirrored
/// lock-free via [`DispatchStatsAtomic`]. `submit` never blocks;
/// `stats` does 10 relaxed atomic loads (~10 ns total).
pub struct QueuedDispatcher {
    producer: Producer<Order, ORDER_RING_CAP>,
    stats: Arc<DispatchStatsAtomic>,
}

/// Worker-side state. Owns the SPSC consumer + the inner
/// `OrderDispatch`. Spawned on its own thread by the cli.
pub struct DispatcherWorker<D: OrderDispatch + Send + 'static> {
    consumer: Consumer<Order, ORDER_RING_CAP>,
    inner: D,
    stats: Arc<DispatchStatsAtomic>,
}

impl QueuedDispatcher {
    /// Construct a producer/worker pair around `inner`. The caller
    /// is responsible for spawning the worker's [`run`] method on
    /// a dedicated thread.
    ///
    /// Boot-only; allocates the ring + the atomic stats mirror.
    pub fn new<D: OrderDispatch + Send + 'static>(inner: D) -> (Self, DispatcherWorker<D>) {
        let ring: Arc<Ring<Order, ORDER_RING_CAP>> = Ring::new();
        let (producer, consumer) = ring.split();
        let stats = Arc::new(DispatchStatsAtomic::default());
        stats.store_from(&inner.stats());
        let worker = DispatcherWorker {
            consumer,
            inner,
            stats: stats.clone(),
        };
        let me = Self { producer, stats };
        (me, worker)
    }
}

impl OrderDispatch for QueuedDispatcher {
    /// Push the order into the SPSC ring. Returns
    /// [`DispatchError::QueueFull`] if the worker can't keep up.
    /// Never touches the network; zero-alloc.
    #[inline]
    fn submit(&mut self, order: &Order) -> Result<(), DispatchError> {
        self.producer
            .try_push(*order)
            .map_err(|_| DispatchError::QueueFull)
    }

    /// Fills flow through a separate path (engine fill ring); the
    /// queued dispatcher returns nothing here.
    #[inline]
    fn try_next_fill(&mut self) -> Option<Fill> {
        None
    }

    /// Lock-free snapshot of the inner dispatcher's counters.
    /// Per-field `Relaxed` loads — coherent per-field, may show a
    /// one-off cross-field skew (e.g. `accepted+rejected` may not
    /// equal `submitted` for a single tick). Acceptable for the
    /// 5 s metrics tick.
    #[inline]
    fn stats(&self) -> DispatchStats {
        self.stats.snapshot()
    }
}

impl<D: OrderDispatch + Send + 'static> DispatcherWorker<D> {
    /// Drain the ring on the calling thread. Returns when `stop`
    /// flips to true. This is the thread entry point — call from
    /// `std::thread::spawn`.
    ///
    /// Hot loop: try_pop → inner.submit → mirror stats via
    /// `DispatchStatsAtomic::store_from`. No lock, no contention.
    /// On empty, sleep [`WORKER_IDLE_BACKOFF`] to avoid CPU burn.
    pub fn run(mut self, stop: &AtomicBool) {
        while !stop.load(Ordering::Acquire) {
            match self.consumer.try_pop() {
                Some(order) => {
                    let _ = self.inner.submit(&order);
                    self.stats.store_from(&self.inner.stats());
                }
                None => {
                    thread::sleep(WORKER_IDLE_BACKOFF);
                }
            }
        }
        // Drain anything left in the ring on shutdown so an
        // operator-visible "5 orders still queued at SIGINT"
        // doesn't silently disappear into the void.
        while let Some(order) = self.consumer.try_pop() {
            let _ = self.inner.submit(&order);
        }
        self.stats.store_from(&self.inner.stats());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PaperDispatcher;
    use core_types::{Price, Qty, Side, VenueId};
    use std::time::Instant;

    fn mk_order(oid: u64) -> Order {
        Order::new(
            0,
            VenueId::Polymarket,
            1,
            Side::Bid,
            0,
            Price::from_raw(500_000),
            Qty::from_raw(1_000_000),
            oid,
        )
    }

    /// End-to-end: producer pushes 100 orders, worker drains them
    /// into a PaperDispatcher, stats reflect the work.
    #[test]
    fn worker_drains_ring_and_mirrors_stats() {
        let (mut q, worker) = QueuedDispatcher::new(PaperDispatcher::new());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let handle = thread::spawn(move || {
            worker.run(&stop_w);
        });

        for i in 0..100u64 {
            q.submit(&mk_order(i)).expect("ring should not be full");
        }

        // Wait for worker to drain + mirror stats. 1 ms upper
        // bound — paper submit is nanoseconds.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let snap = q.stats();
            if snap.accepted == 100 {
                break;
            }
            if Instant::now() > deadline {
                panic!("worker failed to drain; stats={snap:?}");
            }
            thread::sleep(Duration::from_micros(100));
        }

        stop.store(true, Ordering::Release);
        handle.join().expect("worker thread joined");
        assert_eq!(q.stats().accepted, 100);
        assert_eq!(q.stats().rejected, 0);
    }

    /// Filling the ring beyond capacity yields QueueFull, not a
    /// panic, and the worker eventually catches up.
    #[test]
    fn ring_full_surfaces_queue_full() {
        // Worker is intentionally NOT started — every push goes
        // into the ring, none are drained. The (ORDER_RING_CAP +
        // 1)th push must report QueueFull.
        let (mut q, _worker) = QueuedDispatcher::new(PaperDispatcher::new());
        let mut accepted = 0;
        let mut rejected = 0;
        for i in 0..(ORDER_RING_CAP + 16) as u64 {
            match q.submit(&mk_order(i)) {
                Ok(()) => accepted += 1,
                Err(DispatchError::QueueFull) => rejected += 1,
                Err(e) => panic!("unexpected err: {e:?}"),
            }
        }
        assert!(accepted >= ORDER_RING_CAP - 1, "ring should fit ≥ N-1");
        assert!(rejected >= 1, "the over-fill must surface QueueFull");
    }

    /// `stats()` after the worker stops still returns the last
    /// mirrored snapshot — the cli reads it for final reporting.
    #[test]
    fn stats_survives_worker_join() {
        let (mut q, worker) = QueuedDispatcher::new(PaperDispatcher::new());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let h = thread::spawn(move || worker.run(&stop_w));

        for i in 0..5u64 {
            q.submit(&mk_order(i)).unwrap();
        }
        // Wait for the worker to drain.
        let deadline = Instant::now() + Duration::from_secs(1);
        while q.stats().accepted < 5 && Instant::now() < deadline {
            thread::sleep(Duration::from_micros(100));
        }

        stop.store(true, Ordering::Release);
        h.join().unwrap();
        // After join, the snapshot is still readable.
        assert_eq!(q.stats().accepted, 5);
    }
}
