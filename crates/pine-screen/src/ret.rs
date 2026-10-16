// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The daily return series file (contract §3.16.2, §4.5.2).
//! COPY-DOCTRINE: offline file writer/reader; the bytes are encoded once into the file.

use core_crypto::Sha256;
use std::io::Write;
use std::path::{Path, PathBuf};

/// `<dir>/<hex(trial_id)>.<RET_SERIES_EXT>` (`pine_ir::RET_SERIES_EXT` = `"f64"`).
pub fn ret_path(dir: &Path, trial_id: &[u8; 32]) -> PathBuf {
    let hex = crate::ids::hex32(trial_id);
    let mut name = String::with_capacity(64 + 1 + pine_ir::RET_SERIES_EXT.len());
    let mut i = 0;
    while i < hex.len() {
        name.push(char::from(hex[i]));
        i += 1;
    }
    name.push('.');
    name.push_str(pine_ir::RET_SERIES_EXT);
    dir.join(name)
}

/// SHA-256 of the file bytes the series encodes to (little-endian `f64`, oldest first, no header).
pub fn ret_sha256(days: &[f64]) -> [u8; 32] {
    let mut h = Sha256::new();
    let mut i = 0;
    while i < days.len() {
        h.update(&days[i].to_le_bytes());
        i += 1;
    }
    h.finalize()
}

/// Write atomically (tmp + fsync + rename); returns the SHA-256 the ledger records.
pub fn write_ret(dir: &Path, trial_id: &[u8; 32], days: &[f64]) -> std::io::Result<[u8; 32]> {
    let path = ret_path(dir, trial_id);
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut body = Vec::with_capacity(8 * days.len());
    let mut i = 0;
    while i < days.len() {
        body.extend_from_slice(&days[i].to_le_bytes());
        i += 1;
    }
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&body)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(core_crypto::sha256(&body))
}

/// Read a series back (`len % 8 != 0` → `InvalidData`).
pub fn read_ret(path: &Path) -> std::io::Result<Vec<f64>> {
    let b = std::fs::read(path)?;
    if b.len() % 8 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "return series length is not a multiple of 8",
        ));
    }
    let mut out = Vec::with_capacity(b.len() / 8);
    let mut i = 0;
    while i < b.len() {
        out.push(f64::from_le_bytes([
            b[i],
            b[i + 1],
            b[i + 2],
            b[i + 3],
            b[i + 4],
            b[i + 5],
            b[i + 6],
            b[i + 7],
        ]));
        i += 8;
    }
    Ok(out)
}
