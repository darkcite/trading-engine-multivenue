// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The send arm: sign → render into the request body → post over one
//! keep-alive HTTPS connection → reconcile.
//!
//! **Blocking, on its own thread.** Every call is one request/response
//! cycle bounded by `core_net::https_post::REQ_DEADLINE`; the engine
//! loop never calls in here (H8 hands decisions over a ring).
//!
//! ## Reconciliation — what "sent" means here
//!
//! The transaction hash is computed LOCALLY from what was signed
//! (`signer_evm::tx_hash`), before the node answers. So:
//!
//! * the node's `eth_sendRawTransaction` answer must EQUAL it — a
//!   different hash means the node decoded different bytes than were
//!   signed, and the arm HALTS ([`HaltCause`]; nothing further is sent);
//! * a request that left the host without an answer (timeout, torn
//!   connection, a proxy's non-200) is **maybe sent**: it is tracked by
//!   the local hash exactly like an accepted one, and the receipt — or
//!   its absence past [`RECEIPT_TIMEOUT_NS`] — decides;
//! * a receipt must name the hash, the wallet as `from`, and the
//!   expected `to` (for a creation: `to` null and the address
//!   `create_address(from, nonce)`); anything else HALTS the arm.
//!
//! Node refusals are keyed on [`SendRefusal`], never on text: a fee
//! below what the node admits returns the nonce ([`NonceTable::unused`]),
//! a nonce refusal or an unclassifiable one quarantines the wallet
//! until [`EvmArm::sync`] sees the chain settled, insufficient funds
//! parks it as `Unfunded`.

use core_net::{HttpsPost, PostErr};
use signer_eip712::SecretKey;
use signer_evm::{create_address, Eip1559Create, Eip1559Tx, EvmTxErr, PreparedTx};

use crate::calldata::{encode_swap, SwapCall, SWAP_CALLDATA_LEN};
use crate::gas::{fee_paid_wei, GasBid, SWAP_GAS_LIMIT};
use crate::nonce::{NonceTable, WalletState, MAX_WALLETS};
use crate::rpc::{
    classify_send_refusal, scan_hash, scan_next_base_fee, scan_quantity, scan_receipt,
    scan_transfers, scan_word, write_balance, write_call, write_chain_id, write_fee_history,
    write_receipt, write_send_raw, write_tx_count, BlockTag, Receipt, ScanErr, SendRefusal,
    TokenMove,
};
use crate::{MainnetAuthority, Network};

/// Request-body buffer: a creation's init code (the executor is 2.3 KB,
/// 4.7 KB as hex) plus the envelope, with room.
pub const MAX_BODY: usize = 8 * 1024;
/// Response buffer: a swap receipt with its logs is ~3 KB.
pub const MAX_RESP: usize = 32 * 1024;
/// A transaction with no receipt this long after it was sent is
/// presumed dropped: its wallet is quarantined until a sync.
pub const RECEIPT_TIMEOUT_NS: u64 = 30_000_000_000;

/// Why the arm could not be built.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmBootErr {
    /// No keys, or more than [`MAX_WALLETS`].
    WalletCount,
    /// A key that is not a valid secp256k1 scalar.
    BadKey,
    /// Two keys for one address.
    DuplicateWallet,
    /// The HTTPS client's body window is under [`MAX_BODY`].
    BodyWindow,
    /// [`Network::Mainnet`] through [`EvmArm::new`]: mainnet needs a
    /// [`MainnetAuthority`] ([`EvmArm::new_mainnet`], O-HL1).
    MainnetUnauthorised,
}

impl core::fmt::Display for ArmBootErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::WalletCount => "evm arm: 1..=8 wallet keys are required",
            Self::BadKey => "evm arm: a wallet key is not a valid secp256k1 secret",
            Self::DuplicateWallet => "evm arm: two keys resolve to one address",
            Self::BodyWindow => "evm arm: the https client's body window is under MAX_BODY",
            Self::MainnetUnauthorised => {
                "evm arm: HyperEVM MAINNET needs a MainnetAuthority (EvmArm::new_mainnet, O-HL1)"
            }
        })
    }
}

