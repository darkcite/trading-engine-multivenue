// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # xsd_boot — the xsd member's boot bundle (XSD-3)
//!
//! Resolves the four operator artifacts of `strategy-xsd` (statarb doc 08
//! §3.1) against the boot universe, the icdp / vrp precedent:
//!
//! | file | law here |
//! |---|---|
//! | `xsd.toml` | ABSENT default ⇒ the member is not configured and its bit stays unset; an explicit `--xsd` that does not exist refuses the boot; a present file must parse (`core-config::xsd`) |
//! | `xsd-table.tsv` | `target \t partner \t beta_1e9 [\t rank \t t_adf_1e6]`; a row whose descriptor is not in the universe is DROPPED and counted (a monthly table outlives a delisting); zero usable rows ⇒ refuse; ABSENT default ⇒ not configured (same as no `xsd.toml`) |
//! | `xsd-seed.tsv` | `descriptor \t open_ms \t close_1e6`; rows at or after the boot hour are dropped and counted (a seed from the future would alias a live bucket); unknown descriptors dropped and counted; ABSENT is legal — the member warms live |
//! | `xsd-state.tsv` | written by the engine; restored ONLY when its `H` row equals the booted table's hash, otherwise every row is FLATTENED at its target's first fresh tick; a row the member cannot place (a sym outside the table) REFUSES the boot — a position nobody can flatten is worse than a boot that asks for a hand |
//!
//! **BOOT DOCTRINE:** runs once; allocation is fine. The state file's
//! grammar is owned here (the crate exposes `positions_view` /
//! `restore_position` in syms; the file speaks DESCRIPTORS so a universe
//! reorder cannot re-aim a persisted position).

use std::path::{Path, PathBuf};

use core_types::SymbolId;
use strategy_xsd::{XsdParams, XsdPositionView, XsdRestoreRow, XsdTable, XsdTableRow, HOUR_NS};
use tracing::info;

/// `xsd-state.tsv` grammar version.
pub const XSD_STATE_VERSION: u32 = 1;

/// Everything the set builder needs to configure the member.
#[derive(Debug)]
pub struct XsdBoot {
    /// Parameters translated out of `xsd.toml`.
    pub params: XsdParams,
    /// The resolved table (boxed: 384 rows × 16 B is fine on the stack,
    /// but the bundle travels through the boot path by reference).
    pub table: Box<XsdTable>,
    /// Seed rows `(sym, hour, close_1e6)` in file order, filtered.
    pub seed: Vec<(SymbolId, i64, i64)>,
    /// Seed rows dropped: unknown descriptor, at/after the boot hour,
    /// malformed close.
    pub seed_dropped: usize,
    /// Table rows dropped for an unresolvable descriptor.
    pub rows_dropped: usize,
    /// Persisted positions to restore (descriptors resolved).
    pub restore: Vec<XsdRestoreRow>,
    /// `true` when the state file's table hash differs from the booted
    /// table's — every restored row is flattened instead of resumed.
    pub restore_flatten: bool,
    /// The state file existed (the tell distinguishes "no state" from
    /// "state restored" / "state discarded").
    pub state_present: bool,
    /// Where the engine writes the state back.
    pub state_path: PathBuf,
    /// Descriptor per TARGET sym — what the state writer prints.
    pub descriptors: Vec<(SymbolId, String)>,
    /// Where `xsd.toml` came from (boot tell).
    pub params_path: PathBuf,
    /// Where the table came from (boot tell).
    pub table_path: PathBuf,
    /// Where the seed came from — the default path even when nothing was
    /// there (the tell an operator needs to fix a cold boot).
    pub seed_path: PathBuf,
}

impl XsdBoot {
    /// The descriptor of a target sym, for the state writer.
    pub fn descriptor_of(&self, sym: SymbolId) -> Option<&str> {
        let mut i = 0usize;
        while i < self.descriptors.len() {
            if self.descriptors[i].0 == sym {
                return Some(self.descriptors[i].1.as_str());
            }
            i += 1;
        }
        None
    }
}

