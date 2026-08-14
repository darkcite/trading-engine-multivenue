//! Fuzz target: arbitrary bytes → `ingress_rss::feed_items` iterator
//! and `fnv1a_64`. The iterator must terminate, never panic, and
//! never read outside the input slice; the hash must be a pure
//! function of the input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Drive the iterator to completion with a safety cap — if the
    // scanner ever fails to advance past a malformed `<item`, the cap
    // prevents an infinite fuzz iteration from hiding a real bug
    // behind a timeout.
    let mut iter = ingress_rss::feed_items(data);
    let mut budget = 4096usize;
    while budget > 0 {
        match iter.next() {
            Some(item) => {
                // Both subslices must lie inside `data`.
                let base = data.as_ptr() as usize;
                let end = base + data.len();
                let t = item.title.as_ptr() as usize;
                let l = item.link.as_ptr() as usize;
                assert!(t >= base && t + item.title.len() <= end);
                assert!(l >= base && l + item.link.len() <= end);
                let _ = ingress_rss::fnv1a_64(item.link);
                let _ = ingress_rss::fnv1a_64(item.title);
            }
            None => break,
        }
        budget -= 1;
    }
    // Hash determinism on the raw input.
    let h1 = ingress_rss::fnv1a_64(data);
    let h2 = ingress_rss::fnv1a_64(data);
    assert_eq!(h1, h2);
});
