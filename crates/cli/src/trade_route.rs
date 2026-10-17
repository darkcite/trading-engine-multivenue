// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: boot-only. Allocation and copying here are free.
//!
//! PVM B'1 (impl-Bp §3.5, §4.1 item 3): the pine slot's Trade route —
//! descriptors → per-lane `&'static [SymbolId]`, the per-venue capture
//! table and the `6006` refusal.
//!
//! Repo drift recorded at the sites (impl-Bp was written at `90c9c39`):
//! - pin:Bp-slot-pine — the spec's "slot 7" is `strategy_set::SLOT_PINE`
//!   (8); slot 7 is hcv.
//! - pin:Bp-eight-lanes — `engine::NUM_EVENT_LANES` is 8 (MEXC 6,
//!   Hypercall 7), not 6.
//! - pin:Bp-usdm-q1a — the operator answered Q1 = (a): `binance-usdm`
//!   (dated included) route symbols get their own `@aggTrade` on their
//!   bookTicker socket (B'1 T8), so the namespace captures trades.
//! - pin:Bp-mexc-hypercall-unrouted — MEXC and Hypercall capture Trade
//!   rows today, but B'1 puts the route push in five ingress crates only
//!   (§3.3); a route symbol there is refused like a lane without capture.
//! - pin:Bp-6006-const — `pine_ir` has no named `6006`; the code is built
//!   from its 6000–6999 host band here.

use core_types::{SymbolId, VenueId, EVENT_LANE_TRADE, TRADE_ROUTE_MAX};
use engine::{tick_lane_of, NUM_EVENT_LANES};

/// Whether a descriptor's venue lane carries Trade prints the route can use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeCapture {
    /// Captured with a parsed venue time and pushed by the B'1 route:
    /// binance (spot), binance-usdm (T8), okx, deribit, hyperliquid (perp
    /// coins), bybit, bybit-linear.
    Yes,
    /// The lane subscribes no trade stream for this class, or captures
    /// one the B'1 route does not push: hyperliquid HIP-4 outcome legs,
    /// binance-opt, binance-coinm, mexc, mexc-perp, hypercall,
    /// hypercall-idx, hyperevm.
    NoSubscription,
    /// The venue emits no Trade event at all: polymarket.
    NoEvent,
}

/// `6006` (00-INDEX §3.3): a route symbol whose lane has no Trade capture.
pub const ROUTE_REFUSAL_CODE: pine_ir::DiagCode = pine_ir::DiagCode(6006);

/// The engine tick/event lane (`engine::NUM_EVENT_LANES`) hosting
/// `descriptor`, and whether that lane captures its trades. Namespaces
/// per `core_config::instrument_class`. `None` = not a descriptor any
/// lane hosts.
#[must_use]
pub fn trade_capture_of(descriptor: &str) -> Option<(usize, TradeCapture)> {
    let Some((ns, name)) = descriptor.split_once(':') else {
        // A bare decimal token id is a Polymarket leg.
        return if !descriptor.is_empty() && descriptor.bytes().all(|b| b.is_ascii_digit()) {
            lane(VenueId::Polymarket, TradeCapture::NoEvent)
        } else {
            None
        };
    };
    if name.is_empty() {
        return None;
    }
    match ns {
        "binance" | "binance-usdm" => lane(VenueId::Binance, TradeCapture::Yes),
        "binance-opt" | "binance-coinm" => lane(VenueId::Binance, TradeCapture::NoSubscription),
        "okx" => lane(VenueId::Okx, TradeCapture::Yes),
        "deribit" => lane(VenueId::Deribit, TradeCapture::Yes),
        "hyperliquid" => {
            let outcome =
                name.starts_with('#') || name.starts_with("out:") || name.starts_with("native:");
            let cap = if outcome {
                TradeCapture::NoSubscription
            } else {
                TradeCapture::Yes
            };
            lane(VenueId::Hyperliquid, cap)
        }
        "bybit" | "bybit-linear" => lane(VenueId::Bybit, TradeCapture::Yes),
        "mexc" | "mexc-perp" => lane(VenueId::Mexc, TradeCapture::NoSubscription),
        "hypercall" | "hypercall-idx" => lane(VenueId::Hypercall, TradeCapture::NoSubscription),
        // HyperEVM pool events ride the signal lane — no tick/event lane.
        _ => None,
    }
}

