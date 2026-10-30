// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §4.3.9: `PineCtx`, the member's `BarCtx` — the one `dyn` (`&mut dyn BarCtx` at
//! `step_bar` / `step_tick`). Built on the stack per evaluation from shared borrows and an `i64`;
//! it never escapes. The same shape as pine-screen's `ScreenCtx` (D23): an unknown key, or an
//! output past `n_out`, is `None` (`pine-std` reads `na`).
//!
//! Hot module (I4): no allocation, no clock (`now_ms` is the event time it was handed).

use crate::series::SeriesStore;
use pine_broker::NativeBroker;
use pine_ir::{BarCtx, BrokerView, SharedSeries, TradeRec};

/// One evaluation's context.
pub(crate) struct PineCtx<'a> {
    /// Every sub-program instance's series.
    pub(crate) series: &'a SeriesStore,
    /// The evaluating program's own `key → instance` table.
    pub(crate) subs: &'a [(u64, u8)],
    /// The script's broker; `None` for a sub-program instance (§4.3.8).
    pub(crate) broker: Option<&'a NativeBroker>,
    /// The event time, UTC ms (history: the bar's `close_ms`).
    pub(crate) now_ms: i64,
}

impl BarCtx for PineCtx<'_> {
    #[inline]
    fn shared_series(&self, key: u64, out_idx: u16) -> Option<SharedSeries<'_>> {
        let mut i = 0usize;
        while i < self.subs.len() {
            if self.subs[i].0 == key {
                return self
                    .series
                    .view(usize::from(self.subs[i].1), usize::from(out_idx));
            }
            i += 1;
        }
        None
    }

    #[inline]
    fn broker(&self) -> Option<&BrokerView> {
        Some(self.broker?.view())
    }

    #[inline]
    fn open_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.open_trade(i)
    }

    #[inline]
    fn closed_trade(&self, i: u32) -> Option<&TradeRec> {
        self.broker?.closed_trade(i)
    }

    #[inline]
    fn now_ms(&self) -> i64 {
        self.now_ms
    }
}