impl std::error::Error for ArmBootErr {}

/// Why a call failed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ArmErr {
    /// The HTTPS cycle failed.
    Post(PostErr),
    /// A non-200 answer.
    Status(u16),
    /// The answer did not scan (or the node returned an RPC error).
    Scan(ScanErr),
    /// The node answered with a THROTTLE (`-32005 rate limited` on the
    /// public testnet endpoint): the endpoint said no, not the request —
    /// retry later (H9: boot reads this as "dark", never a refusal).
    RateLimited,
    /// Signing or rendering failed.
    Render(EvmTxErr),
    /// The endpoint serves another chain.
    ChainMismatch {
        /// The endpoint's `eth_chainId`.
        got: u64,
    },
    /// A receipt disagrees with the transaction it answers (the arm
    /// halted).
    ReceiptMismatch,
    /// No such wallet, or it is not in the state the call needs.
    Wallet,
}

impl core::fmt::Display for ArmErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Post(e) => write!(f, "evm arm: {e}"),
            Self::Status(s) => write!(f, "evm arm: http status {s}"),
            Self::Scan(e) => write!(f, "evm arm: unreadable answer ({e:?})"),
            Self::RateLimited => f.write_str("evm arm: the endpoint rate-limited the request"),
            Self::Render(e) => write!(f, "evm arm: {e}"),
            Self::ChainMismatch { got } => write!(
                f,
                "evm arm: the endpoint serves chain {got}, not the armed network's"
            ),
            Self::ReceiptMismatch => {
                f.write_str("evm arm: HALT — a receipt disagrees with its transaction")
            }
            Self::Wallet => f.write_str("evm arm: wallet index or state refused"),
        }
    }
}

impl std::error::Error for ArmErr {}

/// Why the arm halted — the FIRST cause; nothing is sent after it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HaltCause {
    /// The endpoint's `eth_chainId` is not the armed network's.
    ChainMismatch {
        /// What the endpoint served.
        got: u64,
    },
    /// The node answered a send with a hash other than the one signed:
    /// it decoded different bytes than were signed.
    HashMismatch,
    /// A receipt named another sender, recipient or contract.
    ReceiptMismatch,
    /// The node refused a send as signed for another chain.
    NodeRefusedChain,
}

impl core::fmt::Display for HaltCause {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ChainMismatch { got } => {
                write!(f, "evm arm HALTED: the endpoint serves chain {got}")
            }
            Self::HashMismatch => {
                f.write_str("evm arm HALTED: the node's transaction hash is not the one signed")
            }
            Self::ReceiptMismatch => {
                f.write_str("evm arm HALTED: a receipt disagrees with its transaction")
            }
            Self::NodeRefusedChain => {
                f.write_str("evm arm HALTED: the node refused the chain id signed for")
            }
        }
    }
}

/// What one send did.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendOutcome {
    /// The node holds the transaction (its hash reconciled).
    Sent {
        /// Wallet index.
        wallet: usize,
        /// Nonce used.
        nonce: u64,
        /// Transaction hash.
        hash: [u8; 32],
    },
    /// The request left the host without a readable answer; tracked by
    /// the local hash until a receipt or the timeout decides.
    MaybeSent {
        /// Wallet index.
        wallet: usize,
        /// Nonce used.
        nonce: u64,
        /// Transaction hash.
        hash: [u8; 32],
    },
    /// The node refused it (see [`SendRefusal`] for the nonce's fate).
    Refused {
        /// Wallet index.
        wallet: usize,
        /// Why.
        why: SendRefusal,
    },
    /// Nothing left the host; the nonce was returned (or the wallet was
    /// not `Ready`).
    NotSent {
        /// Wallet index.
        wallet: usize,
        /// Why.
        err: ArmErr,
    },
    /// The arm is halted (now, or by this send).
    Halted,
}

