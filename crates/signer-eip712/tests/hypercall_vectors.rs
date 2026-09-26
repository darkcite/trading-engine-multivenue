// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Known-answer vectors against the official Hypercall TS SDK (HC8).
//!
//! `fixtures/hypercall/vectors.tsv` was produced by the venue's own
//! `@hypercallxyz/sdk` 0.1.0 signing builders (types, value builders,
//! `buildTypedData`) signed by ethers 6 — its header says how. Every row
//! is dispatched by its type to this crate's struct hash, and three
//! things must be byte-identical: the struct hash, the EIP-712 digest and
//! the 65-byte `r‖s‖v`. Every one of the 19 types must appear, and a row
//! of a type this crate does not know fails. The two signing keys are
//! published, worthless TEST keys.
//!
//! COPY-DOCTRINE: test-only (under `tests/`): the fixture parse copies
//! freely.

use signer_eip712::hypercall as hc;
use std::collections::BTreeSet;

/// Key A — the published test key of the Hyperliquid vectors.
const KEY_A: [u8; 32] = [
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
];
/// Key B — the Hardhat #0 test key.
const KEY_B: [u8; 32] = [
    0xac, 0x09, 0x74, 0xbe, 0xc3, 0x9a, 0x17, 0xe3, 0x6b, 0xa4, 0xa6, 0xb4, 0xd2, 0x38, 0xff, 0x94,
    0x4b, 0xac, 0xb4, 0x78, 0xcb, 0xed, 0x5e, 0xfc, 0xae, 0x78, 0x4d, 0x7b, 0xf4, 0xf2, 0xff, 0x80,
];

fn unhex(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    assert!(s.len() % 2 == 0, "odd hex {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One row's typed fields: `name:type=value`.
struct Fields(Vec<(String, String, String)>);

impl Fields {
    fn parse(s: &str) -> Self {
        Self(
            s.split(';')
                .map(|f| {
                    let (lhs, value) = f.split_once('=').expect("name:type=value");
                    let (name, ty) = lhs.split_once(':').expect("name:type");
                    (name.to_owned(), ty.to_owned(), value.to_owned())
                })
                .collect(),
        )
    }
    fn get(&self, name: &str, ty: &str) -> &str {
        let (_, t, v) = self.0.iter().find(|(n, _, _)| n == name).unwrap_or_else(|| panic!("no field {name}"));
        assert_eq!(t, ty, "field {name}'s type");
        v
    }
    fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(n, _, _)| n.as_str()).collect()
    }
    fn string(&self, name: &str) -> Vec<u8> {
        unhex(self.get(name, "string"))
    }
    fn address(&self, name: &str) -> [u8; 20] {
        unhex(self.get(name, "address")).try_into().expect("20 bytes")
    }
    fn b32(&self, name: &str) -> [u8; 32] {
        unhex(self.get(name, "bytes32")).try_into().expect("32 bytes")
    }
    fn u64(&self, name: &str) -> u64 {
        self.get(name, "uint64").parse().expect("u64")
    }
    fn i128(&self, name: &str) -> i128 {
        self.get(name, "int256").parse().expect("i128")
    }
    fn boolean(&self, name: &str) -> bool {
        match self.get(name, "bool") {
            "true" => true,
            "false" => false,
            other => panic!("bool {other}"),
        }
    }
}

