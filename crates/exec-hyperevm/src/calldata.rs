// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The executor's `swap` call (O-H18,
//! `contracts/hyparb-executor/src/HyparbExecutor.sol`), ABI-encoded into
//! a caller-owned stack array that the transaction then BORROWS — the
//! signer hashes it in place and the renderer hex-encodes it straight
//! into the request body.
//!
//! `swap(address pool, bool zeroForOne, int256 amountSpecified,
//! uint160 sqrtPriceLimitX96, uint256 minOut)`: five static words after
//! the selector. `amountSpecified` follows the V3 convention (positive:
//! exact input; negative: exact output); the executor forwards it and
//! the price limit to the pool, and reverts `BelowMinOut` when the
//! output is short — an unprofitable race costs gas, never inventory.

/// The canonical signature the selector hashes.
pub const SWAP_SIGNATURE: &str = "swap(address,bool,int256,uint160,uint256)";
/// `keccak256(SWAP_SIGNATURE)[..4]`.
pub const SWAP_SELECTOR: [u8; 4] = [0x46, 0x09, 0x85, 0xe8];
/// Selector + five 32-byte words.
pub const SWAP_CALLDATA_LEN: usize = 4 + 5 * 32;

/// `mint(address,uint256)` — the public faucet the testnet tokens expose
/// (measured 2026-09-23: `leUSDT0` mints to any caller); the operator
/// tool funds the executor with it.
pub const MINT_SIGNATURE: &str = "mint(address,uint256)";
/// `keccak256(MINT_SIGNATURE)[..4]`.
pub const MINT_SELECTOR: [u8; 4] = [0x40, 0xc1, 0x0f, 0x19];
/// Selector + an address word + an amount word.
pub const ADDR_AMOUNT_CALLDATA_LEN: usize = 4 + 2 * 32;

/// `owner()` — the executor's immutable owner, the ONLY sender its
/// `swap` accepts (`NotOwner` otherwise). Read at boot: an executor
/// whose owner is not wallet 0 would revert every shadow swap (H9 R4).
pub const OWNER_SIGNATURE: &str = "owner()";
/// `keccak256(OWNER_SIGNATURE)[..4]`.
pub const OWNER_SELECTOR: [u8; 4] = [0x8d, 0xa5, 0xcb, 0x5b];

/// `balanceOf(address)` — an ERC-20's balance view (the operator verbs'
/// inventory reads).
pub const BALANCE_OF_SIGNATURE: &str = "balanceOf(address)";
/// `keccak256(BALANCE_OF_SIGNATURE)[..4]`.
pub const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
/// Selector + one address word.
pub const ADDR_CALLDATA_LEN: usize = 4 + 32;

/// `transfer(address,uint256)` — ERC-20 (the operator funding the
/// executor with WHYPE).
pub const TRANSFER_SIGNATURE: &str = "transfer(address,uint256)";
/// `keccak256(TRANSFER_SIGNATURE)[..4]`.
pub const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// `deposit()` — WHYPE's wrap: the call's value becomes WHYPE held by
/// the sender.
pub const DEPOSIT_SIGNATURE: &str = "deposit()";
/// `keccak256(DEPOSIT_SIGNATURE)[..4]` — the whole calldata.
pub const DEPOSIT_SELECTOR: [u8; 4] = [0xd0, 0xe3, 0x0d, 0xb0];

/// `token0()` — a pool's first token (immutable in every traded family).
pub const TOKEN0_SIGNATURE: &str = "token0()";
/// `keccak256(TOKEN0_SIGNATURE)[..4]`.
pub const TOKEN0_SELECTOR: [u8; 4] = [0x0d, 0xfe, 0x16, 0x81];
/// `token1()` — a pool's second token.
pub const TOKEN1_SIGNATURE: &str = "token1()";
/// `keccak256(TOKEN1_SIGNATURE)[..4]`.
pub const TOKEN1_SELECTOR: [u8; 4] = [0xd2, 0x12, 0x20, 0xa7];

