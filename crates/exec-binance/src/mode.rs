// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **Account mode, scope and the boot assertions (O-BX2, O-BX2a; BX-17,
//! BX-19).**
//!
//! * [`AccountMode`] — classic, Portfolio Margin (PM) or PM Pro. It decides
//!   the order-entry and user-data surfaces (plan §3.3) and, with the
//!   product, whether a maker may rest ([`maker_ok`], BX-17 after §13.5:
//!   makers only on **UM classic**; PM Pro waits on K14, PM on K13, COIN-M
//!   on K16, options on K3; spot has no dead-man at all).
//! * [`AccountScope`] — dedicated (the engine owns the account) or shared
//!   (the artifact's owned lists are the engine's universe; foreign activity
//!   on an owned instrument is a drift HALT, BX-8).
//! * [`built`] — what this phase's gateway speaks. BX6 builds **UM ×
//!   classic** end to end; every other product or mode refuses the boot and
//!   names the phase that brings it (BX7 futures modes, BX8 spot, BX9
//!   options, BX10 Binance Stocks).
//! * The **boot assertions** (BX-19) — pure judges over the venue's
//!   answers. The gateway's boot phase makes the (cold, signed) queries and
//!   refuses the boot on any mismatch; nothing here mutates the account
//!   (every mutation is the operator tool's, `exec-smoke --binance setup`).

use crate::inst::{PRODUCT_COINM, PRODUCT_EQUITY, PRODUCT_OPTIONS, PRODUCT_SPOT, PRODUCT_USDM};

/// The account's margin mode (O-BX2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccountMode {
    /// Classic: per-product WS API sessions.
    Classic = 0,
    /// Portfolio Margin: papi REST, the `/pm` user stream.
    Pm = 1,
    /// Portfolio Margin Pro: fapi/dapi, the `/pm-classic` user stream.
    PmPro = 2,
}

impl AccountMode {
    /// The `exec.toml` word.
    #[must_use]
    pub fn from_word(w: &str) -> Option<Self> {
        match w {
            "classic" => Some(Self::Classic),
            "pm" => Some(Self::Pm),
            "pm_pro" => Some(Self::PmPro),
            _ => None,
        }
    }

    /// The `exec.toml` word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::Pm => "pm",
            Self::PmPro => "pm_pro",
        }
    }
}

/// Who else trades the account (O-BX2a).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AccountScope {
    /// The engine alone.
    Dedicated = 0,
    /// The operator too: the owned lists bound the engine's universe.
    Shared = 1,
}

impl AccountScope {
    /// The `exec.toml` word.
    #[must_use]
    pub fn from_word(w: &str) -> Option<Self> {
        match w {
            "dedicated" => Some(Self::Dedicated),
            "shared" => Some(Self::Shared),
            _ => None,
        }
    }

    /// The `exec.toml` word.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Dedicated => "dedicated",
            Self::Shared => "shared",
        }
    }
}

/// **BX-17 after §13.5**: may a maker rest on `product` in `mode`? Only
/// where the venue dead-man is ours to arm and works — UM classic
/// (`countdownCancelAll`).
#[must_use]
pub const fn maker_ok(product: u8, mode: AccountMode) -> bool {
    product == PRODUCT_USDM && matches!(mode, AccountMode::Classic)
}

/// What this phase's gateway speaks: `Ok` for UM × classic; otherwise the
/// phase that builds the product or mode.
pub const fn built(product: u8, mode: AccountMode) -> Result<(), &'static str> {
    match product {
        PRODUCT_USDM => match mode {
            AccountMode::Classic => Ok(()),
            AccountMode::Pm | AccountMode::PmPro => Err("BX7 (PM and PM Pro sessions)"),
        },
        PRODUCT_COINM => Err("BX7 (COIN-M)"),
        PRODUCT_SPOT => Err("BX8 (spot)"),
        PRODUCT_OPTIONS => Err("BX9 (options)"),
        PRODUCT_EQUITY => Err("BX10 (Binance Stocks)"),
        _ => Err("no phase: an unknown product"),
    }
}