fn lane(venue: VenueId, cap: TradeCapture) -> Option<(usize, TradeCapture)> {
    tick_lane_of(venue).map(|l| (l, cap))
}

/// Per-lane Trade routes, leaked once at boot (≤ `TRADE_ROUTE_MAX` syms
/// each, sorted, unique).
#[derive(Debug, Clone, Copy)]
pub struct TradeRoutes {
    /// Per engine event lane: the syms routed to the pine slot (empty = none).
    pub lanes: [&'static [SymbolId]; NUM_EVENT_LANES],
}

impl TradeRoutes {
    /// No route anywhere: what every boot without the pine slot passes.
    pub const EMPTY: Self = Self {
        lanes: [&[]; NUM_EVENT_LANES],
    };

    /// The route slice the `spawn_*` of `venue`'s event lane receives
    /// (`lanes[lane]`; empty for a venue without a tick/event lane).
    #[must_use]
    pub fn of(&self, venue: VenueId) -> &'static [SymbolId] {
        match tick_lane_of(venue) {
            Some(l) => self.lanes[l],
            None => &[],
        }
    }

    /// `EVENT_LANE_TRADE` for a lane with a non-empty route, else 0. ORed
    /// into the mask each `spawn_*` builds today, so `EMPTY` reproduces
    /// every pre-B'1 mask bit for bit.
    #[must_use]
    pub const fn mask_bit(&self, lane: usize) -> u16 {
        if lane < NUM_EVENT_LANES {
            route_bit(self.lanes[lane])
        } else {
            0
        }
    }
}

/// The bit a `spawn_*` ORs into the mask it builds today, given the
/// route slice it received (`routes.lanes[lane]`): `EVENT_LANE_TRADE`
/// for a non-empty route, else 0 — so an empty route leaves the mask
/// bit for bit (I6).
#[must_use]
pub const fn route_bit(route: &[SymbolId]) -> u16 {
    if route.is_empty() {
        0
    } else {
        EVENT_LANE_TRADE
    }
}

/// T5, the `run` boot (impl-Bp §4.1 item 3): the routes every `spawn_*`
/// receives. `manifest` is the `--pine` manifest when the pine slot is in
/// the boot mask (`None` otherwise → [`TradeRoutes::EMPTY`], every head
/// mask unchanged). R1 (`strategy_pine::route_symbols`) resolves each
/// canonical key with `pine_boot::resolve_key` (the key law impl-08
/// owns) to its universe instrument's capture descriptor; [`resolve`]
/// then builds the per-lane routes or refuses with `6006`.
///
/// pin:Bp-r1-unresolved-6006 — R1 answers a bare `6006` for a key the
/// universe lacks (`pine_boot::build` refuses those first, so this is a
/// fail-closed backstop): the refusal names the first unresolved key,
/// `why` = `NoSubscription` (the unknown-descriptor reading of
/// [`resolve`]).
pub fn boot_routes(
    manifest: Option<&core_config::pine::PineManifest>,
    u: &core_config::universe::AllocatedUniverse,
) -> Result<TradeRoutes, RouteRefusal> {
    let Some(m) = manifest else {
        return Ok(TradeRoutes::EMPTY);
    };
    let pairs = strategy_pine::route_symbols(m, |k| {
        let sym = crate::pine_boot::resolve_key(u, k)?;
        Some((sym, descriptor_of(u, sym)?))
    })
    .map_err(|code| {
        let key = first_unresolved(m, u).unwrap_or_default();
        RouteRefusal {
            code,
            descriptor: key,
            why: TradeCapture::NoSubscription,
        }
    })?;
    let route: Vec<(SymbolId, &str)> = pairs.iter().map(|(s, d)| (*s, d.as_str())).collect();
    resolve(&route)
}