/// The row's struct hash through this crate — and, for the live
/// `PlaceOrder`, the hot path's one-shot signature too.
fn struct_hash(ty: &str, f: &Fields) -> ([u8; 32], bool) {
    let wallet = |f: &Fields| f.address("wallet");
    match ty {
        "PlaceOrder" | "PlaceOrderLegacy" | "PlaceOrderReduceOnly" => {
            let w = wallet(f);
            let (symbol, side, size, price, tif, cloid) = (
                f.string("symbol"),
                f.string("side"),
                f.string("size"),
                f.string("price"),
                f.string("tif"),
                f.string("clientId"),
            );
            let route = if ty == "PlaceOrderLegacy" {
                assert!(!f.names().contains(&"route"), "the legacy type has no route");
                Vec::new()
            } else {
                f.string("route")
            };
            let v = hc::HcPlaceView {
                wallet: &w,
                symbol: &symbol,
                side: &side,
                size: &size,
                price: &price,
                tif: &tif,
                route: &route,
                client_id: &cloid,
                nonce: f.u64("nonce"),
            };
            match ty {
                "PlaceOrder" => (hc::place_order_struct_hash(&v), true),
                "PlaceOrderLegacy" => (hc::place_order_legacy_struct_hash(&v), false),
                _ => {
                    assert!(f.boolean("reduceOnly"), "the reduce-only type signs true");
                    (hc::place_order_reduce_only_struct_hash(&v), false)
                }
            }
        }
        "ReplaceOrder" | "ReplaceOrderReduceOnly" => {
            let w = wallet(f);
            let (order_id, symbol, side, size, price, tif, cloid) = (
                f.string("orderId"),
                f.string("symbol"),
                f.string("side"),
                f.string("size"),
                f.string("price"),
                f.string("tif"),
                f.string("clientId"),
            );
            let v = hc::HcReplaceView {
                wallet: &w,
                order_id: &order_id,
                symbol: &symbol,
                side: &side,
                size: &size,
                price: &price,
                tif: &tif,
                client_id: &cloid,
                nonce: f.u64("nonce"),
            };
            if ty == "ReplaceOrder" {
                (hc::replace_order_struct_hash(&v), false)
            } else {
                assert!(f.boolean("reduceOnly"), "the reduce-only type signs true");
                (hc::replace_order_reduce_only_struct_hash(&v), false)
            }
        }
        "CancelOrder" => (
            hc::cancel_order_struct_hash(&wallet(f), &f.string("orderId"), f.u64("nonce")),
            false,
        ),
        "CancelOrderByClientId" => (
            hc::cancel_order_by_client_id_struct_hash(&wallet(f), &f.string("clientId"), f.u64("nonce")),
            false,
        ),
        "ApproveAgent" => (hc::approve_agent_struct_hash(&f.address("agent"), f.u64("nonce")), false),
        "RevokeAgent" => (hc::revoke_agent_struct_hash(&f.address("agent"), f.u64("nonce")), false),
        "RevokeAllAgents" => (hc::revoke_all_agents_struct_hash(f.u64("nonce")), false),
        "SubmitRFQ" => (
            hc::submit_rfq_struct_hash(&f.b32("rfqId"), &f.b32("legsHash"), &wallet(f), f.u64("nonce")),
            false,
        ),
        "SubmitAutoExecuteRfq" => (
            hc::submit_auto_execute_rfq_struct_hash(
                &f.b32("rfqId"),
                &f.b32("legsHash"),
                f.i128("limitPrice"),
                &wallet(f),
                f.u64("nonce"),
            ),
            false,
        ),
        "AcceptRFQQuote" => (
            hc::accept_rfq_quote_struct_hash(
                &f.b32("rfqId"),
                &f.b32("quoteId"),
                f.i128("netPremium"),
                &wallet(f),
                f.u64("nonce"),
            ),
            false,
        ),
        "SetMarginMode" => (
            hc::set_margin_mode_struct_hash(&wallet(f), &f.string("marginMode"), f.u64("nonce")),
            false,
        ),
        "WithdrawUsdc" => (
            hc::withdraw_usdc_struct_hash(
                &wallet(f),
                &f.address("account"),
                &f.address("destination"),
                &f.string("amount"),
                f.u64("nonce"),
            ),
            false,
        ),
        "SetSettlementPayoutSeen" => (
            hc::set_settlement_payout_seen_struct_hash(
                &wallet(f),
                &f.string("payoutIds"),
                f.boolean("seen"),
                f.u64("nonce"),
            ),
            false,
        ),
        "CreateReferralCode" => (
            hc::create_referral_code_struct_hash(&wallet(f), &f.string("code"), f.u64("nonce")),
            false,
        ),
        "SetReferrer" => (
            hc::set_referrer_struct_hash(&wallet(f), &f.address("referrer"), f.u64("nonce")),
            false,
        ),
        "StandardMarginLiquidationOrder" => {
            let (w, lw) = (wallet(f), f.address("liquidatedWallet"));
            let (req, auc, bid, ph, ath, bih) = (
                f.string("requestId"),
                f.string("auctionId"),
                f.string("bidUsdc"),
                f.string("portfolioHash"),
                f.string("auctionTermsHash"),
                f.string("bidIntentHash"),
            );
            let v = hc::HcLiquidationView {
                wallet: &w,
                liquidated_wallet: &lw,
                request_id: &req,
                auction_id: &auc,
                bid_usdc: &bid,
                portfolio_hash: &ph,
                auction_terms_hash: &ath,
                bid_intent_hash: &bih,
                auction_version: f.u64("auctionVersion"),
                nonce: f.u64("nonce"),
            };
            (hc::standard_margin_liquidation_order_struct_hash(&v), false)
        }
        other => panic!("a vector of a type this crate does not sign: {other}"),
    }
}

