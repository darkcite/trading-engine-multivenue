// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'4 T26 — the Hyperliquid perp boot state (impl-Bp §4.13 steps 5–6, D19) handed to its two
//! readers: the router's perp rows (`RoutedDispatcher::seed_perp`, I31) and PL3's seam
//! (`pine_boot::attach_live_boot` over [`LiveBootRow`]s). DARK: nothing here arms anything; slot
//! 8 stays paper only and the cli never sets the PL2 permit (pin:pl2-cli-never-permits). The
//! venue reads (`clearinghouseState`, `userFillsByTime`) are the arm's, injected as their
//! scanned results — no network here.
//! COPY-DOCTRINE: boot-only; may allocate.
//!
//! Literal readings:
//! - pin:bp4-boot-row-per-bound-coin: one row per coin this boot bound (bind order), flat when
//!   the venue lists no position for it; a position on an unbound coin refused before
//!   (`perp_recon::check_boot_positions`, pin:bp4-unbound-position-refuses).
//! - pin:bp4-day-open-ceil: the ×1e12 sums are carried UP to ×1e6 (a cap's conservative side).
//! - pin:bp4-swept-from-the-arm: `swept` is the E-8 boot sweep's verdict (§4.13 step 7), passed
//!   in by the caller for every coin — this module does not sweep.

use exec_hyperliquid::perp::PerpTable;
use exec_hyperliquid::perp_recon::{
    ceil_1e12_to_1e6, check_boot_positions, FillsDay, PerpPos, PerpPosRefusal,
};

use crate::pine_boot::LiveBootRow;

/// One bound perp coin's venue state at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlPerpBoot {
    /// The engine symbol of the bound coin.
    pub sym: core_types::SymbolId,
    /// Signed `szi` ×1e6 (0 when the venue lists no position).
    pub position_1e6: i64,
    /// `entryPx` ×1e6; 0 when flat.
    pub entry_px_1e6: i64,
    /// Today's opening notional ×1e6 (the router's day-cap seed).
    pub day_open_1e6: i64,
    /// Today's notional, both directions, ×1e6 (the member's D19 day notional).
    pub day_abs_1e6: i64,
}

/// §4.13 steps 5–6: judge the venue's positions against the law (I32), then join each bound coin
/// with its position and its day of fills. `days` holds one `(sym, FillsDay)` per bound coin.
///
/// # Errors
/// The refusal as the boot prints it: a position not isolated, at another leverage or on an
/// unbound coin; a bound coin whose day was not read.
pub fn hl_perp_boot(
    table: &PerpTable,
    positions: &[PerpPos],
    days: &[(core_types::SymbolId, FillsDay)],
) -> Result<Vec<HlPerpBoot>, String> {
    if let Err(r) = check_boot_positions(table, positions) {
        let (k, why) = match r {
            PerpPosRefusal::NotIsolated(k) => (k, "is not isolated"),
            PerpPosRefusal::LeverageMismatch(k) => (k, "is not at [exec.hl_perp] leverage"),
            PerpPosRefusal::UnboundCoin(k) => (k, "is on a coin this boot did not bind"),
        };
        let coin = positions
            .get(k)
            .map(|p| String::from_utf8_lossy(p.coin()).into_owned())
            .unwrap_or_default();
        return Err(format!(
            "hyperliquid perp boot refused: the venue position on {coin} {why} — the venue \
             state disagrees with the law (I32)"
        ));
    }
    let mut out = Vec::with_capacity(table.len());
    for spec in table.specs() {
        let pos = positions.iter().find(|p| p.coin() == spec.coin());
        let Some((_, day)) = days.iter().find(|(s, _)| *s == spec.sym) else {
            return Err(format!(
                "hyperliquid perp boot refused: today's fills on {} were not read",
                String::from_utf8_lossy(spec.coin())
            ));
        };
        let position_1e6 = pos.map_or(0, |p| p.szi_1e6);
        out.push(HlPerpBoot {
            sym: spec.sym,
            position_1e6,
            entry_px_1e6: if position_1e6 == 0 {
                0
            } else {
                pos.map_or(0, |p| p.entry_px_1e6)
            },
            day_open_1e6: ceil_1e12_to_1e6(day.opening_1e12),
            day_abs_1e6: ceil_1e12_to_1e6(day.abs_1e12),
        });
    }
    Ok(out)
}

/// D19 for the router: seed every perp row before the first order (`seed_perp`).
///
/// # Errors
/// The ledger's refusal, naming the symbol.
pub fn seed_router_perps<P, L>(
    router: &mut exec_router::RoutedDispatcher<P, L>,
    rows: &[HlPerpBoot],
) -> Result<(), String>
where
    P: clob_dispatcher::OrderDispatch,
    L: clob_dispatcher::OrderDispatch,
{
    for r in rows {
        router
            .seed_perp(r.sym, r.position_1e6, r.entry_px_1e6, r.day_open_1e6)
            .map_err(|e| {
                format!(
                    "hyperliquid perp boot refused: seed of sym {}: {e:?}",
                    r.sym
                )
            })?;
    }
    Ok(())
}

