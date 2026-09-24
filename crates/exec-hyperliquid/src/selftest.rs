// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The binary certifying its own signing chain, offline, before it
//! talks to anyone.
//!
//! COPY-DOCTRINE: a boot self-test over 25 embedded vectors, run once
//! before the arm exists and never again; every copy in it is cold.
//! `scripts/copy-audit.sh` skips this module on that line.
//!
//! ## The gap this closes
//!
//! [`crate::smoke`] proves the venue verifies a signature this binary
//! produced — but it proves it for the ONE action it sends, a cancel.
//! And msgpack key order is **per action type**: `OrderWire` and
//! `CancelWire` have separate encoders with separate key sequences. So
//! a rebuild that reordered two fields of `OrderWire` would leave the
//! cancel probe perfectly green while every order this engine ever
//! sent was rejected with a valid signature over a digest the venue
//! never computed. The network probe cannot see that, by construction.
//!
//! What can see it is the 25 known-answer vectors the official
//! `hyperliquid-python-sdk` generated. They already guard CI through
//! `tests/hl_vectors.rs` — but `cargo nextest` is not what runs before
//! a restart. **So the vectors are embedded in the binary** and checked
//! by the same gate, which makes the release artifact self-certifying
//! rather than certified by a test suite that may not have been run
//! against it.
//!
//! ## Two checks, and they cover different halves
//!
//! 1. **The chain, over all 27 rows.** For every vector, recompute the
//!    connection id, the `Agent` EIP-712 digest and the 65-byte
//!    signature from the SDK's *recorded* msgpack. This covers keccak,
//!    the domain separator, the nonce/vault/expiry framing and
//!    secp256k1 — but takes the action bytes as given, so it cannot
//!    catch a key-order change.
//! 2. **The encoders, over all five action types.** Rebuild the
//!    msgpack from inputs written out below and compare it to the
//!    recorded bytes. This is the half that catches LAW E-3, and it is
//!    why one case per action type is the minimum: `order`, `cancel`,
//!    `cancelByCloid`, `batchModify` (whose payload nests an
//!    `OrderWire`, so it covers the order key order a second time) and
//!    `reserveRequestWeight` (S7-L1, the request-budget top-up).
//!
//! The inputs below are a second statement of what `gen_vectors.py`
//! fed the SDK. They cannot drift silently: if they ever stop matching
//! the fixture, check 2 fails — here, in CI, and in the pre-restart
//! gate.
//!
//! Nothing here touches the network and nothing here reads the
//! operator's key. It runs on the published, worthless TEST key whose
//! only job is to make the signature reproducible.

use crate::action::{
    encode_batch_modify, encode_cancel, encode_cancel_by_cloid, CancelByCloidWire, CancelWire,
    ModifyWire, OrderWire, Tif, MAX_ACTION,
};
use crate::sign::{connection_id, Network, Vault};

/// The SDK-generated vectors, embedded at build time.
///
/// `include_str!` rather than a runtime read: a gate that can be
/// defeated by moving a file is not a gate.
pub const VECTORS: &str = include_str!("../tests/fixtures/hl/vectors.tsv");

/// The published TEST key from `gen_vectors.py`. Worthless by design;
/// its only purpose is to make signatures reproducible.
const TEST_KEY: [u8; 32] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];

/// HIP-4 YES leg of outcome 3253 — the asset ids this lane actually
/// sends, not a round number that would hide an arithmetic bug.
const HIP4_YES: u32 = 100_000_000 + 10 * 3253;

/// The fixture's row count, EXACTLY. The header says "all 27", so the
/// gate asserts 27 — a `>= 20` (the first cut) let five rows vanish
/// with both gates still green, which is a claim the code did not
/// test. Regenerating the fixture with more vectors moves this number
/// on purpose (S7-L1 moved it from 25 for the two request-weight rows).
pub const VECTOR_ROWS: u32 = 27;

/// Why the binary failed to certify itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelfTestErr {
    /// The embedded fixture is unusable.
    Fixture(&'static str),
    /// A row's recomputed chain diverged from the SDK's.
    Chain {
        /// Which vector.
        name: String,
        /// Which of the three stages.
        stage: &'static str,
    },
    /// **LAW E-3.** An encoder no longer produces the venue's bytes.
    KeyOrder(&'static str),
    /// An encoder overflowed its buffer.
    Encode(&'static str),
}

impl core::fmt::Display for SelfTestErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SelfTestErr::Fixture(w) => {
                write!(f, "hl self-test: the embedded SDK vectors are unusable ({w})")
            }
            SelfTestErr::Chain { name, stage } => write!(
                f,
                "hl self-test: vector {name:?} diverged at the {stage}. This binary's signing \
                 chain no longer agrees with the venue's SDK."
            ),
            SelfTestErr::KeyOrder(name) => write!(
                f,
                "hl self-test: LAW E-3 — the {name} encoder no longer produces the SDK's msgpack. \
                 A different key order is a different hash is a rejected order, and the rejection \
                 carries no hint: the signature is valid, just over a digest the venue did not \
                 compute. This binary must not sign anything."
            ),
            SelfTestErr::Encode(w) => {
                write!(f, "hl self-test: the {w} action did not fit its buffer")
            }
        }
    }
}