/// What one receipt poll found.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PollOutcome {
    /// The wallet has nothing in flight.
    Idle,
    /// Not mined yet.
    Pending,
    /// Mined and reconciled (success or revert) — the receipt is
    /// [`EvmArm::last_receipt`] until the next poll (it is scanned into
    /// the arm's own storage, never moved).
    Mined {
        /// Wallet index.
        wallet: usize,
        /// The caller's correlation tag.
        tag: u64,
        /// The nonce it consumed.
        nonce: u64,
    },
    /// No receipt within [`RECEIPT_TIMEOUT_NS`]: the wallet is
    /// quarantined until a sync.
    TimedOut {
        /// Wallet index.
        wallet: usize,
        /// The caller's correlation tag.
        tag: u64,
        /// The nonce it used.
        nonce: u64,
    },
    /// The poll itself failed; nothing changed.
    Err(ArmErr),
}

/// A wallet's chain state at a sync.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SyncReport {
    /// Mined transaction count.
    pub latest: u64,
    /// Mined plus pool.
    pub pending: u64,
    /// Balance, wei.
    pub balance_wei: u128,
    /// The table's verdict.
    pub state: WalletState,
}

/// The arm's own counters (owned by its thread; H8 mirrors them).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct EvmArmCounters {
    /// Transactions signed and posted.
    pub sends: u64,
    /// Accepted by the node (hash reconciled, or already known).
    pub accepted: u64,
    /// Left the host without a readable answer.
    pub maybe_sent: u64,
    /// Refused: fee below what the node admits (a losing bid).
    pub refused_fee: u64,
    /// Refused: nonce too low / replacement underpriced.
    pub refused_nonce: u64,
    /// Refused: insufficient funds.
    pub refused_funds: u64,
    /// Refused: the endpoint throttled it (nonce not taken).
    pub refused_rate: u64,
    /// Refused: anything else.
    pub refused_other: u64,
    /// Failed before the request left the host.
    pub not_sent: u64,
    /// Mined with status 1.
    pub mined_ok: u64,
    /// Mined with status 0 (e.g. `BelowMinOut` — a lost race).
    pub mined_reverted: u64,
    /// No receipt within the timeout.
    pub timeouts: u64,
    /// Wallet syncs.
    pub syncs: u64,
    /// Halts (hash or receipt mismatch, wrong chain).
    pub halts: u64,
    /// Gas paid by mined transactions, wei.
    pub gas_paid_wei: u128,
}

struct Wallet {
    sk: SecretKey,
    addr: [u8; 20],
}

#[derive(Copy, Clone)]
struct InFlight {
    hash: [u8; 32],
    expect_to: [u8; 20],
    sent_ns: u64,
    nonce: u64,
    tag: u64,
    create: bool,
}

impl InFlight {
    const NONE: Self = Self {
        hash: [0; 32],
        expect_to: [0; 20],
        sent_ns: 0,
        nonce: 0,
        tag: 0,
        create: false,
    };
}

/// What the node's answer to a send meant (decided while the response
/// is borrowed, acted on after).
enum Verdict {
    Accepted,
    Refused(SendRefusal),
    Unreadable,
    Mismatch,
}

/// The HyperEVM send arm. Construct at boot; drive from one thread.
pub struct EvmArm {
    http: HttpsPost,
    network: Network,
    wallets: Box<[Wallet]>,
    nonces: NonceTable,
    inflight: [InFlight; MAX_WALLETS],
    next_id: u64,
    halted: Option<HaltCause>,
    counters: EvmArmCounters,
    /// The last receipt a poll scanned (read after `Mined`).
    last_receipt: Receipt,
    /// Where that receipt's body sits in the response buffer, and the
    /// request id it answered (HYPARB L3 reads its logs).
    last_body: core::ops::Range<usize>,
    last_body_id: u64,
}

impl EvmArm {
    /// Build a TESTNET arm. **Boot-only** (the allocations live here).
    /// Keys stay in their mlock'd pages; each is parsed once into the
    /// signer's key type, as `exec-hyperliquid` does. No request is made.
    /// [`Network::Mainnet`] is refused: mainnet is [`Self::new_mainnet`].
    pub fn new(
        http: HttpsPost,
        network: Network,
        keys: &[core_config::SecretKeyBytes],
    ) -> Result<Self, ArmBootErr> {
        if network == Network::Mainnet {
            return Err(ArmBootErr::MainnetUnauthorised);
        }
        Self::build(http, network, keys)
    }

