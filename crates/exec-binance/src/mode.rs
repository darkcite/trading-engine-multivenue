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
//! * [`built`] — what this phase's gateway speaks: USDⓈ-M and COIN-M in
//!   every mode (BX6, BX7), spot × classic (BX8: IoC-only); every other
//!   product or mode refuses the boot and names what brings it (BX9
//!   options, BX10 Binance Stocks, a ruling for spot beside Portfolio
//!   Margin).
//! * The **boot assertions** (BX-19) — pure judges over the venue's
//!   answers. The gateway's boot phase makes the (cold, signed) queries and
//!   refuses the boot on any mismatch; nothing here mutates the account
//!   (every mutation is the operator tool's, `exec-smoke --binance-setup`).
//!   BX7 adds each owned USDⓈ-M row's margin type and leverage
//!   ([`judge_symbol_config`]); COIN-M's come with its account read. BX8
//!   adds the key's spot permission ([`judge_key`]) and each armed spot
//!   row's permission groups against the account's ([`judge_permission`],
//!   the bStock check).

use crate::inst::{PRODUCT_COINM, PRODUCT_EQUITY, PRODUCT_OPTIONS, PRODUCT_SPOT, PRODUCT_USDM};
use crate::json::{bool_of, field, i64_of, str_of, u64_of, Elems, Pairs};
use ingress_binance::discovery::{
    BnPermissions, BN_PERM_COPY, BN_PERM_DCA, BN_PERM_FLAT, BN_PERM_GRID, BN_PERM_GROUPS_MAX, BN_PERM_MARGIN, BN_PERM_MULTI_SET, BN_PERM_PSB,
    BN_PERM_RPI, BN_PERM_SPOT,
};

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

/// What this phase's gateway speaks: `Ok` for UM and COIN-M × classic
/// (BX7: COIN-M on its own lane, IoC-only — O-BX3a), × Portfolio Margin
/// (BX7, C2: papi, IoC-only — K13) and × PM Pro (BX7, C3: the classic
/// routes, IoC-only until K14 shows the dead-man there), spot × classic
/// (BX8: IoC-only — spot has no dead-man) and options × classic (BX9:
/// IoC-only — the countdown is a market maker's, `-6005`) and Binance
/// Stocks × classic (BX10: DAY orders under the gateway's TTL); otherwise
/// the phase or ruling that brings the product or mode.
pub const fn built(product: u8, mode: AccountMode) -> Result<(), &'static str> {
    match product {
        PRODUCT_USDM | PRODUCT_COINM => match mode {
            AccountMode::Classic | AccountMode::Pm | AccountMode::PmPro => Ok(()),
        },
        PRODUCT_SPOT => match mode {
            AccountMode::Classic => Ok(()),
            // Unproven (no PM Demo exists) and outside the unified
            // account's equity: not built.
            AccountMode::Pm | AccountMode::PmPro => Err("a ruling (spot beside Portfolio Margin or PM Pro)"),
        },
        PRODUCT_OPTIONS => match mode {
            AccountMode::Classic => Ok(()),
            // Options lie outside Portfolio Margin (the venue's FAQ) and no
            // PM Demo exists: not built.
            AccountMode::Pm | AccountMode::PmPro => Err("a ruling (options beside Portfolio Margin or PM Pro)"),
        },
        // BX10: Binance Stocks — LIMIT DAY under the gateway's mandatory TTL
        // (BX-17's exception), REST on SAPI. The gateway speaks it; the
        // ENGINE still refuses it (ruling S-3, O-BX16: no equity market
        // data, R17 — `cli::exec_boot`): only the operator's battery arms it.
        PRODUCT_EQUITY => match mode {
            AccountMode::Classic => Ok(()),
            // A broker path outside every margin program: not built.
            AccountMode::Pm | AccountMode::PmPro => Err("a ruling (Binance Stocks beside Portfolio Margin or PM Pro)"),
        },
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
    /// An owned row is not cross-margined: `ISOLATED`, or a margin type
    /// this build does not know (BX7; fail closed).
    Isolated,
    /// An owned row's initial leverage is above `max_leverage` (BX7).
    Leverage,
    /// The venue's answer does not list an owned row (BX7; fail closed).
    Unlisted,
    /// An armed spot row's permission groups are not the account's (BX8,
    /// the bStock check; [`judge_permission`]).
    SymbolNotPermitted,
    /// The spot account answers `canTrade: false` (BX8) — or the options
    /// account does (BX9).
    CannotTrade,
    /// BX9: the options account is reduce-only (`marginAccount.reduceOnly`).
    ReduceOnly,
}