fn default_or(path: Option<&Path>, default: Result<String, core_config::ConfigError>) -> Result<(PathBuf, bool), String> {
    match path {
        Some(p) => Ok((p.to_path_buf(), true)),
        None => Ok((PathBuf::from(default.map_err(|e| e.to_string())?), false)),
    }
}

/// The current wall hour (boot path; one syscall).
pub fn wall_hour_now() -> i64 {
    let wall_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (wall_ns / HOUR_NS) as i64
}

/// Translate the parsed file into the member's POD.
pub fn params_from_file(file: &core_config::xsd::XsdFile, hash: [u8; 32]) -> XsdParams {
    XsdParams {
        z_window_h: file.z_window_h,
        z_enter_1e9: file.z_enter_1e9,
        z_exit_1e9: file.z_exit_1e9,
        z_stop_1e9: file.z_stop_1e9,
        consensus: file.consensus,
        grid_n: file.grid_n,
        grid_step_1e9: file.grid_step_1e9,
        max_hold_h: file.max_hold_h,
        cooldown_h: file.cooldown_h,
        ttl_ns: (file.ttl_s as u64).saturating_mul(1_000_000_000),
        position_usd_1e6: file.position_usd_1e6,
        max_positions: file.max_positions,
        max_gross_usd_1e6: file.max_gross_usd_1e6,
        direction: file.direction,
        slip_1e9: file.slip_1e9,
        hash,
    }
}

/// Parse `xsd-table.tsv` text against `resolve`. Returns the table, its
/// hash over the EXACT bytes, the target descriptors and the dropped
/// row count.
pub fn parse_table(
    bytes: &[u8],
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<(Box<XsdTable>, Vec<(SymbolId, String)>, usize), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "xsd-table.tsv: not UTF-8".to_owned())?;
    let mut table = Box::new(XsdTable::EMPTY);
    let mut descriptors: Vec<(SymbolId, String)> = Vec::new();
    let mut dropped = 0usize;
    for (idx, raw) in text.lines().enumerate() {
        let ln = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 3 {
            return Err(format!("xsd-table.tsv line {ln}: expected target\\tpartner\\tbeta_1e9"));
        }
        let beta_1e9: i64 = f[2]
            .parse()
            .map_err(|_| format!("xsd-table.tsv line {ln}: bad beta_1e9 `{}`", f[2]))?;
        if beta_1e9 == 0 {
            return Err(format!("xsd-table.tsv line {ln}: zero beta"));
        }
        let (Some(target), Some(partner)) = (resolve(f[0]), resolve(f[1])) else {
            dropped += 1;
            continue;
        };
        if table.n >= strategy_xsd::XSD_MAX_PAIRS {
            return Err(format!(
                "xsd-table.tsv line {ln}: more than {} rows",
                strategy_xsd::XSD_MAX_PAIRS
            ));
        }
        table.rows[table.n] = XsdTableRow {
            target,
            partner,
            beta_1e9,
        };
        table.n += 1;
        if !descriptors.iter().any(|(s, _)| *s == target) {
            descriptors.push((target, f[0].to_owned()));
        }
    }
    if table.n == 0 {
        return Err(format!(
            "xsd-table.tsv: no usable row ({dropped} dropped — none of the descriptors is in the boot universe)"
        ));
    }
    table.hash = core_crypto::sha256(bytes);
    Ok((table, descriptors, dropped))
}

/// Parse `xsd-seed.tsv` text: `(sym, hour, close_1e6)` rows strictly
/// before `boot_hour`, plus the dropped count.
pub fn parse_seed(
    text: &str,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    boot_hour: i64,
) -> Result<(Vec<(SymbolId, i64, i64)>, usize), String> {
    let mut rows = Vec::new();
    let mut dropped = 0usize;
    for (idx, raw) in text.lines().enumerate() {
        let ln = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        if f.len() < 3 {
            return Err(format!("xsd-seed.tsv line {ln}: expected descriptor\\topen_ms\\tclose_1e6"));
        }
        let open_ms: i64 = f[1]
            .parse()
            .map_err(|_| format!("xsd-seed.tsv line {ln}: bad open_ms `{}`", f[1]))?;
        let close_1e6: i64 = f[2]
            .parse()
            .map_err(|_| format!("xsd-seed.tsv line {ln}: bad close_1e6 `{}`", f[2]))?;
        let hour = open_ms.div_euclid(3_600_000);
        match resolve(f[0]) {
            Some(sym) if hour < boot_hour && close_1e6 > 1 => rows.push((sym, hour, close_1e6)),
            _ => dropped += 1,
        }
    }
    Ok((rows, dropped))
}