#[test]
fn every_sdk_vector_is_byte_exact() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hypercall/vectors.tsv");
    let text = std::fs::read_to_string(&path).expect("the vector fixture");
    let keys = [
        ("A", signer_eip712::parse_secret_key(&KEY_A).expect("key A")),
        ("B", signer_eip712::parse_secret_key(&KEY_B).expect("key B")),
    ];
    let mut types = BTreeSet::new();
    let mut rows = 0usize;
    let mut chains = BTreeSet::new();
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') || line.starts_with("name\t") {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 8, "{line}");
        let (name, ty, chain, key) = (cols[0], cols[1], cols[2], cols[3]);
        let chain: u64 = chain.parse().expect("chain id");
        let f = Fields::parse(cols[4]);
        let sk = &keys.iter().find(|(k, _)| *k == key).expect("key A or B").1;
        let (sh, is_live_place) = struct_hash(ty, &f);
        assert_eq!(hex(&sh), cols[5], "{name}: struct hash");
        let ds = hc::hc_domain_separator(chain);
        let digest = hc::hc_eip712_digest(&ds, &sh);
        assert_eq!(hex(&digest), cols[6], "{name}: digest");
        let sig = hc::sign_hc_with_key(sk, &ds, &sh).expect("sign");
        assert_eq!(hex(&sig), cols[7], "{name}: signature");
        if is_live_place {
            let w = f.address("wallet");
            let (symbol, side, size, price, tif, route, cloid) = (
                f.string("symbol"),
                f.string("side"),
                f.string("size"),
                f.string("price"),
                f.string("tif"),
                f.string("route"),
                f.string("clientId"),
            );
            let v = hc::HcPlaceView {
                wallet: &w,
                symbol: &symbol,
                side: &side,
                size: &size,
                price: &price,
                tif: &tif,
                route: &route,
                client_id: &cloid,
                nonce: f.u64("nonce"),
            };
            let one_shot = hc::sign_place_order_with_key(sk, &ds, &v).expect("sign");
            assert_eq!(hex(&one_shot), cols[7], "{name}: the hot path's one-shot");
        }
        types.insert(ty.to_owned());
        chains.insert(chain);
        rows += 1;
    }
    assert!(rows >= 24, "{rows} vectors — the plan asks for at least 24");
    assert_eq!(types.len(), 19, "every type has a vector: {types:?}");
    assert_eq!(
        chains.into_iter().collect::<Vec<_>>(),
        vec![hc::HC_CHAIN_ID_TESTNET, hc::HC_CHAIN_ID_MAINNET],
        "both chains reach a digest"
    );
}