    /// Build a HyperEVM MAINNET arm (O-HL1): the only constructor that
    /// signs for chain 999, and it takes the [`MainnetAuthority`] as
    /// proof that a door allowed it. Boot-only, as [`Self::new`].
    pub fn new_mainnet(
        http: HttpsPost,
        _authority: &MainnetAuthority,
        keys: &[core_config::SecretKeyBytes],
    ) -> Result<Self, ArmBootErr> {
        Self::build(http, Network::Mainnet, keys)
    }

    fn build(
        http: HttpsPost,
        network: Network,
        keys: &[core_config::SecretKeyBytes],
    ) -> Result<Self, ArmBootErr> {
        if keys.is_empty() || keys.len() > MAX_WALLETS {
            return Err(ArmBootErr::WalletCount);
        }
        // Requests are rendered straight into the client's wire buffer.
        if http.body_cap() < MAX_BODY {
            return Err(ArmBootErr::BodyWindow);
        }
        let mut wallets: Vec<Wallet> = Vec::with_capacity(keys.len());
        let mut i = 0;
        while i < keys.len() {
            let sk =
                signer_eip712::parse_secret_key(keys[i].bytes()).map_err(|_| ArmBootErr::BadKey)?;
            let addr = signer_eip712::address_from_private_key(keys[i].bytes())
                .map_err(|_| ArmBootErr::BadKey)?;
            let mut j = 0;
            while j < i {
                if wallets[j].addr == addr {
                    return Err(ArmBootErr::DuplicateWallet);
                }
                j += 1;
            }
            wallets.push(Wallet { sk, addr });
            i += 1;
        }
        // Build the signer's process-wide secp256k1 context NOW (a
        // one-time allocation behind a `OnceLock`), not on the first
        // steady-state send.
        signer_eip712::sign_digest_with_key(&wallets[0].sk, &[1u8; 32])
            .map_err(|_| ArmBootErr::BadKey)?;
        Ok(Self {
            http,
            network,
            nonces: NonceTable::new(keys.len()),
            wallets: wallets.into_boxed_slice(),
            inflight: [InFlight::NONE; MAX_WALLETS],
            next_id: 0,
            halted: None,
            counters: EvmArmCounters::default(),
            last_receipt: Receipt::ZERO,
            last_body: 0..0,
            last_body_id: 0,
        })
    }

    /// The receipt the last `Mined` poll reconciled.
    #[inline]
    #[must_use]
    pub const fn last_receipt(&self) -> &Receipt {
        &self.last_receipt
    }

    /// HYPARB L3 — the ERC-20 `Transfer`s between `a` and `b` in the
    /// receipt the last `Mined` poll reconciled: the receipt IS the
    /// fill. Read it before the next request, which reuses the response
    /// buffer; afterwards the stored request id no longer matches and
    /// the scan refuses rather than reading another answer.
    pub fn last_receipt_transfers(
        &self,
        a: &[u8; 20],
        b: &[u8; 20],
        out: &mut [TokenMove],
    ) -> Result<usize, ScanErr> {
        let body = self
            .http
            .resp()
            .get(self.last_body.clone())
            .ok_or(ScanErr::Malformed)?;
        scan_transfers(body, self.last_body_id, a, b, out)
    }

    /// The armed network.
    #[inline]
    #[must_use]
    pub const fn network(&self) -> Network {
        self.network
    }

    /// Wallets driven.
    #[inline]
    #[must_use]
    pub fn wallets(&self) -> usize {
        self.wallets.len()
    }

    /// Wallet `w`'s address.
    #[inline]
    #[must_use]
    pub fn address(&self, w: usize) -> Option<[u8; 20]> {
        if w < self.wallets.len() {
            Some(self.wallets[w].addr)
        } else {
            None
        }
    }

    /// The nonce table (read-only).
    #[inline]
    #[must_use]
    pub const fn nonces(&self) -> &NonceTable {
        &self.nonces
    }

