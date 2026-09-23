// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Test-only: a model node that answers pool reads the way the three
//! families' contracts do (word counts, the V3 bitmap, the Algebra
//! linked list with its `MIN/MAX_TICK` markers), so the snapshotter and
//! the run loop are exercised against the ABI, not against themselves.

use core_amm::{MAX_TICK, MIN_TICK};

use crate::pools::PoolFamily;
use crate::snapshot::ReadKind;

/// One modelled pool.
#[derive(Clone, Debug)]
pub(crate) struct FakePool {
    pub(crate) family: PoolFamily,
    pub(crate) address: [u8; 20],
    pub(crate) sqrt: u128,
    pub(crate) probe_sqrt: u128,
    pub(crate) tick: i32,
    pub(crate) liq: u128,
    pub(crate) fee: u32,
    pub(crate) spacing: i32,
    /// Initialised ticks, ascending: `(tick, gross, net)`.
    pub(crate) ticks: Vec<(i32, u128, i128)>,
}

fn w_u(v: u128) -> String {
    format!("{v:064x}")
}

fn w_i(v: i128) -> String {
    if v < 0 {
        format!("{}{:032x}", "f".repeat(32), v as u128)
    } else {
        format!("{:064x}", v as u128)
    }
}

impl FakePool {
    fn prev_node(&self, t: i32) -> i32 {
        let mut r = MIN_TICK;
        for &(k, _, _) in &self.ticks {
            if k <= t {
                r = k;
            }
        }
        r
    }

    fn next_node(&self, t: i32) -> i32 {
        for &(k, _, _) in &self.ticks {
            if k > t {
                return k;
            }
        }
        MAX_TICK
    }

    /// The `result` hex string (`0x…`) of one read.
    pub(crate) fn answer(&self, kind: ReadKind, arg: i32, historical: bool) -> String {
        let sqrt = if historical {
            self.probe_sqrt
        } else {
            self.sqrt
        };
        let body = match kind {
            ReadKind::Head | ReadKind::ProbeHead => match self.family {
                PoolFamily::UniswapV3 => format!(
                    "{}{}{}",
                    w_u(sqrt),
                    w_i(self.tick as i128),
                    w_u(1).repeat(5)
                ),
                PoolFamily::Slipstream => format!(
                    "{}{}{}",
                    w_u(sqrt),
                    w_i(self.tick as i128),
                    w_u(1).repeat(4)
                ),
                PoolFamily::Algebra => format!(
                    "{}{}{}{}",
                    w_u(sqrt),
                    w_i(self.tick as i128),
                    w_u(self.fee as u128),
                    w_u(0).repeat(3)
                ),
            },
            ReadKind::Liquidity => w_u(self.liq),
            ReadKind::Fee => w_u(self.fee as u128),
            ReadKind::Spacing => w_i(self.spacing as i128),
            ReadKind::Prev => w_i(self.prev_node(self.tick) as i128),
            ReadKind::Next => w_i(self.next_node(self.tick) as i128),
            ReadKind::Bitmap => {
                let mut bits = [0u64; 4];
                for &(t, _, _) in &self.ticks {
                    let c = t.div_euclid(self.spacing);
                    if c >> 8 == arg {
                        let b = (c & 255) as usize;
                        bits[b >> 6] |= 1 << (b & 63);
                    }
                }
                format!(
                    "{:016x}{:016x}{:016x}{:016x}",
                    bits[3], bits[2], bits[1], bits[0]
                )
            }
            ReadKind::Tick | ReadKind::LinkDown | ReadKind::LinkUp => {
                let (gross, net) = self
                    .ticks
                    .iter()
                    .find(|x| x.0 == arg)
                    .map(|x| (x.1, x.2))
                    .unwrap_or((0, 0));
                match self.family {
                    PoolFamily::UniswapV3 => {
                        format!("{}{}{}", w_u(gross), w_i(net), w_u(0).repeat(6))
                    }
                    PoolFamily::Slipstream => {
                        format!("{}{}{}", w_u(gross), w_i(net), w_u(0).repeat(8))
                    }
                    PoolFamily::Algebra => {
                        let prev = if arg == MIN_TICK {
                            MIN_TICK
                        } else {
                            self.prev_node(arg - 1)
                        };
                        let next = if arg == MAX_TICK {
                            MAX_TICK
                        } else {
                            self.next_node(arg)
                        };
                        format!(
                            "{}{}{}{}{}",
                            w_u(gross),
                            w_i(net),
                            w_i(prev as i128),
                            w_i(next as i128),
                            w_u(0).repeat(2)
                        )
                    }
                }
            }
        };
        format!("0x{body}")
    }

    /// A consistent pool: positions `(lower, upper, L)` define every
    /// tick's gross and net and the in-range liquidity.
    pub(crate) fn from_positions(
        family: PoolFamily,
        address: [u8; 20],
        tick: i32,
        spacing: i32,
        fee: u32,
        positions: &[(i32, i32, u128)],
    ) -> Self {
        let mut map: std::collections::BTreeMap<i32, (u128, i128)> =
            std::collections::BTreeMap::new();
        let mut liq = 0u128;
        for &(lo, hi, l) in positions {
            let e = map.entry(lo).or_insert((0, 0));
            e.0 += l;
            e.1 += l as i128;
            let e = map.entry(hi).or_insert((0, 0));
            e.0 += l;
            e.1 -= l as i128;
            if lo <= tick && tick < hi {
                liq += l;
            }
        }
        let (s_lo, _) = core_amm::sqrt_at_tick(tick);
        Self {
            family,
            address,
            sqrt: s_lo,
            probe_sqrt: s_lo + 12_345,
            tick,
            liq,
            fee,
            spacing,
            ticks: map.into_iter().map(|(t, (g, n))| (t, g, n)).collect(),
        }
    }
}