// -------------------------------------------------------------------------
// The boot assertions (BX-19)
// -------------------------------------------------------------------------

/// A boot assertion that did not hold. Each refuses the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AssertErr {
    /// The account's mode is not `account_mode` (K17's discriminator).
    Mode,
    /// Hedge mode is on (`dualSidePosition: true`); the engine books one
    /// net position per instrument.
    HedgeMode,
    /// Multi-assets margin is on.
    MultiAssets,
    /// The key cannot trade the armed product.
    NoTradePermission,
    /// The key can withdraw: a trading key must not.
    CanWithdraw,
    /// The answer did not scan (fail closed).
    Unreadable,
}

/// The JSON boolean after `"key":` in `body`, or `None`.
fn json_bool(body: &[u8], key: &[u8]) -> Option<bool> {
    let at = core_parse::find_field(body, key)?;
    let at = core_parse::skip_ws(body, at);
    let rest = body.get(at..)?;
    if rest.starts_with(b"true") {
        Some(true)
    } else if rest.starts_with(b"false") {
        Some(false)
    } else {
        None
    }
}

/// `GET /fapi/v1/positionSide/dual` → `{"dualSidePosition":false}` (one-way).
pub fn judge_one_way(body: &[u8]) -> Result<(), AssertErr> {
    match json_bool(body, b"\"dualSidePosition\":") {
        Some(false) => Ok(()),
        Some(true) => Err(AssertErr::HedgeMode),
        None => Err(AssertErr::Unreadable),
    }
}

/// `GET /fapi/v1/multiAssetsMargin` → `{"multiAssetsMargin":false}`.
pub fn judge_single_asset(body: &[u8]) -> Result<(), AssertErr> {
    match json_bool(body, b"\"multiAssetsMargin\":") {
        Some(false) => Ok(()),
        Some(true) => Err(AssertErr::MultiAssets),
        None => Err(AssertErr::Unreadable),
    }
}

/// `GET /sapi/v1/account/apiRestrictions`: the key trades futures and
/// cannot withdraw.
pub fn judge_key_futures(body: &[u8]) -> Result<(), AssertErr> {
    match (
        json_bool(body, b"\"enableFutures\":"),
        json_bool(body, b"\"enableWithdrawals\":"),
    ) {
        (Some(true), Some(false)) => Ok(()),
        (Some(false), Some(_)) => Err(AssertErr::NoTradePermission),
        (Some(true), Some(true)) => Err(AssertErr::CanWithdraw),
        _ => Err(AssertErr::Unreadable),
    }
}

