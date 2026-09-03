// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Golden-frame vectors shared with the Python worker (Phase 8f item 9).
//!
//! Consumes `claude-worker/tests/fixtures/ai_frame_golden.txt` — the SAME
//! file `claude-worker/tests/test_frames.py` pins its packer against — and
//! asserts that `ingress_ai::pack_frame` reproduces every frame byte for
//! byte and that `parse_frame` round-trips it. If either packer drifts,
//! one of the two suites goes red.
//!
//! Test-only code: allocation and `unwrap` are fine here; the hot path is
//! not involved.

use core_types::{AiCmd, AiCmdKind, VenueId};
use ingress_ai::{pack_frame, parse_frame, FRAME_LEN};

const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../claude-worker/tests/fixtures/ai_frame_golden.txt"
);

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex length");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = (bytes[i] as char).to_digit(16).expect("hex digit");
        let lo = (bytes[i + 1] as char).to_digit(16).expect("hex digit");
        out.push(((hi << 4) | lo) as u8);
        i += 2;
    }
    out
}

struct Vector {
    name: String,
    cmd: AiCmd,
    frame_hex: String,
}

fn load_golden() -> ([u8; 32], Vec<Vector>) {
    let text = std::fs::read_to_string(FIXTURE)
        .unwrap_or_else(|e| panic!("golden fixture missing at {FIXTURE}: {e}"));
    let mut key: Option<[u8; 32]> = None;
    let mut vectors = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts[0] == "key" {
            let k = unhex(parts[1]);
            assert_eq!(k.len(), 32, "key must be 32 bytes");
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&k);
            key = Some(arr);
            continue;
        }
        assert_eq!(parts.len(), 14, "bad golden line: {line}");
        let kind = AiCmdKind::from_u8(parts[7].parse::<u8>().unwrap())
            .unwrap_or_else(|| panic!("unknown kind in vector {}", parts[0]));
        let venue = VenueId::from_u8(parts[8].parse::<u8>().unwrap())
            .unwrap_or_else(|| panic!("unknown venue in vector {}", parts[0]));
        let cmd = AiCmd::new(
            parts[1].parse().unwrap(), // ts_ns
            parts[2].parse().unwrap(), // seq
            parts[3].parse().unwrap(), // sym
            parts[4].parse().unwrap(), // px (signed)
            parts[5].parse().unwrap(), // qty (signed)
            parts[6].parse().unwrap(), // ttl_ns
            kind,
            venue,
            parts[9].parse().unwrap(),  // strategy_id
            parts[10].parse().unwrap(), // side
            parts[11].parse().unwrap(), // param_id
            parts[12].parse().unwrap(), // flags
        );
        vectors.push(Vector {
            name: parts[0].to_string(),
            cmd,
            frame_hex: parts[13].to_string(),
        });
    }
    (key.expect("golden fixture has no key line"), vectors)
}

#[test]
fn golden_fixture_covers_every_kind() {
    let (_key, vectors) = load_golden();
    let mut kinds: Vec<u8> = vectors.iter().map(|v| v.cmd.kind).collect();
    kinds.sort_unstable();
    kinds.dedup();
    // 0..=9 (8f) + 10/11 (VM2 seeds) + 12 (RG0 SetRegime) — every wire
    // kind has at least one shared vector.
    assert_eq!(
        kinds,
        (0u8..=AiCmdKind::SetRegime.to_u8()).collect::<Vec<_>>(),
        "one vector per kind"
    );
}

#[test]
fn rust_packer_reproduces_golden_frames() {
    let (key, vectors) = load_golden();
    for v in &vectors {
        let mut frame = [0u8; FRAME_LEN];
        pack_frame(&key, &v.cmd, &mut frame);
        let want = unhex(&v.frame_hex);
        assert_eq!(want.len(), FRAME_LEN, "vector {}: bad frame length", v.name);
        assert_eq!(
            frame.as_slice(),
            want.as_slice(),
            "vector {}: Rust packer drifted from the shared golden bytes",
            v.name
        );
    }
}

#[test]
fn golden_frames_parse_and_round_trip() {
    let (key, vectors) = load_golden();
    for v in &vectors {
        let mut frame = [0u8; FRAME_LEN];
        let raw = unhex(&v.frame_hex);
        frame.copy_from_slice(&raw);
        let parsed = parse_frame(&key, &frame)
            .unwrap_or_else(|e| panic!("vector {}: golden frame refused: {e:?}", v.name));
        assert_eq!(parsed.ts_ns, v.cmd.ts_ns, "vector {}", v.name);
        assert_eq!(parsed.seq, v.cmd.seq, "vector {}", v.name);
        assert_eq!(parsed.sym, v.cmd.sym, "vector {}", v.name);
        assert_eq!(parsed.px, v.cmd.px, "vector {}", v.name);
        assert_eq!(parsed.qty, v.cmd.qty, "vector {}", v.name);
        assert_eq!(parsed.ttl_ns, v.cmd.ttl_ns, "vector {}", v.name);
        assert_eq!(parsed.kind, v.cmd.kind, "vector {}", v.name);
        assert_eq!(parsed.venue, v.cmd.venue, "vector {}", v.name);
        assert_eq!(parsed.strategy_id, v.cmd.strategy_id, "vector {}", v.name);
        assert_eq!(parsed.side, v.cmd.side, "vector {}", v.name);
        assert_eq!(parsed.param_id, v.cmd.param_id, "vector {}", v.name);
        assert_eq!(parsed.flags, v.cmd.flags, "vector {}", v.name);
    }
}