impl std::error::Error for SelfTestErr {}

/// What a completed self-test covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelfTestReport {
    /// Vectors whose chain was recomputed and matched.
    pub rows: u32,
    /// Action types whose msgpack was rebuilt from inputs and matched.
    pub encoders: u32,
}

/// Certify this binary against the embedded SDK vectors.
///
/// Offline. No network, no operator key, no filesystem.
pub fn run() -> Result<SelfTestReport, SelfTestErr> {
    let mut rows = 0u32;

    for line in VECTORS.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split('\t');
        let name = f.next().ok_or(SelfTestErr::Fixture("no name column"))?;
        if name == "name" {
            continue; // header
        }
        let source = f.next().ok_or(SelfTestErr::Fixture("no source"))?;
        let nonce: u64 = f
            .next()
            .ok_or(SelfTestErr::Fixture("no nonce"))?
            .parse()
            .map_err(|_| SelfTestErr::Fixture("nonce is not a number"))?;
        let vault_s = f.next().ok_or(SelfTestErr::Fixture("no vault"))?;
        let exp_s = f.next().ok_or(SelfTestErr::Fixture("no expires_after"))?;
        let msgpack = unhex(f.next().ok_or(SelfTestErr::Fixture("no msgpack"))?)?;
        let want_cid = unhex(f.next().ok_or(SelfTestErr::Fixture("no connection id"))?)?;
        let want_digest = unhex(f.next().ok_or(SelfTestErr::Fixture("no digest"))?)?;
        let want_sig = unhex(f.next().ok_or(SelfTestErr::Fixture("no signature"))?)?;

        let vault = if vault_s == "-" {
            Vault::None
        } else {
            let b = unhex(vault_s.trim_start_matches("0x"))?;
            if b.len() != 20 {
                return Err(SelfTestErr::Fixture("vault is not 20 bytes"));
            }
            let mut a = [0u8; 20];
            a.copy_from_slice(&b[..20]);
            Vault::Address(a)
        };
        let expires_after = if exp_s == "-" {
            None
        } else {
            Some(
                exp_s
                    .parse()
                    .map_err(|_| SelfTestErr::Fixture("expires_after is not a number"))?,
            )
        };
        let net = if source == "a" {
            Network::Mainnet
        } else {
            Network::Testnet
        };

        let cid = connection_id(&msgpack, nonce, vault, expires_after)
            .map_err(|_| SelfTestErr::Encode("connection id"))?;
        diverged(name, "connection id", &cid, &want_cid)?;

        let digest = signer_eip712::hyperliquid::agent_eip712_hash(net.source(), &cid);
        diverged(name, "EIP-712 digest", &digest, &want_digest)?;

        let sk = signer_eip712::parse_secret_key(&TEST_KEY)
            .map_err(|_| SelfTestErr::Fixture("the TEST key is not a valid scalar"))?;
        let sig = signer_eip712::hyperliquid::sign_agent_with_key(&sk, net.source(), &cid)
            .map_err(|_| SelfTestErr::Encode("signature"))?;
        diverged(name, "65-byte signature", &sig, &want_sig)?;

        rows += 1;
    }

    if rows != VECTOR_ROWS {
        return Err(SelfTestErr::Fixture("the fixture does not hold exactly the 27 vectors"));
    }

    let encoders = check_encoders()?;
    Ok(SelfTestReport { rows, encoders })
}

