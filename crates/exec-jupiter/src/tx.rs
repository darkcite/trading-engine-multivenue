// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # tx — the Solana transaction Jupiter builds: parsed in place, guarded,
//! signed in its own buffer (PMJ J3)
//!
//! `POST /orders` answers with a base64 Solana transaction that WE sign. The
//! server built it, so it is untrusted input to a signing key: **this crate
//! never signs a transaction the guard has not passed** ([`guard`]), and an
//! empty allowlist refuses everything.
//!
//! ## The wire (legacy and v0)
//!
//! ```text
//! compact-u16 n · n × 64-byte signatures · message
//! message  = [0x80 | version]? · header[3] · compact-u16 k · k × 32-byte keys
//!          · blockhash[32] · compact-u16 m · m × instruction · (v0) lookups
//! instr.   = program index u8 · compact-u16 a · a × account index u8
//!          · compact-u16 d · d data bytes
//! lookup   = table key[32] · compact-u16 w · w × u8 · compact-u16 r · r × u8
//! ```
//!
//! [`SolTx::parse`] records OFFSETS into the caller's buffer and copies
//! nothing; it mirrors the J0 probe's `decode_tx` (the reference parse) and
//! is pinned by its synthetic transaction. A v0 message's invoked program id
//! must be a STATIC key (the runtime does not load program ids from a lookup
//! table); the parser refuses an index past the static keys outright.
//!
//! ## The guard ([`guard`], plan §3.3; the session prompt's J3 list)
//!
//! Refused unless every line holds:
//! - the fee payer (key 0) is our wallet, and our wallet is a required
//!   signer;
//! - every invoked program id is in `TxGuard::programs`;
//! - every lookup table is in `TxGuard::alts`;
//! - every SPL Token / Token-2022 instruction that names our wallet does so
//!   as the AUTHORITY of a `Transfer` / `TransferChecked`, and those debits
//!   sum to at most the order's `depositAmount`; any other token instruction
//!   naming our wallet (approve, set authority, close, burn, …) refuses;
//! - every System instruction funded by our wallet is a `Transfer` or a
//!   `CreateAccount` (rent), and their lamports sum to at most
//!   `TxGuard::max_sol_lamports`; any other System instruction naming our
//!   wallet refuses;
//! - a Compute Budget unit price at most `TxGuard::max_cu_price`;
//! - nothing after the last byte the wire describes, and the whole
//!   transaction within Solana's 1 232-byte packet.
//!
//! **What the guard cannot see:** a cross-program invocation's own token
//! moves are not in the message. The program allowlist is what bounds them —
//! a program on it is trusted with the authority the message hands it. That
//! limit is recorded in risk-policy "PMJ — slot 11".
//!
//! ## The create-order guard ([`guard_order`]; risk-policy J3, pre-dust (b))
//!
//! Jupiter Predict's program moves the deposit itself, by a CPI the message
//! does not show, so an ORDER build is signed only through [`guard_order`]:
//! [`guard`]'s lines with a top-level token debit bound of ZERO (the
//! create-order's CPI moves exactly its `deposit_amount`; anything beside it
//! would add to it), then the order read out of the signed instruction data
//! ([`CreateOrderView`]: both live discriminators, `create_order` and the
//! deployed-but-unpublished `create_order2`, carry the same Borsh
//! `CreateOrderParams` of IDL v0.10.0). Refused unless:
//! - the transaction holds exactly ONE instruction of the Predict program
//!   and it is a create-order ([`TxErr::JupInstruction`]); its data reads
//!   whole — strings of at most [`CREATE_STR_MAX`] bytes, bool, enum and
//!   Option tags 0 or 1, nothing after the last field ([`TxErr::OrderData`]);
//! - it buys: `is_buy`, and `is_yes` — the answer law's `isYes: true` for
//!   either outcome, J3 ([`TxErr::NotBuy`]);
//! - it is a LIMIT order ([`TxErr::NotLimit`]): only a Limit order binds its
//!   price on chain — the keeper fills Market orders above their
//!   `max_fill_price_usd` (public W4, 2026-10-29: up to +78 %), never a
//!   Limit one;
//! - its `max_fill_price_usd` is at most the member's limit
//!   ([`TxErr::PriceAboveLimit`]), its `deposit_amount` exactly the deposit
//!   we asked ([`TxErr::DepositMismatch`]), and `contracts ×
//!   max_fill_price_usd / 1e6 ≤ deposit_amount`
//!   ([`TxErr::ContractsOverDeposit`]);
//! - no integrator and no integrator fee ([`TxErr::Integrator`]);
//! - by ROLE (the table below, read off public transactions, W5): the payer
//!   and the owner are our wallet, and the order and position accounts are
//!   the build answer's own `orderPubkey` / `positionPubkey` — the keys
//!   jup-gw polls, reconciles and releases by ([`TxErr::OrderAccounts`]);
//!   the deposit's source is the deposit account and the settlement mint the
//!   deposit mint, each a static key ([`TxErr::DebitSource`]); the vault is
//!   the pinned one ([`TxErr::Vault`]);
//! - its `market_id` string is the answer's `order.marketIdHash`
//!   ([`TxErr::MarketMismatch`]) — bound to the ANSWER only: how Jupiter
//!   derives the hash from `POLY-<id>-<o>` is unknown (not md5 / sha of the
//!   obvious strings), a J0 fact; the answer's `marketId` is checked by
//!   jup-gw against the window;
//! - its `contracts` are not zero and equal the answer's `contractsMicro`
//!   ([`TxErr::ContractsMismatch`]);
//! - the transaction holds nothing but Compute Budget instructions and the
//!   one create-order ([`TxErr::ExtraInstruction`]).
//!
//! | role | `create_order` (12 accounts) | `create_order2` (13) |
//! |---|---|---|
//! | payer · owner · authority (the co-signer) · vault · position · order | 0 – 5 | 0 – 5 |
//! | market result (the inserted account, a `MarketResult`) | — | 6 |
//! | owner token account: the CPI deposit's source | 6 | 7 |
//! | settlement mint | 7 | 8 |
//! | order escrow ATA · Token · ATA program · System | 8 – 11 | 9 – 12 |
//!
//! **The vault** is pinned by `TxGuard::vault` (`pmj.toml` `guard_vault`;
//! none pins nothing and refuses every order and claim): a static key equal
//! to it, or — Jupiter loads it through its lookup table — the pinned table
//! entry (`TxGuard::vault_alt` / `vault_alt_entry`, `guard_vault_alt`: a
//! table's written entries never change; the J5 boot re-reads that the
//! entry is the vault before arming).
//!
//! **A claim** ([`guard_claim`]): [`guard`]'s lines with no token leaving our
//! account, and exactly one Predict instruction, `claim_payout` or
//! `claim_payout2` (seen on chain signed by the owner, W5), with no data past
//! its discriminator and 7 accounts: the authority our wallet, the pinned
//! vault, and the account the payout lands in our deposit account. Beside
//! it only Compute Budget instructions and at most one ATA
//! `CreateIdempotent` of our deposit account (payer and owner our wallet,
//! the deposit mint, System, Token) — the public claim's shape
//! ([`TxErr::ExtraInstruction`]).
//!
//! ## One pinned co-signer (DRAFT amendment, R-PMJ-S44 — dormant)
//!
//! LAW PMJ-G requires exactly one signature, ours. The vault draft
//! `pmj-g-amendment-draft.md` (G-S1..G-S5) admits ONE pinned co-signer
//! (`TxGuard::has_cosigner`; `pmj.toml` `guard_cosigners`): exactly two
//! required signers — our wallet as the fee payer and writable signer
//! (key 0), the pinned key as a READ-ONLY signer (key 1); a co-signature
//! already in its slot must verify over the very message we sign
//! ([`TxErr::CosignerSignature`]); an empty slot passes (Jupiter signs at
//! `/execute`); our signature goes into our slot only. **It is not law:**
//! the key stays empty (dormant) in every committed config, so the guard
//! refuses a second signer exactly as before; it becomes law only after
//! Anton's own J0 build confirms the co-signer and he rules (R-PMJ-S44).
//!
//! ## Signing ([`SolTx::sign_into`])
//!
//! The message is the bytes after the signature table; our signature goes
//! RAW into slot `idx` of that table, in the same buffer
//! (`Ed25519Signer::sign_raw`), so the transaction is never assembled twice.
//!
//! Zero allocation; while-index loops; every read bounds-checked against the
//! transaction's length (a malformed answer is a refusal, never a panic).

use signer_ed25519::Ed25519Signer;

/// Solana's packet limit: the largest transaction the network carries.
pub const TX_MAX: usize = 1_232;
/// A public key / account address.
pub const KEY_LEN: usize = 32;
/// A signature.
pub const SIG_LEN: usize = 64;
/// Instructions the parser keeps (more refuses: an order transaction holds
/// a handful).
pub const IX_MAX: usize = 24;
/// Lookup tables the parser keeps (more refuses).
pub const ALT_MAX: usize = 4;
/// Required signers the parser accepts (more refuses).
pub const SIGNERS_MAX: usize = 4;
/// Programs a guard's allowlist holds.
pub const GUARD_PROGRAMS_MAX: usize = 16;
/// Lookup tables a guard's allowlist holds.
pub const GUARD_ALTS_MAX: usize = 8;

/// SPL Token.
pub const TOKEN_PROGRAM: [u8; KEY_LEN] = [
    0x06, 0xdd, 0xf6, 0xe1, 0xd7, 0x65, 0xa1, 0x93, 0xd9, 0xcb, 0xe1, 0x46, 0xce, 0xeb, 0x79, 0xac,
    0x1c, 0xb4, 0x85, 0xed, 0x5f, 0x5b, 0x37, 0x91, 0x3a, 0x8c, 0xf5, 0x85, 0x7e, 0xff, 0x00, 0xa9,
];
/// SPL Token-2022.
pub const TOKEN_2022_PROGRAM: [u8; KEY_LEN] = [
    0x06, 0xdd, 0xf6, 0xe1, 0xee, 0x75, 0x8f, 0xde, 0x18, 0x42, 0x5d, 0xbc, 0xe4, 0x6c, 0xcd, 0xda,
    0xb6, 0x1a, 0xfc, 0x4d, 0x83, 0xb9, 0x0d, 0x27, 0xfe, 0xbd, 0xf9, 0x28, 0xd8, 0xa1, 0x8b, 0xfc,
];
/// The System program (32 zero bytes, base58 `1111…1`).
pub const SYSTEM_PROGRAM: [u8; KEY_LEN] = [0; KEY_LEN];
/// The Compute Budget program.
pub const COMPUTE_BUDGET_PROGRAM: [u8; KEY_LEN] = [
    0x03, 0x06, 0x46, 0x6f, 0xe5, 0x21, 0x17, 0x32, 0xff, 0xec, 0xad, 0xba, 0x72, 0xc3, 0x9b, 0xe7,
    0xbc, 0x8c, 0xe5, 0xbb, 0xc5, 0xf7, 0x12, 0x6b, 0x2c, 0x43, 0x9b, 0x3a, 0x40, 0x00, 0x00, 0x00,
];
/// The Associated Token Account program
/// (`ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL`).
pub const ATA_PROGRAM: [u8; KEY_LEN] = [
    0x8c, 0x97, 0x25, 0x8f, 0x4e, 0x24, 0x89, 0xf1, 0xbb, 0x3d, 0x10, 0x29, 0x14, 0x8e, 0x0d, 0x83,
    0x0b, 0x5a, 0x13, 0x99, 0xda, 0xff, 0x10, 0x84, 0x04, 0x8e, 0x7b, 0xd8, 0xdb, 0xe9, 0xf8, 0x59,
];
/// Jupiter Predict's program (`3ZZuTbwC6aJbvteyVxXUS7gtFYdf7AuXeitx6VyvjvUp`):
/// the program whose one instruction [`guard_order`] and [`guard_claim`]
/// read (`TxGuard::predict_program` defaults to it).
pub const PREDICT_PROGRAM: [u8; KEY_LEN] = [
    0x26, 0x0f, 0x2a, 0x39, 0x21, 0xbe, 0xef, 0xd1, 0x8a, 0x6c, 0xb3, 0x23, 0xf2, 0x40, 0xf6, 0x83,
    0xb1, 0x85, 0xd8, 0xce, 0x9c, 0x66, 0x9c, 0xf9, 0x84, 0xc4, 0x63, 0x55, 0xa3, 0x2f, 0x6d, 0x21,
];
/// `create_order`'s discriminator (`sha256("global:create_order")[..8]`,
/// IDL v0.10.0).
pub const DISC_CREATE_ORDER: [u8; 8] = [0x8d, 0x36, 0x25, 0xcf, 0xed, 0xd2, 0xfa, 0xd7];
/// `create_order2`'s (`sha256("global:create_order2")[..8]`): deployed, not
/// in the published IDL; the same `CreateOrderParams` bytes follow it (W4).
pub const DISC_CREATE_ORDER2: [u8; 8] = [0x96, 0xed, 0x11, 0x3d, 0x3b, 0xc0, 0x7c, 0x34];
/// `claim_payout`'s (IDL v0.10.0; no arguments).
pub const DISC_CLAIM_PAYOUT: [u8; 8] = [0x7f, 0xf0, 0x84, 0x3e, 0xe3, 0xc6, 0x92, 0x85];
/// `claim_payout2`'s (IDL v0.10.0; no arguments; on chain signed by the
/// owner as its `authority`, W5).
pub const DISC_CLAIM_PAYOUT2: [u8; 8] = [0x8d, 0xcd, 0x63, 0xcb, 0x8e, 0xe7, 0x57, 0xe2];
/// The longest `external_order_id` / `market_id` the create-order reader
/// accepts (a 32-character hex hash in every public order seen).
pub const CREATE_STR_MAX: usize = 64;
/// `OrderType::Market`: the keeper may fill it above its cap.
pub const ORDER_TYPE_MARKET: u8 = 0;
/// `OrderType::Limit`: the only order type whose price cap binds.
pub const ORDER_TYPE_LIMIT: u8 = 1;
/// A claim instruction's accounts (`claim_payout` and `claim_payout2`).
pub const CLAIM_ACCOUNTS: u16 = 7;

