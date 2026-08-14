//! # IoBuf — fixed-capacity rx/tx byte window
//!
//! Lifted out of the per-ingress run loops in Phase 8a (it existed as
//! three identical private copies in ingress-rpc / -binance /
//! -polymarket). One allocation at construction, then a cursor pair
//! (`head`, `tail`) forever:
//!
//! * `free_mut()` → writable tail region (compacts lazily when the
//!   tail hits the end and dead bytes exist at the front),
//! * `advance(n)` → commit `n` bytes just written into `free_mut()`,
//! * `filled()` / `filled_mut()` → readable window,
//! * `consume(n)` → release `n` parsed bytes from the front.
//!
//! `consume` is O(1); the residual `copy_within` only runs inside
//! `free_mut` when strictly needed, which amortizes to zero on the
//! typical parse-everything-you-read cycle.

/// Fixed-size byte window with a cursor pair. One heap allocation at
/// construction; zero-alloc thereafter.
pub struct IoBuf {
    data: Box<[u8]>,
    head: usize,
    tail: usize,
}

impl IoBuf {
    /// Allocate a window of `cap` bytes. Boot-time only.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            data: vec![0u8; cap].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    /// Readable region (bytes received/queued, not yet consumed).
    #[inline]
    pub fn filled(&self) -> &[u8] {
        &self.data[self.head..self.tail]
    }

    /// Number of readable bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.tail - self.head
    }

    /// Whether the readable region is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tail == self.head
    }

    /// Mutable view of the readable region (in-place unmasking).
    #[inline]
    pub fn filled_mut(&mut self) -> &mut [u8] {
        &mut self.data[self.head..self.tail]
    }

    /// Writable tail region. Compacts first when the tail is pinned
    /// at capacity and dead bytes exist at the front — the single
    /// documented copy in this type.
    #[inline]
    pub fn free_mut(&mut self) -> &mut [u8] {
        if self.tail == self.data.len() && self.head > 0 {
            self.data.copy_within(self.head..self.tail, 0);
            self.tail -= self.head;
            self.head = 0;
        }
        &mut self.data[self.tail..]
    }

    /// Commit `n` bytes written into [`Self::free_mut`].
    #[inline]
    pub fn advance(&mut self, n: usize) {
        debug_assert!(self.tail + n <= self.data.len());
        self.tail += n;
    }

    /// Release `n` bytes from the front of the readable region.
    #[inline]
    pub fn consume(&mut self, n: usize) {
        debug_assert!(self.head + n <= self.tail);
        self.head += n;
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        }
    }

    /// Drop everything (reconnect reset). O(1).
    #[inline]
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_consume_cycle() {
        let mut b = IoBuf::with_capacity(8);
        let free = b.free_mut();
        free[..3].copy_from_slice(b"abc");
        b.advance(3);
        assert_eq!(b.filled(), b"abc");
        b.consume(2);
        assert_eq!(b.filled(), b"c");
        b.consume(1);
        assert!(b.is_empty());
        // Cursors rewind on empty — full capacity available again.
        assert_eq!(b.free_mut().len(), 8);
    }

    #[test]
    fn free_mut_compacts_when_tail_pinned() {
        let mut b = IoBuf::with_capacity(4);
        b.free_mut()[..4].copy_from_slice(b"wxyz");
        b.advance(4);
        b.consume(2); // head=2, tail=4 (pinned at cap)
        let free = b.free_mut(); // must compact: "yz" to front
        assert_eq!(free.len(), 2);
        free[..1].copy_from_slice(b"!");
        b.advance(1);
        assert_eq!(b.filled(), b"yz!");
    }

    #[test]
    fn clear_resets_without_dealloc() {
        let mut b = IoBuf::with_capacity(4);
        b.free_mut()[..2].copy_from_slice(b"hi");
        b.advance(2);
        b.clear();
        assert!(b.is_empty());
        assert_eq!(b.free_mut().len(), 4);
    }
}