/// `GET /sapi/v1/portfolio/account` (K17): a classic account is refused
/// with `-21001`; a PM account answers `accountType` `PM_1` (PM Pro),
/// `PM_2` (PM) or `PM_3` (PM Pro SPAN). `status` is the HTTP status.
pub fn judge_mode(status: u16, body: &[u8], want: AccountMode) -> Result<(), AssertErr> {
    let seen = if status == 200 {
        let Some(at) = core_parse::find_field(body, b"\"accountType\":\"") else {
            return Err(AssertErr::Unreadable);
        };
        match body.get(at..at + 5) {
            Some(b"PM_2\"") => AccountMode::Pm,
            Some(b"PM_1\"") | Some(b"PM_3\"") => AccountMode::PmPro,
            _ => return Err(AssertErr::Unreadable),
        }
    } else {
        let Some(at) = core_parse::find_field(body, b"\"code\":") else {
            return Err(AssertErr::Unreadable);
        };
        match core_parse::scan_i64(body, core_parse::skip_ws(body, at)) {
            Some((-21_001, _)) => AccountMode::Classic,
            _ => return Err(AssertErr::Unreadable),
        }
    };
    if seen == want {
        Ok(())
    } else {
        Err(AssertErr::Mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_maker_list_is_um_classic_only() {
        let modes = [AccountMode::Classic, AccountMode::Pm, AccountMode::PmPro];
        for p in [PRODUCT_SPOT, PRODUCT_USDM, PRODUCT_COINM, PRODUCT_OPTIONS, PRODUCT_EQUITY] {
            for m in modes {
                assert_eq!(maker_ok(p, m), p == PRODUCT_USDM && m == AccountMode::Classic);
            }
        }
    }

    #[test]
    fn only_um_classic_is_built() {
        assert_eq!(built(PRODUCT_USDM, AccountMode::Classic), Ok(()));
        assert!(built(PRODUCT_USDM, AccountMode::Pm).unwrap_err().starts_with("BX7"));
        assert!(built(PRODUCT_SPOT, AccountMode::Classic).unwrap_err().starts_with("BX8"));
        assert!(built(PRODUCT_OPTIONS, AccountMode::Classic).unwrap_err().starts_with("BX9"));
        assert!(built(PRODUCT_EQUITY, AccountMode::Classic).unwrap_err().starts_with("BX10"));
        assert!(built(PRODUCT_COINM, AccountMode::Classic).unwrap_err().starts_with("BX7"));
    }

    #[test]
    fn words_round_trip() {
        for m in [AccountMode::Classic, AccountMode::Pm, AccountMode::PmPro] {
            assert_eq!(AccountMode::from_word(m.word()), Some(m));
        }
        for s in [AccountScope::Dedicated, AccountScope::Shared] {
            assert_eq!(AccountScope::from_word(s.word()), Some(s));
        }
        assert_eq!(AccountMode::from_word("cross"), None);
    }

    #[test]
    fn the_judges_fail_closed() {
        assert_eq!(judge_one_way(br#"{"dualSidePosition":false}"#), Ok(()));
        assert_eq!(judge_one_way(br#"{"dualSidePosition":true}"#), Err(AssertErr::HedgeMode));
        assert_eq!(judge_one_way(br#"{"dualSidePosition":"no"}"#), Err(AssertErr::Unreadable));
        assert_eq!(judge_single_asset(br#"{"multiAssetsMargin": false}"#), Ok(()));
        assert_eq!(judge_single_asset(br#"{"multiAssetsMargin":true}"#), Err(AssertErr::MultiAssets));
        let key = br#"{"ipRestrict":true,"enableWithdrawals":false,"enableFutures":true}"#;
        assert_eq!(judge_key_futures(key), Ok(()));
        let w = br#"{"enableWithdrawals":true,"enableFutures":true}"#;
        assert_eq!(judge_key_futures(w), Err(AssertErr::CanWithdraw));
        let nf = br#"{"enableWithdrawals":false,"enableFutures":false}"#;
        assert_eq!(judge_key_futures(nf), Err(AssertErr::NoTradePermission));
        assert_eq!(judge_key_futures(b"{}"), Err(AssertErr::Unreadable));
        let classic = br#"{"code":-21001,"msg":"USER_IS_NOT_UNIACCOUNT"}"#;
        assert_eq!(judge_mode(400, classic, AccountMode::Classic), Ok(()));
        assert_eq!(judge_mode(400, classic, AccountMode::Pm), Err(AssertErr::Mode));
        let pm = br#"{"accountType":"PM_2","uniMMR":"5.0"}"#;
        assert_eq!(judge_mode(200, pm, AccountMode::Classic), Err(AssertErr::Mode));
        assert_eq!(judge_mode(200, pm, AccountMode::Pm), Ok(()));
        let pro = br#"{"accountType":"PM_1"}"#;
        assert_eq!(judge_mode(200, pro, AccountMode::PmPro), Ok(()));
        assert_eq!(judge_mode(400, br#"{"code":-1021}"#, AccountMode::Classic), Err(AssertErr::Unreadable));
        assert_eq!(judge_mode(200, b"garbage", AccountMode::Classic), Err(AssertErr::Unreadable));
    }
}