    /// The counters.
    #[inline]
    #[must_use]
    pub const fn counters(&self) -> &EvmArmCounters {
        &self.counters
    }

    /// Why the arm halted, if it has.
    #[inline]
    #[must_use]
    pub const fn halted(&self) -> Option<HaltCause> {
        self.halted
    }

    /// The next `Ready` wallet, round-robin.
    #[inline]
    pub fn pick(&mut self) -> Option<usize> {
        self.nonces.pick()
    }

    #[inline]
    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn halt(&mut self, why: HaltCause) {
        if self.halted.is_none() {
            self.halted = Some(why);
            self.counters.halts += 1;
        }
    }

    /// Post the first `n` bytes of the client's body window (every
    /// writer renders there, in place); the answer's body range.
    fn exchange(&mut self, n: usize) -> Result<core::ops::Range<usize>, ArmErr> {
        let (status, range) = self.http.post(n).map_err(ArmErr::Post)?;
        if status != 200 {
            return Err(ArmErr::Status(status));
        }
        Ok(range)
    }

    /// One read whose answer is a QUANTITY.
    fn read_quantity(&mut self, n: usize, id: u64) -> Result<u128, ArmErr> {
        let r = self.exchange(n)?;
        scan_quantity(&self.http.resp()[r.clone()], id).map_err(|e| self.scan_err(e, r))
    }

    /// A scanner's refusal of the answer at `body` in the response: a
    /// node error whose message is a throttle is [`ArmErr::RateLimited`],
    /// anything else [`ArmErr::Scan`]. (The error's message offsets are
    /// relative to `body`, the slice the scanner saw.)
    fn scan_err(&self, e: ScanErr, body: core::ops::Range<usize>) -> ArmErr {
        if let ScanErr::Rpc(rpc) = e {
            let b = &self.http.resp()[body];
            let (s, t) = (rpc.message_start as usize, rpc.message_end as usize);
            if s <= t && t <= b.len() && classify_send_refusal(&b[s..t]) == SendRefusal::RateLimited
            {
                return ArmErr::RateLimited;
            }
        }
        ArmErr::Scan(e)
    }

    /// Layer 4 at the wire: the endpoint must serve the armed chain. A
    /// mismatch HALTS the arm.
    pub fn verify_chain(&mut self) -> Result<(), ArmErr> {
        let id = self.id();
        let n = write_chain_id(self.http.body_mut(), id).map_err(too_small)?;
        let got = self.read_quantity(n, id)?;
        let want = self.network.chain_id();
        if got != want as u128 {
            let got = u64::try_from(got).unwrap_or(u64::MAX);
            self.halt(HaltCause::ChainMismatch { got });
            return Err(ArmErr::ChainMismatch { got });
        }
        Ok(())
    }

    /// Read wallet `w`'s `latest` / `pending` counts and balance and
    /// reconcile the table: `Ready` only when nothing is outstanding and
    /// the balance is not zero. Refused while `w` is in flight.
    pub fn sync(&mut self, w: usize) -> Result<SyncReport, ArmErr> {
        if w >= self.wallets.len() || self.nonces.state(w) == WalletState::InFlight {
            return Err(ArmErr::Wallet);
        }
        let addr = self.wallets[w].addr;
        let id = self.id();
        let n =
            write_tx_count(self.http.body_mut(), id, &addr, BlockTag::Latest).map_err(too_small)?;
        let latest = self.read_quantity(n, id)?;
        let id = self.id();
        let n = write_tx_count(self.http.body_mut(), id, &addr, BlockTag::Pending)
            .map_err(too_small)?;
        let pending = self.read_quantity(n, id)?;
        let id = self.id();
        let n = write_balance(self.http.body_mut(), id, &addr).map_err(too_small)?;
        let balance_wei = self.read_quantity(n, id)?;
        let latest = u64::try_from(latest).map_err(|_| ArmErr::Scan(ScanErr::Malformed))?;
        let pending = u64::try_from(pending).map_err(|_| ArmErr::Scan(ScanErr::Malformed))?;
        self.nonces.sync(w, latest, pending);
        if balance_wei == 0 {
            self.nonces.unfunded(w);
        }
        self.counters.syncs += 1;
        Ok(SyncReport {
            latest,
            pending,
            balance_wei,
            state: self.nonces.state(w),
        })
    }

