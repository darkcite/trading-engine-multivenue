# PMLR golden fixtures (8f item 10)

Byte-golden PMLR files consumed by `tests/test_pmlr.py` and
`tests/test_features.py`. Per design §11, the bytes were produced by the
**Rust `core-io::PmlrWriter`** (the real engine writer), so the Python
reader (`claude_worker.pmlr`) is pinned to the Rust writer and the two
sides cannot drift silently.

Generator: `generator.rs.txt` — a one-shot scratch crate (NOT part of the
workspace; the unused-code rule keeps throwaway Rust out of the tree).
To regenerate:

```sh
mkdir -p /tmp/pmlr-fixture-gen/src
cp generator.rs.txt /tmp/pmlr-fixture-gen/src/main.rs
# Cargo.toml: package pmlr-fixture-gen, edition 2021, [workspace],
# path-deps core-io + core-types into this repo's crates/.
cd /tmp/pmlr-fixture-gen && cargo run --release -- /tmp/pmlr-fixtures-out
cp /tmp/pmlr-fixtures-out/*.pmlr <this dir>/
```

Files (epoch_ns = 1_755_216_000_000_000_000 in every header):

- `ticks_v2.pmlr` — SlotKind::Tick, version 2, 4 records: two Polymarket
  ticks on sym 7 (mids 500_000 → 510_000) and one tick each on a HIP-4
  Hyperliquid yes/no coin pair, syms `make_symbol_id(Hyperliquid, 10810)`
  = 67119674 (mid 610_000) and 67119675 (mid 390_000) — the two mids sum
  to 1e6, matching the merged-book identity.
- `fills_v2.pmlr` — SlotKind::Fill, version 2, 5 records telling an exact
  P&L story for `test_features.py`: sym 7 buys 20 @ 480_000 and
  10 @ 500_000, sells 15 @ 520_000 (realized exactly $0.50; 15 left at
  basis 7.3e12); plus a HIP-4 pair position (yes: buy 8 @ 600_000,
  no: buy 5 @ 390_000 → |yes−no| net 3).
- `ticks_v1.pmlr` — the v2 writer's tick bytes with the header version
  field patched to 1 and bytes 48..64 of every slot (venue byte + tail
  padding — v2-only fields) filled with `0xAA`, modeling v1's undefined
  padding. No v1 writer exists anymore; this mirrors the crafted-v1
  pattern used by `core-io`'s own reader tests.

Torn-tail cases are not checked in: tests create them by truncating
copies of these files (tearing is a byte-level operation).
