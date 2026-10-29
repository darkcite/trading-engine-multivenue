// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # selftest — the guard's known answers, in the binary that signs (PMJ J16)
//!
//! COPY-DOCTRINE: a boot self-test and an operator verb's — cold, once per
//! live boot of slot 11 and per `pmj-exec --self-test`; never on the order
//! path (the fixtures decode into a stack buffer, nothing is copied twice).
//!
//! The public mainnet transactions of `tests/fixtures/` (their README:
//! other people's landed create-orders and claim, no key of ours; the
//! SDK-vector precedent) run through [`guard_order`] and [`guard_claim`] in
//! THIS binary: the guard each passes — its owner's programs, table, vault
//! (the table's entry 1), mint and deposit account, the co-signer pinned —
//! must pass it, and every guard one line off it must refuse it with that
//! line's own [`TxErr`]. LAW PMJ-G as written (no co-signer pinned) and an
//! empty allowlist refuse every public order. Any miss refuses: a binary
//! whose guard does not answer these must not sign.

use core_crypto::{base58_decode, base64_decode};

use crate::tx::{
    guard_claim, guard_order, OrderAsk, SolTx, TxErr, TxGuard, ATA_PROGRAM, COMPUTE_BUDGET_PROGRAM,
    KEY_LEN, ORDER_TYPE_LIMIT, PREDICT_PROGRAM, TX_MAX,
};

/// `create_order` (v1), a Market order (public, slot 458 816 924).
pub const FX_V1_MARKET: &[u8] = include_bytes!("../tests/fixtures/create-order-v1-market.b64");
/// `create_order` (v1), a Limit order (public, slot 462 839 696).
pub const FX_V1_LIMIT: &[u8] = include_bytes!("../tests/fixtures/create-order-v1-limit.b64");
/// `create_order2`, a Limit order (public, slot 462 844 216).
pub const FX_V2_LIMIT: &[u8] = include_bytes!("../tests/fixtures/create-order2-limit.b64");
/// `claim_payout2` (public, slot 462 862 509).
pub const FX_CLAIM2: &[u8] = include_bytes!("../tests/fixtures/claim-payout2.b64");

const COSIGNER_B58: &[u8] = b"AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf";
const VAULT_B58: &[u8] = b"2y9Ad2GD7gwiMkkMu4bBK5216Pv9YJsBkSHAGwN3rBuJ";
const ALT_B58: &[u8] = b"6awx2Pq4uxeHsBcmutEqErz79C4TUg4EQEiBX5T5BVfX";
const JUPUSD_B58: &[u8] = b"JuprjznTrTSp2UFa3ZBUFgwdAmtZCq4MQCwysN55USD";

/// A public create-order and what its build answer would have said (every
/// key read off the transaction by role; `crate::tx`'s fixture tests pin the
/// same table).
#[derive(Copy, Clone)]
struct Fx {
    owner: &'static [u8],
    source: &'static [u8],
    position: &'static [u8],
    order: &'static [u8],
    market: &'static [u8],
    deposit: u64,
    cap: u64,
    contracts: u64,
}

const V1_MARKET: Fx = Fx {
    owner: b"3QnKgNZByX4gfXdRR2mXekoby7M7CF6hrXAcVHU2YA8D",
    source: b"FQSXTzF7qhGpVVDMckQQ5AAaUBfi7n3NWnJ8eo2qbzDe",
    position: b"AD2yLzZyJ5bkqxgAVP92RjMsUtPegC8dUd3tVmXd6UKL",
    order: b"7HcJs6bfrraWkCkSdARPL66X32uZUgJmTuvwkLzKYuyB",
    market: b"152bec8ae0dfdd1bc81aa9a90ebf6434",
    deposit: 5_000_000,
    cap: 550_000,
    contracts: 8_630_000,
};

const V1_LIMIT: Fx = Fx {
    owner: b"4mc4pL8DCthQ6yzatKfEh63Hm61TWF5Rerq8UmCWoMV2",
    source: b"AtePzmqKm29usLetmsxoJyQuvdApyBAuvT66Gvo66fGZ",
    position: b"AwuF21j7ubqd4waSg2z14Hhde7TiWJrY5ztE1k9ppTCd",
    order: b"Hgooy2co4BNkzNDa9spjmeBxHozQBMGjH2FTz62XyxNo",
    market: b"47c243fed659fdb0a809b48b87b62ecb",
    deposit: 5_000_000,
    cap: 125_000,
    contracts: 36_780_000,
};

const V2_LIMIT: Fx = Fx {
    owner: b"63EEC9FfGyksm7PkVC6z8uAmqozbQcTzbkWJNsgqjkFs",
    source: b"6Sqcd42uPg9YgZ8NGeU2mkUdon15NN35ZkwUbyXzuVMt",
    position: b"J4Nh2LaCCA3fBWBidm9SJ1iXEdNFV7B1NxBD3mDZW1am",
    order: b"3RxBta8aAVS8U8wTLQJ7SCrja5vcxyVtPXw7iMGmSmot",
    market: b"ad44626a542bdd650466f1c527f02905",
    deposit: 6_000_000,
    cap: 30_000,
    contracts: 187_170_000,
};