    /// `owner()` of `contract` (cold — boot and the battery): the one
    /// sender the executor's `swap` accepts. A contract that does not
    /// answer one address-shaped word refuses (`Scan`).
    pub fn owner_of(&mut self, contract: &[u8; 20]) -> Result<[u8; 20], ArmErr> {
        let w = self.call_word(contract, &crate::calldata::OWNER_SELECTOR)?;
        let mut hi = 0usize;
        while hi < 12 {
            if w[hi] != 0 {
                return Err(ArmErr::Scan(ScanErr::Malformed));
            }
            hi += 1;
        }
        let mut a = [0u8; 20];
        // COPY: the 20 B address out of its 32 B ABI word, cold (boot /
        // battery) — a ≤ 64 B POD returned by value — rejected: handing
        // back the word (every caller wants the address).
        a.copy_from_slice(&w[12..]);
        Ok(a)
    }

    /// One single-word view call (`eth_call` at `latest`): `owner()`,
    /// `balanceOf(…)` — the HYPARB L3 reconciler's inventory reads.
    pub fn call_word(&mut self, to: &[u8; 20], data: &[u8]) -> Result<[u8; 32], ArmErr> {
        let id = self.id();
        let n = write_call(self.http.body_mut(), id, to, data).map_err(too_small)?;
        let r = self.exchange(n)?;
        scan_word(&self.http.resp()[r.clone()], id).map_err(|e| self.scan_err(e, r))
    }

    /// The next block's base fee (`eth_feeHistory`).
    pub fn next_base_fee(&mut self) -> Result<u128, ArmErr> {
        let id = self.id();
        let n = write_fee_history(self.http.body_mut(), id).map_err(too_small)?;
        let r = self.exchange(n)?;
        scan_next_base_fee(&self.http.resp()[r.clone()], id).map_err(|e| self.scan_err(e, r))
    }

    /// Send one executor swap from wallet `w` (from [`Self::pick`]).
    /// The calldata is a stack array the transaction borrows.
    pub fn send_swap(
        &mut self,
        w: usize,
        executor: &[u8; 20],
        call: &SwapCall,
        bid: GasBid,
        tag: u64,
        now_ns: u64,
    ) -> SendOutcome {
        let mut cd = [0u8; SWAP_CALLDATA_LEN];
        encode_swap(call, &mut cd);
        self.send_call(w, executor, 0, &cd, SWAP_GAS_LIMIT, bid, tag, now_ns)
    }

    /// Send one call (`to`, `value`, `data`) from wallet `w`.
    #[allow(clippy::too_many_arguments)]
    pub fn send_call(
        &mut self,
        w: usize,
        to: &[u8; 20],
        value: u128,
        data: &[u8],
        gas_limit: u64,
        bid: GasBid,
        tag: u64,
        now_ns: u64,
    ) -> SendOutcome {
        if self.halted.is_some() {
            return SendOutcome::Halted;
        }
        let Some(nonce) = self.nonces.take(w) else {
            return SendOutcome::NotSent {
                wallet: w,
                err: ArmErr::Wallet,
            };
        };
        let tx = Eip1559Tx {
            chain_id: self.network.chain_id(),
            nonce,
            max_priority_fee_per_gas: bid.max_priority_fee_per_gas,
            max_fee_per_gas: bid.max_fee_per_gas,
            gas_limit,
            to: *to,
            value,
            data,
        };
        self.sign_and_post(w, &PreparedTx::call(&tx), *to, false, nonce, tag, now_ns)
    }

