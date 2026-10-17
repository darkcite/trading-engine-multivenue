// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-Bp §3.10 / impl-08 §3.7 R5: the boot record `pine-boot.tsv` (`# pvm-boot-1`) — rendered
//! by the engine host after `on_start` (`PineStrategy::boot_record`), parsed by a replay host's
//! `configure` (`PineBootCfg::boot_record`).
//!
//! COPY-DOCTRINE: boot only (the line after `on_start` and a replay's `configure`); allocates.
//!
//! Literal readings:
//! - pin:08-boot-record-decimal: `run_epoch_ns`, `boot_mono_ns`, the three ×1e6 values and
//!   `client_oid_base` are decimal; `manifest_sha256` / `<sha256>` are 64 lowercase hex,
//!   `warmup_digest_hex16` 16 lowercase hex. Fields are TAB-separated, one record per line.
//! - pin:08-boot-record-route-off: §3.10 names `paper|live`; a slot resolved `Off` renders `off`
//!   (the record states the route the boot ran; nothing is dispatched on it).
//! - pin:08-oid-base-script-word: `client_oid_base` = `epoch << 32 | script << 29` — the engine
//!   client oid with broker oid 0 (§4.5.3), so `oid = base + broker_oid`.
//! - pin:08-boot-record-warm-digest-chart-row: a script's `warmup_digest_hex16` is its CHART row's
//!   warm slice digest, first 8 bytes little-endian (`BarFileHdr::src_digest64`, what the cli's
//!   `warm` puts there); `0` for a cold row.
//! - pin:08-boot-record-facts (lead ruling 2026-10-17): the record carries the per-symbol facts
//!   the boot ran on, so `pine-verify` rebuilds the boot from the record and never re-reads a
//!   venue. The magic stays `# pvm-boot-1`; the header and `script` lines are unchanged; after
//!   the `script` lines come one `symbol` line per `PineBootCfg::symbols` entry (configure
//!   order): `symbol <key> <sym> <venue byte> <mintick_1e6> <pointvalue_1e6> <qty_step_1e6>
//!   <min_notional_1e6> <ticker> <basecurrency> <currency> <type> <venue_read 0|1>` (decimal;
//!   strings verbatim, never containing a TAB or a newline). A replay host (B'3 tier 2) keeps
//!   its screen-universe facts (pin:08-replay-facts-screen-universe); these lines are 8g's.
//! - pin:08-verify-queue-sym-ineligible (lead ruling 2026-10-17): then exactly one
//!   `queue_syms <sym,sym,…>` line — the engine symbols the paper dispatcher's queue law tracks
//!   at this boot (`-` for none). The member does not know them; the host sets them.
//! - pin:08-boot-record-exec-sha: then exactly one `exec_sha256 <64 hex>` line (`PineBootCfg::
//!   exec_sha256`, all zeros without `--exec`): 8g rule 3's `exec.toml` re-hash compares with it
//!   (no decision record carries it). The three trailing kinds are mandatory, in this order, so
//!   a record written before them fails the strict parse (fail closed).
//! - pin:08-boot-record-boot-utc-ms (R5b, risk r21; retires pin:Bp-8g-boot-utc-ms): the header
//!   carries the paper boot's `boot_utc_ms` (`PineBootCfg::boot_utc_ms`, decimal i64 — the live
//!   tape directory's name and `BOOT_HDR`'s `aux`) as a fifth key between `boot_mono_ns` and
//!   `route`. The extension rule is pin:08-boot-record-facts': the magic stays `# pvm-boot-1`,
//!   the line is mandatory, so a record written before it fails the strict parse (fail closed).
//! - pin:08-boot-record-anchor (lead ruling R3, 2026-10-17; retires pin:Bp-recorded-wall-map):
//!   the header carries the live member's exact wall anchor — the `WallAnchor` pair its
//!   `PineBootCfg::anchor` held (`mono_ns`, `wall_ns`, decimal u64, ns) — as two keys
//!   `anchor_mono_ns`, `anchor_wall_ns` right after `route`; replay-from-boot maps a capture
//!   stamp `ts` to wall time as `anchor_wall + (ts − anchor_mono)` (`WallAnchor::wall_of`), the
//!   live member's own map. Same extension rule: the magic stays `# pvm-boot-1`, both lines are
//!   mandatory, so a record written before them fails the strict parse (fail closed).