/// The capture descriptor of `sym` on the three lanes `resolve_key`
/// reads. R1's resolver is higher-ranked (its `&str` out borrows from the
/// key in), so the descriptor is handed out `'static`: leaked once per
/// route symbol at boot (≤ `strategy_pine::MAX_SYMBOLS` short strings).
fn descriptor_of(
    u: &core_config::universe::AllocatedUniverse,
    sym: SymbolId,
) -> Option<&'static str> {
    let inst =
        u.hl.iter()
            .chain(u.bn_usdm.iter())
            .chain(u.bn_spot.iter())
            .find(|i| i.sym == sym)?;
    Some(Box::leak(inst.descriptor.clone().into_boxed_str()))
}

fn first_unresolved(
    m: &core_config::pine::PineManifest,
    u: &core_config::universe::AllocatedUniverse,
) -> Option<String> {
    for sc in &m.scripts {
        let keys =
            std::iter::once(sc.symbol.as_str()).chain(sc.symbols.iter().map(|t| t.1.as_str()));
        for k in keys {
            if crate::pine_boot::resolve_key(u, k).is_none() {
                return Some(k.to_string());
            }
        }
    }
    None
}

/// A route refusal: `6006` naming the manifest key `symbol`.
#[derive(Debug)]
pub struct RouteRefusal {
    /// Always [`ROUTE_REFUSAL_CODE`] (`6006`).
    pub code: pine_ir::DiagCode,
    /// The refused route symbol's descriptor (the manifest `symbol`).
    pub descriptor: String,
    /// Why its lane cannot carry the route.
    pub why: TradeCapture,
}

impl std::fmt::Display for RouteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pine {}: manifest `symbol` {:?} sits on a lane without Trade capture ({:?})",
            self.code.0, self.descriptor, self.why
        )
    }
}

/// `route` = every symbol the pine slot loaded, as `(SymbolId, capture
/// descriptor)`. Refuses the boot when a symbol's lane has no Trade
/// capture (or is unknown), or when one lane would route more than
/// `TRADE_ROUTE_MAX` symbols (pin:Bp-route-max — the spec states the cap,
/// not what exceeding it does; refused with the same code, `why` = Yes).
pub fn resolve(route: &[(SymbolId, &str)]) -> Result<TradeRoutes, RouteRefusal> {
    if route.is_empty() {
        return Ok(TradeRoutes::EMPTY);
    }
    let mut per: [Vec<SymbolId>; NUM_EVENT_LANES] = Default::default();
    for &(sym, desc) in route {
        let refuse = |why| RouteRefusal {
            code: ROUTE_REFUSAL_CODE,
            descriptor: desc.to_string(),
            why,
        };
        match trade_capture_of(desc) {
            Some((l, TradeCapture::Yes)) => {
                if !per[l].contains(&sym) {
                    if per[l].len() == TRADE_ROUTE_MAX {
                        return Err(refuse(TradeCapture::Yes));
                    }
                    per[l].push(sym);
                }
            }
            Some((_, why)) => return Err(refuse(why)),
            None => return Err(refuse(TradeCapture::NoSubscription)),
        }
    }
    let mut out = TradeRoutes::EMPTY;
    let mut l = 0;
    while l < NUM_EVENT_LANES {
        if !per[l].is_empty() {
            let mut v = std::mem::take(&mut per[l]);
            v.sort_unstable();
            out.lanes[l] = Box::leak(v.into_boxed_slice());
        }
        l += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_sorts_and_dedups_per_lane() {
        let r = resolve(&[
            (40, "binance:ethusdt"),
            (7, "binance:btcusdt"),
            (40, "binance:ethusdt"),
        ])
        .unwrap();
        let bn = tick_lane_of(VenueId::Binance).unwrap();
        assert_eq!(r.lanes[bn], &[7, 40]);
    }

    #[test]
    fn resolve_refuses_more_than_route_max_on_one_lane() {
        let descs: Vec<(SymbolId, String)> = (0..=TRADE_ROUTE_MAX as u32)
            .map(|i| (i, format!("okx:X{i}-USDT")))
            .collect();
        let route: Vec<(SymbolId, &str)> = descs.iter().map(|(s, d)| (*s, d.as_str())).collect();
        let e = resolve(&route).unwrap_err();
        assert_eq!(e.code.0, 6006);
    }
}