/// A create-order version's account roles (module docs' table): the roles
/// every version shares are [`ROLE_PAYER`] … [`ROLE_VAULT`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CreateRoles {
    /// The accounts the instruction names.
    pub n_acc: u16,
    /// The owner token account: the CPI deposit's source.
    pub source: u8,
    /// The settlement mint.
    pub mint: u8,
}

/// The fee payer's role (both versions).
pub const ROLE_PAYER: usize = 0;
/// The owner's role (both versions).
pub const ROLE_OWNER: usize = 1;
/// The authority's role (the co-signer on public orders).
pub const ROLE_AUTHORITY: usize = 2;
/// The vault's role (both versions).
pub const ROLE_VAULT: usize = 3;
/// The position account's role (both versions; the answer's `positionPubkey`).
pub const ROLE_POSITION: usize = 4;
/// The order account's role (both versions; the answer's `orderPubkey`).
pub const ROLE_ORDER: usize = 5;
/// `create_order`'s roles (IDL v0.10.0; public slot 458 816 924).
pub const ROLES_V1: CreateRoles = CreateRoles { n_acc: 12, source: 6, mint: 7 };
/// `create_order2`'s roles: a `MarketResult` account inserted at 6 (public
/// slots 462 844 216 and 462 863 510).
pub const ROLES_V2: CreateRoles = CreateRoles { n_acc: 13, source: 7, mint: 8 };
/// A claim's authority (our wallet), vault and payout account.
const CLAIM_AUTHORITY: usize = 0;
const CLAIM_VAULT: usize = 1;
const CLAIM_OWNER_TOKEN: usize = 4;

/// Why a transaction did not parse, or was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TxErr {
    /// Longer than [`TX_MAX`], or empty.
    Length,
    /// The bytes end before the wire says they do, or a compact-u16 is
    /// longer than three bytes.
    Truncated,
    /// Bytes past the end of the message.
    Trailing,
    /// A message version other than legacy or 0.
    Version,
    /// The signature count is not the header's required-signer count, is
    /// zero, or is over [`SIGNERS_MAX`]; or the header is inconsistent with
    /// the keys.
    Header,
    /// More than [`IX_MAX`] instructions or [`ALT_MAX`] lookup tables.
    TooMany,
    /// An account or program index past the keys the message can name.
    Index,
    /// An invoked program id is not a static key.
    ProgramFromTable,
    /// The fee payer is not our wallet.
    FeePayer,
    /// Our wallet is not a required signer.
    NotSigner,
    /// An invoked program is not on the allowlist (an empty one refuses all).
    Program,
    /// A lookup table is not on the allowlist.
    LookupTable,
    /// A token instruction names our wallet other than as a transfer's
    /// authority, or a transfer's data does not read.
    TokenAuthority,
    /// Our wallet's token debits exceed the order's deposit.
    Debit,
    /// A System instruction names our wallet other than as a transfer's or
    /// a rent account's funder.
    SystemUse,
    /// Our wallet's lamports out exceed the guard's cap.
    Lamports,
    /// A compute unit price over the guard's cap, or a compute budget
    /// instruction that does not read.
    ComputePrice,
    /// More than one required signature: our wallet must be the only
    /// signer (a co-signer could hold the transaction, a durable nonce's
    /// authority, …).
    Signers,
    /// A System `AdvanceNonceAccount`: a durable-nonce transaction never
    /// expires, so the server could land it whenever it liked.
    Nonce,
    /// A token debit our wallet authorises from an account other than the
    /// deposit account, or (`TransferChecked`) in another mint — or no
    /// deposit account is configured; for a create-order, its deposit source
    /// or settlement mint (by role) is not the deposit account or mint.
    DebitSource,
    // --- appended (PMJ W5): the codes above stay stable -----------------
    /// (The dormant co-signer amendment, G-S5) a pinned co-signer's slot
    /// holds a signature that does not verify over the message we sign.
    CosignerSignature,
    /// Not exactly one instruction of the Predict program, or not the kind
    /// the build is for (a create-order for an order, a claim for a claim),
    /// or a discriminator the guard does not know.
    JupInstruction,
    /// The Predict instruction's data does not read whole: a string over
    /// [`CREATE_STR_MAX`], a bool, enum or Option tag not 0 or 1, a short
    /// field, bytes past the last one (a claim: any byte past its
    /// discriminator).
    OrderData,
    /// The create-order does not buy (`is_buy`), or not the outcome token
    /// itself (`is_yes`, the answer law's `isYes: true`).
    NotBuy,
    /// A Market order: only a Limit order binds its price on chain.
    NotLimit,
    /// The signed `max_fill_price_usd` is above the member's limit.
    PriceAboveLimit,
    /// The signed `deposit_amount` is not the deposit we asked.
    DepositMismatch,
    /// `contracts × max_fill_price_usd / 1e6` exceeds the signed deposit.
    ContractsOverDeposit,
    /// An integrator or an integrator fee.
    Integrator,
    /// The Predict instruction names another number of accounts than its
    /// kind's, or a role that must be our wallet (payer, owner; a claim's
    /// authority) or our deposit account (a claim's payout account) is not
    /// — or is loaded through a lookup table.
    OrderAccounts,
    /// The vault account is not the pinned one, or none is pinned.
    Vault,
    /// The signed `market_id` is not the answer's `order.marketIdHash` (or
    /// the answer carries none).
    MarketMismatch,
    /// The signed `contracts` are zero, or not the answer's `contractsMicro`.
    ContractsMismatch,
    /// An instruction the build's kind does not admit: an order holds only
    /// Compute Budget and its create-order; a claim adds at most one ATA
    /// `CreateIdempotent` of our deposit account.
    ExtraInstruction,
}

/// One instruction, as offsets into the transaction.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Ix {
    /// The invoked program's static key index.
    pub program: u8,
    /// Offset of the account index list.
    pub acc_off: u16,
    /// Accounts the instruction names.
    pub n_acc: u16,
    /// Offset of the instruction data.
    pub data_off: u16,
    /// Instruction data length.
    pub data_len: u16,
}

/// One address lookup table, as offsets.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Alt {
    /// Offset of the table's key.
    pub key_off: u16,
    /// Writable addresses loaded.
    pub n_writable: u16,
    /// Read-only addresses loaded.
    pub n_readonly: u16,
    /// Offset of the writable entry indices.
    pub w_off: u16,
    /// Offset of the read-only entry indices.
    pub r_off: u16,
}

/// A parsed transaction: offsets into the caller's buffer, nothing copied.
#[derive(Copy, Clone, Debug, Default)]
pub struct SolTx {
    /// Total length.
    pub len: u16,
    /// Signatures in the table.
    pub n_sigs: u8,
    /// Offset of signature 0.
    pub sigs_off: u16,
    /// Offset of the message (the signed bytes run from here to `len`).
    pub msg_off: u16,
    /// `None` for legacy, `Some(0)` for v0.
    pub version: Option<u8>,
    /// `[required signers, read-only signed, read-only unsigned]`.
    pub header: [u8; 3],
    /// Static keys.
    pub n_keys: u16,
    /// Offset of key 0.
    pub keys_off: u16,
    /// Offset of the recent blockhash.
    pub blockhash_off: u16,
    /// Instructions.
    pub n_ix: u8,
    /// The instructions (`n_ix` valid).
    pub ix: [Ix; IX_MAX],
    /// Lookup tables.
    pub n_alt: u8,
    /// The tables (`n_alt` valid).
    pub alt: [Alt; ALT_MAX],
    /// Addresses the tables load (writable + read-only), indexable after
    /// the static keys.
    pub n_loaded: u16,
}

/// What the guard allows.
#[derive(Copy, Clone, Debug)]
pub struct TxGuard {
    /// Our wallet's public key.
    pub wallet: [u8; KEY_LEN],
    /// Program ids the transaction may invoke (`n_programs` valid).
    pub programs: [[u8; KEY_LEN]; GUARD_PROGRAMS_MAX],
    /// Valid prefix of `programs`.
    pub n_programs: u8,
    /// Lookup tables the transaction may name (`n_alts` valid).
    pub alts: [[u8; KEY_LEN]; GUARD_ALTS_MAX],
    /// Valid prefix of `alts`.
    pub n_alts: u8,
    /// The most lamports our wallet may send (transfers + rent funding).
    pub max_sol_lamports: u64,
    /// The highest compute unit price, micro-lamports per unit.
    pub max_cu_price: u64,
    /// The wallet's USDC token account: the only account a token debit
    /// may come from (zero: none configured — every debit is refused).
    pub deposit_account: [u8; KEY_LEN],
    /// The deposit mint (USDC): a `TransferChecked` must name it.
    pub deposit_mint: [u8; KEY_LEN],
    /// (The dormant amendment, R-PMJ-S44) the one pinned co-signer: a
    /// READ-ONLY second required signer the guard admits when
    /// `has_cosigner` (module docs).
    pub cosigner: [u8; KEY_LEN],
    /// Whether `cosigner` is pinned (`pmj.toml` `guard_cosigners`; empty by
    /// default: LAW PMJ-G's one signature).
    pub has_cosigner: bool,
    /// The program whose one instruction [`guard_order`] / [`guard_claim`]
    /// read ([`PREDICT_PROGRAM`]; the rig's fixtures name their own).
    pub predict_program: [u8; KEY_LEN],
    /// The pinned vault (`pmj.toml` `guard_vault`; zero: none — every order
    /// and claim is refused).
    pub vault: [u8; KEY_LEN],
    /// The lookup table whose entry `vault_alt_entry` is the vault
    /// (`guard_vault_alt`; zero: the vault must be a static key).
    pub vault_alt: [u8; KEY_LEN],
    /// The vault's entry in `vault_alt`.
    pub vault_alt_entry: u8,
}

impl TxGuard {
    /// A guard for `wallet` with EMPTY allowlists: it refuses every
    /// transaction (the default until J0's decoded programs and tables are
    /// in the `pmj.toml` `guard_*` keys).
    #[must_use]
    pub const fn refuse_all(wallet: [u8; KEY_LEN]) -> Self {
        Self {
            wallet,
            programs: [[0; KEY_LEN]; GUARD_PROGRAMS_MAX],
            n_programs: 0,
            alts: [[0; KEY_LEN]; GUARD_ALTS_MAX],
            n_alts: 0,
            max_sol_lamports: 0,
            max_cu_price: 0,
            deposit_account: [0; KEY_LEN],
            deposit_mint: [0; KEY_LEN],
            cosigner: [0; KEY_LEN],
            has_cosigner: false,
            predict_program: PREDICT_PROGRAM,
            vault: [0; KEY_LEN],
            vault_alt: [0; KEY_LEN],
            vault_alt_entry: 0,
        }
    }

    /// Allow program `id` (boot). `false` when the list is full.
    pub fn allow_program(&mut self, id: [u8; KEY_LEN]) -> bool {
        let n = self.n_programs as usize;
        if n >= GUARD_PROGRAMS_MAX {
            return false;
        }
        self.programs[n] = id;
        self.n_programs += 1;
        true
    }

    /// Allow lookup table `key` (boot). `false` when the list is full.
    pub fn allow_alt(&mut self, key: [u8; KEY_LEN]) -> bool {
        let n = self.n_alts as usize;
        if n >= GUARD_ALTS_MAX {
            return false;
        }
        self.alts[n] = key;
        self.n_alts += 1;
        true
    }

    fn program_allowed(&self, id: &[u8]) -> bool {
        let mut i = 0usize;
        while i < self.n_programs as usize && i < GUARD_PROGRAMS_MAX {
            if self.programs[i].as_slice() == id {
                return true;
            }
            i += 1;
        }
        false
    }

    fn alt_allowed(&self, key: &[u8]) -> bool {
        let mut i = 0usize;
        while i < self.n_alts as usize && i < GUARD_ALTS_MAX {
            if self.alts[i].as_slice() == key {
                return true;
            }
            i += 1;
        }
        false
    }
}

/// What a passed guard found.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Passed {
    /// Our signature slot.
    pub signer_idx: u8,
    /// Token units our wallet authorises out (≤ the deposit).
    pub token_debit: u64,
    /// Lamports our wallet sends (≤ the cap).
    pub lamports: u64,
    /// The compute unit price asked, micro-lamports.
    pub cu_price: u64,
}

