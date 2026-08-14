//! # ingress-rss
//!
//! RSS news poller. Latency class: `Slow`. Polls a fixed list of feeds
//! at a configurable cadence and emits `Signal`s whose payload points
//! into a preallocated news-payload ring.
//!
//! Phase 0 shipped a one-shot [`first_item`] extractor.
//!
//! Phase 1a adds:
//! * [`ItemIter`] — non-allocating iterator over all `<item>` blocks in
//!   a feed. Holds `(&[u8], cursor)` only.
//! * [`extract_cdata`] — strip a `<![CDATA[ ... ]]>` wrapper (returns
//!   the inner range; no copy).
//! * [`fnv1a_64`] — FNV-1a 64-bit hash for dedupe keys.
//! * [`SeenRing`] — fixed-capacity, cache-aligned ring of recent
//!   link-hash keys, with O(N) `contains` and O(1) `insert`. N is a
//!   const generic; typical values are 512–2048.
//!
//! Phase 1c adds the [`poller`] module — a periodic HTTPS-GET poller on
//! top of [`core_net::Transport`] + [`core_net::http1`]. Steady-state
//! body-parse path is zero-alloc; the per-fetch TLS/TCP connect is
//! allowed to allocate (rustls handshake) since it runs once per
//! poll-interval, not per tick.

#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc,
    clippy::undocumented_unsafe_blocks
)]

pub mod poller;

pub use poller::{
    drive_one_fetch, parse_body_into_signals, run, FeedCfg, FeedSchedule, FetchDriver, FetchState,
    StopFlag, DEFAULT_SIGNAL_RING_CAP, FETCH_BUF_SIZE, REQUEST_BUF_SIZE, USER_AGENT,
};

// ---------------------------------------------------------------
// FeedItem + item extraction
// ---------------------------------------------------------------

/// A minimal RSS item — pointers into the source buffer only, never
/// copied.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct FeedItem<'a> {
    /// Byte range of the `<title>...</title>` text (CDATA-unwrapped).
    pub title: &'a [u8],
    /// Byte range of the `<link>...</link>` text (CDATA-unwrapped).
    pub link: &'a [u8],
}

/// Extract the first `<item>...</item>` block's `<title>` and `<link>`.
///
/// Retained for Phase 0 callers. New callers should use [`feed_items`]
/// which supports multi-item feeds.
pub fn first_item(buf: &[u8]) -> Option<FeedItem<'_>> {
    feed_items(buf).next()
}

/// Non-allocating iterator over every `<item>...</item>` block. Keeps
/// a single cursor into `buf`; never allocates.
pub struct ItemIter<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ItemIter<'a> {
    /// Construct.
    #[inline]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
}

/// Build an [`ItemIter`] for `buf`.
#[inline]
pub fn feed_items(buf: &[u8]) -> ItemIter<'_> {
    ItemIter::new(buf)
}

impl<'a> Iterator for ItemIter<'a> {
    type Item = FeedItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // Locate the next opening `<item` (handles `<item>` and `<item ...>`).
        let rel_open = memchr::memmem::find(&self.buf[self.pos..], b"<item")?;
        let after_open_tag = self.pos + rel_open;
        // Skip past the rest of the opening tag to its `>`.
        let tag_end = self.pos
            + rel_open
            + b"<item".len()
            + memchr::memchr(b'>', &self.buf[after_open_tag + b"<item".len()..])?;
        let item_start = tag_end + 1;

        let rel_close = memchr::memmem::find(&self.buf[item_start..], b"</item>")?;
        let item_end = item_start + rel_close;
        // Advance cursor past the closing tag so next() moves on.
        self.pos = item_end + b"</item>".len();

        let item = &self.buf[item_start..item_end];
        let title_raw = between_tags(item, b"<title>", b"</title>")?;
        let link_raw = between_tags(item, b"<link>", b"</link>")?;

        Some(FeedItem {
            title: extract_cdata(title_raw),
            link: extract_cdata(link_raw),
        })
    }
}

fn between_tags<'a>(buf: &'a [u8], open: &[u8], close: &[u8]) -> Option<&'a [u8]> {
    let s = memchr::memmem::find(buf, open)? + open.len();
    let e = s + memchr::memmem::find(&buf[s..], close)?;
    Some(&buf[s..e])
}

