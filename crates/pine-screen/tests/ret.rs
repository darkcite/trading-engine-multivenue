// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.5 — the daily return series file (contract §3.16.2).

use pine_screen::ids::hex32;
use pine_screen::ret::{read_ret, ret_path, ret_sha256, write_ret};

fn tmp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pine-screen-ret-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("tmp");
    d
}

#[test]
fn ret_file_is_le_f64_without_header() {
    let d = tmp("le");
    let id = [0xabu8; 32];
    let days = [1.5f64, 0.0, -240.25];
    write_ret(&d, &id, &days).expect("write");
    let p = ret_path(&d, &id);
    let name = format!("{}.f64", std::str::from_utf8(&hex32(&id)).expect("hex"));
    assert_eq!(p, d.join(name));
    let b = std::fs::read(&p).expect("read");
    assert_eq!(b.len(), 24);
    assert_eq!(&b[0..8], &1.5f64.to_le_bytes());
    assert_eq!(&b[8..16], &0.0f64.to_le_bytes());
    assert_eq!(&b[16..24], &(-240.25f64).to_le_bytes());
    assert_eq!(read_ret(&p).expect("read"), days.to_vec());
    let mut tmpname = p.as_os_str().to_owned();
    tmpname.push(".tmp");
    assert!(!std::path::Path::new(&tmpname).exists());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ret_sha256_matches_the_file() {
    let d = tmp("sha");
    let id = [3u8; 32];
    let days = [0.25f64, -1.0, 7.0, 0.0];
    let h = write_ret(&d, &id, &days).expect("write");
    let file = std::fs::read(ret_path(&d, &id)).expect("read");
    assert_eq!(h, core_crypto::sha256(&file));
    assert_eq!(h, ret_sha256(&days));
    assert_ne!(ret_sha256(&days[..3]), h);
    assert_eq!(ret_sha256(&[]), core_crypto::sha256(&[]));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn ret_read_refuses_a_ragged_file() {
    let d = tmp("ragged");
    let p = d.join("x.f64");
    std::fs::write(&p, [0u8; 23]).expect("write");
    let e = read_ret(&p).expect_err("ragged");
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    std::fs::write(&p, []).expect("write");
    assert!(read_ret(&p).expect("empty").is_empty());
    let _ = std::fs::remove_dir_all(&d);
}
