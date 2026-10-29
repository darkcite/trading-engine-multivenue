// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # notify — the one-way Telegram notifier (NOTIFY lane, N-DOCTRINE)
//!
//! `multivenue-notify` is a separate process, one launchd job per engine
//! instance. It reads, and only reads:
//!
//! * `GET http://127.0.0.1:<port>/state` once per poll, and `/metrics` on
//!   the same port when the Hyperliquid gateway holds the session anchor
//!   and worth (which `/state` does not carry);
//! * the run's `engine-fills.pmlr`, `engine-orders.pmlr` and
//!   `engine-events.pmlr` (tailed by record index, never written);
//! * the `[hyperliquid] rolling = [...]` line of the instance's
//!   `universe.toml` (family names);
//! * its own `notify.toml` (absent = off) and the two `TELEGRAM_*` keys of
//!   the instance's `.env`;
//! * the AI-session inbox directories (`*.msg` files dropped by
//!   `multivenue-notify send` or `scripts/notify-send.sh`).
//!
//! It writes only its own state directory and its log, and talks to one
//! host off the machine: `api.telegram.org:443`. It never touches
//! `ai.sock`, `exec.HALT`, `exec.toml`, a venue API or a venue key, and
//! the engine never waits for it — its failure cannot affect trading.
//! Notify-only (R-NT-3): it never reads Telegram updates.
//!
//! COPY-DOCTRINE: an offline operator process, never linked into the
//! engine and never reached by the engine loop. It formats text and
//! copies freely; the zero-copy ratchet does not apply.
//!
//! ALLOCATION DOCTRINE: offline; it allocates freely (the house rule for
//! offline consumers: audit-replay, backtest, the operator tools).

pub mod capture;
pub mod clock;
pub mod config;
pub mod envfile;
pub mod fmt;
pub mod inbox;
pub mod json;
pub mod model;
pub mod names;
pub mod outbox;
pub mod persist;
pub mod run;
pub mod state;
pub mod telegram;
pub mod toml;

/// The capture file names the notifier tails (`engine::ENGINE_*_FILE`,
/// copied; pinned by a test).
pub const FILLS_FILE: &str = "engine-fills.pmlr";
/// See [`FILLS_FILE`].
pub const ORDERS_FILE: &str = "engine-orders.pmlr";
/// See [`FILLS_FILE`].
pub const EVENTS_FILE: &str = "engine-events.pmlr";

/// Telegram's per-message budget we keep to, in bytes of text. The house
/// HTTP client caps a whole request (head + body) at 4 096 bytes; the bot
/// token rides in the path, so the text stays well under that.
pub const MAX_TEXT_BYTES: usize = 3000;

/// Truncate `s` to at most `max` bytes on a char boundary, marking the cut.
#[must_use]
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub(4);
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = s[..end].to_string();
    out.push_str(" […]");
    out
}

#[cfg(test)]
mod pins {
    #[test]
    fn capture_file_names_match_the_engine() {
        assert_eq!(super::FILLS_FILE, engine::ENGINE_FILLS_FILE);
        assert_eq!(super::ORDERS_FILE, engine::ENGINE_ORDERS_FILE);
        assert_eq!(super::EVENTS_FILE, engine::ENGINE_EVENTS_FILE);
    }

    #[test]
    fn clip_keeps_short_text_and_cuts_on_a_char_boundary() {
        assert_eq!(super::clip("abc", 10), "abc");
        let s = "ééééééééééé"; // 2 bytes each
        let c = super::clip(s, 9);
        assert!(c.len() <= 9 + 4);
        assert!(c.ends_with(" […]"));
    }
}