/// The claim's owner and its deposit account (the same account its
/// `create_order2` drew from).
const CLAIM_OWNER: &[u8] = b"5wzHj5BRhsViwc79VUtPv1DpAbafkFnYLc81hK2AT2be";
const CLAIM_ACCOUNT: &[u8] = b"911cuEbXmZxEhb2aCVNmukJiiKAftyn8MaYcickGZy5y";

/// The check that missed. Every one refuses: the binary must not sign.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SelfTestErr(
    /// The check that missed.
    pub &'static str,
);

impl core::fmt::Display for SelfTestErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the guard's self-test: {}", self.0)
    }
}

/// The guard's known answers: the count of checks that held.
///
/// # Errors
/// [`SelfTestErr`] — the first check that missed.
pub fn run() -> Result<u32, SelfTestErr> {
    run_with(FX_V1_MARKET, FX_V1_LIMIT, FX_V2_LIMIT, FX_CLAIM2)
}

/// A 32-byte base58 key.
fn key(b58: &[u8]) -> Result<[u8; KEY_LEN], SelfTestErr> {
    let mut k = [0u8; KEY_LEN];
    match base58_decode(b58, &mut k) {
        Ok(KEY_LEN) => Ok(k),
        _ => Err(SelfTestErr("a pinned key does not decode")),
    }
}

/// A fixture's base64 into `buf`: its length.
fn decode(b64: &[u8], buf: &mut [u8; TX_MAX]) -> Result<usize, SelfTestErr> {
    let mut end = b64.len();
    while end > 0 && b64[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    base64_decode(&b64[..end], buf).map_err(|_| SelfTestErr("a fixture does not decode"))
}

/// The guard the fixture's owner passes (`crate::tx`'s `fixture_guard`).
fn guard_of(owner: &[u8], deposit_account: &[u8]) -> Result<TxGuard, SelfTestErr> {
    let mut g = TxGuard::refuse_all(key(owner)?);
    if !(g.allow_program(COMPUTE_BUDGET_PROGRAM) && g.allow_program(PREDICT_PROGRAM) && g.allow_program(ATA_PROGRAM)) {
        return Err(SelfTestErr("the program allowlist is full"));
    }
    let alt = key(ALT_B58)?;
    if !g.allow_alt(alt) {
        return Err(SelfTestErr("the table allowlist is full"));
    }
    g.max_cu_price = 400_000;
    g.deposit_account = key(deposit_account)?;
    g.deposit_mint = key(JUPUSD_B58)?;
    g.vault = key(VAULT_B58)?;
    g.vault_alt = alt;
    g.vault_alt_entry = 1;
    g.cosigner = key(COSIGNER_B58)?;
    g.has_cosigner = true;
    Ok(g)
}

fn ask_of(fx: &Fx) -> Result<OrderAsk<'static>, SelfTestErr> {
    Ok(OrderAsk {
        deposit: fx.deposit,
        limit_micro: fx.cap,
        order: key(fx.order)?,
        position: key(fx.position)?,
        market_hash: fx.market,
        contracts: fx.contracts,
    })
}

/// `got` must be `want`, else `what` misses.
fn expect(got: Result<(), TxErr>, want: Result<(), TxErr>, what: &'static str) -> Result<u32, SelfTestErr> {
    if got == want {
        Ok(1)
    } else {
        Err(SelfTestErr(what))
    }
}