    /// Deploy `init_code` from wallet `w` (cold path — the executor's
    /// deployer, `evm-testnet` / `evm-live deploy`). The deployed address is
    /// `signer_evm::create_address(address(w), nonce)`; the receipt's
    /// `contract` is reconciled against it.
    #[allow(clippy::too_many_arguments)]
    pub fn send_create(
        &mut self,
        w: usize,
        value: u128,
        init_code: &[u8],
        gas_limit: u64,
        bid: GasBid,
        tag: u64,
        now_ns: u64,
    ) -> SendOutcome {
        if self.halted.is_some() {
            return SendOutcome::Halted;
        }
        let Some(nonce) = self.nonces.take(w) else {
            return SendOutcome::NotSent {
                wallet: w,
                err: ArmErr::Wallet,
            };
        };
        let tx = Eip1559Create {
            chain_id: self.network.chain_id(),
            nonce,
            max_priority_fee_per_gas: bid.max_priority_fee_per_gas,
            max_fee_per_gas: bid.max_fee_per_gas,
            gas_limit,
            value,
            init_code,
        };
        let deployed = create_address(&self.wallets[w].addr, nonce);
        self.sign_and_post(
            w,
            &PreparedTx::create(&tx),
            deployed,
            true,
            nonce,
            tag,
            now_ns,
        )
    }

    /// Sign the ONE encoding, hash it, render it into the body, record
    /// the in-flight slot in place, post.
    #[allow(clippy::too_many_arguments)]
    fn sign_and_post(
        &mut self,
        w: usize,
        p: &PreparedTx<'_>,
        expect_to: [u8; 20],
        create: bool,
        nonce: u64,
        tag: u64,
        now_ns: u64,
    ) -> SendOutcome {
        let id = self.id();
        let sig = match p.sign(&self.wallets[w].sk) {
            Ok(s) => s,
            Err(e) => return self.not_sent(w, ArmErr::Render(e)),
        };
        let st = match p.signed(&sig) {
            Ok(st) => st,
            Err(e) => return self.not_sent(w, ArmErr::Render(e)),
        };
        let n = match write_send_raw(self.http.body_mut(), id, &st) {
            Ok(n) => n,
            Err(e) => return self.not_sent(w, ArmErr::Render(e)),
        };
        let f = &mut self.inflight[w];
        f.hash = st.hash();
        f.expect_to = expect_to;
        f.sent_ns = now_ns;
        f.nonce = nonce;
        f.tag = tag;
        f.create = create;
        self.post_send(w, id, n)
    }

    fn not_sent(&mut self, w: usize, err: ArmErr) -> SendOutcome {
        self.nonces.unused(w);
        self.counters.not_sent += 1;
        SendOutcome::NotSent { wallet: w, err }
    }

    /// Post a rendered send (its in-flight slot already recorded) and
    /// act on the answer (module doc).
    fn post_send(&mut self, w: usize, id: u64, n: usize) -> SendOutcome {
        self.counters.sends += 1;
        let (hash, nonce) = (self.inflight[w].hash, self.inflight[w].nonce);
        let range = match self.exchange(n) {
            Ok(r) => r,
            Err(ArmErr::Post(p)) if !p.left_host => {
                return self.not_sent(w, ArmErr::Post(p));
            }
            Err(_) => {
                // Left the host (or a proxy answered non-200): the node
                // may hold it. The local hash tracks it either way.
                self.counters.maybe_sent += 1;
                return SendOutcome::MaybeSent {
                    wallet: w,
                    nonce,
                    hash,
                };
            }
        };
        let verdict = {
            let resp = &self.http.resp()[range];
            match scan_hash(resp, id) {
                Ok(h) if h == hash => Verdict::Accepted,
                Ok(_) => Verdict::Mismatch,
                Err(ScanErr::Rpc(e)) => Verdict::Refused(classify_send_refusal(
                    &resp[e.message_start as usize..e.message_end as usize],
                )),
                Err(_) => Verdict::Unreadable,
            }
        };
        match verdict {
            Verdict::Accepted | Verdict::Refused(SendRefusal::AlreadyKnown) => {
                self.counters.accepted += 1;
                SendOutcome::Sent {
                    wallet: w,
                    nonce,
                    hash,
                }
            }
            Verdict::Unreadable => {
                self.counters.maybe_sent += 1;
                SendOutcome::MaybeSent {
                    wallet: w,
                    nonce,
                    hash,
                }
            }
            Verdict::Mismatch => {
                self.nonces.quarantine(w);
                self.halt(HaltCause::HashMismatch);
                SendOutcome::Halted
            }
            Verdict::Refused(why) => {
                match why {
                    SendRefusal::FeeTooLow => {
                        self.nonces.unused(w);
                        self.counters.refused_fee += 1;
                    }
                    SendRefusal::RateLimited => {
                        self.nonces.unused(w);
                        self.counters.refused_rate += 1;
                    }
                    SendRefusal::InsufficientFunds => {
                        self.nonces.unfunded(w);
                        self.counters.refused_funds += 1;
                    }
                    SendRefusal::NonceTooLow | SendRefusal::ReplacementUnderpriced => {
                        self.nonces.quarantine(w);
                        self.counters.refused_nonce += 1;
                    }
                    SendRefusal::WrongChain => {
                        self.nonces.quarantine(w);
                        self.counters.refused_other += 1;
                        self.halt(HaltCause::NodeRefusedChain);
                        return SendOutcome::Halted;
                    }
                    SendRefusal::Other | SendRefusal::AlreadyKnown => {
                        self.nonces.quarantine(w);
                        self.counters.refused_other += 1;
                    }
                }
                SendOutcome::Refused { wallet: w, why }
            }
        }
    }