/// The answer's own boolean `key` — named once, at the top level
/// ([`field`]: a nested key is not it, a second copy refuses; BX7) of an
/// answer that is that object whole (nothing after its brace: BX9's parser
/// review) — or `None`.
fn json_bool(body: &[u8], key: &[u8]) -> Option<bool> {
    if !crate::rest::whole(body) {
        return None;
    }
    let (a, z) = field(body, key)?;
    bool_of(body, a, z).ok()
}

/// `GET /fapi/v1/positionSide/dual` → `{"dualSidePosition":false}` (one-way).
pub fn judge_one_way(body: &[u8]) -> Result<(), AssertErr> {
    match json_bool(body, b"dualSidePosition") {
        Some(false) => Ok(()),
        Some(true) => Err(AssertErr::HedgeMode),
        None => Err(AssertErr::Unreadable),
    }
}

/// `GET /fapi/v1/multiAssetsMargin` → `{"multiAssetsMargin":false}`.
pub fn judge_single_asset(body: &[u8]) -> Result<(), AssertErr> {
    match json_bool(body, b"multiAssetsMargin") {
        Some(false) => Ok(()),
        Some(true) => Err(AssertErr::MultiAssets),
        None => Err(AssertErr::Unreadable),
    }
}

/// `GET /fapi/v1/symbolConfig?symbol=…` (BX7, BX-19): the owned row's
/// margin type is `CROSSED` and its initial leverage at most
/// `max_leverage`. The answer is an array; the entry naming `symbol` is
/// judged — no such entry is refused, as is one short of either field, a
/// leverage of 0 (no leverage is not one), a key an entry names twice and
/// a symbol two entries name (which would hold?).
pub fn judge_symbol_config(body: &[u8], symbol: &[u8], max_leverage: u8) -> Result<(), AssertErr> {
    match symbol_config(body, symbol)? {
        (true, 0) => Err(AssertErr::Unreadable),
        (true, l) if l <= u64::from(max_leverage) => Ok(()),
        (true, _) => Err(AssertErr::Leverage),
        (false, _) => Err(AssertErr::Isolated),
    }
}

/// **The `symbolConfig` entry naming `symbol`** (BX11, the account tool's
/// read): its margin type is `CROSSED` (`true`) or another, and its
/// leverage — under [`judge_symbol_config`]'s laws: no such entry is
/// `Unlisted`; one short of either field, a key an entry names twice, or a
/// symbol two entries name, `Unreadable`.
pub fn symbol_config(body: &[u8], symbol: &[u8]) -> Result<(bool, u64), AssertErr> {
    let mut e = Elems::new(body, 0).map_err(|()| AssertErr::Unreadable)?;
    let mut verdict = Err(AssertErr::Unlisted);
    let mut listed = false;
    while let Some((at, _)) = e.next().map_err(|()| AssertErr::Unreadable)? {
        let mut w = Pairs::new(body, at).map_err(|()| AssertErr::Unreadable)?;
        let (mut sym, mut margin, mut lev) = (None, None, None);
        // The keys read, a bit each: a second copy is refused.
        let mut seen = 0u8;
        while let Some((k, a, z)) = w.next().map_err(|()| AssertErr::Unreadable)? {
            let bit = match k {
                b"symbol" => {
                    sym = str_of(body, a, z).ok();
                    1
                }
                b"marginType" => {
                    margin = str_of(body, a, z).ok();
                    1 << 1
                }
                b"leverage" => {
                    lev = u64_of(body, a, z).ok();
                    1 << 2
                }
                _ => 0,
            };
            if seen & bit != 0 {
                return Err(AssertErr::Unreadable);
            }
            seen |= bit;
        }
        if sym != Some(symbol) {
            continue;
        }
        if listed {
            return Err(AssertErr::Unreadable);
        }
        listed = true;
        verdict = match (margin, lev) {
            (Some(m), Some(l)) => Ok((m == b"CROSSED", l)),
            _ => Err(AssertErr::Unreadable),
        };
    }
    verdict
}