/// A Solana compact-u16 at `buf[off..]`: `(value, next offset)`.
#[inline]
fn cu16(buf: &[u8], off: usize) -> Result<(usize, usize), TxErr> {
    let mut v = 0usize;
    let mut i = 0usize;
    while i < 3 {
        let b = *buf.get(off + i).ok_or(TxErr::Truncated)?;
        v |= usize::from(b & 0x7F) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((v, off + i + 1));
        }
        i += 1;
    }
    Err(TxErr::Truncated)
}

#[inline]
fn need(buf: &[u8], end: usize) -> Result<(), TxErr> {
    if end > buf.len() {
        return Err(TxErr::Truncated);
    }
    Ok(())
}

#[inline]
fn u64_le(b: &[u8], off: usize) -> Option<u64> {
    let s = b.get(off..off + 8)?;
    let mut w = [0u8; 8];
    // COPY: 8 B of instruction data into a word for the amount (per
    // instruction checked, cold).
    w.copy_from_slice(s);
    Some(u64::from_le_bytes(w))
}

#[inline]
fn u32_le(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    let mut w = [0u8; 4];
    // COPY: 4 B of instruction data into a word for the discriminant.
    w.copy_from_slice(s);
    Some(u32::from_le_bytes(w))
}

impl SolTx {
    /// Parse `buf` (the decoded transaction). Copies nothing.
    ///
    /// # Errors
    /// [`TxErr`] — the first thing that does not read.
    pub fn parse(buf: &[u8]) -> Result<Self, TxErr> {
        if buf.is_empty() || buf.len() > TX_MAX {
            return Err(TxErr::Length);
        }
        let mut t = Self {
            len: buf.len() as u16,
            ..Self::default()
        };
        let (n_sigs, off) = cu16(buf, 0)?;
        if n_sigs == 0 || n_sigs > SIGNERS_MAX {
            return Err(TxErr::Header);
        }
        t.n_sigs = n_sigs as u8;
        t.sigs_off = off as u16;
        let mut o = off + n_sigs * SIG_LEN;
        need(buf, o + 1)?;
        t.msg_off = o as u16;
        if buf[o] & 0x80 != 0 {
            let v = buf[o] & 0x7F;
            if v != 0 {
                return Err(TxErr::Version);
            }
            t.version = Some(v);
            o += 1;
        }
        need(buf, o + 3)?;
        t.header = [buf[o], buf[o + 1], buf[o + 2]];
        o += 3;
        let (n_keys, ko) = cu16(buf, o)?;
        o = ko;
        need(buf, o + n_keys * KEY_LEN)?;
        t.n_keys = n_keys as u16;
        t.keys_off = o as u16;
        o += n_keys * KEY_LEN;
        let required = t.header[0] as usize;
        if required != n_sigs
            || required == 0
            || required > n_keys
            || t.header[1] as usize >= required
            || t.header[2] as usize > n_keys - required
        {
            return Err(TxErr::Header);
        }
        need(buf, o + KEY_LEN)?;
        t.blockhash_off = o as u16;
        o += KEY_LEN;
        let (n_ix, io) = cu16(buf, o)?;
        o = io;
        if n_ix > IX_MAX {
            return Err(TxErr::TooMany);
        }
        t.n_ix = n_ix as u8;
        let mut k = 0usize;
        while k < n_ix {
            need(buf, o + 1)?;
            let program = buf[o];
            o += 1;
            let (n_acc, ao) = cu16(buf, o)?;
            need(buf, ao + n_acc)?;
            let (dlen, d_off) = cu16(buf, ao + n_acc)?;
            need(buf, d_off + dlen)?;
            t.ix[k] = Ix {
                program,
                acc_off: ao as u16,
                n_acc: n_acc as u16,
                data_off: d_off as u16,
                data_len: dlen as u16,
            };
            o = d_off + dlen;
            k += 1;
        }
        if t.version.is_some() {
            let (n_alt, lo) = cu16(buf, o)?;
            o = lo;
            if n_alt > ALT_MAX {
                return Err(TxErr::TooMany);
            }
            t.n_alt = n_alt as u8;
            let mut a = 0usize;
            while a < n_alt {
                need(buf, o + KEY_LEN)?;
                let key_off = o;
                o += KEY_LEN;
                let (nw, wo) = cu16(buf, o)?;
                need(buf, wo + nw)?;
                let (nr, ro) = cu16(buf, wo + nw)?;
                need(buf, ro + nr)?;
                o = ro + nr;
                t.alt[a] = Alt {
                    key_off: key_off as u16,
                    n_writable: nw as u16,
                    n_readonly: nr as u16,
                    w_off: wo as u16,
                    r_off: ro as u16,
                };
                t.n_loaded += (nw + nr) as u16;
                a += 1;
            }
        }
        if o != buf.len() {
            return Err(TxErr::Trailing);
        }
        // Every index names a key the message can resolve; every invoked
        // program a STATIC one.
        let names = n_keys + t.n_loaded as usize;
        let mut k = 0usize;
        while k < n_ix {
            let ix = t.ix[k];
            if ix.program as usize >= n_keys {
                return Err(if (ix.program as usize) < names {
                    TxErr::ProgramFromTable
                } else {
                    TxErr::Index
                });
            }
            let mut j = 0usize;
            while j < ix.n_acc as usize {
                if buf[ix.acc_off as usize + j] as usize >= names {
                    return Err(TxErr::Index);
                }
                j += 1;
            }
            k += 1;
        }
        Ok(t)
    }

    /// Static key `i` (`i < n_keys`).
    #[inline]
    #[must_use]
    pub fn key<'a>(&self, buf: &'a [u8], i: usize) -> &'a [u8] {
        let o = self.keys_off as usize + i * KEY_LEN;
        &buf[o..o + KEY_LEN]
    }

    /// The account index `j` of instruction `k`, resolved to a static key
    /// when it is one (`None` for an address a lookup table loads).
    #[inline]
    fn acc_key<'a>(&self, buf: &'a [u8], ix: &Ix, j: usize) -> Option<&'a [u8]> {
        let idx = *buf.get(ix.acc_off as usize + j)? as usize;
        if idx < self.n_keys as usize {
            Some(self.key(buf, idx))
        } else {
            None
        }
    }

    /// The lookup-table entry an account index past the static keys loads:
    /// `(table, entry)`, `table` an index into [`SolTx::alt`], in the
    /// runtime's order (every table's writable entries, then every table's
    /// read-only ones); `None` for a static index or one past the loads.
    #[must_use]
    pub fn loaded_entry(&self, buf: &[u8], idx: usize) -> Option<(usize, u8)> {
        let mut j = idx.checked_sub(self.n_keys as usize)?;
        let mut pass = 0u8;
        while pass < 2 {
            let mut a = 0usize;
            while a < self.n_alt as usize && a < ALT_MAX {
                let t = self.alt[a];
                let (n, off) = if pass == 0 {
                    (t.n_writable as usize, t.w_off as usize)
                } else {
                    (t.n_readonly as usize, t.r_off as usize)
                };
                if j < n {
                    return buf.get(off + j).map(|&e| (a, e));
                }
                j -= n;
                a += 1;
            }
            pass += 1;
        }
        None
    }

    /// The signed bytes: the message.
    #[inline]
    #[must_use]
    pub fn message<'a>(&self, buf: &'a [u8]) -> &'a [u8] {
        &buf[self.msg_off as usize..self.len as usize]
    }

    /// Sign the message with `signer` into signature slot `idx`, in place.
    ///
    /// # Errors
    /// [`TxErr::NotSigner`] for a slot past the table.
    pub fn sign_into(&self, buf: &mut [u8], idx: u8, signer: &Ed25519Signer) -> Result<(), TxErr> {
        if idx >= self.n_sigs {
            return Err(TxErr::NotSigner);
        }
        let o = self.sigs_off as usize + idx as usize * SIG_LEN;
        let m = self.msg_off as usize;
        let end = self.len as usize;
        if o + SIG_LEN > m || end > buf.len() {
            return Err(TxErr::Truncated);
        }
        // The signature slots precede the message: one split lends the
        // slot (mutably) and the message (shared) at once, and the signer
        // writes the signature straight into its slot — no copy.
        let (head, rest) = buf.split_at_mut(m);
        let Ok(slot) = <&mut [u8; SIG_LEN]>::try_from(&mut head[o..o + SIG_LEN]) else {
            return Err(TxErr::Truncated);
        };
        signer.sign_raw(&rest[..end - m], slot);
        Ok(())
    }
}

/// The guard (module docs): `Ok` only when every line holds; `deposit` is
/// the token amount the order asked to deposit.
///
/// # Errors
/// [`TxErr`] — the first line that does not hold.
pub fn guard(buf: &[u8], tx: &SolTx, g: &TxGuard, deposit: u64) -> Result<Passed, TxErr> {
    let mut p = Passed::default();
    if g.n_programs == 0 {
        return Err(TxErr::Program);
    }
    if tx.key(buf, 0) != g.wallet.as_slice() {
        return Err(TxErr::FeePayer);
    }
    // Our wallet is the ONLY signer: the server can neither co-sign nor
    // hold the transaction behind another key — unless one co-signer is
    // pinned (the dormant amendment, R-PMJ-S44: G-S1..G-S5).
    match tx.header[0] {
        1 => {}
        // Key 0 is our wallet (the FeePayer line above), so the co-signer
        // is key 1; it is a read-only signer iff header[1] >= 1
        // (SolTx::parse keeps header[1] < header[0], so key 0 stays
        // writable).
        2 if g.has_cosigner => {
            if tx.key(buf, 1) != g.cosigner.as_slice() || tx.header[1] < 1 {
                return Err(TxErr::Signers);
            }
            let o = tx.sigs_off as usize + SIG_LEN;
            let Some(Ok(sig)) = buf.get(o..o + SIG_LEN).map(<&[u8; SIG_LEN]>::try_from) else {
                return Err(TxErr::Truncated);
            };
            // Empty: Jupiter signs at /execute. Present: it must be the
            // pinned key's signature of the very message we sign.
            if sig != &[0u8; SIG_LEN]
                && !signer_ed25519::verify(&g.cosigner, tx.message(buf), sig)
            {
                return Err(TxErr::CosignerSignature);
            }
        }
        _ => return Err(TxErr::Signers),
    }
    // Our signer slot: the wallet among the required signers.
    let mut found = false;
    let mut s = 0usize;
    while s < tx.header[0] as usize {
        if tx.key(buf, s) == g.wallet.as_slice() {
            p.signer_idx = s as u8;
            found = true;
            break;
        }
        s += 1;
    }
    if !found {
        return Err(TxErr::NotSigner);
    }
    let mut a = 0usize;
    while a < tx.n_alt as usize {
        let o = tx.alt[a].key_off as usize;
        if !g.alt_allowed(&buf[o..o + KEY_LEN]) {
            return Err(TxErr::LookupTable);
        }
        a += 1;
    }
    let mut k = 0usize;
    while k < tx.n_ix as usize {
        let ix = tx.ix[k];
        k += 1;
        let prog = tx.key(buf, ix.program as usize);
        if !g.program_allowed(prog) {
            return Err(TxErr::Program);
        }
        let data = &buf[ix.data_off as usize..ix.data_off as usize + ix.data_len as usize];
        // Where our wallet appears among the instruction's accounts.
        let mut ours = [false; 8];
        let mut any = false;
        let mut j = 0usize;
        while j < ix.n_acc as usize {
            if tx.acc_key(buf, &ix, j) == Some(g.wallet.as_slice()) {
                any = true;
                if j < ours.len() {
                    ours[j] = true;
                } else {
                    // Named past the slots any checked instruction reads.
                    ours[ours.len() - 1] = true;
                }
            }
            j += 1;
        }
        if prog == TOKEN_PROGRAM.as_slice() || prog == TOKEN_2022_PROGRAM.as_slice() {
            if !any {
                continue;
            }
            // Transfer (3): [source, destination, authority];
            // TransferChecked (12): [source, mint, destination, authority].
            let (auth, amount) = match data.first() {
                Some(3) if data.len() == 9 => (2usize, u64_le(data, 1)),
                Some(12) if data.len() == 10 => (3usize, u64_le(data, 1)),
                _ => return Err(TxErr::TokenAuthority),
            };
            let Some(amount) = amount else {
                return Err(TxErr::TokenAuthority);
            };
            let mut only_auth = ours[auth];
            let mut j = 0usize;
            while j < ours.len() {
                if j != auth && ours[j] {
                    only_auth = false;
                }
                j += 1;
            }
            if !only_auth || (ix.n_acc as usize) <= auth {
                return Err(TxErr::TokenAuthority);
            }
            // The debit's source is the deposit account, and a checked
            // transfer's mint the deposit mint: no other token our wallet
            // holds can leave under the deposit's bound.
            if g.deposit_account == [0; KEY_LEN]
                || tx.acc_key(buf, &ix, 0) != Some(g.deposit_account.as_slice())
                || (data[0] == 12 && tx.acc_key(buf, &ix, 1) != Some(g.deposit_mint.as_slice()))
            {
                return Err(TxErr::DebitSource);
            }
            p.token_debit = p.token_debit.saturating_add(amount);
            if p.token_debit > deposit {
                return Err(TxErr::Debit);
            }
        } else if prog == SYSTEM_PROGRAM.as_slice() {
            // AdvanceNonceAccount (4), whatever accounts it names: a
            // durable-nonce transaction never expires.
            if u32_le(data, 0) == Some(4) {
                return Err(TxErr::Nonce);
            }
            if !any {
                continue;
            }
            // CreateAccount (0) and Transfer (2): account 0 is the funder.
            let lamports = match u32_le(data, 0) {
                Some(0) if data.len() >= 12 => u64_le(data, 4),
                Some(2) if data.len() == 12 => u64_le(data, 4),
                _ => return Err(TxErr::SystemUse),
            };
            let Some(lamports) = lamports else {
                return Err(TxErr::SystemUse);
            };
            let mut j = 1usize;
            while j < ours.len() {
                if ours[j] {
                    // Our wallet as the NEW account or the recipient: not a
                    // shape an order needs.
                    return Err(TxErr::SystemUse);
                }
                j += 1;
            }
            if !ours[0] {
                return Err(TxErr::SystemUse);
            }
            p.lamports = p.lamports.saturating_add(lamports);
            if p.lamports > g.max_sol_lamports {
                return Err(TxErr::Lamports);
            }
        } else if prog == COMPUTE_BUDGET_PROGRAM.as_slice() {
            // SetComputeUnitPrice (3, u64 micro-lamports); the unit limit
            // (2), the heap frame (1) and the loaded-data limit (4) cost
            // nothing past the price.
            match data.first() {
                Some(3) => {
                    let Some(price) = u64_le(data, 1) else {
                        return Err(TxErr::ComputePrice);
                    };
                    if data.len() != 9 || price > g.max_cu_price {
                        return Err(TxErr::ComputePrice);
                    }
                    p.cu_price = price;
                }
                Some(1 | 2 | 4) => {}
                _ => return Err(TxErr::ComputePrice),
            }
        }
        // Any other allowed program: trusted with what the message hands it
        // (module docs, "What the guard cannot see").
    }
    Ok(p)
}