/// [`run`] over the given fixtures (a test corrupts one).
fn run_with(v1_market: &[u8], v1_limit: &[u8], v2_limit: &[u8], claim: &[u8]) -> Result<u32, SelfTestErr> {
    let mut buf = [0u8; TX_MAX];
    let mut n = 0u32;

    // A Limit create_order2 at its own cap passes its owner's guard.
    let len = decode(v2_limit, &mut buf)?;
    let raw = &buf[..len];
    let t = SolTx::parse(raw).map_err(|_| SelfTestErr("the create_order2 Limit fixture does not parse"))?;
    let g = guard_of(V2_LIMIT.owner, V2_LIMIT.source)?;
    let ask = ask_of(&V2_LIMIT)?;
    match guard_order(raw, &t, &g, &ask) {
        Ok((p, v))
            if p.signer_idx == 0
                && v.version == 2
                && v.order_type == ORDER_TYPE_LIMIT
                && v.max_fill_price_usd == V2_LIMIT.cap
                && v.deposit_amount == V2_LIMIT.deposit
                && v.contracts == V2_LIMIT.contracts =>
        {
            n += 1;
        }
        _ => return Err(SelfTestErr("a public Limit create_order2 at its own cap was not passed as read")),
    }
    let order = |g: &TxGuard, a: &OrderAsk<'_>| guard_order(raw, &t, g, a).map(|_| ());
    // LAW PMJ-G as written (no co-signer pinned) refuses it.
    let mut law = g;
    law.has_cosigner = false;
    n += expect(order(&law, &ask), Err(TxErr::Signers), "LAW PMJ-G as written passed a co-signed order")?;
    // An empty allowlist refuses everything.
    let empty = TxGuard::refuse_all(g.wallet);
    if order(&empty, &ask).is_ok() {
        return Err(SelfTestErr("an empty allowlist passed an order"));
    }
    n += 1;
    // A limit one micro dollar under the signed cap; a deposit off by one.
    let low = OrderAsk { limit_micro: V2_LIMIT.cap - 1, ..ask };
    n += expect(order(&g, &low), Err(TxErr::PriceAboveLimit), "a cap above the limit passed")?;
    let off = OrderAsk { deposit: V2_LIMIT.deposit - 1, ..ask };
    n += expect(order(&g, &off), Err(TxErr::DepositMismatch), "another deposit passed")?;
    // Another deposit account; no vault; another wallet.
    let mut other = g;
    other.deposit_account = [0x33; KEY_LEN];
    n += expect(order(&other, &ask), Err(TxErr::DebitSource), "another deposit account passed")?;
    other = g;
    other.vault = [0; KEY_LEN];
    n += expect(order(&other, &ask), Err(TxErr::Vault), "no pinned vault passed")?;
    other = g;
    other.wallet = [0x11; KEY_LEN];
    n += expect(order(&other, &ask), Err(TxErr::FeePayer), "another wallet's order passed")?;
    // The answer's own words: another order account, another market.
    let swapped = OrderAsk { order: ask.position, position: ask.order, ..ask };
    n += expect(order(&g, &swapped), Err(TxErr::OrderAccounts), "the answer's swapped accounts passed")?;
    let market = OrderAsk { market_hash: V1_LIMIT.market, ..ask };
    n += expect(order(&g, &market), Err(TxErr::MarketMismatch), "another market's hash passed")?;
    // A claim's guard refuses a create-order.
    n += expect(guard_claim(raw, &t, &g).map(|_| ()), Err(TxErr::JupInstruction), "the claim guard passed an order")?;

    // A Limit create_order (v1) passes; a Market one is refused.
    let len = decode(v1_limit, &mut buf)?;
    let raw = &buf[..len];
    let t = SolTx::parse(raw).map_err(|_| SelfTestErr("the create_order Limit fixture does not parse"))?;
    let g = guard_of(V1_LIMIT.owner, V1_LIMIT.source)?;
    match guard_order(raw, &t, &g, &ask_of(&V1_LIMIT)?) {
        Ok((_, v)) if v.version == 1 && v.max_fill_price_usd == V1_LIMIT.cap => n += 1,
        _ => return Err(SelfTestErr("a public Limit create_order at its own cap was not passed as read")),
    }
    let len = decode(v1_market, &mut buf)?;
    let raw = &buf[..len];
    let t = SolTx::parse(raw).map_err(|_| SelfTestErr("the create_order Market fixture does not parse"))?;
    let g = guard_of(V1_MARKET.owner, V1_MARKET.source)?;
    n += expect(
        guard_order(raw, &t, &g, &ask_of(&V1_MARKET)?).map(|_| ()),
        Err(TxErr::NotLimit),
        "a Market order passed (only a Limit binds its price)",
    )?;

    // The public claim passes the claim guard (one signer: no co-signer)
    // and no order guard; its payout must land in our deposit account.
    let len = decode(claim, &mut buf)?;
    let raw = &buf[..len];
    let t = SolTx::parse(raw).map_err(|_| SelfTestErr("the claim fixture does not parse"))?;
    let mut g = guard_of(CLAIM_OWNER, CLAIM_ACCOUNT)?;
    g.has_cosigner = false;
    n += expect(guard_claim(raw, &t, &g).map(|_| ()), Ok(()), "the public claim_payout2 was refused")?;
    let mut other = g;
    other.deposit_account = [0x33; KEY_LEN];
    n += expect(guard_claim(raw, &t, &other).map(|_| ()), Err(TxErr::OrderAccounts), "a claim paying another account passed")?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guards_known_answers_hold_in_this_binary() {
        assert_eq!(run(), Ok(15), "every check of the self-test");
    }

    #[test]
    fn a_corrupted_fixture_refuses_the_self_test() {
        // One base64 digit of the Limit order changed: a signature, a key or
        // a parameter no longer matches what the self-test pins.
        let mut v2 = FX_V2_LIMIT.to_vec();
        let at = v2.len() / 2;
        v2[at] = if v2[at] == b'A' { b'B' } else { b'A' };
        assert!(run_with(FX_V1_MARKET, FX_V1_LIMIT, &v2, FX_CLAIM2).is_err());
        // A Market order in the Limit order's place.
        assert!(run_with(FX_V1_MARKET, FX_V1_LIMIT, FX_V1_MARKET, FX_CLAIM2).is_err());
        // A claim that is not the claim.
        assert!(run_with(FX_V1_MARKET, FX_V1_LIMIT, FX_V2_LIMIT, FX_V2_LIMIT).is_err());
        assert!(run_with(FX_V1_MARKET, FX_V1_LIMIT, FX_V2_LIMIT, b"not base64!").is_err());
    }
}