/// `sweep(address token, address to, uint256 amount)` — the executor's
/// owner-only unwind (O-H18).
pub const SWEEP_SIGNATURE: &str = "sweep(address,address,uint256)";
/// `keccak256(SWEEP_SIGNATURE)[..4]`.
pub const SWEEP_SELECTOR: [u8; 4] = [0x62, 0xc0, 0x67, 0x67];
/// Selector + two address words + an amount word.
pub const SWEEP_CALLDATA_LEN: usize = 4 + 3 * 32;

/// One executor swap. POD, passed by value across the arm's ring.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SwapCall {
    /// V3 `amountSpecified`: > 0 exact input, < 0 exact output.
    pub amount_specified: i128,
    /// `sqrtPriceLimitX96`, low 128 bits.
    pub sqrt_limit_lo: u128,
    /// The least output the executor accepts, token raw units.
    pub min_out: u128,
    /// `sqrtPriceLimitX96`, high 32 bits (of the `uint160`).
    pub sqrt_limit_hi: u32,
    /// The pool the executor swaps against.
    pub pool: [u8; 20],
    /// `zeroForOne`: token0 in, token1 out.
    pub zero_for_one: bool,
}

/// Big-endian `v` into the low 16 bytes of the 32-byte word at `w`.
#[inline(always)]
fn put_u128(dst: &mut [u8; SWAP_CALLDATA_LEN], w: usize, v: u128) {
    let mut i = 0;
    while i < 16 {
        dst[w + 16 + i] = (v >> (8 * (15 - i))) as u8;
        i += 1;
    }
}

/// Render `c` into `dst`. Every byte of `dst` is written (the static
/// words' padding included), so a reused buffer carries nothing over.
#[inline]
pub fn encode_swap(c: &SwapCall, dst: &mut [u8; SWAP_CALLDATA_LEN]) {
    let mut i = 0;
    while i < 4 {
        dst[i] = SWAP_SELECTOR[i];
        i += 1;
    }
    // Word 0: address, left-padded.
    let w0 = 4;
    i = 0;
    while i < 12 {
        dst[w0 + i] = 0;
        i += 1;
    }
    i = 0;
    while i < 20 {
        dst[w0 + 12 + i] = c.pool[i];
        i += 1;
    }
    // Word 1: bool.
    let w1 = w0 + 32;
    i = 0;
    while i < 31 {
        dst[w1 + i] = 0;
        i += 1;
    }
    dst[w1 + 31] = c.zero_for_one as u8;
    // Word 2: int256, sign-extended from i128.
    let w2 = w1 + 32;
    let ext = if c.amount_specified < 0 { 0xff } else { 0x00 };
    i = 0;
    while i < 16 {
        dst[w2 + i] = ext;
        i += 1;
    }
    put_u128(dst, w2, c.amount_specified as u128);
    // Word 3: uint160 = hi (32 bits) ‖ lo (128 bits), left-padded.
    let w3 = w2 + 32;
    i = 0;
    while i < 12 {
        dst[w3 + i] = 0;
        i += 1;
    }
    i = 0;
    while i < 4 {
        dst[w3 + 12 + i] = (c.sqrt_limit_hi >> (8 * (3 - i))) as u8;
        i += 1;
    }
    put_u128(dst, w3, c.sqrt_limit_lo);
    // Word 4: uint256 from u128.
    let w4 = w3 + 32;
    i = 0;
    while i < 16 {
        dst[w4 + i] = 0;
        i += 1;
    }
    put_u128(dst, w4, c.min_out);
}

/// The selector into `dst[..4]`.
#[inline(always)]
fn put_selector(dst: &mut [u8], selector: [u8; 4]) {
    let mut i = 0;
    while i < 4 {
        dst[i] = selector[i];
        i += 1;
    }
}