/// The fixture's header pins the separators and every typehash; they
/// must be this crate's constants, byte for byte.
#[test]
fn the_fixture_header_is_this_crates_constants() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hypercall/vectors.tsv");
    let text = std::fs::read_to_string(&path).expect("the vector fixture");
    let consts: [(&str, [u8; 32], &str); 19] = [
        ("PlaceOrder", hc::PLACE_ORDER_TYPEHASH, hc::PLACE_ORDER_TYPE),
        ("PlaceOrderLegacy", hc::PLACE_ORDER_LEGACY_TYPEHASH, hc::PLACE_ORDER_LEGACY_TYPE),
        ("PlaceOrderReduceOnly", hc::PLACE_ORDER_REDUCE_ONLY_TYPEHASH, hc::PLACE_ORDER_REDUCE_ONLY_TYPE),
        ("ReplaceOrder", hc::REPLACE_ORDER_TYPEHASH, hc::REPLACE_ORDER_TYPE),
        ("ReplaceOrderReduceOnly", hc::REPLACE_ORDER_REDUCE_ONLY_TYPEHASH, hc::REPLACE_ORDER_REDUCE_ONLY_TYPE),
        ("CancelOrder", hc::CANCEL_ORDER_TYPEHASH, hc::CANCEL_ORDER_TYPE),
        ("CancelOrderByClientId", hc::CANCEL_ORDER_BY_CLIENT_ID_TYPEHASH, hc::CANCEL_ORDER_BY_CLIENT_ID_TYPE),
        ("ApproveAgent", hc::APPROVE_AGENT_TYPEHASH, hc::APPROVE_AGENT_TYPE),
        ("RevokeAgent", hc::REVOKE_AGENT_TYPEHASH, hc::REVOKE_AGENT_TYPE),
        ("RevokeAllAgents", hc::REVOKE_ALL_AGENTS_TYPEHASH, hc::REVOKE_ALL_AGENTS_TYPE),
        ("SubmitRFQ", hc::SUBMIT_RFQ_TYPEHASH, hc::SUBMIT_RFQ_TYPE),
        ("SubmitAutoExecuteRfq", hc::SUBMIT_AUTO_EXECUTE_RFQ_TYPEHASH, hc::SUBMIT_AUTO_EXECUTE_RFQ_TYPE),
        ("AcceptRFQQuote", hc::ACCEPT_RFQ_QUOTE_TYPEHASH, hc::ACCEPT_RFQ_QUOTE_TYPE),
        ("SetMarginMode", hc::SET_MARGIN_MODE_TYPEHASH, hc::SET_MARGIN_MODE_TYPE),
        ("WithdrawUsdc", hc::WITHDRAW_USDC_TYPEHASH, hc::WITHDRAW_USDC_TYPE),
        ("SetSettlementPayoutSeen", hc::SET_SETTLEMENT_PAYOUT_SEEN_TYPEHASH, hc::SET_SETTLEMENT_PAYOUT_SEEN_TYPE),
        ("CreateReferralCode", hc::CREATE_REFERRAL_CODE_TYPEHASH, hc::CREATE_REFERRAL_CODE_TYPE),
        ("SetReferrer", hc::SET_REFERRER_TYPEHASH, hc::SET_REFERRER_TYPE),
        (
            "StandardMarginLiquidationOrder",
            hc::STANDARD_MARGIN_LIQUIDATION_ORDER_TYPEHASH,
            hc::STANDARD_MARGIN_LIQUIDATION_ORDER_TYPE,
        ),
    ];
    for (tag, hash, ty) in consts {
        let want = format!("# typehash {tag}: {} {ty}", hex(&hash));
        assert!(text.lines().any(|l| l == want), "header line for {tag}: {want}");
    }
    for (chain, tag) in [(hc::HC_CHAIN_ID_MAINNET, "999"), (hc::HC_CHAIN_ID_TESTNET, "998")] {
        let want = format!("# domain separator, chain {tag}: {}", hex(&hc::hc_domain_separator(chain)));
        assert!(text.lines().any(|l| l == want), "{want}");
    }
}