/// A create-order instruction's data (`CreateOrderParams`, IDL v0.10.0),
/// read in place by [`CreateOrderView::parse`]: the numbers, and the two
/// strings as offsets into the data.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CreateOrderView {
    /// 1 = `create_order`, 2 = `create_order2`.
    pub version: u8,
    /// `external_order_id`: `(offset, length)` in the data.
    pub external_id: (u16, u16),
    /// `market_id` (a hex hash on public orders): `(offset, length)`.
    pub market_id: (u16, u16),
    /// `is_yes`.
    pub is_yes: bool,
    /// `is_buy`.
    pub is_buy: bool,
    /// `contracts`, micro contracts.
    pub contracts: u64,
    /// `max_fill_price_usd`, micro USD.
    pub max_fill_price_usd: u64,
    /// `deposit_amount`, deposit-mint base units.
    pub deposit_amount: u64,
    /// `order_type`: [`ORDER_TYPE_MARKET`] or [`ORDER_TYPE_LIMIT`].
    pub order_type: u8,
    /// `integrator` is `Some`.
    pub has_integrator: bool,
    /// The integrator key's offset in the data (0 when none).
    pub integrator_off: u16,
    /// `integrator_fee_bps`.
    pub integrator_fee_bps: Option<u16>,
}

/// A Borsh string at `*o`: `(offset, length)`, at most [`CREATE_STR_MAX`].
#[inline]
fn borsh_str(d: &[u8], o: &mut usize) -> Result<(u16, u16), TxErr> {
    let n = u32_le(d, *o).ok_or(TxErr::OrderData)? as usize;
    let start = *o + 4;
    if n > CREATE_STR_MAX || start + n > d.len() {
        return Err(TxErr::OrderData);
    }
    *o = start + n;
    Ok((start as u16, n as u16))
}

/// A Borsh bool (or a one-byte tag) at `*o`: 0 or 1, else refused.
#[inline]
fn borsh_flag(d: &[u8], o: &mut usize) -> Result<bool, TxErr> {
    let v = match d.get(*o) {
        Some(0) => false,
        Some(1) => true,
        _ => return Err(TxErr::OrderData),
    };
    *o += 1;
    Ok(v)
}

/// A Borsh u64 at `*o`.
#[inline]
fn borsh_u64(d: &[u8], o: &mut usize) -> Result<u64, TxErr> {
    let v = u64_le(d, *o).ok_or(TxErr::OrderData)?;
    *o += 8;
    Ok(v)
}

impl CreateOrderView {
    /// Read `data` — the instruction's data, its discriminator first.
    /// Copies nothing; every read is bounds-checked.
    ///
    /// # Errors
    /// [`TxErr::JupInstruction`] for a discriminator that is not a
    /// create-order's; [`TxErr::OrderData`] for params that do not read
    /// whole (module docs).
    pub fn parse(data: &[u8]) -> Result<Self, TxErr> {
        let disc = data.get(..8).ok_or(TxErr::JupInstruction)?;
        let version = if disc == DISC_CREATE_ORDER.as_slice() {
            1
        } else if disc == DISC_CREATE_ORDER2.as_slice() {
            2
        } else {
            return Err(TxErr::JupInstruction);
        };
        let mut o = 8usize;
        let mut v = Self {
            version,
            ..Self::default()
        };
        v.external_id = borsh_str(data, &mut o)?;
        v.market_id = borsh_str(data, &mut o)?;
        v.is_yes = borsh_flag(data, &mut o)?;
        v.is_buy = borsh_flag(data, &mut o)?;
        v.contracts = borsh_u64(data, &mut o)?;
        v.max_fill_price_usd = borsh_u64(data, &mut o)?;
        v.deposit_amount = borsh_u64(data, &mut o)?;
        // `OrderType` is a one-byte enum: Market 0, Limit 1.
        v.order_type = u8::from(borsh_flag(data, &mut o)?);
        if borsh_flag(data, &mut o)? {
            if o + KEY_LEN > data.len() {
                return Err(TxErr::OrderData);
            }
            v.has_integrator = true;
            v.integrator_off = o as u16;
            o += KEY_LEN;
        }
        if borsh_flag(data, &mut o)? {
            let Some(b) = data.get(o..o + 2) else {
                return Err(TxErr::OrderData);
            };
            v.integrator_fee_bps = Some(u16::from_le_bytes([b[0], b[1]]));
            o += 2;
        }
        if o != data.len() {
            return Err(TxErr::OrderData);
        }
        Ok(v)
    }

    /// This version's account roles.
    #[inline]
    #[must_use]
    pub const fn roles(&self) -> CreateRoles {
        if self.version == 1 {
            ROLES_V1
        } else {
            ROLES_V2
        }
    }
}

/// The order we asked for, and the build answer's own words about it:
/// what the signed create-order must say.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct OrderAsk<'a> {
    /// The deposit asked (`depositAmount`), deposit-mint base units: the
    /// signed `deposit_amount` must equal it.
    pub deposit: u64,
    /// The member's limit (the arm's `p − e`), micro USD: the signed
    /// `max_fill_price_usd` must not exceed it.
    pub limit_micro: u64,
    /// The answer's `order.orderPubkey`: the signed order account (role 5).
    pub order: [u8; KEY_LEN],
    /// The answer's `order.positionPubkey`: the signed position (role 4).
    pub position: [u8; KEY_LEN],
    /// The answer's `order.marketIdHash`: the signed `market_id` string.
    pub market_hash: &'a [u8],
    /// The answer's `contractsMicro`: the signed `contracts`.
    pub contracts: u64,
}

/// The ONE instruction of the Predict program and its data.
fn predict_ix<'a>(buf: &'a [u8], tx: &SolTx, g: &TxGuard) -> Result<(Ix, &'a [u8]), TxErr> {
    let mut found: Option<Ix> = None;
    let mut k = 0usize;
    while k < tx.n_ix as usize && k < IX_MAX {
        let ix = tx.ix[k];
        k += 1;
        if tx.key(buf, ix.program as usize) == g.predict_program.as_slice() {
            if found.is_some() {
                return Err(TxErr::JupInstruction);
            }
            found = Some(ix);
        }
    }
    let Some(ix) = found else {
        return Err(TxErr::JupInstruction);
    };
    let d0 = ix.data_off as usize;
    let Some(d) = buf.get(d0..d0 + ix.data_len as usize) else {
        return Err(TxErr::Truncated);
    };
    Ok((ix, d))
}

/// Account `j` of `ix` is the pinned vault: a static key equal to it, or
/// the pinned lookup-table entry (module docs).
fn vault_ok(buf: &[u8], tx: &SolTx, ix: &Ix, j: usize, g: &TxGuard) -> bool {
    if g.vault == [0; KEY_LEN] || j >= ix.n_acc as usize {
        return false;
    }
    let Some(&idx) = buf.get(ix.acc_off as usize + j) else {
        return false;
    };
    let idx = idx as usize;
    if idx < tx.n_keys as usize {
        return tx.key(buf, idx) == g.vault.as_slice();
    }
    if g.vault_alt == [0; KEY_LEN] {
        return false;
    }
    match tx.loaded_entry(buf, idx) {
        Some((a, e)) => {
            let o = tx.alt[a].key_off as usize;
            e == g.vault_alt_entry && buf.get(o..o + KEY_LEN) == Some(g.vault_alt.as_slice())
        }
        None => false,
    }
}

/// The guard of an ORDER build (module docs): [`guard`]'s lines with no
/// top-level token debit (the create-order's CPI moves the deposit), then
/// the create-order's own. jup-gw signs an order only through this.
///
/// # Errors
/// [`TxErr`] — the first line that does not hold.
pub fn guard_order(
    buf: &[u8],
    tx: &SolTx,
    g: &TxGuard,
    ask: &OrderAsk<'_>,
) -> Result<(Passed, CreateOrderView), TxErr> {
    let p = guard(buf, tx, g, 0)?;
    let (ix, data) = predict_ix(buf, tx, g)?;
    let v = CreateOrderView::parse(data)?;
    // Nothing beside the create-order but Compute Budget.
    let mut k = 0usize;
    while k < tx.n_ix as usize && k < IX_MAX {
        let prog = tx.key(buf, tx.ix[k].program as usize);
        k += 1;
        if prog != COMPUTE_BUDGET_PROGRAM.as_slice() && prog != g.predict_program.as_slice() {
            return Err(TxErr::ExtraInstruction);
        }
    }
    let r = v.roles();
    let wallet = Some(g.wallet.as_slice());
    if ix.n_acc != r.n_acc
        || tx.acc_key(buf, &ix, ROLE_PAYER) != wallet
        || tx.acc_key(buf, &ix, ROLE_OWNER) != wallet
        || ask.order == [0; KEY_LEN]
        || ask.position == [0; KEY_LEN]
        || tx.acc_key(buf, &ix, ROLE_ORDER) != Some(ask.order.as_slice())
        || tx.acc_key(buf, &ix, ROLE_POSITION) != Some(ask.position.as_slice())
    {
        return Err(TxErr::OrderAccounts);
    }
    if g.deposit_account == [0; KEY_LEN]
        || g.deposit_mint == [0; KEY_LEN]
        || tx.acc_key(buf, &ix, r.source as usize) != Some(g.deposit_account.as_slice())
        || tx.acc_key(buf, &ix, r.mint as usize) != Some(g.deposit_mint.as_slice())
    {
        return Err(TxErr::DebitSource);
    }
    if !vault_ok(buf, tx, &ix, ROLE_VAULT, g) {
        return Err(TxErr::Vault);
    }
    if !v.is_buy || !v.is_yes {
        return Err(TxErr::NotBuy);
    }
    if v.order_type != ORDER_TYPE_LIMIT {
        return Err(TxErr::NotLimit);
    }
    if v.max_fill_price_usd > ask.limit_micro {
        return Err(TxErr::PriceAboveLimit);
    }
    if v.deposit_amount != ask.deposit {
        return Err(TxErr::DepositMismatch);
    }
    let paid = u128::from(v.contracts) * u128::from(v.max_fill_price_usd);
    if paid > u128::from(v.deposit_amount) * 1_000_000 {
        return Err(TxErr::ContractsOverDeposit);
    }
    if v.has_integrator || v.integrator_fee_bps.is_some() {
        return Err(TxErr::Integrator);
    }
    let (mo, mn) = v.market_id;
    let signed_market = data.get(mo as usize..mo as usize + mn as usize).unwrap_or(&[]);
    if ask.market_hash.is_empty() || signed_market != ask.market_hash {
        return Err(TxErr::MarketMismatch);
    }
    if v.contracts == 0 || v.contracts != ask.contracts {
        return Err(TxErr::ContractsMismatch);
    }
    Ok((p, v))
}