/// Strip a `<![CDATA[ ... ]]>` wrapper if present, returning the inner
/// byte slice. If no wrapper is present, returns `inner` unchanged.
///
/// Zero-copy — returns a subslice of the input.
#[inline]
pub fn extract_cdata(inner: &[u8]) -> &[u8] {
    const OPEN: &[u8] = b"<![CDATA[";
    const CLOSE: &[u8] = b"]]>";
    // Trim leading/trailing ASCII whitespace without allocating.
    let mut s = 0usize;
    let mut e = inner.len();
    while s < e && (inner[s] == b' ' || inner[s] == b'\t' || inner[s] == b'\n' || inner[s] == b'\r')
    {
        s += 1;
    }
    while e > s
        && (inner[e - 1] == b' '
            || inner[e - 1] == b'\t'
            || inner[e - 1] == b'\n'
            || inner[e - 1] == b'\r')
    {
        e -= 1;
    }
    let trimmed = &inner[s..e];
    if trimmed.starts_with(OPEN) && trimmed.ends_with(CLOSE) {
        &trimmed[OPEN.len()..trimmed.len() - CLOSE.len()]
    } else {
        trimmed
    }
}

// ---------------------------------------------------------------
// FNV-1a 64-bit
// ---------------------------------------------------------------

const FNV_OFFSET_64: u64 = 0xCBF2_9CE4_8422_2325;
const FNV_PRIME_64: u64 = 0x0000_0100_0000_01B3;

/// FNV-1a 64-bit hash. Branchless inner loop; zero-alloc.
///
/// Used as the dedupe key for RSS links — collision resistance isn't
/// critical because the `SeenRing` is a recency window, not a cache.
#[inline]
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET_64;
    let mut i = 0usize;
    while i < bytes.len() {
        h ^= bytes[i] as u64;
        h = h.wrapping_mul(FNV_PRIME_64);
        i += 1;
    }
    h
}

// ---------------------------------------------------------------
// SeenRing — recency dedupe window
// ---------------------------------------------------------------

/// Fixed-capacity, cache-aligned ring of recent link-hash keys.
///
/// * `insert` is O(1) (overwrites the oldest slot).
/// * `contains` is O(N), but N is typically small (≤ 2048) and the
///   inner loop is a tight u64 scan that fits in L1.
/// * No allocation after construction.
#[repr(C, align(64))]
pub struct SeenRing<const N: usize> {
    slots: [u64; N],
    next: usize,
    /// Number of valid slots filled since boot, capped at N.
    filled: usize,
}

impl<const N: usize> SeenRing<N> {
    /// Construct. `N` must be > 0; enforced by `const _` check.
    #[inline]
    pub const fn new() -> Self {
        // Ensure N > 0 at compile time.
        assert!(N > 0, "SeenRing capacity must be > 0");
        Self {
            slots: [0u64; N],
            next: 0,
            filled: 0,
        }
    }

    /// True if `key` has been inserted recently. O(N) scan.
    #[inline]
    pub fn contains(&self, key: u64) -> bool {
        let end = self.filled;
        let mut i = 0usize;
        while i < end {
            if self.slots[i] == key {
                return true;
            }
            i += 1;
        }
        false
    }

    /// Insert `key`. Overwrites the oldest slot when the ring is full.
    /// Returns `true` iff the key is new (was not already present).
    #[inline]
    pub fn insert(&mut self, key: u64) -> bool {
        if self.contains(key) {
            return false;
        }
        self.slots[self.next] = key;
        self.next = (self.next + 1) % N;
        if self.filled < N {
            self.filled += 1;
        }
        true
    }

    /// Current fill count (0..=N).
    #[inline]
    pub const fn len(&self) -> usize {
        self.filled
    }

    /// True iff no keys have been inserted yet.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.filled == 0
    }

    /// Capacity (== `N`).
    #[inline]
    pub const fn capacity(&self) -> usize {
        N
    }
}

