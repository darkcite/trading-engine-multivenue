// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Known-answer vectors against the official Hyperliquid Python SDK.
//!
//! **This file is the only thing standing between LAW E-3 and a
//! silently rejected order.** The action is hashed as MessagePack,
//! MessagePack preserves insertion order, and the docs do not state the
//! order. So the order is not asserted from a specification — it is
//! asserted against bytes the venue's own signer produced.
//!
//! Each case below is hand-written in Rust and matched by NAME to a row
//! of `fixtures/hl/vectors.tsv`, which
//! `fixtures/hl/gen_vectors.py` produced by driving
//! `hyperliquid.utils.signing` directly. A case with no row fails; a
//! row with no case fails. Four things are compared, and all four must
//! be byte-identical:
//!
//! 1. the msgpack encoding of the action,
//! 2. the connection id (keccak of action ‖ nonce ‖ vault ‖ expiry),
//! 3. the `Agent` EIP-712 digest,
//! 4. the 65-byte `r‖s‖v` signature.
//!
//! The signing key in the fixture is a published, worthless TEST key
//! whose only purpose is to make (4) reproducible.

use exec_hyperliquid::action::{
    encode_batch_modify, encode_cancel, encode_cancel_by_cloid, encode_order, CancelByCloidWire,
    CancelWire, ModifyWire, OrderWire, Tif, MAX_ACTION,
};
use exec_hyperliquid::sign::{connection_id, Network, Vault};
use signer_eip712::hyperliquid as hl;

const TEST_KEY: [u8; 32] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

const VAULT: [u8; 20] = [
    0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef, 0x12, 0x34, 0x56, 0x78, 0x90, 0xab, 0xcd, 0xef,
    0x12, 0x34, 0x56, 0x78,
];

const HIP4_YES: u32 = 100_000_000 + 10 * 3253;
const HIP4_NO: u32 = 100_000_000 + 10 * 3253 + 1;

fn cloid_a() -> [u8; 16] {
    let mut c = [0u8; 16];
    c[15] = 1;
    c
}

fn cloid_b() -> [u8; 16] {
    [
        0x4d, 0x56, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2a,
    ]
}

/// One fixture row.
struct Row {
    source: String,
    nonce: u64,
    vault: Vault,
    expires_after: Option<u64>,
    msgpack: Vec<u8>,
    connection_id: Vec<u8>,
    digest: Vec<u8>,
    sig65: Vec<u8>,
}

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex: {s}");
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

fn load() -> std::collections::HashMap<String, Row> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/hl/vectors.tsv");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f[0] == "name" {
            continue; // header
        }
        assert_eq!(f.len(), 9, "bad row: {line}");
        let vault = if f[3] == "-" {
            Vault::None
        } else {
            let b = unhex(f[3].trim_start_matches("0x"));
            let mut a = [0u8; 20];
            a.copy_from_slice(&b);
            Vault::Address(a)
        };
        out.insert(
            f[0].to_owned(),
            Row {
                source: f[1].to_owned(),
                nonce: f[2].parse().expect("nonce"),
                vault,
                expires_after: if f[4] == "-" {
                    None
                } else {
                    Some(f[4].parse().expect("expires_after"))
                },
                msgpack: unhex(f[5]),
                connection_id: unhex(f[6]),
                digest: unhex(f[7]),
                sig65: unhex(f[8]),
            },
        );
    }
    assert!(out.len() >= 20, "the gate wants >= 20 vectors, found {}", out.len());
    out
}