/// Parse `xsd-state.tsv` text. Returns the table hash it was written
/// under and the rows; a descriptor the universe lacks is an error.
pub fn parse_state(
    text: &str,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
) -> Result<([u8; 32], Vec<XsdRestoreRow>), String> {
    let mut version_ok = false;
    let mut hash: Option<[u8; 32]> = None;
    let mut rows = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let ln = idx + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').map(str::trim).collect();
        let num = |i: usize, what: &str| -> Result<i64, String> {
            f.get(i)
                .ok_or_else(|| format!("xsd-state.tsv line {ln}: short row"))?
                .parse::<i64>()
                .map_err(|_| format!("xsd-state.tsv line {ln}: bad {what}"))
        };
        match f[0] {
            "V" => {
                if num(1, "version")? != XSD_STATE_VERSION as i64 {
                    return Err(format!("xsd-state.tsv line {ln}: unsupported version"));
                }
                version_ok = true;
            }
            "H" => {
                let h = f.get(1).ok_or_else(|| format!("xsd-state.tsv line {ln}: short row"))?;
                hash = Some(parse_hex32(h).ok_or_else(|| format!("xsd-state.tsv line {ln}: bad hash"))?);
            }
            "P" => {
                let d = f.get(1).ok_or_else(|| format!("xsd-state.tsv line {ln}: short row"))?;
                let sym = resolve(d).ok_or_else(|| {
                    format!("xsd-state.tsv line {ln}: `{d}` is not in the boot universe — a position nobody can flatten; move the file aside to boot without it")
                })?;
                rows.push(XsdRestoreRow {
                    sym,
                    side: num(2, "side")? as u8,
                    d: num(3, "d")? as i8,
                    grid_units: num(4, "grid_units")? as u8,
                    qty_1e6: num(5, "qty_1e6")?,
                    notional_1e6: num(6, "notional_1e6")?,
                    entry_hour: num(7, "entry_hour")?,
                    last_add_hour: num(8, "last_add_hour")?,
                });
            }
            other => return Err(format!("xsd-state.tsv line {ln}: unknown tag `{other}`")),
        }
    }
    if !version_ok {
        return Err("xsd-state.tsv: missing V row".to_owned());
    }
    let hash = hash.ok_or_else(|| "xsd-state.tsv: missing H row".to_owned())?;
    Ok((hash, rows))
}

/// Render the state file from the member's view. Cold path.
pub fn render_state<'a>(
    table_hash: &[u8; 32],
    descriptor_of: &dyn Fn(SymbolId) -> Option<&'a str>,
    positions: &[XsdPositionView],
    out: &mut String,
) {
    use core::fmt::Write as _;
    out.clear();
    out.push_str(
        "# xsd-state.tsv — written by the engine, read at boot. Not for hand editing.\n\
         # V version | H table_hash (restored only under the same table) |\n\
         # P descriptor side(1 long/2 short) d grid_units qty_1e6 notional_1e6 entry_hour last_add_hour\n",
    );
    let _ = writeln!(out, "V\t{XSD_STATE_VERSION}");
    let _ = writeln!(out, "H\t{}", hex_lower(table_hash));
    for p in positions {
        let Some(d) = descriptor_of(p.sym) else {
            // A target the boot bundle cannot name — impossible by
            // construction (the view only lists table targets); never
            // write a row the next boot cannot read.
            debug_assert!(false, "xsd position on an unnamed sym");
            continue;
        };
        let _ = writeln!(
            out,
            "P\t{d}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            p.side, p.d, p.grid_units, p.qty_1e6, p.notional_1e6, p.entry_hour, p.last_add_hour
        );
    }
}

