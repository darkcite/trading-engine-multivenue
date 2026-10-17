// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The exit-order ids TradingView gives the exits the broker generates itself (impl-05 §4.6.2,
//! CI-032).
//!
//! `pin:05-exit-signal-text` — a closed trade's `exit_id` is the key of the exit ORDER's id, and
//! TradingView names the orders it generates: its List of Trades "Signal" column shows that id
//! (the E fixtures' trades bodies, 2026-10-17 capture). Collected over every E fixture:
//!
//! | cause | TradingView's exit id | fixtures |
//! |---|---|---|
//! | `strategy.close_all` | `Close position order` | E008 E058–E060 E062 E098 E105 E117 + every last-bar flatten |
//! | `strategy.close(id)` | `Close entry(s) order <id>` | E001–E004 E007 E013–E023 E030–E034 … E115 |
//! | margin call | `Margin call` | E071 E072 |
//! | `strategy.risk.max_intraday_loss` | `Close Position (Max intraday Loss)` | E092 |
//! | `strategy.risk.max_cons_loss_days` | `Close Position (Max consecutive loss days)` | E094 |
//! | `strategy.risk.max_intraday_filled_orders` | `Close Position (Max number of filled orders in one day)` | E093 |
//! | `strategy.exit` / reversal by `strategy.entry` | the exit's / the new entry's own id | E009 E057 E058 E121 |
//!
//! `strategy.risk.max_drawdown` tripped in no capture (E091 never flattens on TradingView), so its
//! text is unknown: [`RISK_MAX_DRAWDOWN`] stays `NONE` until a capture shows it (no invented text).
//! `strategy.close(id)`'s id is `"Close entry(s) order " ++ id`: the broker holds only the entry
//! id's FNV key, never its bytes, so it records the `Close` intent's own `id` — the emitter keys it
//! as [`close_entry_key`] does (request to pine-std, CI-032).

use pine_ir::{fnv1a64, fnv1a64_continue, str_key, StrKey};

/// `strategy.close_all`.
pub(crate) const CLOSE_ALL: StrKey = str_key(b"Close position order");
/// A margin call (§4.9.2, `trade_flag::LIQUIDATION`).
pub(crate) const MARGIN_CALL: StrKey = str_key(b"Margin call");
/// `strategy.risk.max_intraday_loss` (§4.10, `trade_flag::RISK_FLATTEN`).
pub(crate) const RISK_INTRADAY_LOSS: StrKey = str_key(b"Close Position (Max intraday Loss)");
/// `strategy.risk.max_cons_loss_days`.
pub(crate) const RISK_CONS_LOSS_DAYS: StrKey =
    str_key(b"Close Position (Max consecutive loss days)");
/// `strategy.risk.max_intraday_filled_orders`.
pub(crate) const RISK_INTRADAY_ORDERS: StrKey =
    str_key(b"Close Position (Max number of filled orders in one day)");
/// `strategy.risk.max_drawdown`: uncaptured, `NONE` (see the module doc).
pub(crate) const RISK_MAX_DRAWDOWN: StrKey = StrKey::NONE;

/// `strategy.close(id)`'s exit id prefix.
const CLOSE_ENTRY_PREFIX: &[u8] = b"Close entry(s) order ";

/// The key of `"Close entry(s) order " ++ id` without joining the bytes (FNV-1a continues over
/// parts; `str_key`'s zero → 1 rule kept). What a `Close` intent's `id` must carry.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const fn close_entry_key(id: &[u8]) -> StrKey {
    let h = fnv1a64_continue(fnv1a64(CLOSE_ENTRY_PREFIX), id);
    StrKey(if h == 0 { 1 } else { h })
}

#[cfg(test)]
mod tests {
    use super::{close_entry_key, CLOSE_ALL, MARGIN_CALL};
    use pine_ir::str_key;

    #[test]
    fn exit_ids_are_tradingview_texts() {
        // CI-032: the keys match the texts the List of Trades prints (E008, E071, E001).
        assert_eq!(CLOSE_ALL, str_key(b"Close position order"));
        assert_eq!(MARGIN_CALL, str_key(b"Margin call"));
        assert_eq!(close_entry_key(b"L1"), str_key(b"Close entry(s) order L1"));
        assert_ne!(close_entry_key(b"L1"), str_key(b"L1"));
    }
}