/// Rebuild one action per type from inputs and demand the SDK's bytes.
/// **This is the half that catches LAW E-3.**
fn check_encoders() -> Result<u32, SelfTestErr> {
    let mut buf = [0u8; MAX_ACTION];

    // order — via batchModify below AND directly here, because the
    // order key sequence is the one that costs money when it moves.
    let n = crate::action::encode_order(
        &mut buf,
        &[OrderWire::new(0, true, 50_000_000, 1_000_000_000, Tif::Gtc)],
        b"na",
    )
    .map_err(|_| SelfTestErr::Encode("order"))?;
    want("order_gtc_buy", &buf[..n], "order")?;

    let n = encode_cancel(&mut buf, &[CancelWire { asset: 0, oid: 12_345 }])
        .map_err(|_| SelfTestErr::Encode("cancel"))?;
    want("cancel_single", &buf[..n], "cancel")?;

    let n = encode_cancel_by_cloid(
        &mut buf,
        &[CancelByCloidWire {
            asset: HIP4_YES,
            cloid: CLOID_B,
        }],
    )
    .map_err(|_| SelfTestErr::Encode("cancelByCloid"))?;
    want("cancel_by_cloid", &buf[..n], "cancelByCloid")?;

    let n = encode_batch_modify(
        &mut buf,
        &[ModifyWire {
            order: OrderWire::new(HIP4_YES, true, 48_000_000, 2_500_000_000, Tif::Alo)
                .with_cloid(CLOID_B),
            oid: 555,
            oid_cloid: [0u8; 16],
            oid_is_cloid: false,
        }],
    )
    .map_err(|_| SelfTestErr::Encode("batchModify"))?;
    want("batch_modify_1", &buf[..n], "batchModify")?;

    // S7-L1: the request-budget top-up — the one action the arm signs
    // that is neither an order nor a cancel.
    let n = crate::action::encode_reserve_weight(&mut buf, 5_000)
        .map_err(|_| SelfTestErr::Encode("reserveRequestWeight"))?;
    want("reserve_weight", &buf[..n], "reserveRequestWeight")?;

    Ok(5)
}

/// The cloid `gen_vectors.py` used for the cloid-bearing cases.
const CLOID_B: [u8; 16] = [
    0x4d, 0x56, 0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x2a,
];

/// Compare `got` against the named row's recorded msgpack.
fn want(row: &str, got: &[u8], label: &'static str) -> Result<(), SelfTestErr> {
    let recorded = msgpack_of(row)?;
    if got != recorded.as_slice() {
        return Err(SelfTestErr::KeyOrder(label));
    }
    Ok(())
}

/// The recorded msgpack for one named row.
fn msgpack_of(row: &str) -> Result<Vec<u8>, SelfTestErr> {
    for line in VECTORS.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split('\t');
        if f.next() == Some(row) {
            let hexs = f
                .nth(4)
                .ok_or(SelfTestErr::Fixture("row has no msgpack column"))?;
            return unhex(hexs);
        }
    }
    Err(SelfTestErr::Fixture("a required vector row is missing"))
}

fn diverged(name: &str, stage: &'static str, got: &[u8], want: &[u8]) -> Result<(), SelfTestErr> {
    if got == want {
        return Ok(());
    }
    Err(SelfTestErr::Chain {
        name: name.to_owned(),
        stage,
    })
}

fn unhex(s: &str) -> Result<Vec<u8>, SelfTestErr> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return Err(SelfTestErr::Fixture("odd-length hex"));
    }
    let mut out = Vec::with_capacity(b.len() / 2);
    for i in 0..b.len() / 2 {
        let hi = nib(b[i * 2]).ok_or(SelfTestErr::Fixture("non-hex byte"))?;
        let lo = nib(b[i * 2 + 1]).ok_or(SelfTestErr::Fixture("non-hex byte"))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn nib(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_binary_certifies_itself() {
        let r = run().expect("the embedded vectors must reproduce");
        assert_eq!(r.rows, VECTOR_ROWS, "{r:?}");
        assert_eq!(r.encoders, 5, "one case per action type, no fewer");
    }

    /// The self-test is only worth running if it can FAIL. Feed the
    /// comparator bytes that do not match and require a refusal.
    #[test]
    fn a_divergence_is_detected_rather_than_rounded_off() {
        let real = msgpack_of("cancel_single").expect("row");
        let mut broken = real.clone();
        let last = broken.len() - 1;
        broken[last] ^= 0x01;
        assert_eq!(
            want("cancel_single", &broken, "cancel").unwrap_err(),
            SelfTestErr::KeyOrder("cancel")
        );
        // And the unmodified bytes still pass, so the check is not
        // simply always-fail.
        assert!(want("cancel_single", &real, "cancel").is_ok());
    }

    #[test]
    fn a_missing_row_refuses_rather_than_passing_vacuously() {
        assert_eq!(
            msgpack_of("no_such_vector").unwrap_err(),
            SelfTestErr::Fixture("a required vector row is missing")
        );
    }

    #[test]
    fn the_failure_message_says_what_it_means() {
        let m = SelfTestErr::KeyOrder("order").to_string();
        assert!(m.contains("LAW E-3"), "{m}");
        assert!(m.contains("must not sign"), "{m}");
    }
}