/// Encode, hash, sign — and compare all four against the SDK.
fn check(rows: &std::collections::HashMap<String, Row>, name: &str, encoded: &[u8]) {
    let r = rows
        .get(name)
        .unwrap_or_else(|| panic!("no fixture row named `{name}`"));

    assert_eq!(
        hex(encoded),
        hex(&r.msgpack),
        "\n[{name}] MSGPACK DIVERGED — this is LAW E-3. \
         A different key order is a different hash is a rejected order.\n"
    );

    let net = if r.source == "a" {
        Network::Mainnet
    } else {
        Network::Testnet
    };
    let cid = connection_id(encoded, r.nonce, r.vault, r.expires_after).expect("connection id");
    assert_eq!(hex(&cid), hex(&r.connection_id), "[{name}] connection id");

    let digest = hl::agent_eip712_hash(net.source(), &cid);
    assert_eq!(hex(&digest), hex(&r.digest), "[{name}] EIP-712 digest");

    let sk = signer_eip712::parse_secret_key(&TEST_KEY).expect("key");
    let sig = hl::sign_agent_with_key(&sk, net.source(), &cid).expect("sign");
    assert_eq!(hex(&sig), hex(&r.sig65), "[{name}] 65-byte signature");
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn order(name: &str, rows: &std::collections::HashMap<String, Row>, wires: &[OrderWire], grouping: &[u8]) {
    let mut buf = [0u8; MAX_ACTION];
    let n = encode_order(&mut buf, wires, grouping).expect("encode");
    check(rows, name, &buf[..n]);
}

#[test]
fn every_vector_reproduces_byte_for_byte() {
    let r = load();
    let mut seen: Vec<&str> = Vec::new();
    let mut hit = |n: &'static str| seen.push(n);

    // --- the three tifs, both sides, with and without cloid ---------
    order("order_gtc_buy", &r, &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc)], b"na");
    hit("order_gtc_buy");
    order("order_ioc_buy", &r, &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Ioc)], b"na");
    hit("order_ioc_buy");
    order("order_alo_buy", &r, &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Alo)], b"na");
    hit("order_alo_buy");
    order("order_gtc_sell", &r, &[OrderWire::new(0, false, 50_000_000, 1_000_000_000, Tif::Gtc)], b"na");
    hit("order_gtc_sell");
    order(
        "order_ioc_cloid",
        &r,
        &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Ioc).with_cloid(cloid_a())],
        b"na",
    );
    hit("order_ioc_cloid");
    order(
        "order_alo_cloid_testnet",
        &r,
        &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Alo).with_cloid(cloid_b())],
        b"na",
    );
    hit("order_alo_cloid_testnet");
    order(
        "order_reduce_only",
        &r,
        &[OrderWire::new(0, false, 50_000_000, 1_000_000_000, Tif::Gtc).reduce_only()],
        b"na",
    );
    hit("order_reduce_only");

    // --- HIP-4: the asset ids and 4dp prices this lane actually sends
    order(
        "hip4_yes_ioc",
        &r,
        &[OrderWire::new(HIP4_YES, true, 45_670_000, 2_500_000_000, Tif::Ioc).with_cloid(cloid_b())],
        b"na",
    );
    hit("hip4_yes_ioc");
    order("hip4_no_alo", &r, &[OrderWire::new(HIP4_NO, false, 99_900_000, 100_000_000, Tif::Alo)], b"na");
    hit("hip4_no_alo");
    order("hip4_min_px", &r, &[OrderWire::new(HIP4_YES, true, 100_000, 1_000_000_000, Tif::Ioc)], b"na");
    hit("hip4_min_px");
    order("hip4_max_px", &r, &[OrderWire::new(HIP4_YES, true, 99_900_000, 1_000_000_000, Tif::Ioc)], b"na");
    hit("hip4_max_px");
    // 0.5000 must render "0.5" — the trailing-zero law, inside a signature.
    order(
        "hip4_trailing_zero_px",
        &r,
        &[OrderWire::new(HIP4_YES, true, 50_000_000, 10_000_000_000, Tif::Gtc)],
        b"na",
    );
    hit("hip4_trailing_zero_px");

    // --- batches ----------------------------------------------------
    order(
        "order_batch_2",
        &r,
        &[
            OrderWire::new(HIP4_YES, true, 40_000_000, 500_000_000, Tif::Ioc),
            OrderWire::new(HIP4_NO, true, 60_000_000, 500_000_000, Tif::Ioc),
        ],
        b"na",
    );
    hit("order_batch_2");
    order(
        "order_batch_3_mixed",
        &r,
        &[
            OrderWire::new(0, true, 50_000_000, 100_000_000, Tif::Gtc).with_cloid(cloid_a()),
            OrderWire::new(1, false, 150_000_000, 200_000_000, Tif::Ioc).reduce_only(),
            OrderWire::new(HIP4_YES, true, 25_000_000, 300_000_000, Tif::Alo).with_cloid(cloid_b()),
        ],
        b"na",
    );
    hit("order_batch_3_mixed");
    order(
        "order_grouping_tpsl",
        &r,
        &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc)],
        b"normalTpsl",
    );
    hit("order_grouping_tpsl");

    // --- the two tail bytes of the action hash -----------------------
    for (name, vault, exp) in [
        ("order_vault", Vault::Address(VAULT), None),
        ("order_expires", Vault::None, Some(1_789_000_000_000u64)),
        ("order_vault_expires", Vault::Address(VAULT), Some(1_789_000_000_000u64)),
    ] {
        let row = r.get(name).unwrap_or_else(|| panic!("no row {name}"));
        assert_eq!(row.vault, vault, "[{name}] fixture vault");
        assert_eq!(row.expires_after, exp, "[{name}] fixture expiry");
        let mut buf = [0u8; MAX_ACTION];
        let n = encode_order(
            &mut buf,
            &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc)],
            b"na",
        )
        .expect("encode");
        check(&r, name, &buf[..n]);
        seen.push(name);
    }

    // --- cancels -----------------------------------------------------
    let mut buf = [0u8; MAX_ACTION];
    let n = encode_cancel(&mut buf, &[CancelWire { asset: 0, oid: 12_345 }]).unwrap();
    check(&r, "cancel_single", &buf[..n]);
    seen.push("cancel_single");

    let n = encode_cancel(
        &mut buf,
        &[
            CancelWire { asset: 0, oid: 1 },
            CancelWire { asset: 1, oid: 2 },
            CancelWire { asset: HIP4_YES, oid: 99_999_999 },
        ],
    )
    .unwrap();
    check(&r, "cancel_batch_3", &buf[..n]);
    seen.push("cancel_batch_3");

    let n = encode_cancel(&mut buf, &[CancelWire { asset: 0, oid: 7 }]).unwrap();
    check(&r, "cancel_testnet", &buf[..n]);
    seen.push("cancel_testnet");

    let n = encode_cancel_by_cloid(
        &mut buf,
        &[CancelByCloidWire { asset: HIP4_YES, cloid: cloid_b() }],
    )
    .unwrap();
    check(&r, "cancel_by_cloid", &buf[..n]);
    seen.push("cancel_by_cloid");

    let n = encode_cancel_by_cloid(
        &mut buf,
        &[
            CancelByCloidWire { asset: 0, cloid: cloid_a() },
            CancelByCloidWire { asset: HIP4_NO, cloid: cloid_b() },
        ],
    )
    .unwrap();
    check(&r, "cancel_by_cloid_batch_2", &buf[..n]);
    seen.push("cancel_by_cloid_batch_2");

    // --- modify -------------------------------------------------------
    let n = encode_batch_modify(
        &mut buf,
        &[ModifyWire {
            order: OrderWire::new(HIP4_YES, true, 48_000_000, 2_500_000_000, Tif::Alo)
                .with_cloid(cloid_b()),
            oid: 555,
            oid_cloid: [0u8; 16],
            oid_is_cloid: false,
        }],
    )
    .unwrap();
    check(&r, "batch_modify_1", &buf[..n]);
    seen.push("batch_modify_1");

    let n = encode_batch_modify(
        &mut buf,
        &[
            ModifyWire {
                order: OrderWire::new(0, true, 50_000_000, 100_000_000, Tif::Gtc),
                oid: 1,
                oid_cloid: [0u8; 16],
                oid_is_cloid: false,
            },
            ModifyWire {
                order: OrderWire::new(1, false, 250_000_000, 300_000_000, Tif::Ioc)
                    .with_cloid(cloid_a()),
                oid: 0,
                oid_cloid: cloid_a(),
                oid_is_cloid: true,
            },
        ],
    )
    .unwrap();
    check(&r, "batch_modify_2", &buf[..n]);
    seen.push("batch_modify_2");

    // Every fixture row must have been exercised: a row nobody checks
    // is a vector that proves nothing.
    let mut missing: Vec<&String> = r.keys().filter(|k| !seen.contains(&k.as_str())).collect();
    missing.sort();
    assert!(missing.is_empty(), "fixture rows never checked: {missing:?}");
    assert_eq!(seen.len(), r.len());
}
