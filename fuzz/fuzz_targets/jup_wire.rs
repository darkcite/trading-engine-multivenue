// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (PMJ J3): arbitrary bytes → the slot-11 arm's untrusted
//! inputs — as a raw Solana transaction (parse, then the guard over an
//! allowlist that admits Token, System and Compute Budget), as the base64
//! text of one (decode, parse, guard), as each Prediction API answer
//! (build, execute, status, error, the recon page, the positions, a claim,
//! the balances), and as a `pmj-held.state` file. PMJ W5: the same bytes
//! through the order guard (`guard_order`, the co-signer amendment pinned
//! and not) and the claim guard, as create-order instruction data (bare,
//! and after each create-order discriminator), and as a `build_extra`
//! member rendered into the build body.
//!
//! None of it may panic or read out of bounds on any input; a guard that
//! passes must never report a debit above the deposit it was given, or
//! lamports above its cap; an order guard that passes must have read a
//! Limit buy at or under the limit, depositing exactly the ask, its
//! contracts paid for, with no integrator.

#![no_main]

use exec_jupiter::held::{parse_held, HeldLeg, HELD_MAX};
use exec_jupiter::json::{
    scan_balances, scan_build, scan_claim, scan_error, scan_execute, scan_orders, scan_positions,
    scan_status, Balances, BuildAnswer, ClaimAnswer, ErrorAnswer, ExecuteAnswer, OrderRecord,
    StatusAnswer,
};
use exec_jupiter::render::{order_body, BuildExtra, Outcome, ORDER_BODY_MAX};
use exec_jupiter::tx::{
    guard, guard_claim, guard_order, CreateOrderView, OrderAsk, SolTx, TxGuard,
    COMPUTE_BUDGET_PROGRAM, CREATE_STR_MAX, DISC_CREATE_ORDER, DISC_CREATE_ORDER2,
    ORDER_TYPE_LIMIT, SYSTEM_PROGRAM, TOKEN_2022_PROGRAM, TOKEN_PROGRAM, TX_MAX,
};
use libfuzzer_sys::fuzz_target;

const DEPOSIT: u64 = 5_000_000;
const LIMIT: u64 = 960_000;

fn check(buf: &[u8], g: &TxGuard) {
    if let Ok(t) = SolTx::parse(buf) {
        let _ = t.message(buf);
        if let Ok(p) = guard(buf, &t, g, DEPOSIT) {
            assert!(p.token_debit <= DEPOSIT, "a passed guard over the deposit");
            assert!(p.lamports <= g.max_sol_lamports, "a passed guard over the lamport cap");
            assert!(p.cu_price <= g.max_cu_price, "a passed guard over the price cap");
            assert!(p.signer_idx < t.n_sigs);
        }
        let mut i = 0usize;
        while i < t.n_keys as usize + t.n_loaded as usize + 2 {
            let _ = t.loaded_entry(buf, i);
            i += 1;
        }
        let ask = OrderAsk {
            deposit: DEPOSIT,
            limit_micro: LIMIT,
            order: [0x0A; 32],
            position: [0x0B; 32],
            market_hash: b"ad44626a542bdd650466f1c527f02905",
            contracts: 5_263_157,
        };
        if let Ok((p, v)) = guard_order(buf, &t, g, &ask) {
            assert_eq!(p.token_debit, 0, "an order's deposit moves by its CPI only");
            assert!(v.is_buy && v.is_yes && v.order_type == ORDER_TYPE_LIMIT, "a Limit buy");
            assert!(v.max_fill_price_usd <= LIMIT, "at or under the limit");
            assert_eq!(v.deposit_amount, DEPOSIT, "exactly the deposit asked");
            assert!(u128::from(v.contracts) * u128::from(v.max_fill_price_usd) <= u128::from(DEPOSIT) * 1_000_000);
            assert!(!v.has_integrator && v.integrator_fee_bps.is_none());
            assert_eq!(v.contracts, ask.contracts, "the answer's contracts");
            assert!(t.header[0] == 1 || (g.has_cosigner && t.header[0] == 2));
        }
        if let Ok(p) = guard_claim(buf, &t, g) {
            assert_eq!(p.token_debit, 0, "no token leaves on a claim");
        }
    }
}