/// An address, left-padded, into the 32-byte word at `w`.
#[inline(always)]
fn put_addr_word(dst: &mut [u8], w: usize, a: &[u8; 20]) {
    let mut i = 0;
    while i < 12 {
        dst[w + i] = 0;
        i += 1;
    }
    i = 0;
    while i < 20 {
        dst[w + 12 + i] = a[i];
        i += 1;
    }
}

/// A `u128`, as a uint256, into the 32-byte word at `w`.
#[inline(always)]
fn put_amount_word(dst: &mut [u8], w: usize, v: u128) {
    let mut i = 0;
    while i < 16 {
        dst[w + i] = 0;
        dst[w + 16 + i] = (v >> (8 * (15 - i))) as u8;
        i += 1;
    }
}

/// Render `selector(address a)` into `dst` (every byte written).
#[inline]
pub fn encode_addr(selector: [u8; 4], a: &[u8; 20], dst: &mut [u8; ADDR_CALLDATA_LEN]) {
    put_selector(dst, selector);
    put_addr_word(dst, 4, a);
}

/// Render `selector(address to, uint256 amount)` into `dst` (every byte
/// written).
#[inline]
pub fn encode_addr_amount(
    selector: [u8; 4],
    to: &[u8; 20],
    amount: u128,
    dst: &mut [u8; ADDR_AMOUNT_CALLDATA_LEN],
) {
    put_selector(dst, selector);
    put_addr_word(dst, 4, to);
    put_amount_word(dst, 36, amount);
}

