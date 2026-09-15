// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What a strategy slot is allowed to do with an order it emits.
//!
//! Three states, one byte, and the DEFAULT IS THE SAFE ONE. Every path
//! that cannot answer the question — an absent artifact, an absent
//! slot, `STRATEGY_ID_NONE`, an out-of-range id, a byte the parser
//! never wrote — resolves to [`ExecMode::Paper`]. That is the whole
//! fail-closed law of E1 in one sentence: *nothing reaches a venue
//! unless the artifact said so, in range, on purpose.*

/// Execution mode for one strategy slot.
///
/// `#[repr(u8)]` because the discriminant is what the route table
/// stores and what the `/state` gauge publishes; the numbers are part
/// of the operator-visible contract (`engine_exec_slot<N>_mode`) and
/// must not be reordered.
#[repr(u8)]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum ExecMode {
    /// The slot's orders go to the paper matcher. **The default**, and
    /// what every slot is until an artifact says otherwise.
    #[default]
    Paper = 0,
    /// The slot's orders go to the live venue arm. Reachable only
    /// through an artifact AND `--arm-live` agreeing exactly.
    Live = 1,
    /// The slot may not trade at all — every submit is refused and
    /// counted. Not the same as Paper: `Off` is an operator saying
    /// "stop", Paper is an operator saying "model it".
    Off = 2,
}

impl ExecMode {
    /// Decode a stored byte. **Anything that is not a known
    /// discriminant is [`ExecMode::Paper`]** — a torn or
    /// never-written byte must not be able to mean `Live`.
    #[inline(always)]
    #[must_use]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => ExecMode::Live,
            2 => ExecMode::Off,
            _ => ExecMode::Paper,
        }
    }

    /// The stored byte.
    #[inline(always)]
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The operator-facing word, for boot tells and parse errors.
    #[inline]
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ExecMode::Paper => "paper",
            ExecMode::Live => "live",
            ExecMode::Off => "off",
        }
    }

    /// Parse the artifact's spelling. `None` on anything else — the
    /// caller turns that into a boot refusal naming the line, never a
    /// silent default (an operator who typed `liv` meant `live`, and
    /// booting them into paper would be a lie).
    #[inline]
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "paper" => Some(ExecMode::Paper),
            "live" => Some(ExecMode::Live),
            "off" => Some(ExecMode::Off),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_bytes_decode_to_paper() {
        assert_eq!(ExecMode::from_u8(0), ExecMode::Paper);
        assert_eq!(ExecMode::from_u8(1), ExecMode::Live);
        assert_eq!(ExecMode::from_u8(2), ExecMode::Off);
        // Every other byte in the whole domain fails closed.
        for b in 3u8..=255 {
            assert_eq!(
                ExecMode::from_u8(b),
                ExecMode::Paper,
                "byte {b} must decode to Paper"
            );
        }
    }

    #[test]
    fn default_is_paper() {
        assert_eq!(ExecMode::default(), ExecMode::Paper);
        assert_eq!(ExecMode::default().as_u8(), 0);
    }

    #[test]
    fn round_trips_through_the_stored_byte() {
        for m in [ExecMode::Paper, ExecMode::Live, ExecMode::Off] {
            assert_eq!(ExecMode::from_u8(m.as_u8()), m);
        }
    }

    #[test]
    fn parse_accepts_only_the_three_spellings() {
        assert_eq!(ExecMode::parse("paper"), Some(ExecMode::Paper));
        assert_eq!(ExecMode::parse("live"), Some(ExecMode::Live));
        assert_eq!(ExecMode::parse("off"), Some(ExecMode::Off));
        for bad in ["", "Live", "LIVE", "liv", "real", "0", "1", " live"] {
            assert_eq!(ExecMode::parse(bad), None, "`{bad}` must not parse");
        }
    }

    #[test]
    fn words_match_the_artifact_spelling() {
        for m in [ExecMode::Paper, ExecMode::Live, ExecMode::Off] {
            assert_eq!(ExecMode::parse(m.as_str()), Some(m));
        }
    }
}