fn create_data(d: &[u8]) {
    if let Ok(v) = CreateOrderView::parse(d) {
        assert!(v.external_id.1 as usize <= CREATE_STR_MAX && v.market_id.1 as usize <= CREATE_STR_MAX);
        assert!((v.market_id.0 + v.market_id.1) as usize <= d.len());
        assert!(v.order_type <= ORDER_TYPE_LIMIT);
        assert!(v.version == 1 || v.version == 2);
    }
}

fuzz_target!(|data: &[u8]| {
    let mut g = TxGuard::refuse_all([0x11; 32]);
    g.allow_program(TOKEN_PROGRAM);
    g.allow_program(TOKEN_2022_PROGRAM);
    g.allow_program(SYSTEM_PROGRAM);
    g.allow_program(COMPUTE_BUDGET_PROGRAM);
    g.allow_program([0x66; 32]);
    g.allow_alt([0x88; 32]);
    g.max_sol_lamports = 3_000_000;
    g.max_cu_price = 50_000;
    g.deposit_account = [0x33; 32];
    g.deposit_mint = [0x55; 32];
    g.predict_program = [0x66; 32];
    g.vault = [0x77; 32];
    g.vault_alt = [0x88; 32];
    g.vault_alt_entry = 1;
    let mut pinned = g;
    pinned.cosigner = [0x7A; 32];
    pinned.has_cosigner = true;
    check(data, &g);
    check(data, &pinned);
    let mut buf = [0u8; TX_MAX];
    if let Ok(n) = core_crypto::base64_decode(data, &mut buf) {
        check(&buf[..n], &g);
        check(&buf[..n], &pinned);
    }
    create_data(data);
    let mut cd = [0u8; TX_MAX + 8];
    let n = data.len().min(TX_MAX);
    for disc in [DISC_CREATE_ORDER, DISC_CREATE_ORDER2] {
        cd[..8].copy_from_slice(&disc);
        cd[8..8 + n].copy_from_slice(&data[..n]);
        create_data(&cd[..8 + n]);
    }
    let mut x = BuildExtra::NONE;
    let mut part = data.split(|b| *b == b'\n');
    let mut k = 0usize;
    while k < 6 {
        let Some(spec) = part.next() else {
            break;
        };
        let _ = x.push(spec);
        k += 1;
    }
    let mut body = [0u8; ORDER_BODY_MAX];
    let key = [b'1'; 44];
    let r = order_body(&mut body, &key, u64::MAX, Outcome::Down, u64::MAX, &key, &x, u64::MAX);
    assert!(r.is_some(), "any admitted extras fit the bound");
    let mut b = BuildAnswer::default();
    if scan_build(data, &mut b).is_ok() {
        let _ = b.transaction.of(data);
        let _ = b.context.of(data);
        assert!(b.n_required as usize <= exec_jupiter::tx::SIGNERS_MAX);
    }
    let mut e = ExecuteAnswer::default();
    let _ = scan_execute(data, &mut e);
    let mut s = StatusAnswer::default();
    if scan_status(data, &mut s).is_ok() {
        if let Some(r) = s.record() {
            assert!(r.state.terminal() && r.has_fill && (r.filled_micro == 0 || r.has_avg));
            assert!(!s.conflict, "disagreeing fill numbers are never read");
        }
    }
    let mut r = ErrorAnswer::default();
    let _ = scan_error(data, &mut r);
    let mut o = OrderRecord::default();
    let _ = scan_orders(data, b"Ord1111", &mut o);
    let mut n = 0usize;
    if let Ok(k) = scan_positions(data, &mut |p| {
        let _ = p.pubkey.of(data);
        n += 1;
    }) {
        assert_eq!(k, n, "every position read is reported");
    }
    let mut c = ClaimAnswer::default();
    if scan_claim(data, &mut c).is_ok() {
        let _ = c.transaction.of(data);
    }
    let mut bal = Balances::default();
    let _ = scan_balances(data, b"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", &mut bal);
    let mut legs = [HeldLeg::EMPTY; HELD_MAX];
    if let Ok(k) = parse_held(data, b"FVen3X669xLzsi6N2V91DoiyzHzg1uAgqiT8jZ9nS96Z", &mut legs) {
        assert!(k <= HELD_MAX);
    }
});
