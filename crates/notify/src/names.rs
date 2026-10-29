// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Naming symbols for messages. Hyperliquid's rolling outcome legs are
//! `make_symbol_id(Hyperliquid, 4096 + 2·f + s)`: family `f` in the file
//! order of `[hyperliquid] rolling = [...]` in the instance's
//! `universe.toml`, side `s` 0 Yes / 1 No (the ordinal law of
//! `ingress_hyperliquid::family`, pinned by a test). The family's period
//! (`out:BTC:15m` → 900 s) gives an instance's expiry: a fill belongs to
//! the instance that expires at the next period boundary.
//!
//! Only the `rolling` line is read — never the rest of the universe — so
//! the notifier does not depend on `core-config`, which the two branches
//! have diverged in. Any other symbol prints as its raw id.

use std::path::Path;

/// `core_types::VenueId::Hyperliquid` (pinned).
pub const VENUE_HYPERLIQUID: u8 = 4;
/// The rolling pool's ordinal base (pinned).
pub const HL_ROLLING_ORDINAL_BASE: u32 = 4096;
/// At most this many rolling families.
pub const HL_ROLLING_MAX: u32 = 8;

/// One rolling family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Family {
    /// `BTC`.
    pub coin: String,
    /// Instance period, seconds.
    pub period_s: i64,
    /// `BTC 15m`.
    pub label: String,
}

/// A symbol, named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymName {
    /// The family, if this is a rolling leg the universe names.
    pub family: Option<Family>,
    /// `Yes` / `No` for a rolling leg.
    pub leg: Option<&'static str>,
    /// `BTC 15m Yes`, or `sym 0x04001000`.
    pub text: String,
}

/// The family table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Names {
    /// File order.
    pub families: Vec<Family>,
}

