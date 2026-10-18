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
    /// deposit account is configured.
    DebitSource,
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
    // hold the transaction behind another key.
    if tx.header[0] != 1 {
        return Err(TxErr::Signers);
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
}
