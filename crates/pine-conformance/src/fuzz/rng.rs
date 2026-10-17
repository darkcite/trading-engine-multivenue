// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The generator's own PRNG (impl-06 §4.8.5): `Xoshiro256StarStar` seeded by `SplitMix64(seed)`.
//! Owned by one `generate` call — no global state, never clock-seeded (P2, P5).

/// SplitMix64 (Steele, Lea, Flood 2014) — expands the 64-bit seed into the xoshiro state.
#[derive(Debug, Clone, Copy)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A stream from `seed`.
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next output.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// xoshiro256** 1.0 (Blackman, Vigna 2018).
#[derive(Debug, Clone, Copy)]
pub struct Xoshiro256StarStar {
    s: [u64; 4],
}

impl Xoshiro256StarStar {
    /// Seeded by four `SplitMix64(seed)` outputs (the reference seeding; never all-zero).
    pub fn seed_from(seed: u64) -> Self {
        let mut sm = SplitMix64::new(seed);
        Self {
            s: [sm.next_u64(), sm.next_u64(), sm.next_u64(), sm.next_u64()],
        }
    }

    /// The next output.
    pub fn next_u64(&mut self) -> u64 {
        let r = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }

    /// Uniform in `0..n` (`n > 0`) by Lemire's multiply-shift; the tiny bias is irrelevant here
    /// and the mapping is fixed for a `GRAMMAR_VERSION`.
    pub fn below(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        ((u64::from(self.next_u64() as u32) * u64::from(n)) >> 32) as u32
    }

    /// Uniform in `lo..=hi`.
    pub fn range_i32(&mut self, lo: i32, hi: i32) -> i32 {
        debug_assert!(lo <= hi);
        lo + self.below((hi - lo + 1) as u32) as i32
    }

    /// True with probability `permille / 1000`.
    pub fn permille(&mut self, permille: u16) -> bool {
        self.below(1000) < u32::from(permille)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference vectors: SplitMix64(0) and xoshiro256** from state {1, 2, 3, 4}.
    #[test]
    fn rng_matches_reference_vectors() {
        let mut sm = SplitMix64::new(0);
        assert_eq!(sm.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(sm.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        let mut x = Xoshiro256StarStar { s: [1, 2, 3, 4] };
        assert_eq!(x.next_u64(), 11520);
        assert_eq!(x.next_u64(), 0);
        assert_eq!(x.next_u64(), 1_509_978_240);
    }
}
