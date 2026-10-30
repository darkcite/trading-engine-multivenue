// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! CHD minimal perfect hash over FNV-1a 64 (impl-00 §4.7). The slot function mixes before
//! masking (the v1 XOR-then-mask slot could not separate two keys of one bucket). `mix`,
//! `bucket` and `slot` are the same arithmetic as `src/lookup.rs`, which is normative.

/// The CHD displacement multiplier (golden ratio, 64-bit) — `src/lookup.rs` `DISP_MUL`.
pub const DISP_MUL: u64 = 0x9E37_79B9_7F4A_7C15;

/// The fixed seed list; the first that builds wins.
pub const SEEDS: [u64; 16] = [
    0x0000_0000_0000_0000,
    0x5851_F42D_4C95_7F2D,
    0x1405_7B7E_F767_814F,
    0x9E37_79B9_7F4A_7C15,
    0xBF58_476D_1CE4_E5B9,
    0x94D0_49BB_1331_11EB,
    0x2545_F491_4F6C_DD1D,
    0xD6E8_FEB8_6659_FD93,
    0xA076_1D64_78BD_642F,
    0xE703_7ED1_A0B4_28DB,
    0x8EBC_6AF0_9C88_C6E3,
    0x5896_5AB8_4A7B_E0B7,
    0x1D8E_4E27_C47D_124F,
    0xFF51_AFD7_ED55_8CCD,
    0xC4CE_B9FE_1A85_EC53,
    0x9FB2_1C65_1E98_DF25,
];

/// Largest displacement tried per bucket before moving to the next seed.
pub const MAX_DISP: u32 = 1 << 20;

/// A built table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mph {
    /// The seed XORed into every key hash.
    pub seed: u64,
    /// Bucket count `m` (≥ 2).
    pub buckets: u32,
    /// Displacement per bucket.
    pub disp: Vec<u32>,
    /// Slot → key index, `u16::MAX` = empty; a power of two long (≥ 2).
    pub slots: Vec<u16>,
}

/// splitmix64's finaliser.
#[inline]
pub fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// The bucket of seeded hash `h`.
#[inline]
pub fn bucket(h: u64, m: u32) -> usize {
    ((mix(h) >> 32) as u32 % m) as usize
}

/// The slot of seeded hash `h` under displacement `d` in a table of `t` (a power of two).
#[inline]
pub fn slot(h: u64, d: u32, t: usize) -> usize {
    (mix(h ^ (d as u64 + 1).wrapping_mul(DISP_MUL)) as usize) & (t - 1)
}

impl Mph {
    /// The key index the table maps FNV-1a hash `h0` to, if its slot is occupied.
    pub fn index_of(&self, h0: u64) -> Option<usize> {
        let h = h0 ^ self.seed;
        let b = bucket(h, self.buckets);
        let s = self.slots[slot(h, self.disp[b], self.slots.len())];
        if s == u16::MAX {
            None
        } else {
            Some(s as usize)
        }
    }
}

/// Build the table for `keys` (distinct byte strings; at most 65 535). `Err` carries the R14 or
/// R13 message.
pub fn build(keys: &[&[u8]]) -> Result<Mph, String> {
    let n = keys.len();
    if n > u16::MAX as usize {
        return Err(format!("R13: {n} keys exceed 65 535"));
    }
    let t = (n + n / 4).next_power_of_two().max(2);
    let m = n.div_ceil(5).max(2);
    let mut hs: Vec<u64> = Vec::with_capacity(n);
    for k in keys {
        hs.push(pine_ir::fnv1a64(k));
    }
    'seed: for &seed in SEEDS.iter() {
        let mut groups: Vec<Vec<usize>> = vec![Vec::new(); m];
        for (i, &h0) in hs.iter().enumerate() {
            groups[bucket(h0 ^ seed, m as u32)].push(i);
        }
        let mut order: Vec<usize> = (0..m).collect();
        order.sort_by(|&a, &b| groups[b].len().cmp(&groups[a].len()).then(a.cmp(&b)));
        let mut slots = vec![u16::MAX; t];
        let mut disp = vec![0u32; m];
        let mut cand: Vec<usize> = Vec::new();
        for &b in &order {
            if groups[b].is_empty() {
                continue;
            }
            let mut d: u32 = 0;
            loop {
                cand.clear();
                let mut ok = true;
                for &i in &groups[b] {
                    let s = slot(hs[i] ^ seed, d, t);
                    if slots[s] != u16::MAX || cand.contains(&s) {
                        ok = false;
                        break;
                    }
                    cand.push(s);
                }
                if ok {
                    for (j, &i) in groups[b].iter().enumerate() {
                        slots[cand[j]] = i as u16;
                    }
                    disp[b] = d;
                    break;
                }
                d += 1;
                if d > MAX_DISP {
                    continue 'seed;
                }
            }
        }
        let out = Mph {
            seed,
            buckets: m as u32,
            disp,
            slots,
        };
        for (i, &h0) in hs.iter().enumerate() {
            if out.index_of(h0) != Some(i) {
                return Err(format!("R14: MPH self-check failed for key {i}, n={n}"));
            }
        }
        return Ok(out);
    }
    Err(format!(
        "R14: MPH failed after {} seeds, n={n}",
        SEEDS.len()
    ))
}
