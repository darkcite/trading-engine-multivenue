// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! TERM boot wiring (TERM plan §4.1–4.2, slots per O-T17, paper per O-T18).
//!
//! Only when the boot's strategy mask names a TERM slot: read the door key
//! from `~/multivenue/term.env` (R-T13: it must be `0600`; the boot refuses
//! otherwise), attach one ring lane per enabled TERM slot to its member,
//! spawn the `term-door` thread on `~/multivenue/term.sock`. The door's boot
//! epoch IS the engine's `boot.boot_wall_ns`, which `/state` already
//! publishes (T-L11); `~/multivenue/term.epoch` (`0600`) carries the same
//! value for a terminal without a `/state` reader. The returned guard stops the
//! door when the engine loop ends. Cold: boot only, never the hot loop.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ingress_term::listener::{spawn, DoorCfg, Lane};
use ingress_term::{rings, Door};
use strategy_set::{StrategySet, TERM_MASK};

/// A growing command older than this at the door is refused.
const DOOR_MAX_AGE_NS: u64 = 2_000_000_000;

/// Keeps the `term-door` thread up; dropping it stops and joins the door.
pub struct TermDoorGuard {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The boot tell: socket, slots, mode and epoch.
    pub tell: String,
}

impl Drop for TermDoorGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn wall_ns() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

fn clock() -> (u64, u64) {
    (wall_ns(), core_time::now_ns())
}

/// Parse `TERM_DOOR_HMAC_KEY=<64 hex>` out of `term.env`'s text.
pub fn parse_key(text: &str) -> Result<[u8; 32], String> {
    let line = text
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("TERM_DOOR_HMAC_KEY="))
        .ok_or("term.env has no TERM_DOOR_HMAC_KEY line")?;
    let hex = line.trim().trim_matches('"');
    if hex.len() != 64 {
        return Err("TERM_DOOR_HMAC_KEY must be 64 hex characters".into());
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| "TERM_DOOR_HMAC_KEY is not hex")?;
    }
    if k == [0; 32] {
        return Err("TERM_DOOR_HMAC_KEY is all zeros".into());
    }
    Ok(k)
}

/// Read the key, refusing a file anyone but the owner can read (R-T13).
pub fn read_key(path: &std::path::Path) -> Result<[u8; 32], String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(format!("{} must be 0600 (R-T13)", path.display()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse_key(&text)
}

/// Wire TERM if the boot enabled any TERM slot. `dir` is `~/multivenue`.
pub fn wire_in(set: &mut StrategySet, dir: &std::path::Path, epoch: u64) -> Result<Option<TermDoorGuard>, String> {
    let mask = set.enabled_mask() & TERM_MASK;
    if mask == 0 {
        return Ok(None);
    }
    let key = read_key(&dir.join("term.env"))?;
    let mut lanes = Vec::with_capacity(4);
    let mut slots = Vec::with_capacity(4);
    for slot in 12u8..=15 {
        if mask & (1 << slot) == 0 {
            continue;
        }
        let ((reduce, grow, evt), (rr, gr, et)) = rings();
        let m = set.term_mut(slot).ok_or("a TERM slot without a member")?;
        m.attach(rr, gr, et);
        lanes.push(Lane { slot, reduce, grow, evt });
        slots.push(slot);
    }
    let sock: PathBuf = dir.join("term.sock");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(dir.join("term.epoch"))
        .map_err(|e| format!("term.epoch: {e}"))?;
    writeln!(f, "{epoch}").map_err(|e| format!("term.epoch: {e}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn(DoorCfg { path: sock.clone(), door: Door::new(key, epoch, DOOR_MAX_AGE_NS), clock }, lanes, stop.clone())
        .map_err(|e| format!("term door {}: {e}", sock.display()))?;
    let tell = format!("term: door {} · slots {slots:?} · PAPER · epoch {epoch}", sock.display());
    Ok(Some(TermDoorGuard { stop, handle: Some(handle), tell }))
}

/// `wire_in` at `$HOME/multivenue`; `epoch` is `/state`'s `boot.boot_wall_ns`.
pub fn wire(set: &mut StrategySet, epoch: u64) -> Result<Option<TermDoorGuard>, String> {
    if set.enabled_mask() & TERM_MASK == 0 {
        return Ok(None);
    }
    let home = std::env::var_os("HOME").ok_or("HOME is unset")?;
    wire_in(set, &PathBuf::from(home).join("multivenue"), epoch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_parsing_and_permissions() {
        assert!(parse_key("TERM_DOOR_HMAC_KEY=00").is_err());
        assert!(parse_key(&format!("TERM_DOOR_HMAC_KEY={}", "0".repeat(64))).is_err());
        let hex = "ab".repeat(32);
        assert_eq!(parse_key(&format!("# c\nTERM_DOOR_HMAC_KEY=\"{hex}\"\n")).unwrap(), [0xab; 32]);
        let dir = std::env::temp_dir().join(format!("term-boot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("term.env");
        std::fs::write(&p, format!("TERM_DOOR_HMAC_KEY={hex}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_key(&p).unwrap_err().contains("0600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key(&p).unwrap(), [0xab; 32]);
        // No TERM slot enabled: nothing is read or spawned.
        let mut set = StrategySet::new(strategy_set::ALL_MASK);
        assert!(wire_in(&mut set, &dir, 7).unwrap().is_none());
        // TERM enabled: the door comes up on the socket, the epoch is written
        // 0600, and the guard tears the door down.
        let mut set = StrategySet::new(TERM_MASK);
        let g = wire_in(&mut set, &dir, 1_761_980_000_000_000_000).unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("term.epoch")).unwrap().trim(), "1761980000000000000");
        assert!(dir.join("term.sock").exists());
        let em = std::fs::metadata(dir.join("term.epoch")).unwrap().permissions().mode();
        assert_eq!(em & 0o777, 0o600);
        assert!(g.tell.contains("[12, 13, 14, 15]"));
        drop(g);
        assert!(!dir.join("term.sock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