    /// Poll wallet `w`'s in-flight transaction for its receipt. On
    /// `Mined` the reconciled receipt is [`Self::last_receipt`].
    pub fn poll(&mut self, w: usize, now_ns: u64) -> PollOutcome {
        if w >= self.wallets.len() || self.nonces.state(w) != WalletState::InFlight {
            return PollOutcome::Idle;
        }
        let id = self.id();
        let n = match write_receipt(self.http.body_mut(), id, &self.inflight[w].hash) {
            Ok(n) => n,
            Err(e) => return PollOutcome::Err(too_small(e)),
        };
        let f = &self.inflight[w];
        let (tag, nonce) = (f.tag, f.nonce);
        let timed_out = now_ns.saturating_sub(f.sent_ns) > RECEIPT_TIMEOUT_NS;
        let got = match self.exchange(n) {
            Ok(r) => {
                self.last_body = r.clone();
                self.last_body_id = id;
                scan_receipt(&self.http.resp()[r.clone()], id, &mut self.last_receipt)
                    .map_err(|e| self.scan_err(e, r))
            }
            Err(e) => Err(e),
        };
        match got {
            Ok(true) => {
                let f = &self.inflight[w];
                let r = &self.last_receipt;
                let to_ok = if f.create {
                    r.is_create && r.contract == f.expect_to
                } else {
                    !r.is_create && r.to == f.expect_to
                };
                if r.tx_hash != f.hash || r.from != self.wallets[w].addr || !to_ok {
                    self.nonces.quarantine(w);
                    self.halt(HaltCause::ReceiptMismatch);
                    return PollOutcome::Err(ArmErr::ReceiptMismatch);
                }
                let (ok, paid) = (
                    r.status == 1,
                    fee_paid_wei(r.gas_used, r.effective_gas_price),
                );
                self.nonces.mined(w);
                if ok {
                    self.counters.mined_ok += 1;
                } else {
                    self.counters.mined_reverted += 1;
                }
                self.counters.gas_paid_wei = self.counters.gas_paid_wei.saturating_add(paid);
                PollOutcome::Mined {
                    wallet: w,
                    tag,
                    nonce,
                }
            }
            Ok(false) | Err(_) if timed_out => {
                self.nonces.quarantine(w);
                self.counters.timeouts += 1;
                PollOutcome::TimedOut {
                    wallet: w,
                    tag,
                    nonce,
                }
            }
            Ok(false) => PollOutcome::Pending,
            Err(e) => PollOutcome::Err(e),
        }
    }
}

#[inline(always)]
fn too_small(_: ingress_rpc::RpcWriteErr) -> ArmErr {
    ArmErr::Render(EvmTxErr::BufferTooSmall)
}