/// Load the whole bundle. `Ok(None)` = the member is not configured
/// (no `xsd.toml`, or no table, at the default paths).
pub fn load_xsd_boot(
    params_path: Option<&Path>,
    table_path: Option<&Path>,
    seed_path: Option<&Path>,
    state_path: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    boot_hour: i64,
) -> Result<Option<XsdBoot>, String> {
    let (params_path, explicit) = default_or(params_path, core_config::xsd::default_xsd_path())?;
    if !params_path.exists() {
        if explicit {
            return Err(format!("xsd: {} does not exist", params_path.display()));
        }
        info!(path = %params_path.display(), "xsd: artifact absent — the member is not configured");
        return Ok(None);
    }
    let (file, bytes) = core_config::xsd::load(&params_path).map_err(|e| e.to_string())?;
    let params = params_from_file(&file, core_crypto::sha256(&bytes));

    let (table_path, table_explicit) = default_or(table_path, core_config::xsd::default_xsd_table_path())?;
    if !table_path.exists() {
        if table_explicit {
            return Err(format!("xsd: {} does not exist", table_path.display()));
        }
        info!(path = %table_path.display(), "xsd: table absent — the member is not configured");
        return Ok(None);
    }
    let table_bytes = std::fs::read(&table_path).map_err(|e| format!("xsd: {}: {e}", table_path.display()))?;
    let (table, descriptors, rows_dropped) = parse_table(&table_bytes, resolve)?;

    let (seed_path, seed_explicit) = default_or(seed_path, core_config::xsd::default_xsd_seed_path())?;
    let (seed, seed_dropped) = if seed_path.exists() {
        let text = std::fs::read_to_string(&seed_path).map_err(|e| format!("xsd: {}: {e}", seed_path.display()))?;
        parse_seed(&text, resolve, boot_hour)?
    } else if seed_explicit {
        return Err(format!("xsd: {} does not exist", seed_path.display()));
    } else {
        (Vec::new(), 0)
    };

    let (state_path, _) = default_or(state_path, core_config::xsd::default_xsd_state_path())?;
    let (restore, restore_flatten, state_present) = if state_path.exists() {
        let text = std::fs::read_to_string(&state_path).map_err(|e| format!("xsd: {}: {e}", state_path.display()))?;
        let (hash, rows) = parse_state(&text, resolve)?;
        let same = hash == table.hash;
        (rows, !same, true)
    } else {
        (Vec::new(), false, false)
    };

    Ok(Some(XsdBoot {
        params,
        table,
        seed,
        seed_dropped,
        rows_dropped,
        restore,
        restore_flatten,
        state_present,
        state_path,
        descriptors,
        params_path,
        table_path,
        seed_path,
    }))
}