/// Render the executor's `sweep(token, to, amount)` into `dst` (every
/// byte written).
#[inline]
pub fn encode_sweep(
    token: &[u8; 20],
    to: &[u8; 20],
    amount: u128,
    dst: &mut [u8; SWEEP_CALLDATA_LEN],
) {
    put_selector(dst, SWEEP_SELECTOR);
    put_addr_word(dst, 4, token);
    put_addr_word(dst, 36, to);
    put_amount_word(dst, 68, amount);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn pool() -> [u8; 20] {
        let s = "6c9a33e3b592c0d65b3ba59355d5be0d38259285";
        let mut a = [0u8; 20];
        let mut i = 0;
        while i < 20 {
            a[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
            i += 1;
        }
        a
    }

    #[test]
    fn the_selector_is_the_signatures_keccak() {
        let h = signer_eip712::keccak256(SWAP_SIGNATURE.as_bytes());
        assert_eq!(h[..4], SWAP_SELECTOR);
    }

    /// Byte-equal to `eth_abi.encode` (eth-abi, the independent
    /// implementation eth-account uses), generated 2026-09-23.
    #[test]
    fn exact_input_with_a_160_bit_limit_matches_eth_abi() {
        let c = SwapCall {
            amount_specified: 1_500_000_000_000_000_000,
            sqrt_limit_lo: 0x0102030405060708090a0b0c0d0e0f10,
            min_out: 987_654_321,
            sqrt_limit_hi: 0x1234,
            pool: pool(),
            zero_for_one: true,
        };
        let mut dst = [0xa5u8; SWAP_CALLDATA_LEN];
        encode_swap(&c, &mut dst);
        assert_eq!(
            hex(&dst),
            "460985e80000000000000000000000006c9a33e3b592c0d65b3ba59355d5be0d38259285000000000000000000000000000000000000000000000000000000000000000100000000000000000000000000000000000000000000000014d1120d7b160000000000000000000000000000000012340102030405060708090a0b0c0d0e0f10000000000000000000000000000000000000000000000000000000003ade68b1"
        );
    }

    #[test]
    fn the_owner_selector_is_its_signatures_keccak() {
        let h = signer_eip712::keccak256(OWNER_SIGNATURE.as_bytes());
        assert_eq!(h[..4], OWNER_SELECTOR);
    }

    #[test]
    fn mint_matches_eth_abi_and_its_selector_is_the_keccak() {
        let h = signer_eip712::keccak256(MINT_SIGNATURE.as_bytes());
        assert_eq!(h[..4], MINT_SELECTOR);
        let mut dst = [0xa5u8; ADDR_AMOUNT_CALLDATA_LEN];
        encode_addr_amount(
            MINT_SELECTOR,
            &pool(),
            1_000_000_000_000_000_000_000_005,
            &mut dst,
        );
        assert_eq!(
            hex(&dst),
            "40c10f190000000000000000000000006c9a33e3b592c0d65b3ba59355d5be0d3825928500000000000000000000000000000000000000000000d3c21bcecceda1000005"
        );
    }

    #[test]
    fn every_operator_selector_is_its_signatures_keccak() {
        let sigs = [
            (BALANCE_OF_SIGNATURE, BALANCE_OF_SELECTOR),
            (TRANSFER_SIGNATURE, TRANSFER_SELECTOR),
            (DEPOSIT_SIGNATURE, DEPOSIT_SELECTOR),
            (TOKEN0_SIGNATURE, TOKEN0_SELECTOR),
            (TOKEN1_SIGNATURE, TOKEN1_SELECTOR),
            (SWEEP_SIGNATURE, SWEEP_SELECTOR),
        ];
        let mut i = 0;
        while i < sigs.len() {
            let h = signer_eip712::keccak256(sigs[i].0.as_bytes());
            assert_eq!(h[..4], sigs[i].1, "{}", sigs[i].0);
            i += 1;
        }
    }

    /// Byte-equal to `eth_abi.encode` (eth-abi 5, generated 2026-09-24).
    #[test]
    fn sweep_balance_of_and_transfer_match_eth_abi() {
        let x = {
            let s = "2c7536e3605d9c16a7a3d7b1898e529396a65c23";
            let mut a = [0u8; 20];
            let mut i = 0;
            while i < 20 {
                a[i] = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
                i += 1;
            }
            a
        };
        let mut sw = [0xa5u8; SWEEP_CALLDATA_LEN];
        encode_sweep(&pool(), &x, 123_456_789_012_345_678_901_234_567, &mut sw);
        assert_eq!(
            hex(&sw),
            "62c067670000000000000000000000006c9a33e3b592c0d65b3ba59355d5be0d382592850000000000000000000000002c7536e3605d9c16a7a3d7b1898e529396a65c23000000000000000000000000000000000000000000661efdf158f2a82c9f4b87"
        );
        let mut b = [0xa5u8; ADDR_CALLDATA_LEN];
        encode_addr(BALANCE_OF_SELECTOR, &x, &mut b);
        assert_eq!(
            hex(&b),
            "70a082310000000000000000000000002c7536e3605d9c16a7a3d7b1898e529396a65c23"
        );
        let mut t = [0xa5u8; ADDR_AMOUNT_CALLDATA_LEN];
        encode_addr_amount(TRANSFER_SELECTOR, &pool(), u128::MAX, &mut t);
        assert_eq!(
            hex(&t),
            "a9059cbb0000000000000000000000006c9a33e3b592c0d65b3ba59355d5be0d3825928500000000000000000000000000000000ffffffffffffffffffffffffffffffff"
        );
    }

    #[test]
    fn exact_output_is_sign_extended_over_all_256_bits() {
        let c = SwapCall {
            amount_specified: -((1i128 << 100) + 7),
            sqrt_limit_lo: 4_295_128_740,
            min_out: 0,
            sqrt_limit_hi: 0,
            pool: pool(),
            zero_for_one: false,
        };
        let mut dst = [0x5au8; SWAP_CALLDATA_LEN];
        encode_swap(&c, &mut dst);
        assert_eq!(
            hex(&dst),
            "460985e80000000000000000000000006c9a33e3b592c0d65b3ba59355d5be0d382592850000000000000000000000000000000000000000000000000000000000000000ffffffffffffffffffffffffffffffffffffffeffffffffffffffffffffffff900000000000000000000000000000000000000000000000000000001000276a40000000000000000000000000000000000000000000000000000000000000000"
        );
    }
}