use crate::{PineSymbolFacts, SlotRoute};

/// The record's first line.
pub const BOOT_RECORD_MAGIC: &str = "# pvm-boot-1";
/// The file name in the run's capture directory (impl-Bp §8: `<run dir>/pine-boot.tsv`).
pub const BOOT_RECORD_FILE: &str = "pine-boot.tsv";

/// One `script` line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootRecordScript {
    /// Manifest index.
    pub idx: u8,
    /// The script source's SHA-256.
    pub sha256: [u8; 32],
    /// `reconcile_boot`'s position ×1e6.
    pub position_1e6: i64,
    /// `reconcile_boot`'s average price ×1e6.
    pub avg_px_1e6: i64,
    /// `reconcile_boot`'s day notional ×1e6.
    pub day_notional_1e6: i64,
    /// pin:08-oid-base-script-word.
    pub client_oid_base: u64,
    /// pin:08-boot-record-warm-digest-chart-row.
    pub warmup_digest: u64,
}

/// `pine-boot.tsv`, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootRecord {
    /// SHA-256 of the manifest bytes.
    pub manifest_sha256: [u8; 32],
    /// The run directory (`run-<epoch_ns>`) this boot captures into.
    pub run_epoch_ns: u64,
    /// Engine monotonic clock when slot 8 finished `on_start`.
    pub boot_mono_ns: u64,
    /// pin:08-boot-record-boot-utc-ms: the boot instant, UTC ms (the live tape's label).
    pub boot_utc_ms: i64,
    /// The route the boot ran.
    pub route: SlotRoute,
    /// pin:08-boot-record-anchor: the live member's wall anchor (`PineBootCfg::anchor`).
    pub anchor: core_time::WallAnchor,
    /// One per manifest script, manifest order.
    pub scripts: Vec<BootRecordScript>,
    /// pin:08-boot-record-facts: the symbol facts the boot ran on (configure order).
    pub symbols: Vec<PineSymbolFacts>,
    /// pin:08-verify-queue-sym-ineligible: the symbols the paper queue law tracked (host-set).
    pub queue_syms: Vec<core_types::SymbolId>,
    /// pin:08-boot-record-exec-sha: SHA-256 of the boot's `exec.toml` (zeros without one).
    pub exec_sha256: [u8; 32],
}

fn hex(out: &mut String, b: &[u8]) {
    const H: &[u8; 16] = b"0123456789abcdef";
    for x in b {
        out.push(char::from(H[usize::from(x >> 4)]));
        out.push(char::from(H[usize::from(x & 15)]));
    }
}

fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    let b = s.as_bytes();
    if b.len() != 2 * N {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (k, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * k])? << 4) | nib(b[2 * k + 1])?;
    }
    Some(out)
}

const fn route_name(r: SlotRoute) -> &'static str {
    match r {
        SlotRoute::Off => "off",
        SlotRoute::Paper => "paper",
        SlotRoute::Live => "live",
    }
}