/// Write the state file atomically (temp beside it, then rename) — the
/// `vrp_boot::write_state` law; a reader sees the old file or the new.
pub fn write_state(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tsv.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("xsd: {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("xsd: {}: {e}", path.display()))
}

/// Lower-hex of a 32-byte hash.
pub fn hex_lower(h: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        use core::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < 32 {
        let hi = (b[2 * i] as char).to_digit(16)?;
        let lo = (b[2 * i + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
        i += 1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, VenueId};

    fn resolver(d: &str) -> Option<SymbolId> {
        match d {
            "binance-usdm:aaausdt" => Some(make_symbol_id(VenueId::Binance, 512)),
            "binance-usdm:bbbusdt" => Some(make_symbol_id(VenueId::Binance, 513)),
            "binance-usdm:cccusdt" => Some(make_symbol_id(VenueId::Binance, 514)),
            _ => None,
        }
    }

    const TABLE: &str = "# target\tpartner\tbeta_1e9\trank\tt_adf_1e6\n\
        binance-usdm:aaausdt\tbinance-usdm:bbbusdt\t1000000000\t1\t-4500000\n\
        binance-usdm:aaausdt\tbinance-usdm:cccusdt\t900000000\t2\t-4100000\n\
        binance-usdm:aaausdt\tbinance-usdm:zzzusdt\t1100000000\t3\t-3900000\n\
        binance-usdm:bbbusdt\tbinance-usdm:aaausdt\t1000000000\t1\t-4400000\n";

    #[test]
    fn table_parses_drops_unresolved_and_hashes_the_bytes() {
        let (t, descs, dropped) = parse_table(TABLE.as_bytes(), &resolver).unwrap();
        assert_eq!(t.n, 3);
        assert_eq!(dropped, 1);
        assert_eq!(descs.len(), 2);
        assert_eq!(descs[0].1, "binance-usdm:aaausdt");
        assert_eq!(t.hash, core_crypto::sha256(TABLE.as_bytes()));
        assert_eq!(t.rows[1].beta_1e9, 900_000_000);
        // Nothing resolves ⇒ refuse.
        assert!(parse_table(TABLE.as_bytes(), &|_d: &str| None).is_err());
        // Malformed rows refuse.
        assert!(parse_table(b"a\tb\n", &resolver).is_err());
        assert!(parse_table(b"binance-usdm:aaausdt\tbinance-usdm:bbbusdt\t0\n", &resolver).is_err());
        assert!(parse_table(b"binance-usdm:aaausdt\tbinance-usdm:bbbusdt\tx\n", &resolver).is_err());
    }

    #[test]
    fn seed_keeps_only_past_hours_of_known_descriptors() {
        let boot_hour = 496_992i64;
        let text = format!(
            "# descriptor\topen_ms\tclose_1e6\n\
             binance-usdm:aaausdt\t{}\t123456\n\
             binance-usdm:aaausdt\t{}\t123457\n\
             binance-usdm:zzzusdt\t{}\t5\n\
             binance-usdm:bbbusdt\t{}\t1\n",
            (boot_hour - 1) * 3_600_000,
            boot_hour * 3_600_000,
            (boot_hour - 2) * 3_600_000,
            (boot_hour - 3) * 3_600_000,
        );
        let (rows, dropped) = parse_seed(&text, &resolver, boot_hour).unwrap();
        assert_eq!(rows, vec![(make_symbol_id(VenueId::Binance, 512), boot_hour - 1, 123_456)]);
        assert_eq!(dropped, 3, "future hour, unknown descriptor, micro-dollar close");
        assert!(parse_seed("a\tb\n", &resolver, boot_hour).is_err());
    }

    #[test]
    fn state_round_trips_and_refuses_orphans() {
        let hash = [0xab; 32];
        let positions = [XsdPositionView {
            sym: make_symbol_id(VenueId::Binance, 512),
            side: 1,
            d: 1,
            grid_units: 2,
            qty_1e6: 9_500_000,
            notional_1e6: 2_000_000_000,
            entry_hour: 496_990,
            last_add_hour: 496_991,
        }];
        let mut out = String::new();
        render_state(&hash, &|s| if s == make_symbol_id(VenueId::Binance, 512) { Some("binance-usdm:aaausdt") } else { None }, &positions, &mut out);
        assert!(out.contains("H\tabababab"));
        let (h, rows) = parse_state(&out, &resolver).unwrap();
        assert_eq!(h, hash);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sym, make_symbol_id(VenueId::Binance, 512));
        assert_eq!(rows[0].grid_units, 2);
        assert_eq!(rows[0].entry_hour, 496_990);
        // An orphan descriptor refuses.
        let orphan = out.replace("aaausdt", "zzzusdt");
        assert!(parse_state(&orphan, &resolver).is_err());
        // Missing rows / bad version refuse.
        assert!(parse_state("P\tx\n", &resolver).is_err());
        assert!(parse_state("V\t2\nH\tab\n", &resolver).is_err());
        assert!(parse_state(&out.replace("V\t1", "V\t9"), &resolver).is_err());
        // Empty positions still round-trip.
        let mut empty = String::new();
        render_state(&hash, &|_| None, &[], &mut empty);
        let (h2, rows2) = parse_state(&empty, &resolver).unwrap();
        assert_eq!(h2, hash);
        assert!(rows2.is_empty());
    }

    #[test]
    fn bundle_absent_defaults_are_not_configured_and_explicit_absences_refuse() {
        let dir = std::env::temp_dir().join(format!("xsd-boot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("xsd.toml");
        let table = dir.join("xsd-table.tsv");
        let seed = dir.join("xsd-seed.tsv");
        let state = dir.join("xsd-state.tsv");
        // Explicit params path missing ⇒ error.
        assert!(load_xsd_boot(Some(&toml), None, None, None, &resolver, 1).is_err());
        std::fs::write(
            &toml,
            "[xsd]\nz_window_h = 720\nz_enter_1e9 = 3000000000\nz_exit_1e9 = 0\nz_stop_1e9 = 5000000000\n\
             consensus = 1\ngrid_n = 1\ngrid_step_1e9 = 500000000\nmax_hold_s = 864000\ncooldown_s = 3600\n\
             ttl_s = 300\nposition_usd_1e6 = 1000000000\nmax_positions = 82\nmax_gross_usd_1e6 = 100000000000\n\
             direction = 1\n",
        )
        .unwrap();
        // Explicit table missing ⇒ error.
        assert!(load_xsd_boot(Some(&toml), Some(&table), None, None, &resolver, 1).is_err());
        std::fs::write(&table, TABLE).unwrap();
        let b = load_xsd_boot(Some(&toml), Some(&table), Some(&seed), Some(&state), &resolver, 496_992)
            .unwrap_err();
        assert!(b.contains("does not exist"), "explicit seed missing: {b}");
        std::fs::write(&seed, "binance-usdm:aaausdt\t1789167600000\t123456\n").unwrap();
        let b = load_xsd_boot(Some(&toml), Some(&table), Some(&seed), Some(&state), &resolver, 496_992)
            .unwrap()
            .expect("configured");
        assert_eq!(b.params.max_hold_h, 240);
        assert_eq!(b.params.ttl_ns, 300_000_000_000);
        assert_eq!(b.table.n, 3);
        assert_eq!(b.rows_dropped, 1);
        assert_eq!(b.seed.len(), 1);
        assert!(!b.state_present && b.restore.is_empty() && !b.restore_flatten);
        assert_eq!(b.descriptor_of(make_symbol_id(VenueId::Binance, 512)), Some("binance-usdm:aaausdt"));
        assert_eq!(b.descriptor_of(make_symbol_id(VenueId::Binance, 514)), None, "partner-only sym");
        // A state file under the SAME hash resumes; under another hash flattens.
        let mut st = String::new();
        render_state(&b.table.hash, &|s| b.descriptor_of(s), &[XsdPositionView {
            sym: make_symbol_id(VenueId::Binance, 512),
            side: 2,
            d: -1,
            grid_units: 1,
            qty_1e6: 1,
            notional_1e6: 1,
            entry_hour: 1,
            last_add_hour: 1,
        }], &mut st);
        std::fs::write(&state, &st).unwrap();
        let b2 = load_xsd_boot(Some(&toml), Some(&table), Some(&seed), Some(&state), &resolver, 496_992)
            .unwrap()
            .unwrap();
        assert!(b2.state_present && !b2.restore_flatten && b2.restore.len() == 1);
        std::fs::write(&state, st.replace(&hex_lower(&b.table.hash), &hex_lower(&[0x11; 32]))).unwrap();
        let b3 = load_xsd_boot(Some(&toml), Some(&table), Some(&seed), Some(&state), &resolver, 496_992)
            .unwrap()
            .unwrap();
        assert!(b3.restore_flatten && b3.restore.len() == 1);
        // write_state is atomic-by-rename and readable back.
        write_state(&state, "V\t1\nH\t0000000000000000000000000000000000000000000000000000000000000000\n").unwrap();
        assert!(std::fs::read_to_string(&state).unwrap().starts_with("V\t1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