/// The guard of a CLAIM build (module docs): [`guard`]'s lines with no
/// token leaving our account, then exactly one Predict instruction, a
/// `claim_payout` or `claim_payout2` whose authority is our wallet, whose
/// payout lands in our deposit account, and whose vault is the pinned one.
///
/// # Errors
/// [`TxErr`] — the first line that does not hold.
pub fn guard_claim(buf: &[u8], tx: &SolTx, g: &TxGuard) -> Result<Passed, TxErr> {
    let p = guard(buf, tx, g, 0)?;
    let (ix, data) = predict_ix(buf, tx, g)?;
    let Some(disc) = data.get(..8) else {
        return Err(TxErr::JupInstruction);
    };
    if disc != DISC_CLAIM_PAYOUT.as_slice() && disc != DISC_CLAIM_PAYOUT2.as_slice() {
        return Err(TxErr::JupInstruction);
    }
    if data.len() != 8 {
        return Err(TxErr::OrderData);
    }
    if ix.n_acc != CLAIM_ACCOUNTS
        || tx.acc_key(buf, &ix, CLAIM_AUTHORITY) != Some(g.wallet.as_slice())
        || g.deposit_account == [0; KEY_LEN]
        || tx.acc_key(buf, &ix, CLAIM_OWNER_TOKEN) != Some(g.deposit_account.as_slice())
    {
        return Err(TxErr::OrderAccounts);
    }
    if !vault_ok(buf, tx, &ix, CLAIM_VAULT, g) {
        return Err(TxErr::Vault);
    }
    // Beside the claim: Compute Budget, and at most one ATA
    // CreateIdempotent of our deposit account.
    let mut atas = 0u8;
    let mut k = 0usize;
    while k < tx.n_ix as usize && k < IX_MAX {
        let x = tx.ix[k];
        k += 1;
        let prog = tx.key(buf, x.program as usize);
        if prog == COMPUTE_BUDGET_PROGRAM.as_slice() || prog == g.predict_program.as_slice() {
            continue;
        }
        if prog != ATA_PROGRAM.as_slice() || !ata_of_deposit(buf, tx, &x, g) {
            return Err(TxErr::ExtraInstruction);
        }
        atas += 1;
        if atas > 1 {
            return Err(TxErr::ExtraInstruction);
        }
    }
    Ok(p)
}

/// `x` is the ATA program's `CreateIdempotent` (data `[1]`) of our deposit
/// account: `[payer, account, owner, mint, System, Token]`, the payer and
/// the owner our wallet, the account our deposit account, the mint the
/// deposit mint — every one a static key.
fn ata_of_deposit(buf: &[u8], tx: &SolTx, x: &Ix, g: &TxGuard) -> bool {
    let d0 = x.data_off as usize;
    let wallet = Some(g.wallet.as_slice());
    x.n_acc == 6
        && buf.get(d0..d0 + x.data_len as usize) == Some(&[1u8][..])
        && g.deposit_account != [0; KEY_LEN]
        && tx.acc_key(buf, x, 0) == wallet
        && tx.acc_key(buf, x, 1) == Some(g.deposit_account.as_slice())
        && tx.acc_key(buf, x, 2) == wallet
        && tx.acc_key(buf, x, 3) == Some(g.deposit_mint.as_slice())
        && tx.acc_key(buf, x, 4) == Some(SYSTEM_PROGRAM.as_slice())
        && tx.acc_key(buf, x, 5) == Some(TOKEN_PROGRAM.as_slice())
}

#[cfg(test)]
mod tests {
    // The parser against the J0 probe's reference parse, and the guard against
    // a clean order transaction and every hostile shape the session prompt
    // names — each a test of its own.

    use super::*;

    const WALLET: [u8; 32] = [0x11; 32];
    const OTHER: [u8; 32] = [0x22; 32];
    const USER_ATA: [u8; 32] = [0x33; 32];
    const VAULT_ATA: [u8; 32] = [0x44; 32];
    const MINT: [u8; 32] = [0x55; 32];
    const JUP: [u8; 32] = [0x66; 32];
    const ROGUE: [u8; 32] = [0x77; 32];
    const TABLE: [u8; 32] = [0x88; 32];
    const DEPOSIT: u64 = 5_000_000;

