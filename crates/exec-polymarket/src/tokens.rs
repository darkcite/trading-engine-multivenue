// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! pm-gw's token table (PM15-6b, plan D-7): the windows the discovery
//! thread found, keyed by their instance id — both legs' 32 B token id
//! (the signed struct's `tokenId`) and their body prefix (rendered once,
//! here: every field fixed for the account, the token and BUY), the
//! condition id (recon's `market`) and the minimum size. One row per
//! family per window mod [`TOKEN_SPAN`]: a window's row outlives its end
//! by three windows — the late trades, recon and the redemption's reads
//! of its legs all find it (discovery fetches one window ahead, so a
//! parity table would lose a window the moment it ended).
//!
//! Cold path: a window arrives every 15 minutes per family; an order
//! reads its row by index.

use core_types::pm_roll::{pm_instance_id, pm_instance_parts, PM_ROLLING_MAX};
use ingress_polymarket::gamma15::PmWindowSpec;
use signer_eip712::pmv2::u256_from_decimal;

use crate::account::PmAccount;
use crate::render::{render_token_prefix, PREFIX_MAX};

/// Windows a family's rows hold.
pub const TOKEN_SPAN: usize = 4;

/// Rows: one per family per window mod [`TOKEN_SPAN`].
pub const TOKEN_ROWS: usize = PM_ROLLING_MAX * TOKEN_SPAN;

/// One leg of a window.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TokenLeg {
    /// The token id, big-endian `uint256`.
    pub id_be: [u8; 32],
    /// Bytes of `dec` used.
    pub dec_len: usize,
    /// The token id's decimal text (the account read's `token_id`).
    pub dec: [u8; 80],
    /// Bytes of `prefix` used.
    pub prefix_len: usize,
    /// The body's prefix for a BUY of this token (`render::render_token_prefix`).
    pub prefix: [u8; PREFIX_MAX],
}

/// One window.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct TokenRow {
    /// Its instance id (0 = empty).
    pub instance: u32,
    /// Its window index.
    pub window: u32,
    /// `orderMinSize`, shares ×1e6.
    pub min_size_1e6: i64,
    /// Bytes of `condition_id` used.
    pub condition_len: usize,
    /// `conditionId`, `0x` + 64 hex.
    pub condition_id: [u8; 66],
    /// `[Up, Down]`.
    pub legs: [TokenLeg; 2],
}

const EMPTY_LEG: TokenLeg = TokenLeg {
    id_be: [0; 32],
    dec_len: 0,
    dec: [0; 80],
    prefix_len: 0,
    prefix: [0; PREFIX_MAX],
};

const EMPTY_ROW: TokenRow = TokenRow {
    instance: 0,
    window: 0,
    min_size_1e6: 0,
    condition_len: 0,
    condition_id: [0; 66],
    legs: [EMPTY_LEG; 2],
};

/// Why a window was not taken.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TokenErr {
    /// A family or window the instance law does not name.
    Instance,
    /// Older than the window its row holds.
    Stale,
    /// A token id that is not a decimal `uint256`.
    TokenId,
}

/// The table (module docs).
pub struct PmTokens {
    rows: Box<[TokenRow; TOKEN_ROWS]>,
    taken: u64,
}

impl Default for PmTokens {
    fn default() -> Self {
        Self::new()
    }
}

impl PmTokens {
    /// Empty — **boot-only** (the rows are boxed here, ≈ 31 KiB).
    #[must_use]
    pub fn new() -> Self {
        Self {
            rows: exec_gateway::boxed_array(EMPTY_ROW),
            taken: 0,
        }
    }

    /// Windows taken so far.
    #[inline]
    #[must_use]
    pub const fn taken(&self) -> u64 {
        self.taken
    }

    #[inline(always)]
    fn row_of(window: u32, family: usize) -> usize {
        (family * TOKEN_SPAN + window as usize % TOKEN_SPAN) % TOKEN_ROWS
    }

    /// Take one window from discovery (a repeat of the window its row
    /// holds is a no-op).
    ///
    /// # Errors
    /// [`TokenErr`] — the window is not taken (counted by the caller).
    pub fn offer(&mut self, spec: &PmWindowSpec, acct: &PmAccount) -> Result<(), TokenErr> {
        let family = usize::from(spec.family);
        let Some(instance) = pm_instance_id(spec.window, family) else {
            return Err(TokenErr::Instance);
        };
        let r = Self::row_of(spec.window, family);
        let held = &self.rows[r];
        if held.instance == instance {
            return Ok(());
        }
        if held.instance != 0 && held.window > spec.window {
            return Err(TokenErr::Stale);
        }
        let (Some(up), Some(down)) = (
            u256_from_decimal(spec.up.as_bytes()),
            u256_from_decimal(spec.down.as_bytes()),
        ) else {
            return Err(TokenErr::TokenId);
        };
        let row = &mut self.rows[r];
        row.instance = instance;
        row.window = spec.window;
        row.min_size_1e6 = spec.min_size_1e6;
        let cond = spec.condition_id();
        row.condition_len = cond.len();
        // COPY: the condition id (66 B) into its row — cold, once a window.
        row.condition_id[..cond.len()].copy_from_slice(cond);
        row.legs[0].id_be = up;
        row.legs[1].id_be = down;
        let mut leg = 0usize;
        while leg < 2 {
            let dec = if leg == 0 {
                spec.up.as_bytes()
            } else {
                spec.down.as_bytes()
            };
            let l = &mut row.legs[leg];
            l.dec_len = dec.len().min(80);
            // COPY: the decimal id (≤ 78 B) — cold, once a window.
            l.dec[..l.dec_len].copy_from_slice(&dec[..l.dec_len]);
            l.prefix_len = render_token_prefix(
                &acct.maker_text,
                &acct.signer_text,
                dec,
                acct.sig_type,
                &mut l.prefix,
            );
            leg += 1;
        }
        self.taken += 1;
        Ok(())
    }

    /// The row of `instance`, if the table holds that window.
    #[inline]
    #[must_use]
    pub fn find(&self, instance: u32) -> Option<&TokenRow> {
        let (window, family) = pm_instance_parts(instance);
        if family >= PM_ROLLING_MAX || instance == 0 {
            return None;
        }
        let row = &self.rows[Self::row_of(window, family)];
        if row.instance == instance {
            Some(row)
        } else {
            None
        }
    }
}

impl TokenRow {
    /// The condition id's text.
    #[inline]
    #[must_use]
    pub fn condition_id(&self) -> &[u8] {
        &self.condition_id[..self.condition_len.min(66)]
    }
}