impl<const N: usize> Default for SeenRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = br#"<rss><channel>
      <item>
        <title>Fed pauses rate hike</title>
        <link>https://example.com/a</link>
      </item>
      <item>
        <title>Later item</title>
        <link>https://example.com/b</link>
      </item>
    </channel></rss>"#;

    #[test]
    fn first_item_returns_the_first_item() {
        let i = first_item(SAMPLE).unwrap();
        assert_eq!(i.title, b"Fed pauses rate hike");
        assert_eq!(i.link, b"https://example.com/a");
    }

    #[test]
    fn first_item_none_on_empty_feed() {
        assert_eq!(first_item(b"<rss/>"), None);
    }

    #[test]
    fn feed_items_walks_all_items() {
        let items: Vec<FeedItem<'_>> = feed_items(SAMPLE).collect();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].link, b"https://example.com/a");
        assert_eq!(items[1].link, b"https://example.com/b");
    }

    #[test]
    fn feed_items_handles_item_with_attributes() {
        let b = br#"<rss>
            <item rdf:about="foo">
              <title>T1</title>
              <link>L1</link>
            </item>
        </rss>"#;
        let v: Vec<FeedItem<'_>> = feed_items(b).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].title, b"T1");
    }

    #[test]
    fn extract_cdata_strips_wrapper() {
        let inner = b"  <![CDATA[Hello, world]]>\n";
        assert_eq!(extract_cdata(inner), b"Hello, world");
    }

    #[test]
    fn extract_cdata_passes_through_non_cdata() {
        let inner = b"  plain text \n";
        assert_eq!(extract_cdata(inner), b"plain text");
    }

    #[test]
    fn feed_items_unwraps_cdata_in_title_and_link() {
        let b = br#"<rss>
            <item>
              <title><![CDATA[Foo & bar]]></title>
              <link><![CDATA[https://example.com/?q=a&b=c]]></link>
            </item>
        </rss>"#;
        let v: Vec<FeedItem<'_>> = feed_items(b).collect();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].title, b"Foo & bar");
        assert_eq!(v[0].link, b"https://example.com/?q=a&b=c");
    }

    // ---- FNV ----

    #[test]
    fn fnv1a_64_known_vectors() {
        // Classic test vector: FNV-1a("" ) == offset basis.
        assert_eq!(fnv1a_64(b""), FNV_OFFSET_64);
        // Deterministic: same input → same hash.
        assert_eq!(fnv1a_64(b"hello"), fnv1a_64(b"hello"));
        // Different inputs differ.
        assert_ne!(fnv1a_64(b"abc"), fnv1a_64(b"abd"));
    }

    // ---- SeenRing ----

    #[test]
    fn seen_ring_inserts_and_checks() {
        let mut r: SeenRing<4> = SeenRing::new();
        assert!(r.is_empty());
        assert!(r.insert(1));
        assert!(r.insert(2));
        assert!(!r.insert(1)); // already present
        assert!(r.contains(1));
        assert!(r.contains(2));
        assert!(!r.contains(99));
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn seen_ring_wraps_on_overflow() {
        let mut r: SeenRing<3> = SeenRing::new();
        r.insert(10);
        r.insert(20);
        r.insert(30);
        // Ring is full. Next insert evicts slot 0 (which held 10).
        r.insert(40);
        assert_eq!(r.len(), 3);
        assert!(r.contains(20));
        assert!(r.contains(30));
        assert!(r.contains(40));
        // Note: "contains" only looks at filled slots; 10 was overwritten.
        assert!(!r.contains(10));
    }

    #[test]
    fn seen_ring_capacity_reports_const_generic() {
        let r: SeenRing<8> = SeenRing::new();
        assert_eq!(r.capacity(), 8);
    }

    #[test]
    fn seen_ring_is_cache_aligned() {
        assert_eq!(::core::mem::align_of::<SeenRing<4>>(), 64);
    }
}

// ---------------------------------------------------------------
// Property tests
// ---------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn fnv1a_is_deterministic(bytes in proptest::collection::vec(any::<u8>(), 0..=200)) {
            prop_assert_eq!(fnv1a_64(&bytes), fnv1a_64(&bytes));
        }

        #[test]
        fn fnv1a_prefix_changes_hash(
            bytes in proptest::collection::vec(any::<u8>(), 0..=200),
            extra in any::<u8>(),
        ) {
            let mut longer = bytes.clone();
            longer.push(extra);
            // Distinct-length inputs produce different hashes with overwhelming probability.
            // Guard: only assert non-equality when there's actually content in the prefix.
            if !bytes.is_empty() {
                prop_assert_ne!(fnv1a_64(&bytes), fnv1a_64(&longer));
            }
        }

        #[test]
        fn item_iter_never_panics_on_arbitrary_input(buf in proptest::collection::vec(any::<u8>(), 0..=400)) {
            for _ in feed_items(&buf) {
                // Just exercise the iterator.
            }
        }

        #[test]
        fn seen_ring_matches_fifo_dedupe_model(
            keys in proptest::collection::vec(0u64..1000u64, 0..=50),
        ) {
            // Reference model: FIFO dedupe ring. First occurrence is
            // what lives in a slot; repeats are no-ops. When ring is
            // full, the oldest slot is overwritten by the next new
            // key.
            let mut r: SeenRing<16> = SeenRing::new();
            let mut model: Vec<u64> = Vec::new();
            for &k in &keys {
                r.insert(k);
                if !model.contains(&k) {
                    if model.len() == 16 {
                        model.remove(0);
                    }
                    model.push(k);
                }
            }
            // Ring must report the same membership as the model.
            prop_assert_eq!(r.len(), model.len());
            for &k in &model {
                prop_assert!(r.contains(k), "expected {} in ring", k);
            }
        }
    }
}
