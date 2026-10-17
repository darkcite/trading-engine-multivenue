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

use crate::SlotRoute;

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
    /// The route the boot ran.
    pub route: SlotRoute,
    /// One per manifest script, manifest order.
    pub scripts: Vec<BootRecordScript>,
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
    /// The file's bytes: the magic line, four header lines, one `script` line per script.
    #[must_use]
    pub fn render(&self) -> String {
        let mut s = String::with_capacity(256 + 160 * self.scripts.len());
        s.push_str(BOOT_RECORD_MAGIC);
        s.push_str("\nmanifest_sha256\t");
        hex(&mut s, &self.manifest_sha256);
        s.push_str(&format!(
            "\nrun_epoch_ns\t{}\nboot_mono_ns\t{}\nroute\t{}\n",
            self.run_epoch_ns,
            self.boot_mono_ns,
            route_name(self.route)
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
        s
    }

    /// Strict parse: the magic, the four header keys in order, then `script` lines numbered from
    /// 0 without a gap. Nothing else (blank lines included) is accepted.
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
        let route = match kv("route")?.as_str() {
            "paper" => SlotRoute::Paper,
            "live" => SlotRoute::Live,
            "off" => SlotRoute::Off,
            other => return Err(format!("route `{other}` is not paper|live|off")),
        };
        let mut scripts = Vec::new();
        for l in lines {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() != 8 || f[0] != "script" {
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
        Ok(Self {
            manifest_sha256,
            run_epoch_ns,
            boot_mono_ns,
            route,
            scripts,
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
            route: SlotRoute::Paper,
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
        }
    }

    #[test]
    fn boot_record_round_trips() {
        let r = rec();
        let text = r.render();
        assert!(text.starts_with("# pvm-boot-1\nmanifest_sha256\tabab"));
        assert!(text.contains("\nroute\tpaper\n"));
        assert!(text.contains("\t0123456789abcdef\n"));
        assert_eq!(BootRecord::parse(&text).unwrap(), r);
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