impl BootRecord {
    /// The file's bytes: the magic line, five header lines, one `script` line per script, the
    /// `symbol` lines, `queue_syms`, `exec_sha256`.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(256 + 160 * self.scripts.len());
        s.push_str(BOOT_RECORD_MAGIC);
        s.push_str("\nmanifest_sha256\t");
        hex(&mut s, &self.manifest_sha256);
        s.push_str(&format!(
            "\nrun_epoch_ns\t{}\nboot_mono_ns\t{}\nboot_utc_ms\t{}\nroute\t{}\n",
            self.run_epoch_ns,
            self.boot_mono_ns,
            self.boot_utc_ms,
            route_name(self.route)
        ));
        // pin:08-boot-record-anchor.
        s.push_str(&format!(
            "anchor_mono_ns\t{}\nanchor_wall_ns\t{}\n",
            self.anchor.mono_ns, self.anchor.wall_ns
        ));
        for sc in &self.scripts {
            s.push_str(&format!("script\t{}\t", sc.idx));
            hex(&mut s, &sc.sha256);
            s.push_str(&format!(
                "\t{}\t{}\t{}\t{}\t",
                sc.position_1e6, sc.avg_px_1e6, sc.day_notional_1e6, sc.client_oid_base
            ));
            hex(&mut s, &sc.warmup_digest.to_be_bytes());
            s.push('\n');
        }
        // pin:08-boot-record-facts.
        for f in &self.symbols {
            debug_assert!(
                f.syminfo_strs
                    .iter()
                    .chain(core::iter::once(&f.key))
                    .all(|x| !x.contains(['\t', '\n', '\r'])),
                "pine: a boot-record string holds a TAB or a newline"
            );
            s.push_str(&format!(
                "symbol\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                f.key,
                f.sym,
                f.venue as u8,
                f.mintick_1e6,
                f.pointvalue_1e6,
                f.qty_step_1e6,
                f.min_notional_1e6,
                f.syminfo_strs[0],
                f.syminfo_strs[1],
                f.syminfo_strs[2],
                f.syminfo_strs[3],
                u8::from(f.venue_read)
            ));
        }
        // pin:08-verify-queue-sym-ineligible.
        s.push_str("queue_syms\t");
        if self.queue_syms.is_empty() {
            s.push('-');
        }
        for (k, q) in self.queue_syms.iter().enumerate() {
            if k > 0 {
                s.push(',');
            }
            s.push_str(&q.to_string());
        }
        // pin:08-boot-record-exec-sha.
        s.push_str("\nexec_sha256\t");
        hex(&mut s, &self.exec_sha256);
        s.push('\n');
        s
    }

    /// pin:08-boot-record-facts: one `symbol` line's 12 fields after the kind.
    fn parse_symbol(f: &[&str]) -> Result<PineSymbolFacts, String> {
        let int = |s: &str, what: &str| {
            s.parse::<i64>()
                .map_err(|_| format!("symbol {what} is not an i64"))
        };
        if f[0].is_empty() {
            return Err("symbol key is empty".to_string());
        }
        let venue = f[2]
            .parse::<u8>()
            .ok()
            .and_then(core_types::VenueId::from_u8)
            .ok_or_else(|| format!("symbol venue `{}` is not a venue byte", f[2]))?;
        let venue_read = match f[11] {
            "0" => false,
            "1" => true,
            v => return Err(format!("symbol venue_read `{v}` is not 0|1")),
        };
        Ok(PineSymbolFacts {
            key: f[0].to_string(),
            sym: f[1]
                .parse::<core_types::SymbolId>()
                .map_err(|_| "symbol sym is not a u32")?,
            venue,
            mintick_1e6: int(f[3], "mintick_1e6")?,
            pointvalue_1e6: int(f[4], "pointvalue_1e6")?,
            qty_step_1e6: int(f[5], "qty_step_1e6")?,
            min_notional_1e6: int(f[6], "min_notional_1e6")?,
            syminfo_strs: [
                f[7].to_string(),
                f[8].to_string(),
                f[9].to_string(),
                f[10].to_string(),
            ],
            venue_read,
        })
    }

    /// pin:08-verify-queue-sym-ineligible: `-` or a comma list of decimal symbols.
    fn parse_queue(v: &str) -> Result<Vec<core_types::SymbolId>, String> {
        if v == "-" {
            return Ok(Vec::new());
        }
        v.split(',')
            .map(|q| {
                q.parse::<core_types::SymbolId>()
                    .map_err(|_| format!("queue_syms `{q}` is not a u32"))
            })
            .collect()
    }

    /// Strict parse: the magic, the five header keys in order, then `script` lines numbered from
    /// 0 without a gap, the `symbol` lines, `queue_syms`, `exec_sha256`. Nothing else (blank
    /// lines included) is accepted.
    ///
    /// # Errors
    /// The first malformed line, human-readable.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        if lines.next() != Some(BOOT_RECORD_MAGIC) {
            return Err(format!("first line is not `{BOOT_RECORD_MAGIC}`"));
        }
        let mut kv = |key: &str| -> Result<String, String> {
            let l = lines.next().ok_or_else(|| format!("missing `{key}`"))?;
            let (k, v) = l
                .split_once('\t')
                .ok_or_else(|| format!("`{key}` line has no TAB"))?;
            if k != key {
                return Err(format!("expected `{key}`, found `{k}`"));
            }
            Ok(v.to_string())
        };
        let manifest_sha256 = unhex::<32>(&kv("manifest_sha256")?)
            .ok_or("manifest_sha256 is not 64 lowercase hex")?;
        let run_epoch_ns = kv("run_epoch_ns")?
            .parse::<u64>()
            .map_err(|_| "run_epoch_ns is not a u64")?;
        let boot_mono_ns = kv("boot_mono_ns")?
            .parse::<u64>()
            .map_err(|_| "boot_mono_ns is not a u64")?;
        // pin:08-boot-record-boot-utc-ms: mandatory (an older record fails closed).
        let boot_utc_ms = kv("boot_utc_ms")?
            .parse::<i64>()
            .map_err(|_| "boot_utc_ms is not an i64")?;
        let route = match kv("route")?.as_str() {
            "paper" => SlotRoute::Paper,
            "live" => SlotRoute::Live,
            "off" => SlotRoute::Off,
            other => return Err(format!("route `{other}` is not paper|live|off")),
        };
        // pin:08-boot-record-anchor: mandatory (an older record fails closed).
        let anchor_mono_ns = kv("anchor_mono_ns")?
            .parse::<u64>()
            .map_err(|_| "anchor_mono_ns is not a u64")?;
        let anchor_wall_ns = kv("anchor_wall_ns")?
            .parse::<u64>()
            .map_err(|_| "anchor_wall_ns is not a u64")?;
        let anchor = core_time::WallAnchor::new(anchor_mono_ns, anchor_wall_ns);
        let mut scripts = Vec::new();
        let mut symbols = Vec::new();
        let mut queue_syms: Option<Vec<core_types::SymbolId>> = None;
        let mut exec_sha256: Option<[u8; 32]> = None;
        for l in lines {
            let f: Vec<&str> = l.split('\t').collect();
            // pin:08-boot-record-facts: `script*`, `symbol*`, `queue_syms`, `exec_sha256` — in
            // that order, the last two exactly once, nothing after.
            if exec_sha256.is_some() {
                return Err(format!("a line after exec_sha256: `{l}`"));
            }
            if queue_syms.is_some() {
                if f.len() != 2 || f[0] != "exec_sha256" {
                    return Err(format!("expected `exec_sha256`, found `{l}`"));
                }
                exec_sha256 = Some(unhex::<32>(f[1]).ok_or("exec_sha256 is not 64 lowercase hex")?);
                continue;
            }
            if f[0] == "queue_syms" {
                if f.len() != 2 {
                    return Err(format!("not a queue_syms line: `{l}`"));
                }
                queue_syms = Some(Self::parse_queue(f[1])?);
                continue;
            }
            if f[0] == "symbol" {
                if f.len() != 13 {
                    return Err(format!("not a symbol line: `{l}`"));
                }
                symbols.push(Self::parse_symbol(&f[1..])?);
                continue;
            }
            if f.len() != 8 || f[0] != "script" || !symbols.is_empty() {
                return Err(format!("not a script line: `{l}`"));
            }
            let idx = f[1].parse::<u8>().map_err(|_| "script idx is not a u8")?;
            if usize::from(idx) != scripts.len() {
                return Err(format!(
                    "script idx {idx} out of order (expected {})",
                    scripts.len()
                ));
            }
            let int = |s: &str, what: &str| {
                s.parse::<i64>()
                    .map_err(|_| format!("{what} is not an i64"))
            };
            scripts.push(BootRecordScript {
                idx,
                sha256: unhex::<32>(f[2]).ok_or("script sha256 is not 64 lowercase hex")?,
                position_1e6: int(f[3], "position_1e6")?,
                avg_px_1e6: int(f[4], "avg_px_1e6")?,
                day_notional_1e6: int(f[5], "day_notional_1e6")?,
                client_oid_base: f[6]
                    .parse::<u64>()
                    .map_err(|_| "client_oid_base is not a u64")?,
                warmup_digest: u64::from_be_bytes(
                    unhex::<8>(f[7]).ok_or("warmup_digest_hex16 is not 16 lowercase hex")?,
                ),
            });
        }
        let queue_syms = queue_syms.ok_or("missing `queue_syms`")?;
        let exec_sha256 = exec_sha256.ok_or("missing `exec_sha256`")?;
        Ok(Self {
            manifest_sha256,
            run_epoch_ns,
            boot_mono_ns,
            boot_utc_ms,
            route,
            anchor,
            scripts,
            symbols,
            queue_syms,
            exec_sha256,
        })
    }
}