fn parse_period(p: &str) -> Option<i64> {
    let (num, unit) = p.split_at(p.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    (n > 0).then_some(n * mult)
}

/// `out:BTC:15m` / `native:BTC:1d` → a family.
#[must_use]
pub fn parse_descriptor(d: &str) -> Option<Family> {
    let mut it = d.split(':');
    let _kind = it.next()?;
    let coin = it.next()?.to_string();
    let period = it.next()?;
    if coin.is_empty() || it.next().is_some() {
        return None;
    }
    let period_s = parse_period(period)?;
    Some(Family {
        label: format!("{coin} {period}"),
        coin,
        period_s,
    })
}

/// The quoted strings of a TOML array text such as `["a", 'b']`.
fn quoted_strings(arr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    for c in arr.chars() {
        match q {
            None if c == '"' || c == '\'' => q = Some(c),
            None => {}
            Some(open) if c == open => {
                out.push(std::mem::take(&mut cur));
                q = None;
            }
            Some(_) => cur.push(c),
        }
    }
    out
}

impl Names {
    /// The `[hyperliquid] rolling` families of a universe file's text.
    #[must_use]
    pub fn from_universe(text: &str) -> Names {
        let mut section = String::new();
        let mut lines = text.lines();
        while let Some(line) = lines.next() {
            let t = crate::toml::strip_comment(line).trim();
            if t.starts_with('[') && !t.starts_with("[[") {
                section = t.trim_matches(|c| c == '[' || c == ']').trim().to_string();
                continue;
            }
            if t.starts_with("[[") {
                section = String::new();
                continue;
            }
            if section != "hyperliquid" {
                continue;
            }
            let Some(rest) = t.strip_prefix("rolling") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let mut arr = rest.trim().to_string();
            while !arr.contains(']') {
                match lines.next() {
                    Some(l) => {
                        arr.push(' ');
                        arr.push_str(crate::toml::strip_comment(l).trim());
                    }
                    None => break,
                }
            }
            let families = quoted_strings(&arr)
                .iter()
                .filter_map(|d| parse_descriptor(d))
                .collect();
            return Names { families };
        }
        Names::default()
    }

    /// Read a universe file; unreadable = no names (raw ids print).
    #[must_use]
    pub fn load(path: Option<&Path>) -> Names {
        path.and_then(|p| std::fs::read_to_string(p).ok())
            .map_or_else(Names::default, |t| Names::from_universe(&t))
    }

    /// Name a symbol.
    #[must_use]
    pub fn name(&self, sym: u32) -> SymName {
        let venue = (sym >> 24) as u8;
        let ord = sym & 0x00FF_FFFF;
        if venue == VENUE_HYPERLIQUID
            && (HL_ROLLING_ORDINAL_BASE..HL_ROLLING_ORDINAL_BASE + 2 * HL_ROLLING_MAX)
                .contains(&ord)
        {
            let k = ord - HL_ROLLING_ORDINAL_BASE;
            let f = (k / 2) as usize;
            let leg = if k % 2 == 0 { "Yes" } else { "No" };
            if let Some(fam) = self.families.get(f) {
                return SymName {
                    text: format!("{} {leg}", fam.label),
                    family: Some(fam.clone()),
                    leg: Some(leg),
                };
            }
            return SymName {
                text: format!("HL rolling #{f} {leg}"),
                family: None,
                leg: Some(leg),
            };
        }
        SymName {
            family: None,
            leg: None,
            text: format!("sym {sym:#010x}"),
        }
    }
}

/// The expiry of the instance live at `ts_s` (the next boundary).
#[must_use]
pub fn expiry_after(ts_s: i64, period_s: i64) -> i64 {
    (ts_s.div_euclid(period_s) + 1) * period_s
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNIVERSE: &str = r#"
[polymarket]
rolling = ["btc-updown-15m"]
[hyperliquid]
coins = ["BTC", "ETH"]
rolling = [
  "out:BTC:15m", "out:ETH:15m",  # two
  'out:SOL:15m', "native:HYPE:1d",
]
[binance]
rolling = ["nope:X:1m"]
"#;

    #[test]
    fn reads_only_the_hyperliquid_rolling_line() {
        let n = Names::from_universe(UNIVERSE);
        let labels: Vec<_> = n.families.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["BTC 15m", "ETH 15m", "SOL 15m", "HYPE 1d"]);
        assert_eq!(n.families[3].period_s, 86_400);
        assert_eq!(Names::from_universe("x = 1\n"), Names::default());
    }

    #[test]
    fn names_legs_by_the_ordinal_law() {
        let n = Names::from_universe(UNIVERSE);
        let hl = |ord: u32| (u32::from(VENUE_HYPERLIQUID) << 24) | ord;
        assert_eq!(n.name(hl(4096)).text, "BTC 15m Yes");
        assert_eq!(n.name(hl(4097)).text, "BTC 15m No");
        assert_eq!(n.name(hl(4101)).text, "SOL 15m No");
        assert_eq!(n.name(hl(4105)).text, "HL rolling #4 No");
        assert_eq!(n.name(hl(7)).text, "sym 0x04000007");
        assert_eq!(n.name(5).text, "sym 0x00000005");
    }

    #[test]
    fn expiry_is_the_next_boundary() {
        assert_eq!(expiry_after(899, 900), 900);
        assert_eq!(expiry_after(900, 900), 1800);
        assert_eq!(expiry_after(1, 86_400), 86_400);
    }

    #[test]
    fn pins_the_ordinal_law_and_the_venue_byte() {
        assert_eq!(
            HL_ROLLING_ORDINAL_BASE,
            ingress_hyperliquid::family::HL_ROLLING_ORDINAL_BASE
        );
        assert_eq!(VENUE_HYPERLIQUID, core_types::VenueId::Hyperliquid as u8);
        let sym = core_types::make_symbol_id(
            core_types::VenueId::Hyperliquid,
            HL_ROLLING_ORDINAL_BASE + 3, // family 1 (ETH), side 1 (No)
        );
        let n = Names::from_universe(UNIVERSE);
        assert_eq!(n.name(sym).text, "ETH 15m No");
    }
}