/// `GET /sapi/v1/account/apiRestrictions` in `mode`: the key cannot
/// withdraw and trades what is armed — futures (`enableFutures`) when a
/// futures lane is, spot (`enableSpotAndMarginTrading`; BX8) when spot is,
/// options (`enableVanillaOptions`; BX9) when the key signs options — and in
/// Portfolio Margin it trades PM (`enablePortfolioMarginTrading`; BX7, C2).
pub fn judge_key(body: &[u8], mode: AccountMode, futures: bool, spot: bool, options: bool) -> Result<(), AssertErr> {
    if futures {
        judge_key_futures(body)?;
    } else {
        match json_bool(body, b"enableWithdrawals") {
            Some(false) => {}
            Some(true) => return Err(AssertErr::CanWithdraw),
            None => return Err(AssertErr::Unreadable),
        }
    }
    if spot {
        match json_bool(body, b"enableSpotAndMarginTrading") {
            Some(true) => {}
            Some(false) => return Err(AssertErr::NoTradePermission),
            None => return Err(AssertErr::Unreadable),
        }
    }
    if options {
        match json_bool(body, b"enableVanillaOptions") {
            Some(true) => {}
            Some(false) => return Err(AssertErr::NoTradePermission),
            None => return Err(AssertErr::Unreadable),
        }
    }
    if matches!(mode, AccountMode::Pm) {
        return match json_bool(body, b"enablePortfolioMarginTrading") {
            Some(true) => Ok(()),
            Some(false) => Err(AssertErr::NoTradePermission),
            None => Err(AssertErr::Unreadable),
        };
    }
    Ok(())
}

/// `GET /sapi/v1/account/apiRestrictions`: the key trades futures and
/// cannot withdraw.
pub fn judge_key_futures(body: &[u8]) -> Result<(), AssertErr> {
    match (
        json_bool(body, b"enableFutures"),
        json_bool(body, b"enableWithdrawals"),
    ) {
        (Some(true), Some(false)) => Ok(()),
        (Some(false), Some(_)) => Err(AssertErr::NoTradePermission),
        (Some(true), Some(true)) => Err(AssertErr::CanWithdraw),
        _ => Err(AssertErr::Unreadable),
    }
}

/// The permission names this build knows (`BN_PERM_*`'s name bits).
const PERM_KNOWN: u16 = BN_PERM_SPOT | BN_PERM_MARGIN | BN_PERM_GRID | BN_PERM_COPY | BN_PERM_DCA | BN_PERM_PSB | BN_PERM_RPI;

/// **BX8 — may the account trade the symbol** (the bStock check, BX-19):
/// the symbol's `permissionSets` (discovery) against the account's
/// `permissions` (`account.status`). The venue's rule: each inner set
/// needs ONE member the account holds — a name (`SPOT`, `MARGIN`, …) or a
/// `TRD_GRP_n`. Fail closed: a symbol with several sets (the digest is
/// their union, the venue ANDs them), or a name outside any set (no spot
/// shape: `BN_PERM_FLAT`, the parser review's P2-5), is refused; a member that only a
/// name this build does not know (`BN_PERM_OTHER`), or a group past the
/// bitset (`BN_PERM_GROUP_OVERFLOW`), could match is undecidable, and
/// refused: only a known name or a group in the bitset held by both
/// passes.
pub fn judge_permission(sym: &BnPermissions, acct: &BnPermissions) -> Result<(), AssertErr> {
    if sym.names & (BN_PERM_MULTI_SET | BN_PERM_FLAT) != 0 {
        return Err(AssertErr::SymbolNotPermitted);
    }
    if sym.names & acct.names & PERM_KNOWN != 0 {
        return Ok(());
    }
    let mut i = 0;
    while i < BN_PERM_GROUPS_MAX / 64 {
        if sym.groups[i] & acct.groups[i] != 0 {
            return Ok(());
        }
        i += 1;
    }
    Err(AssertErr::SymbolNotPermitted)
}