/// pin:08-oid-base-script-word.
#[must_use]
pub const fn client_oid_base(epoch: u32, script: u8) -> u64 {
    ((epoch as u64) << 32) | ((script as u64) << crate::oid::BROKER_OID_BITS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> BootRecord {
        BootRecord {
            manifest_sha256: [0xab; 32],
            run_epoch_ns: 1_760_000_000_123_456_789,
            boot_mono_ns: 42,
            boot_utc_ms: 1_760_000_000_124,
            route: SlotRoute::Paper,
            anchor: core_time::WallAnchor::new(31_000_000_017, 1_760_000_000_123_999_001),
            scripts: vec![
                BootRecordScript {
                    idx: 0,
                    sha256: [1; 32],
                    position_1e6: 0,
                    avg_px_1e6: 0,
                    day_notional_1e6: 0,
                    client_oid_base: client_oid_base(1_760_000_000, 0),
                    warmup_digest: 0x0123_4567_89ab_cdef,
                },
                BootRecordScript {
                    idx: 1,
                    sha256: [2; 32],
                    position_1e6: -5,
                    avg_px_1e6: 7,
                    day_notional_1e6: 9,
                    client_oid_base: client_oid_base(1_760_000_000, 1),
                    warmup_digest: 0,
                },
            ],
            symbols: vec![
                PineSymbolFacts {
                    key: "hyperliquid:btc".to_string(),
                    sym: 4096,
                    venue: core_types::VenueId::Hyperliquid,
                    mintick_1e6: 100_000,
                    pointvalue_1e6: 1_000_000,
                    qty_step_1e6: 10,
                    min_notional_1e6: 10_000_000,
                    syminfo_strs: ["BTC", "BTC", "USDC", "crypto"].map(String::from),
                    venue_read: true,
                },
                PineSymbolFacts {
                    key: "binance_usdm:ethusdt".to_string(),
                    sym: 101,
                    venue: core_types::VenueId::Binance,
                    mintick_1e6: 10_000,
                    pointvalue_1e6: 1_000_000,
                    qty_step_1e6: 0,
                    min_notional_1e6: 20_000_000,
                    syminfo_strs: ["ETHUSDT", "ETH", "USDT", "crypto"].map(String::from),
                    venue_read: false,
                },
            ],
            queue_syms: vec![4096, 4097],
            exec_sha256: [0xcd; 32],
        }
    }

    #[test]
    fn boot_record_round_trips() {
        let r = rec();
        let text = r.render();
        assert!(text.starts_with("# pvm-boot-1\nmanifest_sha256\tabab"));
        assert!(text.contains("\nboot_mono_ns\t42\nboot_utc_ms\t1760000000124\nroute\tpaper\n"));
        assert!(text.contains("\t0123456789abcdef\n"));
        assert_eq!(BootRecord::parse(&text).unwrap(), r);
    }

    /// pin:08-boot-record-anchor: the live wall anchor (ns, exact) right after `route`; both
    /// keys mandatory, in place, u64 — a record without them fails closed.
    #[test]
    fn boot_record_carries_the_live_wall_anchor() {
        let r = rec();
        let text = r.render();
        assert!(text.contains(
            "\nroute\tpaper\nanchor_mono_ns\t31000000017\nanchor_wall_ns\t1760000000123999001\nscript\t0\t"
        ), "{text}");
        assert_eq!(BootRecord::parse(&text).unwrap().anchor, r.anchor);
        for bad in [
            text.replacen("anchor_mono_ns\t31000000017\n", "", 1),
            text.replacen("anchor_wall_ns\t1760000000123999001\n", "", 1),
            text.replacen("anchor_mono_ns\t31000000017", "anchor_mono_ns\t-1", 1),
            text.replacen(
                "anchor_wall_ns\t1760000000123999001",
                "anchor_wall_ns\t1.5",
                1,
            ),
            text.replacen(
                "anchor_mono_ns\t31000000017\nanchor_wall_ns\t1760000000123999001",
                "anchor_wall_ns\t1760000000123999001\nanchor_mono_ns\t31000000017",
                1,
            ),
        ] {
            assert_ne!(bad, text);
            assert!(BootRecord::parse(&bad).is_err(), "accepted: {bad}");
        }
    }

    /// pin:08-boot-record-facts / pin:08-verify-queue-sym-ineligible / pin:08-boot-record-exec-sha:
    /// the facts, the queue symbols and the exec digest come back exactly; none is `-`.
    #[test]
    fn boot_record_carries_facts_queue_syms_and_exec_sha() {
        let r = rec();
        let text = r.render();
        assert!(text.contains(
            "\nsymbol\thyperliquid:btc\t4096\t4\t100000\t1000000\t10\t10000000\tBTC\tBTC\tUSDC\tcrypto\t1\n"
        ), "{text}");
        assert!(text.contains("\nqueue_syms\t4096,4097\nexec_sha256\tcdcd"));
        assert!(text.ends_with(&format!("{}\n", "cd".repeat(32))));
        let back = BootRecord::parse(&text).unwrap();
        assert_eq!(back.symbols, r.symbols);
        assert_eq!(back.queue_syms, vec![4096, 4097]);
        let mut none = r.clone();
        none.queue_syms.clear();
        none.symbols.clear();
        let t = none.render();
        assert!(t.contains("\nqueue_syms\t-\n"), "{t}");
        assert_eq!(BootRecord::parse(&t).unwrap(), none);
    }

    #[test]
    fn boot_record_parse_is_strict() {
        let good = rec().render();
        for bad in [
            good.replacen("# pvm-boot-1", "# pvm-boot-2", 1),
            good.replacen("route\tpaper", "route\tlimbo", 1),
            good.replacen("script\t1\t", "script\t2\t", 1),
            good.replacen("0123456789abcdef", "0123456789ABCDEF", 1),
            good.replacen("boot_mono_ns", "boot_mono", 1),
            format!("{good}\n"),
            // pin:08-boot-record-boot-utc-ms: mandatory, in place, an i64.
            good.replacen("boot_utc_ms\t1760000000124\n", "", 1),
            good.replacen("boot_utc_ms\t1760000000124", "boot_utc_ms\t1.5", 1),
            good.replacen(
                "boot_utc_ms\t1760000000124\nroute\tpaper",
                "route\tpaper\nboot_utc_ms\t1760000000124",
                1,
            ),
            // pin:08-boot-record-facts: the trailing kinds are mandatory, ordered and typed.
            good.replacen("queue_syms\t4096,4097\n", "", 1),
            good.replacen(&format!("exec_sha256\t{}\n", "cd".repeat(32)), "", 1),
            good.replacen("queue_syms\t4096,4097", "queue_syms\t", 1),
            good.replacen("queue_syms\t4096,4097", "queue_syms\t4096,x", 1),
            good.replacen("\t4\t100000\t", "\t250\t100000\t", 1),
            good.replacen("\tcrypto\t1\n", "\tcrypto\t2\n", 1),
            good.replacen("\tcrypto\t1\n", "\tcrypto\n", 1),
            good.replacen("symbol\thyperliquid:btc\t", "symbol\t\t", 1),
            format!("{good}queue_syms\t-\n"),
            // A well-formed, in-sequence script line after the symbol lines.
            good.replacen(
                "queue_syms\t",
                &format!(
                    "script\t2\t{}\t0\t0\t0\t0\t{}\nqueue_syms\t",
                    "0".repeat(64),
                    "0".repeat(16)
                ),
                1,
            ),
            // The pre-extension record (no trailing kinds) fails closed.
            good.split("symbol\t")
                .next()
                .unwrap_or_default()
                .to_string(),
        ] {
            assert!(BootRecord::parse(&bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn oid_base_is_the_broker_oid_zero_client_oid() {
        let b = client_oid_base(77, 3);
        assert_eq!(b + 5, crate::oid::encode_client_oid(77, 3, 5));
    }
}