/// PL3's seam: the HL perp boot state as [`LiveBootRow`]s (bind order), which
/// `attach_live_boot` judges (pin:bp4-swept-from-the-arm). Boot only.
#[must_use]
pub fn live_rows_from_hl(rows: &[HlPerpBoot], swept: bool) -> Vec<LiveBootRow> {
    rows.iter()
        .map(|r| LiveBootRow {
            sym: r.sym,
            position_1e6: r.position_1e6,
            entry_px_1e6: r.entry_px_1e6,
            day_abs_1e6: r.day_abs_1e6,
            day_read: true,
            swept,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use exec_hyperliquid::perp::PerpWant;

    const META: &[u8] = br#"{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40},{"szDecimals":4,"name":"ETH","maxLeverage":25}]}"#;
    const E12: i128 = 1_000_000_000_000;

    fn table() -> PerpTable {
        let w = |sym, coin: &'static [u8]| PerpWant {
            sym,
            coin,
            leverage: 3,
            min_notional_1e6: 0,
            owner_slot: 8,
        };
        let mut t = PerpTable::new();
        t.bind_from_meta(META, &[w(900, b"BTC"), w(901, b"ETH")])
            .unwrap();
        t
    }

    fn pos(coin: &[u8], szi: i64, px: i64, lev: u16, isolated: u8) -> PerpPos {
        let mut c = [0u8; 20];
        c[..coin.len()].copy_from_slice(coin);
        PerpPos {
            szi_1e6: szi,
            entry_px_1e6: px,
            leverage: lev,
            isolated,
            coin_len: coin.len() as u8,
            coin: c,
        }
    }

    fn day(open: i128, abs: i128) -> FillsDay {
        FillsDay {
            opening_1e12: open,
            abs_1e12: abs,
            last_time_ms: 0,
        }
    }

    /// The bound coins join their positions and days; a coin the venue lists no position for is
    /// flat; the router seed restores both and PL3 receives one row per coin.
    #[test]
    fn hl_boot_state_seeds_the_router_and_reaches_pl3() {
        let t = table();
        let p = [pos(b"ETH", -2_000_000, 3_000_000_000, 3, 1)];
        let days = [(900, day(0, 0)), (901, day(1_500 * E12 + 1, 2_000 * E12))];
        let rows = hl_perp_boot(&t, &p, &days).unwrap();
        assert_eq!(
            rows,
            vec![
                HlPerpBoot {
                    sym: 900,
                    position_1e6: 0,
                    entry_px_1e6: 0,
                    day_open_1e6: 0,
                    day_abs_1e6: 0
                },
                HlPerpBoot {
                    sym: 901,
                    position_1e6: -2_000_000,
                    entry_px_1e6: 3_000_000_000,
                    day_open_1e6: 1_500_000_001,
                    day_abs_1e6: 2_000_000_000
                },
            ]
        );
        let mut r = exec_router::RoutedDispatcher::new(
            exec_router::ExecRoute::all_paper(),
            clob_dispatcher::PaperDispatcher::new(),
            exec_router::NullLiveDispatcher::new(),
            core_time::WallAnchor::new(1, 1),
        );
        r.bind_perp(900, 8).unwrap();
        r.bind_perp(901, 8).unwrap();
        seed_router_perps(&mut r, &rows).unwrap();
        assert_eq!(r.ledger().instrument_position_1e6(8, 901), Some(-2_000_000));
        assert_eq!(r.ledger().perp_day_open_1e6(8), 1_500_000_001);
        assert!(
            seed_router_perps(&mut r, &rows).is_err(),
            "one seed per row"
        );
        let live = live_rows_from_hl(&rows, true);
        assert_eq!(live.len(), 2);
        assert_eq!(
            (
                live[1].sym,
                live[1].position_1e6,
                live[1].day_abs_1e6,
                live[1].swept
            ),
            (901, -2_000_000, 2_000_000_000, true)
        );
    }

    /// I32 at the cli: a cross position refuses the boot; a coin whose day was not read refuses.
    #[test]
    fn hl_boot_state_refuses_what_the_law_refuses() {
        let t = table();
        let cross = [pos(b"BTC", 1_000, 60_000_000_000, 3, 0)];
        let days = [(900, day(0, 0)), (901, day(0, 0))];
        let e = hl_perp_boot(&t, &cross, &days).unwrap_err();
        assert!(e.contains("BTC is not isolated"), "{e}");
        let e = hl_perp_boot(&t, &[], &days[..1]).unwrap_err();
        assert!(e.contains("fills on ETH were not read"), "{e}");
    }
}
