// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Fuzz target (PMJ J3): arbitrary bytes → the slot-11 arm's untrusted
//! inputs — as a raw Solana transaction (parse, then the guard over an
//! allowlist that admits Token, System and Compute Budget), as the base64
//! text of one (decode, parse, guard), as each Prediction API answer
//! (build, execute, status, error, the recon page, the positions, a claim,
//! the balances), and as a `pmj-held.state` file.
//!
//! None of it may panic or read out of bounds on any input; a guard that
//! passes must never report a debit above the deposit it was given, or
//! lamports above its cap.

#![no_main]

use exec_jupiter::held::{parse_held, HeldLeg, HELD_MAX};
use exec_jupiter::json::{
    scan_balances, scan_build, scan_claim, scan_error, scan_execute, scan_orders, scan_positions,
    scan_status, Balances, BuildAnswer, ClaimAnswer, ErrorAnswer, ExecuteAnswer, OrderRecord,
    StatusAnswer,
};
use exec_jupiter::tx::{
    guard, SolTx, TxGuard, COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM, TOKEN_2022_PROGRAM,
    TOKEN_PROGRAM, TX_MAX,
};
use libfuzzer_sys::fuzz_target;

const DEPOSIT: u64 = 5_000_000;

fn check(buf: &[u8], g: &TxGuard) {
    if let Ok(t) = SolTx::parse(buf) {
        let _ = t.message(buf);
        if let Ok(p) = guard(buf, &t, g, DEPOSIT) {
            assert!(p.token_debit <= DEPOSIT, "a passed guard over the deposit");
            assert!(p.lamports <= g.max_sol_lamports, "a passed guard over the lamport cap");
            assert!(p.cu_price <= g.max_cu_price, "a passed guard over the price cap");
            assert!(p.signer_idx < t.n_sigs);
        }
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
    check(data, &g);
    let mut buf = [0u8; TX_MAX];
    if let Ok(n) = core_crypto::base64_decode(data, &mut buf) {
        check(&buf[..n], &g);
    }
    let mut b = BuildAnswer::default();
    if scan_build(data, &mut b).is_ok() {
        let _ = b.transaction.of(data);
        let _ = b.context.of(data);
        assert!(b.n_required as usize <= exec_jupiter::tx::SIGNERS_MAX);
    }
    let mut e = ExecuteAnswer::default();
    let _ = scan_execute(data, &mut e);
    let mut s = StatusAnswer::default();
    let _ = scan_status(data, &mut s);
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