    fn cu16(v: usize, out: &mut Vec<u8>) {
        let mut v = v;
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        }
    }

    /// One instruction: the program's key index, the account indices, the data.
    struct I(u8, Vec<u8>, Vec<u8>);

    /// A transaction: `n_sigs` zero signatures, the keys, the header, the
    /// instructions, and (v0) the lookup tables `(key, writable, readonly)`.
    fn build(keys: &[[u8; 32]], header: [u8; 3], ixs: &[I], alts: Option<&[([u8; 32], Vec<u8>, Vec<u8>)]>) -> Vec<u8> {
        let mut m = Vec::new();
        if alts.is_some() {
            m.push(0x80);
        }
        m.extend_from_slice(&header);
        cu16(keys.len(), &mut m);
        for k in keys {
            m.extend_from_slice(k);
        }
        m.extend_from_slice(&[0xBB; 32]);
        cu16(ixs.len(), &mut m);
        for ix in ixs {
            m.push(ix.0);
            cu16(ix.1.len(), &mut m);
            m.extend_from_slice(&ix.1);
            cu16(ix.2.len(), &mut m);
            m.extend_from_slice(&ix.2);
        }
        if let Some(alts) = alts {
            cu16(alts.len(), &mut m);
            for (key, w, r) in alts {
                m.extend_from_slice(key);
                cu16(w.len(), &mut m);
                m.extend_from_slice(w);
                cu16(r.len(), &mut m);
                m.extend_from_slice(r);
            }
        }
        let mut t = Vec::new();
        cu16(header[0] as usize, &mut t);
        t.resize(t.len() + 64 * header[0] as usize, 0);
        t.extend_from_slice(&m);
        t
    }

    fn transfer_checked(amount: u64) -> Vec<u8> {
        let mut d = vec![12u8];
        d.extend_from_slice(&amount.to_le_bytes());
        d.push(6);
        d
    }

    /// Keys of the clean order: 0 wallet (fee payer, signer), 1 our USDC
    /// account, 2 the vault's, 3 the mint, 4 Jupiter's program, 5 Token,
    /// 6 Compute Budget, 7 System.
    fn keys() -> Vec<[u8; 32]> {
        vec![WALLET, USER_ATA, VAULT_ATA, MINT, JUP, TOKEN_PROGRAM, COMPUTE_BUDGET_PROGRAM, SYSTEM_PROGRAM]
    }

    const HEADER: [u8; 3] = [1, 0, 5];

    fn clean_ixs() -> Vec<I> {
        let mut limit = vec![2u8];
        limit.extend_from_slice(&200_000u32.to_le_bytes());
        let mut price = vec![3u8];
        price.extend_from_slice(&1_000u64.to_le_bytes());
        vec![
            I(6, vec![], limit),
            I(6, vec![], price),
            I(4, vec![0, 1, 2, 3], vec![0xAA; 24]),
            I(5, vec![1, 3, 2, 0], transfer_checked(DEPOSIT)),
        ]
    }

    fn guard_ok() -> TxGuard {
        let mut g = TxGuard::refuse_all(WALLET);
        assert!(g.allow_program(JUP));
        assert!(g.allow_program(TOKEN_PROGRAM));
        assert!(g.allow_program(COMPUTE_BUDGET_PROGRAM));
        assert!(g.allow_program(SYSTEM_PROGRAM));
        assert!(g.allow_alt(TABLE));
        g.max_sol_lamports = 3_000_000;
        g.max_cu_price = 50_000;
        g.deposit_account = USER_ATA;
        g.deposit_mint = MINT;
        g
    }

    fn check(raw: &[u8], g: &TxGuard) -> Result<Passed, TxErr> {
        let t = SolTx::parse(raw)?;
        guard(raw, &t, g, DEPOSIT)
    }

    /// The J0 probe's `self_test` transaction, byte for byte: what its
    /// `decode_tx` reads, this parser reads.
    #[test]
    fn the_probe_reference_transaction_parses_as_decode_tx_reads_it() {
        let mut msg = vec![0x80u8, 1, 0, 1, 4];
        msg.extend_from_slice(&[1u8; 32]);
        msg.extend_from_slice(&[2u8; 32]);
        msg.extend_from_slice(&[3u8; 32]);
        msg.extend_from_slice(&TOKEN_PROGRAM);
        msg.extend_from_slice(&[0u8; 32]);
        let mut data = vec![12u8];
        data.extend_from_slice(&5_000_000u64.to_le_bytes());
        data.push(6);
        msg.extend_from_slice(&[1, 3, 2, 1, 2, data.len() as u8]);
        msg.extend_from_slice(&data);
        msg.push(1);
        msg.extend_from_slice(&[9u8; 32]);
        msg.extend_from_slice(&[1, 0, 1, 1]);
        let mut raw = vec![1u8];
        raw.extend_from_slice(&[0u8; 64]);
        raw.extend_from_slice(&msg);
        let t = SolTx::parse(&raw).expect("the probe's transaction");
        // decode_tx: version 0, n_signatures 1, message_offset 65, header
        // [1, 0, 1], programs == [Token], token_amount 5 000 000, one lookup
        // table (1 writable, 1 read-only), trailing_bytes 0.
        assert_eq!(t.version, Some(0));
        assert_eq!((t.n_sigs, t.msg_off), (1, 65));
        assert_eq!(t.header, [1, 0, 1]);
        assert_eq!((t.n_keys, t.n_ix, t.n_alt, t.n_loaded), (4, 1, 1, 2));
        assert_eq!(t.key(&raw, t.ix[0].program as usize), TOKEN_PROGRAM.as_slice());
        let d = t.ix[0];
        assert_eq!(raw[d.data_off as usize], 12);
        assert_eq!(u64_le(&raw[d.data_off as usize..], 1), Some(5_000_000));
        let a = t.alt[0];
        assert_eq!(&raw[a.key_off as usize..a.key_off as usize + 32], &[9u8; 32]);
        assert_eq!((a.n_writable, a.n_readonly), (1, 1));
        assert_eq!(t.len as usize, raw.len());
        assert_eq!(t.message(&raw).len(), raw.len() - 65);
    }

    #[test]
    fn a_clean_order_passes_and_signs_in_place() {
        let mut raw = build(&keys(), HEADER, &clean_ixs(), Some(&[(TABLE, vec![0], vec![1])]));
        let g = guard_ok();
        let t = SolTx::parse(&raw).expect("parses");
        let p = guard(&raw, &t, &g, DEPOSIT).expect("passes");
        assert_eq!(p, Passed { signer_idx: 0, token_debit: DEPOSIT, lamports: 0, cu_price: 1_000 });
        // Sign with RFC 8032 TEST 1's key as the wallet (the guard was built
        // on WALLET; re-key the fee payer to the signer's public key).
        let v = &signer_ed25519::RFC8032[0];
        let s = signer_ed25519::Ed25519Signer::from_seed(&v.seed).expect("signer");
        let ko = t.keys_off as usize;
        raw[ko..ko + 32].copy_from_slice(&v.public);
        let mut g2 = guard_ok();
        g2.wallet = v.public;
        let t = SolTx::parse(&raw).expect("parses");
        let p = guard(&raw, &t, &g2, DEPOSIT).expect("passes");
        t.sign_into(&mut raw, p.signer_idx, &s).expect("slot 0");
        let mut sig = [0u8; 64];
        sig.copy_from_slice(&raw[1..65]);
        assert!(signer_ed25519::verify(&v.public, t.message(&raw), &sig), "the slot verifies");
        assert_eq!(t.sign_into(&mut raw, 1, &s), Err(TxErr::NotSigner));
    }

    #[test]
    fn an_empty_allowlist_refuses_everything() {
        let raw = build(&keys(), HEADER, &clean_ixs(), None);
        assert_eq!(check(&raw, &TxGuard::refuse_all(WALLET)), Err(TxErr::Program));
    }

    #[test]
    fn hostile_an_extra_transfer_is_refused() {
        let mut ixs = clean_ixs();
        ixs.push(I(5, vec![1, 3, 2, 0], transfer_checked(1)));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Debit));
        // A plain Transfer is counted the same way.
        let mut ixs = clean_ixs();
        let mut d = vec![3u8];
        d.extend_from_slice(&1u64.to_le_bytes());
        ixs.push(I(5, vec![1, 2, 0], d));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Debit));
    }

    #[test]
    fn hostile_an_unknown_program_is_refused() {
        let mut k = keys();
        k.push(ROGUE);
        let mut ixs = clean_ixs();
        ixs.push(I(8, vec![0, 1], vec![1, 2, 3]));
        let raw = build(&k, [1, 0, 6], &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Program));
    }

    #[test]
    fn hostile_a_foreign_fee_payer_is_refused() {
        // OTHER pays and signs first; our wallet is the second signer.
        let mut k = keys();
        k.insert(0, OTHER);
        let mut ixs = clean_ixs();
        for ix in ixs.iter_mut() {
            ix.0 += 1;
            for a in ix.1.iter_mut() {
                *a += 1;
            }
        }
        let raw = build(&k, [2, 0, 5], &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::FeePayer));
    }

    #[test]
    fn hostile_an_unknown_lookup_table_is_refused() {
        let raw = build(&keys(), HEADER, &clean_ixs(), Some(&[(ROGUE, vec![0], vec![])]));
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::LookupTable));
    }

    #[test]
    fn hostile_a_program_loaded_from_a_lookup_table_is_refused() {
        let mut ixs = clean_ixs();
        // Index 8: the first address the table loads.
        ixs.push(I(8, vec![0], vec![1]));
        let raw = build(&keys(), HEADER, &ixs, Some(&[(TABLE, vec![0], vec![])]));
        assert_eq!(SolTx::parse(&raw).err(), Some(TxErr::ProgramFromTable));
    }

    #[test]
    fn hostile_an_approve_or_close_by_our_wallet_is_refused() {
        for disc in [4u8, 6, 9, 8, 13] {
            let mut ixs = clean_ixs();
            let mut d = vec![disc];
            d.extend_from_slice(&1u64.to_le_bytes());
            ixs.push(I(5, vec![1, 2, 0], d));
            let raw = build(&keys(), HEADER, &ixs, None);
            assert_eq!(check(&raw, &guard_ok()), Err(TxErr::TokenAuthority), "token instruction {disc}");
        }
        // Our wallet as a transfer's SOURCE account rather than its authority.
        let mut ixs = clean_ixs();
        ixs[3] = I(5, vec![0, 3, 2, 0], transfer_checked(DEPOSIT));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::TokenAuthority));
    }

    #[test]
    fn system_transfers_from_our_wallet_are_capped_and_other_uses_refused() {
        let lamports = |n: u64| {
            let mut d = 2u32.to_le_bytes().to_vec();
            d.extend_from_slice(&n.to_le_bytes());
            d
        };
        let mut ixs = clean_ixs();
        ixs.push(I(7, vec![0, 2], lamports(2_000_000)));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()).map(|p| p.lamports), Ok(2_000_000));
        ixs.push(I(7, vec![0, 2], lamports(1_000_001)));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Lamports));
        // Our wallet as the recipient, and an Assign of it.
        let mut ixs = clean_ixs();
        ixs.push(I(7, vec![2, 0], lamports(1)));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::SystemUse));
        let mut ixs = clean_ixs();
        let mut d = 1u32.to_le_bytes().to_vec();
        d.extend_from_slice(&[0u8; 32]);
        ixs.push(I(7, vec![0], d));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::SystemUse));
    }

    #[test]
    fn a_compute_price_over_the_cap_is_refused() {
        let mut ixs = clean_ixs();
        let mut price = vec![3u8];
        price.extend_from_slice(&50_001u64.to_le_bytes());
        ixs[1] = I(6, vec![], price);
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::ComputePrice));
    }

    #[test]
    fn malformed_wire_is_refused_never_a_panic() {
        let raw = build(&keys(), HEADER, &clean_ixs(), Some(&[(TABLE, vec![0], vec![1])]));
        let mut extra = raw.clone();
        extra.push(0);
        assert_eq!(SolTx::parse(&extra).err(), Some(TxErr::Trailing));
        let mut i = 0usize;
        while i < raw.len() {
            assert!(SolTx::parse(&raw[..i]).is_err(), "prefix {i}");
            i += 1;
        }
        assert_eq!(SolTx::parse(&[]).err(), Some(TxErr::Length));
        assert_eq!(SolTx::parse(&vec![1u8; TX_MAX + 1]).err(), Some(TxErr::Length));
        // Version 1, a signature count that is not the header's, an account
        // index past every name.
        let mut v1 = raw.clone();
        v1[65] = 0x81;
        assert_eq!(SolTx::parse(&v1).err(), Some(TxErr::Version));
        let two = build(&keys(), [2, 0, 5], &clean_ixs(), None);
        let mut bad = two.clone();
        bad[0] = 1;
        assert!(SolTx::parse(&bad).is_err());
        let mut ixs = clean_ixs();
        ixs.push(I(4, vec![40], vec![]));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(SolTx::parse(&raw).err(), Some(TxErr::Index));
    }

    #[test]
    fn hostile_a_second_required_signer_is_refused() {
        // Our wallet pays and signs slot 0; a server key is a second
        // required signer (a co-signer, a durable nonce's authority).
        let mut k = keys();
        k.insert(1, OTHER);
        let mut ixs = clean_ixs();
        for ix in ixs.iter_mut() {
            ix.0 += 1;
            for a in ix.1.iter_mut() {
                if *a >= 1 {
                    *a += 1;
                }
            }
        }
        let raw = build(&k, [2, 0, 5], &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Signers));
    }

    #[test]
    fn hostile_a_durable_nonce_advance_is_refused_whatever_it_names() {
        // AdvanceNonceAccount [nonce, recent blockhashes sysvar, authority]:
        // none of them our wallet.
        let mut ixs = clean_ixs();
        ixs.insert(0, I(7, vec![2, 3, 4], 4u32.to_le_bytes().to_vec()));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Nonce));
    }

    #[test]
    fn hostile_a_debit_from_another_account_or_mint_is_refused() {
        // TransferChecked from the vault's account (not ours) with our
        // wallet as the authority.
        let mut ixs = clean_ixs();
        ixs[3] = I(5, vec![2, 3, 1, 0], transfer_checked(DEPOSIT));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::DebitSource));
        // From our account, but naming another mint (key 4).
        let mut ixs = clean_ixs();
        ixs[3] = I(5, vec![1, 4, 2, 0], transfer_checked(DEPOSIT));
        let raw = build(&keys(), HEADER, &ixs, None);
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::DebitSource));
        // No deposit account configured: every debit is refused.
        let raw = build(&keys(), HEADER, &clean_ixs(), None);
        let mut g = guard_ok();
        g.deposit_account = [0; 32];
        assert_eq!(check(&raw, &g), Err(TxErr::DebitSource));
        assert!(check(&raw, &guard_ok()).is_ok());
    }

    #[test]
    fn our_wallet_must_be_a_required_signer() {
        // The fee payer check comes first; a wallet that pays but whose key
        // also names a later required slot still signs slot 0.
        let raw = build(&keys(), HEADER, &clean_ixs(), None);
        let mut g = guard_ok();
        g.wallet = OTHER;
        assert_eq!(check(&raw, &g), Err(TxErr::FeePayer));
    }

    // --- PMJ W5: the constants, the table loads ------------------------

    fn key58(b58: &str) -> [u8; 32] {
        let mut k = [0u8; 32];
        assert!(matches!(core_crypto::base58_decode(b58.as_bytes(), &mut k), Ok(32)), "{b58}");
        k
    }

    #[test]
    fn the_program_ids_and_discriminators_are_the_chains() {
        assert_eq!(PREDICT_PROGRAM, key58("3ZZuTbwC6aJbvteyVxXUS7gtFYdf7AuXeitx6VyvjvUp"));
        assert_eq!(ATA_PROGRAM, key58("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"));
        assert_eq!(TOKEN_PROGRAM, key58("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"));
        for (disc, name) in [
            (DISC_CREATE_ORDER, "global:create_order"),
            (DISC_CREATE_ORDER2, "global:create_order2"),
            (DISC_CLAIM_PAYOUT, "global:claim_payout"),
            (DISC_CLAIM_PAYOUT2, "global:claim_payout2"),
        ] {
            assert_eq!(disc.as_slice(), &core_crypto::sha256(name.as_bytes())[..8], "{name}");
        }
        assert_eq!(TxGuard::refuse_all(WALLET).predict_program, PREDICT_PROGRAM);
        assert!(!TxGuard::refuse_all(WALLET).has_cosigner, "LAW PMJ-G by default");
    }

    #[test]
    fn a_loaded_index_resolves_to_its_table_entry_writable_first() {
        // Two tables: A loads writable [7, 9] and read-only [3]; B loads
        // writable [1] and read-only [4, 5]. The runtime orders every
        // writable entry first: A.w, B.w, then A.r, B.r.
        let raw = build(
            &keys(),
            HEADER,
            &clean_ixs(),
            Some(&[(TABLE, vec![7, 9], vec![3]), (ROGUE, vec![1], vec![4, 5])]),
        );
        let t = SolTx::parse(&raw).expect("parses");
        let n = t.n_keys as usize;
        let want = [(0usize, 7u8), (0, 9), (1, 1), (0, 3), (1, 4), (1, 5)];
        let mut i = 0usize;
        while i < want.len() {
            assert_eq!(t.loaded_entry(&raw, n + i), Some(want[i]), "loaded {i}");
            i += 1;
        }
        assert_eq!(t.loaded_entry(&raw, n + want.len()), None, "past the loads");
        assert_eq!(t.loaded_entry(&raw, n - 1), None, "a static key");
    }

    // --- PMJ W5: the create-order data ----------------------------------

    /// `CreateOrderParams` as Borsh, after `disc`.
    #[allow(clippy::too_many_arguments)]
    fn params(
        disc: [u8; 8],
        ext: &[u8],
        market: &[u8],
        flags: (u8, u8),
        contracts: u64,
        cap: u64,
        deposit: u64,
        order_type: u8,
        integrator: Option<[u8; 32]>,
        fee_bps: Option<u16>,
    ) -> Vec<u8> {
        let mut d = disc.to_vec();
        d.extend_from_slice(&(ext.len() as u32).to_le_bytes());
        d.extend_from_slice(ext);
        d.extend_from_slice(&(market.len() as u32).to_le_bytes());
        d.extend_from_slice(market);
        d.extend_from_slice(&[flags.0, flags.1]);
        d.extend_from_slice(&contracts.to_le_bytes());
        d.extend_from_slice(&cap.to_le_bytes());
        d.extend_from_slice(&deposit.to_le_bytes());
        d.push(order_type);
        match integrator {
            Some(k) => {
                d.push(1);
                d.extend_from_slice(&k);
            }
            None => d.push(0),
        }
        match fee_bps {
            Some(b) => {
                d.push(1);
                d.extend_from_slice(&b.to_le_bytes());
            }
            None => d.push(0),
        }
        d
    }

    const HASH: &[u8] = b"ad44626a542bdd650466f1c527f02905";

    fn limit_params(contracts: u64, cap: u64, deposit: u64) -> Vec<u8> {
        params(DISC_CREATE_ORDER2, HASH, HASH, (1, 1), contracts, cap, deposit, ORDER_TYPE_LIMIT, None, None)
    }

    #[test]
    fn create_order_params_read_in_place_for_both_discriminators() {
        let d = limit_params(187_170_000, 30_000, 6_000_000);
        let v = CreateOrderView::parse(&d).expect("reads");
        assert_eq!(v.version, 2);
        assert_eq!(v.roles(), ROLES_V2);
        assert_eq!((v.is_yes, v.is_buy, v.order_type), (true, true, ORDER_TYPE_LIMIT));
        assert_eq!((v.contracts, v.max_fill_price_usd, v.deposit_amount), (187_170_000, 30_000, 6_000_000));
        assert_eq!((v.has_integrator, v.integrator_fee_bps), (false, None));
        let (o, n) = v.market_id;
        assert_eq!(&d[o as usize..(o + n) as usize], HASH);
        let mut d1 = d.clone();
        d1[..8].copy_from_slice(&DISC_CREATE_ORDER);
        let v1 = CreateOrderView::parse(&d1).expect("reads");
        assert_eq!((v1.version, v1.roles()), (1, ROLES_V1));
        let k = [0x5A; 32];
        let d = params(DISC_CREATE_ORDER, b"", b"m", (1, 0), 1, 2, 3, ORDER_TYPE_MARKET, Some(k), Some(25));
        let v = CreateOrderView::parse(&d).expect("reads");
        assert!(v.has_integrator && !v.is_buy);
        assert_eq!(&d[v.integrator_off as usize..v.integrator_off as usize + 32], &k);
        assert_eq!((v.integrator_fee_bps, v.order_type), (Some(25), ORDER_TYPE_MARKET));
    }

    #[test]
    fn create_order_params_that_do_not_read_whole_are_refused() {
        let good = limit_params(1, 2, 3);
        assert!(CreateOrderView::parse(&good).is_ok());
        // Another discriminator, or none.
        let mut other = good.clone();
        other[..8].copy_from_slice(&DISC_CLAIM_PAYOUT);
        assert_eq!(CreateOrderView::parse(&other), Err(TxErr::JupInstruction));
        assert_eq!(CreateOrderView::parse(&good[..7]), Err(TxErr::JupInstruction));
        // Every proper prefix is short; a byte more is trailing.
        let mut i = 8usize;
        while i < good.len() {
            assert_eq!(CreateOrderView::parse(&good[..i]), Err(TxErr::OrderData), "prefix {i}");
            i += 1;
        }
        let mut long = good.clone();
        long.push(0);
        assert_eq!(CreateOrderView::parse(&long), Err(TxErr::OrderData));
        // A string over the bound (65 B), and a length past the data.
        let big = [b'a'; CREATE_STR_MAX + 1];
        let d = params(DISC_CREATE_ORDER2, &big, HASH, (1, 1), 1, 2, 3, 1, None, None);
        assert_eq!(CreateOrderView::parse(&d), Err(TxErr::OrderData));
        let at_bound = params(DISC_CREATE_ORDER2, &big[..CREATE_STR_MAX], HASH, (1, 1), 1, 2, 3, 1, None, None);
        assert!(CreateOrderView::parse(&at_bound).is_ok());
        let mut huge = good.clone();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(CreateOrderView::parse(&huge), Err(TxErr::OrderData));
        // A bool, the enum or an Option tag that is not 0 or 1.
        let flags = 8 + 4 + HASH.len() + 4 + HASH.len();
        let tail = good.len() - 3;
        for at in [flags, flags + 1, tail, tail + 1, tail + 2] {
            let mut bad = good.clone();
            bad[at] = 2;
            assert_eq!(CreateOrderView::parse(&bad), Err(TxErr::OrderData), "byte {at}");
        }
        // Some(integrator) whose key is cut short.
        let mut cut = good[..tail + 1].to_vec();
        cut.push(1);
        cut.extend_from_slice(&[7u8; 31]);
        assert_eq!(CreateOrderView::parse(&cut), Err(TxErr::OrderData));
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            /// Any params Borsh-encoded read back exactly.
            #[test]
            fn create_order_params_round_trip(
                ext in proptest::collection::vec(any::<u8>(), 0..=CREATE_STR_MAX),
                market in proptest::collection::vec(any::<u8>(), 0..=CREATE_STR_MAX),
                yes in 0u8..2,
                buy in 0u8..2,
                contracts in any::<u64>(),
                cap in any::<u64>(),
                deposit in any::<u64>(),
                order_type in 0u8..2,
                integrator in proptest::option::of(any::<[u8; 32]>()),
                fee in proptest::option::of(any::<u16>()),
                v2 in any::<bool>(),
            ) {
                let disc = if v2 { DISC_CREATE_ORDER2 } else { DISC_CREATE_ORDER };
                let d = params(disc, &ext, &market, (yes, buy), contracts, cap, deposit, order_type, integrator, fee);
                let v = CreateOrderView::parse(&d).expect("a well-formed encoding reads");
                prop_assert_eq!(v.version, if v2 { 2 } else { 1 });
                let (eo, en) = v.external_id;
                prop_assert_eq!(&d[eo as usize..(eo + en) as usize], ext.as_slice());
                let (mo, mn) = v.market_id;
                prop_assert_eq!(&d[mo as usize..(mo + mn) as usize], market.as_slice());
                prop_assert_eq!((v.is_yes, v.is_buy), (yes == 1, buy == 1));
                prop_assert_eq!((v.contracts, v.max_fill_price_usd, v.deposit_amount), (contracts, cap, deposit));
                prop_assert_eq!(v.order_type, order_type);
                prop_assert_eq!(v.has_integrator, integrator.is_some());
                prop_assert_eq!(v.integrator_fee_bps, fee);
            }

            /// Arbitrary bytes after a create-order discriminator never
            /// panic, and what reads lies inside the data.
            #[test]
            fn arbitrary_create_order_data_never_panics(
                body in proptest::collection::vec(any::<u8>(), 0..200),
                v2 in any::<bool>(),
            ) {
                let mut d = if v2 { DISC_CREATE_ORDER2 } else { DISC_CREATE_ORDER }.to_vec();
                d.extend_from_slice(&body);
                if let Ok(v) = CreateOrderView::parse(&d) {
                    prop_assert!(v.external_id.1 as usize <= CREATE_STR_MAX);
                    prop_assert!(v.market_id.1 as usize <= CREATE_STR_MAX);
                    prop_assert!((v.market_id.0 + v.market_id.1) as usize <= d.len());
                    prop_assert!(v.order_type <= ORDER_TYPE_LIMIT);
                }
            }
        }
    }

    // --- PMJ W5: the co-signer amendment (dormant, R-PMJ-S44) ------------

    /// The clean order with `OTHER` as the second required signer at key 1
    /// (`header` [2, ro_signed, 5]).
    fn cosigned(ro_signed: u8, extra: Option<I>) -> Vec<u8> {
        let mut k = keys();
        k.insert(1, OTHER);
        let mut ixs = clean_ixs();
        if let Some(x) = extra {
            ixs.push(x);
        }
        for ix in ixs.iter_mut() {
            ix.0 += 1;
            for a in ix.1.iter_mut() {
                if *a >= 1 {
                    *a += 1;
                }
            }
        }
        build(&k, [2, ro_signed, 5], &ixs, Some(&[(TABLE, vec![0], vec![1])]))
    }

    fn pinned(cosigner: [u8; 32]) -> TxGuard {
        let mut g = guard_ok();
        g.cosigner = cosigner;
        g.has_cosigner = true;
        g
    }

    #[test]
    fn a_pinned_read_only_cosigner_with_an_empty_slot_passes() {
        let raw = cosigned(1, None);
        let p = check(&raw, &pinned(OTHER)).expect("passes");
        assert_eq!(p.signer_idx, 0, "our slot is 0");
        // Without the pin, LAW PMJ-G refuses it as before.
        assert_eq!(check(&raw, &guard_ok()), Err(TxErr::Signers));
    }

    #[test]
    fn a_second_signer_other_than_the_pinned_key_is_refused() {
        assert_eq!(check(&cosigned(1, None), &pinned(ROGUE)), Err(TxErr::Signers));
    }

    #[test]
    fn a_writable_cosigner_is_refused() {
        assert_eq!(check(&cosigned(0, None), &pinned(OTHER)), Err(TxErr::Signers));
    }

    #[test]
    fn the_pinned_key_as_fee_payer_is_refused() {
        // OTHER pays (key 0), our wallet is key 1.
        let mut k = keys();
        k.insert(0, OTHER);
        let mut ixs = clean_ixs();
        for ix in ixs.iter_mut() {
            ix.0 += 1;
            for a in ix.1.iter_mut() {
                *a += 1;
            }
        }
        let raw = build(&k, [2, 1, 5], &ixs, None);
        let mut g = pinned(OTHER);
        assert_eq!(check(&raw, &g), Err(TxErr::FeePayer));
        g.cosigner = WALLET;
        assert_eq!(check(&raw, &g), Err(TxErr::FeePayer));
    }

    #[test]
    fn three_required_signers_are_refused_even_with_a_pin() {
        let mut k = keys();
        k.insert(1, OTHER);
        k.insert(2, ROGUE);
        let mut ixs = clean_ixs();
        for ix in ixs.iter_mut() {
            ix.0 += 2;
            for a in ix.1.iter_mut() {
                if *a >= 1 {
                    *a += 2;
                }
            }
        }
        let raw = build(&k, [3, 2, 5], &ixs, None);
        assert_eq!(check(&raw, &pinned(OTHER)), Err(TxErr::Signers));
    }

    #[test]
    fn a_durable_nonce_is_refused_with_a_pinned_cosigner() {
        // AdvanceNonceAccount (System, key 7 before the shift).
        let raw = cosigned(1, Some(I(7, vec![2, 3, 4], 4u32.to_le_bytes().to_vec())));
        assert_eq!(check(&raw, &pinned(OTHER)), Err(TxErr::Nonce));
    }

    #[test]
    fn every_other_line_still_holds_with_a_pinned_cosigner() {
        let g = pinned(OTHER);
        // An unknown program (ROGUE appended as key 9 after the shift).
        let mut k = keys();
        k.insert(1, OTHER);
        k.push(ROGUE);
        let mut ixs = clean_ixs();
        for ix in ixs.iter_mut() {
            ix.0 += 1;
            for a in ix.1.iter_mut() {
                if *a >= 1 {
                    *a += 1;
                }
            }
        }
        ixs.push(I(9, vec![0], vec![1]));
        let raw = build(&k, [2, 1, 6], &ixs, None);
        assert_eq!(check(&raw, &g), Err(TxErr::Program));
        // An unlisted lookup table.
        let mut raw = cosigned(1, None);
        let t = SolTx::parse(&raw).expect("parses");
        let o = t.alt[0].key_off as usize;
        raw[o..o + 32].copy_from_slice(&ROGUE);
        assert_eq!(check(&raw, &g), Err(TxErr::LookupTable));
        // A debit from the vault's account, a debit over the deposit, a
        // compute price over the cap.
        let raw = cosigned(1, Some(I(5, vec![2, 3, 1, 0], transfer_checked(1))));
        assert_eq!(check(&raw, &g), Err(TxErr::DebitSource));
        let raw = cosigned(1, Some(I(5, vec![1, 3, 2, 0], transfer_checked(1))));
        assert_eq!(check(&raw, &g), Err(TxErr::Debit));
        let mut price = vec![3u8];
        price.extend_from_slice(&50_001u64.to_le_bytes());
        let raw = cosigned(1, Some(I(6, vec![], price)));
        assert_eq!(check(&raw, &g), Err(TxErr::ComputePrice));
    }

    // --- PMJ W5: the public fixtures (mainnet test vectors) --------------

    const FX_V1_MARKET: &str = include_str!("../tests/fixtures/create-order-v1-market.b64");
    const FX_V1_LIMIT: &str = include_str!("../tests/fixtures/create-order-v1-limit.b64");
    const FX_V2_MARKET: &str = include_str!("../tests/fixtures/create-order2-market.b64");
    const FX_V2_LIMIT: &str = include_str!("../tests/fixtures/create-order2-limit.b64");
    const FX_CLAIM2: &str = include_str!("../tests/fixtures/claim-payout2.b64");

    const COSIGNER_B58: &str = "AJooF3xRkSx1PUC5Y9dNZHy9LMHcx5xKhPxj4iW4jepf";
    const VAULT_B58: &str = "2y9Ad2GD7gwiMkkMu4bBK5216Pv9YJsBkSHAGwN3rBuJ";
    const ALT_B58: &str = "6awx2Pq4uxeHsBcmutEqErz79C4TUg4EQEiBX5T5BVfX";
    const JUPUSD_B58: &str = "JuprjznTrTSp2UFa3ZBUFgwdAmtZCq4MQCwysN55USD";

    fn fixture(b64: &str) -> Vec<u8> {
        let mut out = vec![0u8; TX_MAX];
        let n = core_crypto::base64_decode(b64.trim().as_bytes(), &mut out).expect("a fixture's base64");
        out.truncate(n);
        out
    }

    /// The guard of the fixture's owner: the programs, table, vault (its
    /// table entry 1), mint and deposit account of the public transaction,
    /// the co-signer pinned.
    fn fixture_guard(owner: &str, deposit_account: &str) -> TxGuard {
        let mut g = TxGuard::refuse_all(key58(owner));
        assert!(g.allow_program(COMPUTE_BUDGET_PROGRAM));
        assert!(g.allow_program(PREDICT_PROGRAM));
        assert!(g.allow_program(ATA_PROGRAM));
        assert!(g.allow_alt(key58(ALT_B58)));
        g.max_cu_price = 400_000;
        g.deposit_account = key58(deposit_account);
        g.deposit_mint = key58(JUPUSD_B58);
        g.vault = key58(VAULT_B58);
        g.vault_alt = key58(ALT_B58);
        g.vault_alt_entry = 1;
        g.cosigner = key58(COSIGNER_B58);
        g.has_cosigner = true;
        g
    }

    /// A public create-order and what its build answer would have said:
    /// every key in full, read off the transaction by role (W5's table), so
    /// the tests stand alone.
    #[derive(Copy, Clone)]
    struct Fx {
        b64: &'static str,
        owner: &'static str,
        /// The owner's token account in JupUSD: the CPI deposit's source.
        source: &'static str,
        position: &'static str,
        order: &'static str,
        market: &'static [u8],
        deposit: u64,
        cap: u64,
        contracts: u64,
        version: u8,
        order_type: u8,
    }

    const FIXTURES: [Fx; 4] = [
        Fx {
            b64: FX_V1_MARKET,
            owner: "3QnKgNZByX4gfXdRR2mXekoby7M7CF6hrXAcVHU2YA8D",
            source: "FQSXTzF7qhGpVVDMckQQ5AAaUBfi7n3NWnJ8eo2qbzDe",
            position: "AD2yLzZyJ5bkqxgAVP92RjMsUtPegC8dUd3tVmXd6UKL",
            order: "7HcJs6bfrraWkCkSdARPL66X32uZUgJmTuvwkLzKYuyB",
            market: b"152bec8ae0dfdd1bc81aa9a90ebf6434",
            deposit: 5_000_000,
            cap: 550_000,
            contracts: 8_630_000,
            version: 1,
            order_type: ORDER_TYPE_MARKET,
        },
        Fx {
            b64: FX_V1_LIMIT,
            owner: "4mc4pL8DCthQ6yzatKfEh63Hm61TWF5Rerq8UmCWoMV2",
            source: "AtePzmqKm29usLetmsxoJyQuvdApyBAuvT66Gvo66fGZ",
            position: "AwuF21j7ubqd4waSg2z14Hhde7TiWJrY5ztE1k9ppTCd",
            order: "Hgooy2co4BNkzNDa9spjmeBxHozQBMGjH2FTz62XyxNo",
            market: b"47c243fed659fdb0a809b48b87b62ecb",
            deposit: 5_000_000,
            cap: 125_000,
            contracts: 36_780_000,
            version: 1,
            order_type: ORDER_TYPE_LIMIT,
        },
        Fx {
            b64: FX_V2_MARKET,
            owner: "5wzHj5BRhsViwc79VUtPv1DpAbafkFnYLc81hK2AT2be",
            source: "911cuEbXmZxEhb2aCVNmukJiiKAftyn8MaYcickGZy5y",
            position: "FNjqfrMeQmi77BDkqqBfaZ2nCTZdzhHn2AhMWXTVrFHY",
            order: "T3hBWcG1jC4es2jTW88R3AigMB5rFRQRsKVqapjV78S",
            market: b"1e685b58e58ffbe34197cfbe31161f8b",
            deposit: 5_000_000,
            cap: 340_000,
            contracts: 13_790_000,
            version: 2,
            order_type: ORDER_TYPE_MARKET,
        },
        Fx {
            b64: FX_V2_LIMIT,
            owner: "63EEC9FfGyksm7PkVC6z8uAmqozbQcTzbkWJNsgqjkFs",
            source: "6Sqcd42uPg9YgZ8NGeU2mkUdon15NN35ZkwUbyXzuVMt",
            position: "J4Nh2LaCCA3fBWBidm9SJ1iXEdNFV7B1NxBD3mDZW1am",
            order: "3RxBta8aAVS8U8wTLQJ7SCrja5vcxyVtPXw7iMGmSmot",
            market: b"ad44626a542bdd650466f1c527f02905",
            deposit: 6_000_000,
            cap: 30_000,
            contracts: 187_170_000,
            version: 2,
            order_type: ORDER_TYPE_LIMIT,
        },
    ];

    /// The answer the fixture's build would have given, at its own cap.
    fn ask_of(fx: &Fx) -> OrderAsk<'static> {
        OrderAsk {
            deposit: fx.deposit,
            limit_micro: fx.cap,
            order: key58(fx.order),
            position: key58(fx.position),
            market_hash: fx.market,
            contracts: fx.contracts,
        }
    }

    /// The fixture's create-order and the static key at role `role`.
    fn role_key(raw: &[u8], t: &SolTx, role: Option<usize>) -> [u8; 32] {
        let mut k = 0usize;
        while k < t.n_ix as usize {
            let ix = t.ix[k];
            k += 1;
            if t.key(raw, ix.program as usize) != PREDICT_PROGRAM.as_slice() {
                continue;
            }
            let d = &raw[ix.data_off as usize..ix.data_off as usize + ix.data_len as usize];
            let r = CreateOrderView::parse(d).expect("a create-order").roles();
            let mut s = [0u8; 32];
            s.copy_from_slice(t.acc_key(raw, &ix, role.unwrap_or(r.source as usize)).expect("static"));
            return s;
        }
        panic!("no create-order")
    }

    #[test]
    fn the_public_create_orders_read_as_their_role_table_says() {
        for fx in FIXTURES {
            let raw = fixture(fx.b64);
            let t = SolTx::parse(&raw).expect("a public transaction parses");
            assert_eq!((t.header[0], t.header[1]), (2, 1), "two signers, the co-signer read-only");
            assert_eq!(t.key(&raw, 1), key58(COSIGNER_B58).as_slice());
            assert_eq!(role_key(&raw, &t, None), key58(fx.source), "the CPI transfer's source, by role");
            assert_eq!(role_key(&raw, &t, Some(ROLE_POSITION)), key58(fx.position));
            assert_eq!(role_key(&raw, &t, Some(ROLE_ORDER)), key58(fx.order));
            let g = fixture_guard(fx.owner, fx.source);
            let ask = ask_of(&fx);
            let r = guard_order(&raw, &t, &g, &ask);
            if fx.order_type == ORDER_TYPE_LIMIT {
                let (p, v) = r.expect("a public Limit order at its own cap passes with the co-signer pinned");
                assert_eq!((p.signer_idx, p.token_debit), (0, 0));
                assert_eq!(
                    (v.version, v.order_type, v.max_fill_price_usd, v.deposit_amount, v.contracts),
                    (fx.version, fx.order_type, fx.cap, fx.deposit, fx.contracts)
                );
                assert!(u128::from(v.contracts) * u128::from(fx.cap) <= u128::from(fx.deposit) * 1_000_000);
                // A micro-dollar of limit under its cap, a deposit off by one.
                let low = OrderAsk { limit_micro: fx.cap - 1, ..ask };
                assert_eq!(guard_order(&raw, &t, &g, &low).map(|x| x.1), Err(TxErr::PriceAboveLimit));
                let off = OrderAsk { deposit: fx.deposit - 1, ..ask };
                assert_eq!(guard_order(&raw, &t, &g, &off).map(|x| x.1), Err(TxErr::DepositMismatch));
            } else {
                assert_eq!(r.map(|x| x.1), Err(TxErr::NotLimit), "a Market order binds no price");
            }
            // LAW PMJ-G as written (no pin) refuses every public order.
            let mut law = g;
            law.has_cosigner = false;
            assert_eq!(guard_order(&raw, &t, &law, &ask).map(|x| x.1), Err(TxErr::Signers));
            // A claim's guard refuses a create-order.
            assert_eq!(guard_claim(&raw, &t, &g), Err(TxErr::JupInstruction));
        }
    }

    #[test]
    fn a_pinned_cosigner_with_its_valid_signature_in_place_passes() {
        let fx = FIXTURES[3];
        let raw = fixture(fx.b64);
        let t = SolTx::parse(&raw).expect("parses");
        let sig: [u8; 64] = raw[65..129].try_into().expect("slot 1");
        assert!(signer_ed25519::verify(&key58(COSIGNER_B58), t.message(&raw), &sig), "the landed co-signature");
        let g = fixture_guard(fx.owner, fx.source);
        assert!(guard_order(&raw, &t, &g, &ask_of(&fx)).is_ok());
        // An empty co-signer slot passes too (Jupiter signs at /execute).
        let mut empty = raw.clone();
        empty[65..129].fill(0);
        assert!(guard_order(&empty, &t, &g, &ask_of(&fx)).is_ok());
    }

    #[test]
    fn a_cosigner_slot_with_an_invalid_signature_is_refused() {
        let fx = FIXTURES[3];
        let mut raw = fixture(fx.b64);
        raw[100] ^= 0x01;
        let t = SolTx::parse(&raw).expect("parses");
        let g = fixture_guard(fx.owner, fx.source);
        assert_eq!(guard_order(&raw, &t, &g, &ask_of(&fx)).map(|x| x.1), Err(TxErr::CosignerSignature));
        // Our own slot is never checked: we overwrite it.
        let mut ours = fixture(fx.b64);
        ours[10] ^= 0x01;
        assert!(guard_order(&ours, &t, &g, &ask_of(&fx)).is_ok());
    }

    #[test]
    fn a_public_order_off_its_pins_is_refused_by_role() {
        let fx = FIXTURES[3];
        let raw = fixture(fx.b64);
        let t = SolTx::parse(&raw).expect("parses");
        let ask = ask_of(&fx);
        let base = fixture_guard(fx.owner, fx.source);
        let refused = |g: &TxGuard| guard_order(&raw, &t, g, &ask).map(|x| x.1);
        // Another deposit account; another mint; no deposit account.
        let mut g = base;
        g.deposit_account = [0x33; 32];
        assert_eq!(refused(&g), Err(TxErr::DebitSource));
        g = base;
        g.deposit_mint = key58("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
        assert_eq!(refused(&g), Err(TxErr::DebitSource));
        // The vault: none pinned; pinned as a static key only (the build
        // loads it through the table); another entry; another table.
        g = base;
        g.vault = [0; 32];
        assert_eq!(refused(&g), Err(TxErr::Vault));
        g = base;
        g.vault_alt = [0; 32];
        assert_eq!(refused(&g), Err(TxErr::Vault));
        g = base;
        g.vault_alt_entry = 5;
        assert_eq!(refused(&g), Err(TxErr::Vault));
        g = base;
        g.vault_alt = [0x88; 32];
        assert_eq!(refused(&g), Err(TxErr::Vault));
        // Another program id as the Predict program: no instruction of it.
        g = base;
        g.predict_program = COMPUTE_BUDGET_PROGRAM;
        assert_eq!(refused(&g), Err(TxErr::JupInstruction));
        // The guard of someone else's wallet.
        g = base;
        g.wallet = [0x11; 32];
        assert_eq!(refused(&g), Err(TxErr::FeePayer));
        assert!(refused(&base).is_ok());
    }

    /// MAJOR 1 (risk review W5-1): the answer's order and position keys,
    /// market hash and contracts are the signed create-order's.
    #[test]
    fn a_public_order_whose_answer_names_other_keys_market_or_contracts_is_refused() {
        let fx = FIXTURES[3];
        let raw = fixture(fx.b64);
        let t = SolTx::parse(&raw).expect("parses");
        let g = fixture_guard(fx.owner, fx.source);
        let ask = ask_of(&fx);
        let check = |a: &OrderAsk<'_>| guard_order(&raw, &t, &g, a).map(|x| x.1);
        assert!(check(&ask).is_ok());
        // The order key is the position's, and the position's the order's.
        let swapped = OrderAsk { order: ask.position, position: ask.order, ..ask };
        assert_eq!(check(&swapped), Err(TxErr::OrderAccounts));
        assert_eq!(check(&OrderAsk { order: [0x0A; 32], ..ask }), Err(TxErr::OrderAccounts));
        assert_eq!(check(&OrderAsk { position: [0x0B; 32], ..ask }), Err(TxErr::OrderAccounts));
        assert_eq!(check(&OrderAsk { order: [0; 32], ..ask }), Err(TxErr::OrderAccounts));
        // Another market's hash, or none.
        let other = FIXTURES[0].market;
        assert_eq!(check(&OrderAsk { market_hash: other, ..ask }), Err(TxErr::MarketMismatch));
        assert_eq!(check(&OrderAsk { market_hash: b"", ..ask }), Err(TxErr::MarketMismatch));
        assert_eq!(check(&OrderAsk { market_hash: &fx.market[..31], ..ask }), Err(TxErr::MarketMismatch));
        // Other contracts, or none.
        assert_eq!(check(&OrderAsk { contracts: fx.contracts + 1, ..ask }), Err(TxErr::ContractsMismatch));
        assert_eq!(check(&OrderAsk { contracts: 0, ..ask }), Err(TxErr::ContractsMismatch));
    }

    #[test]
    fn the_public_claim_payout2_passes_the_claim_guard_and_no_order_guard() {
        // Owner 5wzHj5…, the same account its create_order2 drew from.
        let raw = fixture(FX_CLAIM2);
        let t = SolTx::parse(&raw).expect("parses");
        assert_eq!(t.header[0], 1, "a claim has one signer, the owner");
        let fx = FIXTURES[2];
        let mut g = fixture_guard(fx.owner, fx.source);
        g.has_cosigner = false;
        let p = guard_claim(&raw, &t, &g).expect("the public claim passes");
        assert_eq!((p.signer_idx, p.token_debit, p.lamports), (0, 0, 0));
        assert_eq!(guard_order(&raw, &t, &g, &ask_of(&fx)).map(|x| x.1), Err(TxErr::JupInstruction));
        // The payout must land in our deposit account; the vault is pinned.
        let mut other = g;
        other.deposit_account = [0x33; 32];
        assert_eq!(guard_claim(&raw, &t, &other), Err(TxErr::OrderAccounts));
        other = g;
        other.vault = [0x44; 32];
        assert_eq!(guard_claim(&raw, &t, &other), Err(TxErr::Vault));
        // claim_payout's discriminator reads the same; another does not.
        let ix = t.ix[3];
        let mut v1 = raw.clone();
        v1[ix.data_off as usize..ix.data_off as usize + 8].copy_from_slice(&DISC_CLAIM_PAYOUT);
        assert!(guard_claim(&v1, &t, &g).is_ok());
        let mut v9 = raw.clone();
        v9[ix.data_off as usize..ix.data_off as usize + 8].copy_from_slice(&DISC_CREATE_ORDER2);
        assert_eq!(guard_claim(&v9, &t, &g), Err(TxErr::JupInstruction));
    }

    /// MINOR 5 (risk review W5-1): the claim's ATA instruction is exactly a
    /// CreateIdempotent of our deposit account, paid by our wallet.
    #[test]
    fn a_claims_ata_instruction_must_create_our_deposit_account_idempotently() {
        let raw = fixture(FX_CLAIM2);
        let t = SolTx::parse(&raw).expect("parses");
        let fx = FIXTURES[2];
        let mut g = fixture_guard(fx.owner, fx.source);
        g.has_cosigner = false;
        assert!(guard_claim(&raw, &t, &g).is_ok());
        // ix[2] is the ATA program's: [payer 0, account 1, owner 0, mint 7,
        // System 8, Token 9], data [1].
        let ata = t.ix[2];
        assert_eq!(t.key(&raw, ata.program as usize), ATA_PROGRAM.as_slice());
        let a0 = ata.acc_off as usize;
        let d0 = ata.data_off as usize;
        for (at, to, why) in [
            (a0, 2u8, "another payer (the vault, key 2)"),
            (a0 + 1, 3, "another account created"),
            (a0 + 2, 2, "another owner"),
            (a0 + 3, 9, "another mint"),
            (a0 + 5, 7, "another token program"),
            (d0, 0, "Create, not CreateIdempotent"),
        ] {
            let mut bad = raw.clone();
            bad[at] = to;
            let tb = SolTx::parse(&bad).expect("still parses");
            assert_eq!(guard_claim(&bad, &tb, &g), Err(TxErr::ExtraInstruction), "{why}");
        }
    }
}