/// `GET /sapi/v1/portfolio/account` (K17): a classic account is refused
/// with `-21001`; a PM account answers `accountType` `PM_1` (PM Pro),
/// `PM_2` (PM) or `PM_3` (PM Pro SPAN). `status` is the HTTP status. Both
/// are the answer's own top-level fields, named once in a well-formed
/// object ([`field`]) and read whole (BX7): the code at most 18 digits,
/// never a prefix of a longer value.
pub fn judge_mode(status: u16, body: &[u8], want: AccountMode) -> Result<(), AssertErr> {
    let seen = if status == 200 {
        let ty = field(body, b"accountType").and_then(|(a, z)| str_of(body, a, z).ok());
        match ty {
            Some(b"PM_2") => AccountMode::Pm,
            Some(b"PM_1" | b"PM_3") => AccountMode::PmPro,
            _ => return Err(AssertErr::Unreadable),
        }
    } else if field(body, b"code").and_then(|(a, z)| i64_of(body, a, z).ok()) == Some(-21_001) {
        AccountMode::Classic
    } else {
        return Err(AssertErr::Unreadable);
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

    fn perms(names: &[&[u8]]) -> BnPermissions {
        let mut p = BnPermissions::EMPTY;
        for n in names {
            p.add(n);
        }
        p
    }

    /// BX8, the bStock check: the venue's rule — the account holds a member
    /// of the symbol's set — and fail closed past it (several sets; a
    /// member only an unknown name or a group past the bitset could match).
    #[test]
    fn the_permission_judge() {
        let refused = Err(AssertErr::SymbolNotPermitted);
        let bstock = perms(&[b"TRD_GRP_004", b"TRD_GRP_261"]);
        assert_eq!(judge_permission(&bstock, &perms(&[b"SPOT"])), refused, "a missing group");
        assert_eq!(judge_permission(&bstock, &perms(&[b"SPOT", b"TRD_GRP_261"])), Ok(()));
        let btc = perms(&[b"SPOT", b"MARGIN", b"TRD_GRP_004"]);
        assert_eq!(judge_permission(&btc, &perms(&[b"SPOT"])), Ok(()), "a name in common");
        assert_eq!(judge_permission(&btc, &perms(&[b"TRD_GRP_004"])), Ok(()), "a group in common");
        assert_eq!(judge_permission(&btc, &perms(&[b"TRD_GRP_005"])), refused);
        assert_eq!(judge_permission(&btc, &BnPermissions::EMPTY), refused, "an account with none");
        let d = crate::inst::tests::fapi_rows(crate::inst::tests::SPOT);
        let doge = d.find(b"DOGEUSDT").unwrap().perm;
        assert_ne!(doge.names & BN_PERM_MULTI_SET, 0, "two sets");
        assert_eq!(judge_permission(&doge, &perms(&[b"SPOT", b"TRD_GRP_002"])), refused, "the venue ANDs the sets; the digest cannot");
        assert_eq!(judge_permission(&perms(&[b"LEVERAGED"]), &perms(&[b"LEVERAGED"])), refused, "an unknown name, unseen");
        assert_eq!(judge_permission(&perms(&[b"TRD_GRP_600"]), &perms(&[b"TRD_GRP_600"])), refused, "past the bitset");
        assert_eq!(judge_permission(&perms(&[b"TRD_GRP_511"]), &perms(&[b"TRD_GRP_511"])), Ok(()), "the bitset's last group");
        // The venue's spelling (P2-6a): names compare as strings —
        // `TRD_GRP_4` is not `TRD_GRP_004`.
        assert_eq!(judge_permission(&perms(&[b"TRD_GRP_004"]), &perms(&[b"TRD_GRP_4"])), refused, "a spelling the venue never matches");
        assert_ne!(perms(&[b"TRD_GRP_0004"]).names & ingress_binance::discovery::BN_PERM_OTHER, 0, "a fourth digit led by zero: no group");
        assert_eq!(perms(&[b"TRD_GRP_04"]).has_group(4), Some(false), "two digits: no group");
        // A name outside any set (P2-5): no spot shape, refused.
        let mut flat = perms(&[b"SPOT"]);
        flat.names |= BN_PERM_FLAT;
        assert_eq!(judge_permission(&flat, &perms(&[b"SPOT"])), refused, "a flat list");
    }

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
    fn the_classic_futures_are_built() {
        assert_eq!(built(PRODUCT_USDM, AccountMode::Classic), Ok(()));
        assert_eq!(built(PRODUCT_COINM, AccountMode::Classic), Ok(()), "BX7: COIN-M classic");
        assert_eq!(built(PRODUCT_USDM, AccountMode::Pm), Ok(()), "BX7: Portfolio Margin");
        assert_eq!(built(PRODUCT_COINM, AccountMode::Pm), Ok(()));
        assert_eq!(built(PRODUCT_USDM, AccountMode::PmPro), Ok(()), "BX7: PM Pro");
        assert_eq!(built(PRODUCT_COINM, AccountMode::PmPro), Ok(()));
        assert!(!maker_ok(PRODUCT_USDM, AccountMode::PmPro), "K14: no dead-man proven under PM Pro");
        assert!(!maker_ok(PRODUCT_USDM, AccountMode::Pm), "K13: no dead-man in PM");
        assert_eq!(built(PRODUCT_SPOT, AccountMode::Classic), Ok(()), "BX8: spot classic");
        assert!(built(PRODUCT_SPOT, AccountMode::Pm).unwrap_err().starts_with("a ruling"));
        assert!(built(PRODUCT_SPOT, AccountMode::PmPro).unwrap_err().starts_with("a ruling"));
        assert!(!maker_ok(PRODUCT_SPOT, AccountMode::Classic), "BX-17: spot has no dead-man");
        assert_eq!(built(PRODUCT_OPTIONS, AccountMode::Classic), Ok(()), "BX9: options classic");
        assert!(built(PRODUCT_OPTIONS, AccountMode::Pm).unwrap_err().starts_with("a ruling"));
        assert!(built(PRODUCT_OPTIONS, AccountMode::PmPro).unwrap_err().starts_with("a ruling"));
        assert!(!maker_ok(PRODUCT_OPTIONS, AccountMode::Classic), "BX-17: options have no dead-man of ours (-6005)");
        // BX10: the gateway speaks Binance Stocks on a classic account (the
        // engine refuses it apart: S-3).
        assert_eq!(built(PRODUCT_EQUITY, AccountMode::Classic), Ok(()), "BX10: Binance Stocks classic");
        assert!(built(PRODUCT_EQUITY, AccountMode::Pm).unwrap_err().starts_with("a ruling"));
        assert!(built(PRODUCT_EQUITY, AccountMode::PmPro).unwrap_err().starts_with("a ruling"));
        assert!(!maker_ok(PRODUCT_EQUITY, AccountMode::Classic), "BX-17: Stocks has no dead-man (DAY under the TTL)");
        // O-BX3a: no maker rests on COIN-M (no dead-man of ours there yet).
        assert!(maker_ok(PRODUCT_USDM, AccountMode::Classic));
        assert!(!maker_ok(PRODUCT_COINM, AccountMode::Classic));
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

    /// BX7 (BX-19): cross margin and the leverage cap, per owned row.
    #[test]
    fn the_symbol_config_judge_wants_cross_under_the_cap() {
        let ok = br#"[{"symbol":"BTCUSDT","marginType":"CROSSED","isAutoAddMargin":false,"leverage":20,"maxNotionalValue":"1000000"}]"#;
        assert_eq!(judge_symbol_config(ok, b"BTCUSDT", 20), Ok(()));
        assert_eq!(judge_symbol_config(ok, b"BTCUSDT", 19), Err(AssertErr::Leverage));
        let iso = br#"[{"symbol":"BTCUSDT","marginType":"ISOLATED","leverage":"5"}]"#;
        assert_eq!(judge_symbol_config(iso, b"BTCUSDT", 20), Err(AssertErr::Isolated));
        let odd = br#"[{"symbol":"BTCUSDT","marginType":"PORTFOLIO","leverage":5}]"#;
        assert_eq!(judge_symbol_config(odd, b"BTCUSDT", 20), Err(AssertErr::Isolated), "an unknown type is not cross");
        // Another symbol's entry is not the row's; none at all is refused.
        let other = br#"[{"symbol":"ETHUSDT","marginType":"CROSSED","leverage":5},{"symbol":"BTCUSDT","marginType":"CROSSED","leverage":3}]"#;
        assert_eq!(judge_symbol_config(other, b"BTCUSDT", 3), Ok(()));
        assert_eq!(judge_symbol_config(other, b"XRPUSDT", 20), Err(AssertErr::Unlisted));
        assert_eq!(judge_symbol_config(b"[]", b"BTCUSDT", 20), Err(AssertErr::Unlisted));
        let short = br#"[{"symbol":"BTCUSDT","marginType":"CROSSED"}]"#;
        assert_eq!(judge_symbol_config(short, b"BTCUSDT", 20), Err(AssertErr::Unreadable));
        assert_eq!(judge_symbol_config(br#"{"code":-1121,"msg":"Invalid symbol."}"#, b"BTCUSDT", 20), Err(AssertErr::Unreadable));
        // BX7 (P9): no leverage is not one; the row named twice — or a key
        // named twice — is refused, whichever copy says what.
        let zero = br#"[{"symbol":"BTCUSDT","marginType":"CROSSED","leverage":0}]"#;
        assert_eq!(judge_symbol_config(zero, b"BTCUSDT", 20), Err(AssertErr::Unreadable));
        let twice = br#"[{"symbol":"BTCUSDT","marginType":"CROSSED","leverage":5},{"symbol":"BTCUSDT","marginType":"ISOLATED","leverage":125}]"#;
        assert_eq!(judge_symbol_config(twice, b"BTCUSDT", 20), Err(AssertErr::Unreadable));
        let key_twice = br#"[{"symbol":"BTCUSDT","marginType":"ISOLATED","marginType":"CROSSED","leverage":5}]"#;
        assert_eq!(judge_symbol_config(key_twice, b"BTCUSDT", 20), Err(AssertErr::Unreadable));
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
        // BX7 (P2): the code read whole — never a numeral that wraps to
        // -21001, a prefix of a longer value, or one past i64 (which used
        // to overflow the negation) — and the type the answer's own, once.
        for body in [
            &br#"{"code":-18446744073709572617}"#[..],
            &br#"{"code":-9223372036854775808}"#[..],
            &br#"{"code":-21001x}"#[..],
            &br#"{"code":-21001.9}"#[..],
            &br#"{"x":{"code":-21001}}"#[..],
            &br#"{"code":-1,"code":-21001}"#[..],
        ] {
            assert_eq!(judge_mode(400, body, AccountMode::Classic), Err(AssertErr::Unreadable), "{}", std::str::from_utf8(body).unwrap());
        }
        let nested = br#"{"x":{"accountType":"PM_2"},"accountType":"PM_1"}"#;
        assert_eq!(judge_mode(200, nested, AccountMode::Pm), Err(AssertErr::Mode), "the answer's own type, PM_1");
        assert_eq!(judge_mode(200, br#"{"accountType":"PM_2","accountType":"PM_1"}"#, AccountMode::Pm), Err(AssertErr::Unreadable));
        // The judges' booleans: the answer's own, once.
        let dup = br#"{"enableWithdrawals":false,"enableFutures":true,"enableWithdrawals":true}"#;
        assert_eq!(judge_key_futures(dup), Err(AssertErr::Unreadable));
        // BX8: the key trades what is armed — spot's own flag, futures'
        // only when a futures lane is; never one that can withdraw.
        let both = br#"{"enableWithdrawals":false,"enableFutures":true,"enableSpotAndMarginTrading":true}"#;
        let spot_only = br#"{"enableWithdrawals":false,"enableFutures":false,"enableSpotAndMarginTrading":true}"#;
        let fut_only = br#"{"enableWithdrawals":false,"enableFutures":true,"enableSpotAndMarginTrading":false}"#;
        let c = AccountMode::Classic;
        assert_eq!(judge_key(both, c, true, true, false), Ok(()));
        assert_eq!(judge_key(spot_only, c, false, true, false), Ok(()));
        assert_eq!(judge_key(spot_only, c, true, true, false), Err(AssertErr::NoTradePermission));
        assert_eq!(judge_key(fut_only, c, true, true, false), Err(AssertErr::NoTradePermission));
        assert_eq!(judge_key(fut_only, c, true, false, false), Ok(()));
        let w = br#"{"enableWithdrawals":true,"enableSpotAndMarginTrading":true}"#;
        assert_eq!(judge_key(w, c, false, true, false), Err(AssertErr::CanWithdraw));
        assert_eq!(judge_key(br#"{"enableWithdrawals":false}"#, c, false, true, false), Err(AssertErr::Unreadable));
        // BX9: the options key trades options and cannot withdraw.
        let opt = br#"{"enableWithdrawals":false,"enableVanillaOptions":true}"#;
        assert_eq!(judge_key(opt, c, false, false, true), Ok(()));
        assert_eq!(judge_key(fut_only, c, false, false, true), Err(AssertErr::Unreadable), "no options flag: unread");
        assert_eq!(judge_key(br#"{"enableWithdrawals":false,"enableVanillaOptions":false}"#, c, false, false, true), Err(AssertErr::NoTradePermission));
        assert_eq!(judge_key(br#"{"enableWithdrawals":true,"enableVanillaOptions":true}"#, c, false, false, true), Err(AssertErr::CanWithdraw));
        assert_eq!(judge_one_way(br#"{"x":{"dualSidePosition":false}}"#), Err(AssertErr::Unreadable), "a nested key is not the answer's");
        // BX9 (the parser review): the answer is the object, whole.
        assert_eq!(judge_one_way(b"{\"dualSidePosition\":false} \n"), Ok(()));
        assert_eq!(judge_one_way(br#"{"dualSidePosition":false} {"dualSidePosition":true}"#), Err(AssertErr::Unreadable));
        assert_eq!(judge_key(br#"{"enableWithdrawals":false,"enableVanillaOptions":true}]"#, c, false, false, true), Err(AssertErr::Unreadable));
    }
}
