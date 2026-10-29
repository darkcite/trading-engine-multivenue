// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # clob_law — a rolling window's venue law, read from the CLOB (PM15-9b)
//!
//! Gamma ([`crate::gamma15`]) is the venue's catalogue, not its matching
//! engine: its `orderPriceMinTickSize` has lagged the book's live tick
//! (the plan's G2 lesson), and it says nothing of the crypto taker hold.
//! So every window Gamma admits is read once more, from the CLOB's own
//! market record, `GET https://clob.polymarket.com/clob-markets/{conditionId}`
//! — by the discovery thread ([`crate::pm15_discovery`]), per window,
//! the boot's first windows included.
//!
//! ## Wire shape (live 2026-10-29)
//!
//! ```json
//! {"r":{"mi":50,"ma":4.5,"moas":30},
//!  "t":[{"t":"9980…","o":"Up"},{"t":"4937…","o":"Down"}],
//!  "c":"0x232a…7fe6","mos":5,"mts":0.01,"mbf":1000,"tbf":1000,
//!  "ao":true,"aot":"2026-10-28T09:52:49Z","itode":true,
//!  "fd":{"r":0.07,"e":1,"to":true},"v":"v1"}
//! ```
//!
//! One object, nothing but whitespace after it; key order is not
//! assumed; every key the law does not read (`r`, `t`, `mbf`, `tbf`,
//! `aot`, `v`, anything new) is skipped structurally
//! ([`core_parse::skip_json_value`]). A read field whose value is `null`
//! counts as absent (so a market without the hold or the fee is refused
//! for THAT, not for its shape). Numbers are read as Gamma's are
//! ([`core_parse::scan_number_sci_1e9`]). [`parse_clob_market`] fills a
//! [`PmClobLaw`] in place — written once, untouched on `false`.
//!
//! ## The law ([`check_clob_law`])
//!
//! The market H90 was measured on, as the CLOB states it:
//!
//! | field | wanted | refusal |
//! |---|---|---|
//! | `c` | the condition id asked for (hex, any case) | [`PmLawErr::ClobShape`] |
//! | `itode` | present and `true`: the 150 ms crypto taker hold (since 2026-09-04) | [`PmLawErr::ClobHold`] |
//! | `fd` | `r` 0.07, `e` 1, `to` true (taker only) | [`PmLawErr::ClobFee`] |
//! | `mts` | 0.01 or 0.001 (the ladder's two grids) | [`PmLawErr::ClobTick`] |
//! | `mos` | > 0 | [`PmLawErr::ClobTick`] |
//! | `ao` | `true` (the book accepts orders) | [`PmLawErr::ClobShape`] |
//!
//! A record that does not parse — a 200 whose body is not one such
//! object, e.g. a CDN error page — is [`PmLawErr::ClobShape`] too (the
//! discovery's call). On a pass the spec binds the venue's own record:
//! the tick is the CLOB's `mts` and the minimum order size the CLOB's
//! `mos`, each OVERRIDING Gamma's `orderPriceMinTickSize` /
//! `orderMinSize` whenever the two differ — the CLOB is the venue that
//! matches the order; Gamma's catalogue lags (the plan's G2 lesson).
//!
//! ## What a refusal does (the refusal, not a halt)
//!
//! The discovery hands a window to the ingress only once BOTH laws pass.
//! A window the CLOB law refuses is never bound: no roll is created for
//! it, so the member (slot 9 or slot 11) cannot trade that family in that
//! window. Nothing else stops: the dormancy is per family PER WINDOW, the
//! other families trade on, and the family resumes by itself — its
//! window is re-read every [`crate::pm15_discovery::RETRY_LAW_NS`] and
//! bound as soon as the law holds again. Each refusal is counted
//! `discovery_law_refused` with its cause on `last_law_refusal`; when the
//! family had passed the CLOB law earlier in the session the refusal is
//! also a CHANGE: counted `venue_law_changed` once per change, its cause
//! on `last_law_change`, with one operator line (an unparsable 200 that
//! follows a pass counts as a change too). Windows handed over BEFORE the
//! change keep their binding: at most the look-ahead beyond the current
//! window — one for pm15, two for pm5.
//!
//! Cold path (the discovery thread, once per window): no allocation and
//! no copy — the law is a fixed POD of scalars, the condition id is
//! compared in place.

use core::fmt;

use core_parse::{scan_number_sci_1e9, skip_json_value, skip_ws};

use crate::discovery::{bare_bool, next_member, open_object, quoted_span};
use crate::gamma15::{PmLawErr, PM15_FEE_RATE_1E9, TICK_001_1E9, TICK_01_1E9};

/// The fee exponent the H90 law was measured under, ×1e9 (1).
const FEE_EXPONENT_1E9: i64 = 1_000_000_000;

/// The CLOB's market record, as far as the law reads it (module docs).
/// Absent fields read 0 / `false`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PmClobLaw {
    /// `fd.r` — the fee rate, ×1e9.
    pub fee_rate_1e9: i64,
    /// `fd.e` — the fee exponent, ×1e9.
    pub fee_exponent_1e9: i64,
    /// `mts` — the book's minimum tick, ×1e9.
    pub mts_1e9: i64,
    /// `mos` — the minimum order size, shares ×1e6.
    pub mos_1e6: i64,
    /// `c` equals the condition id asked for.
    pub c_matches: bool,
    /// An `itode` boolean was present.
    pub itode_seen: bool,
    /// `itode` — the crypto taker hold is on.
    pub itode: bool,
    /// An `fd` object was present.
    pub fd_seen: bool,
    /// `fd.to` — the fee is charged to takers only.
    pub fee_taker_only: bool,
    /// `ao` — the book accepts orders.
    pub ao: bool,
    /// Explicit padding — always zero.
    pub _pad: [u8; 2],
}

const _: () = assert!(core::mem::size_of::<PmClobLaw>() == 40);

impl PmClobLaw {
    /// Nothing read.
    pub const EMPTY: Self = Self {
        fee_rate_1e9: 0,
        fee_exponent_1e9: 0,
        mts_1e9: 0,
        mos_1e6: 0,
        c_matches: false,
        itode_seen: false,
        itode: false,
        fd_seen: false,
        fee_taker_only: false,
        ao: false,
        _pad: [0; 2],
    };
}

impl Default for PmClobLaw {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// The operator's rendering (the discovery's boot tell and its alert):
/// `hold=itode fee=0.07/1/taker tick=0.01 min=5 accepting=yes`. Writes
/// into the caller's formatter — no allocation.
impl fmt::Display for PmClobLaw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hold = match (self.itode_seen, self.itode) {
            (true, true) => "itode",
            (true, false) => "none",
            (false, _) => "absent",
        };
        write!(f, "hold={hold} ")?;
        if self.fd_seen {
            let who = if self.fee_taker_only { "taker" } else { "maker+taker" };
            write!(
                f,
                "fee={}/{}/{who} ",
                Fixed(self.fee_rate_1e9, 9),
                Fixed(self.fee_exponent_1e9, 9)
            )?;
        } else {
            f.write_str("fee=absent ")?;
        }
        write!(
            f,
            "tick={} min={} accepting={}",
            Fixed(self.mts_1e9, 9),
            Fixed(self.mos_1e6, 6),
            if self.ao { "yes" } else { "no" }
        )?;
        if !self.c_matches {
            f.write_str(" market=OTHER")?;
        }
        Ok(())
    }
}

/// A fixed-point value `.0` with `.1` decimals, rendered with its
/// trailing zeros trimmed (`70_000_000, 9` → `0.07`).
struct Fixed(i64, u32);

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = 10u64.pow(self.1.min(18));
        let a = self.0.unsigned_abs();
        if self.0 < 0 {
            f.write_str("-")?;
        }
        write!(f, "{}", a / scale)?;
        let mut frac = a % scale;
        if frac == 0 {
            return Ok(());
        }
        let mut w = self.1.min(18) as usize;
        while frac % 10 == 0 {
            frac /= 10;
            w -= 1;
        }
        write!(f, ".{frac:0w$}")
    }
}

/// Parse one `/clob-markets/{conditionId}` answer into `out`, `cid` being
/// the condition id that was asked for (module docs). `true` on a parsed
/// object — `out` is written once then, and untouched on `false`.
pub fn parse_clob_market(body: &[u8], cid: &[u8], out: &mut PmClobLaw) -> bool {
    match parse_object(body, cid) {
        Some(law) => {
            *out = law;
            true
        }
        None => false,
    }
}

/// Check a parsed record against the law H90 was measured under (module
/// docs); on a pass, the tick to bind (`mts` ×1e9 — it overrides Gamma's
/// whenever the two differ, as `mos` overrides Gamma's minimum size).
///
/// # Errors
/// [`PmLawErr::ClobShape`] / [`PmLawErr::ClobHold`] /
/// [`PmLawErr::ClobFee`] / [`PmLawErr::ClobTick`] — the first check that
/// failed, in the module table's order.
pub fn check_clob_law(law: &PmClobLaw) -> Result<i64, PmLawErr> {
    if !law.c_matches {
        return Err(PmLawErr::ClobShape);
    }
    if !law.itode_seen || !law.itode {
        return Err(PmLawErr::ClobHold);
    }
    if !law.fd_seen
        || law.fee_rate_1e9 != PM15_FEE_RATE_1E9
        || law.fee_exponent_1e9 != FEE_EXPONENT_1E9
        || !law.fee_taker_only
    {
        return Err(PmLawErr::ClobFee);
    }
    if (law.mts_1e9 != TICK_01_1E9 && law.mts_1e9 != TICK_001_1E9) || law.mos_1e6 <= 0 {
        return Err(PmLawErr::ClobTick);
    }
    if !law.ao {
        return Err(PmLawErr::ClobShape);
    }
    // The venue's live tick, whatever Gamma's says.
    Ok(law.mts_1e9)
}

/// `null` at `i` (a read field's value): the field is absent.
#[inline]
fn is_null(body: &[u8], i: usize) -> bool {
    body.get(i..i + 4) == Some(b"null".as_slice())
}

/// The record's one object, or `None`.
fn parse_object(body: &[u8], cid: &[u8]) -> Option<PmClobLaw> {
    let mut i = skip_ws(body, 0);
    if body.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut law = PmClobLaw::EMPTY;
    while let Some(key) = next_member(body, &mut i).ok()? {
        if is_null(body, i) {
            i += 4;
            continue;
        }
        match key {
            b"c" => {
                let (s, end) = quoted_span(body, i).ok()?;
                law.c_matches = !cid.is_empty() && s.eq_ignore_ascii_case(cid);
                i = end;
            }
            b"itode" => {
                let (v, end) = bare_bool(body, i).ok()?;
                law.itode_seen = true;
                law.itode = v;
                i = end;
            }
            b"ao" => {
                let (v, end) = bare_bool(body, i).ok()?;
                law.ao = v;
                i = end;
            }
            b"mts" => {
                let (v, end) = scan_number_sci_1e9(body, i)?;
                law.mts_1e9 = v;
                i = end;
            }
            b"mos" => {
                let (v, end) = scan_number_sci_1e9(body, i)?;
                law.mos_1e6 = v / 1_000;
                i = end;
            }
            b"fd" => {
                i = parse_fee(body, open_object(body, i).ok()?, &mut law)?;
            }
            _ => i = skip_json_value(body, i)?,
        }
    }
    // One object, nothing after it.
    if skip_ws(body, i) != body.len() {
        return None;
    }
    Some(law)
}

/// The members of the `fd` object (after its `{`); the position after
/// its `}`.
fn parse_fee(body: &[u8], mut i: usize, law: &mut PmClobLaw) -> Option<usize> {
    law.fd_seen = true;
    while let Some(key) = next_member(body, &mut i).ok()? {
        if is_null(body, i) {
            i += 4;
            continue;
        }
        match key {
            b"r" => {
                let (v, end) = scan_number_sci_1e9(body, i)?;
                law.fee_rate_1e9 = v;
                i = end;
            }
            b"e" => {
                let (v, end) = scan_number_sci_1e9(body, i)?;
                law.fee_exponent_1e9 = v;
                i = end;
            }
            b"to" => {
                let (v, end) = bare_bool(body, i).ok()?;
                law.fee_taker_only = v;
                i = end;
            }
            _ => i = skip_json_value(body, i)?,
        }
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The condition id of the live answer.
    const CID: &str = "0x232a3351a0f7775a834b8c4deb6d9fe09f11b96226937e5268ede62cae817fe6";

    /// The live 2026-10-29 answer's top-level members, in wire order. The
    /// two token ids were elided in the capture (`9980…`, `4937…`); they
    /// are padded to 77 digits here — the law never reads them.
    const MEMBERS: [&str; 12] = [
        r#""r":{"mi":50,"ma":4.5,"moas":30}"#,
        r#""t":[{"t":"99800000000000000000000000000000000000000000000000000000000000000000000000001","o":"Up"},{"t":"49370000000000000000000000000000000000000000000000000000000000000000000000002","o":"Down"}]"#,
        r#""c":"0x232a3351a0f7775a834b8c4deb6d9fe09f11b96226937e5268ede62cae817fe6""#,
        r#""mos":5"#,
        r#""mts":0.01"#,
        r#""mbf":1000"#,
        r#""tbf":1000"#,
        r#""ao":true"#,
        r#""aot":"2026-10-28T09:52:49Z""#,
        r#""itode":true"#,
        r#""fd":{"r":0.07,"e":1,"to":true}"#,
        r#""v":"v1""#,
    ];

    fn live() -> String {
        format!("{{{}}}", MEMBERS.join(","))
    }

    fn law_of(body: &str) -> Option<PmClobLaw> {
        let mut out = PmClobLaw::EMPTY;
        parse_clob_market(body.as_bytes(), CID.as_bytes(), &mut out).then_some(out)
    }

    fn check_of(body: &str) -> Result<i64, PmLawErr> {
        check_clob_law(&law_of(body).expect("parses"))
    }

    #[test]
    fn the_live_answer_passes_and_binds_its_tick() {
        let body = live();
        assert!(body.starts_with(r#"{"r":{"mi":50"#) && body.ends_with(r#""v":"v1"}"#));
        let law = law_of(&body).expect("the live answer parses");
        assert_eq!(
            law,
            PmClobLaw {
                fee_rate_1e9: 70_000_000,
                fee_exponent_1e9: 1_000_000_000,
                mts_1e9: 10_000_000,
                mos_1e6: 5_000_000,
                c_matches: true,
                itode_seen: true,
                itode: true,
                fd_seen: true,
                fee_taker_only: true,
                ao: true,
                _pad: [0; 2],
            }
        );
        assert_eq!(check_clob_law(&law), Ok(10_000_000));
        assert_eq!(
            law.to_string(),
            "hold=itode fee=0.07/1/taker tick=0.01 min=5 accepting=yes"
        );
        // Whitespace around and between the members is the wire's right.
        let spaced = format!(" \n{{ {} }}\r\n", MEMBERS.join(" ,\n "));
        assert_eq!(law_of(&spaced), Some(law));
        // The condition id is hex: its case does not matter.
        let upper = body.replacen(CID, &CID.to_ascii_uppercase().replacen("0X", "0x", 1), 1);
        assert_eq!(check_of(&upper), Ok(10_000_000));
    }

    /// The tick bound is the record's own `mts`, on either grid (it
    /// overrides Gamma's whenever they differ — G2), and the minimum is
    /// the record's own `mos`.
    #[test]
    fn the_clob_tick_and_minimum_are_the_records_own() {
        let fine = live()
            .replacen(r#""mts":0.01"#, r#""mts":0.001"#, 1)
            .replacen(r#""mos":5"#, r#""mos":2.5"#, 1);
        let law = law_of(&fine).unwrap();
        assert_eq!(check_clob_law(&law), Ok(TICK_001_1E9));
        assert_eq!(law.mos_1e6, 2_500_000);
        let coarse = law_of(&live()).unwrap();
        assert_eq!(check_clob_law(&coarse), Ok(TICK_01_1E9));
        assert_eq!(coarse.mos_1e6, 5_000_000);
    }

    #[test]
    fn every_breach_is_named() {
        let body = live();
        let cases = [
            (r#""itode":true"#, r#""itode":false"#, PmLawErr::ClobHold),
            (r#","itode":true"#, "", PmLawErr::ClobHold),
            (r#""itode":true"#, r#""itode":null"#, PmLawErr::ClobHold),
            (r#""r":0.07"#, r#""r":0.035"#, PmLawErr::ClobFee),
            (r#""e":1"#, r#""e":2"#, PmLawErr::ClobFee),
            (r#""to":true"#, r#""to":false"#, PmLawErr::ClobFee),
            (r#","fd":{"r":0.07,"e":1,"to":true}"#, "", PmLawErr::ClobFee),
            (r#""fd":{"r":0.07,"e":1,"to":true}"#, r#""fd":null"#, PmLawErr::ClobFee),
            (r#""r":0.07"#, r#""r":null"#, PmLawErr::ClobFee),
            (r#""mts":0.01"#, r#""mts":0.005"#, PmLawErr::ClobTick),
            (r#""mts":0.01"#, r#""mts":0.1"#, PmLawErr::ClobTick),
            (r#","mts":0.01"#, "", PmLawErr::ClobTick),
            (r#""mos":5"#, r#""mos":0"#, PmLawErr::ClobTick),
            (r#""mos":5"#, r#""mos":-5"#, PmLawErr::ClobTick),
            (r#""ao":true"#, r#""ao":false"#, PmLawErr::ClobShape),
            (r#","ao":true"#, "", PmLawErr::ClobShape),
            (CID, "0x0000000000000000000000000000000000000000000000000000000000000001", PmLawErr::ClobShape),
            (CID, "0x232a", PmLawErr::ClobShape),
        ];
        let mut k = 0usize;
        while k < cases.len() {
            let (from, to, want) = cases[k];
            assert_eq!(body.matches(from).count(), 1, "case {k}: `{from}` is unique");
            assert_eq!(check_of(&body.replacen(from, to, 1)), Err(want), "case {k}: {to}");
            k += 1;
        }
        // The first failing check names the refusal (the table's order).
        let two = body
            .replacen(r#""itode":true"#, r#""itode":false"#, 1)
            .replacen(r#""r":0.07"#, r#""r":0.035"#, 1);
        assert_eq!(check_of(&two), Err(PmLawErr::ClobHold));
        // An empty `cid` never matches.
        let mut out = PmClobLaw::EMPTY;
        assert!(parse_clob_market(body.as_bytes(), b"", &mut out));
        assert_eq!(check_clob_law(&out), Err(PmLawErr::ClobShape));
        assert!(out.to_string().ends_with(" market=OTHER"));
        // The renderings of a refused record name what was read.
        let off = law_of(
            &body
                .replacen(r#""itode":true"#, r#""itode":false"#, 1)
                .replacen(r#""to":true"#, r#""to":false"#, 1)
                .replacen(r#""r":0.07"#, r#""r":0.035"#, 1),
        )
        .unwrap();
        assert_eq!(
            off.to_string(),
            "hold=none fee=0.035/1/maker+taker tick=0.01 min=5 accepting=yes"
        );
        assert_eq!(
            PmClobLaw::default().to_string(),
            "hold=absent fee=absent tick=0 min=0 accepting=no market=OTHER"
        );
    }

    /// Anything but one well-formed object is no record: `out` untouched.
    #[test]
    fn a_body_that_is_not_one_object_is_refused_untouched() {
        let body = live();
        let sentinel = PmClobLaw {
            mts_1e9: 7,
            ..PmClobLaw::EMPTY
        };
        let bad = [
            String::new(),
            String::from("[]"),
            String::from("null"),
            String::from("{"),
            format!("[{body}]"),
            format!("{body}{body}"),
            format!("{body}x"),
            body.replacen(r#""ao":true"#, r#""ao":1"#, 1),
            body.replacen(r#""itode":true"#, r#""itode":"true""#, 1),
            body.replacen(r#""mts":0.01"#, r#""mts":"0.01""#, 1),
            body.replacen(r#""fd":{"#, r#""fd":["#, 1),
            body.replacen(r#""to":true}"#, r#""to":true]"#, 1),
            body.replacen(r#""ao":true"#, r#""ao":nul"#, 1),
        ];
        let mut k = 0usize;
        while k < bad.len() {
            let mut out = sentinel;
            assert!(
                !parse_clob_market(bad[k].as_bytes(), CID.as_bytes(), &mut out),
                "case {k}: {}",
                bad[k]
            );
            assert_eq!(out, sentinel, "case {k}: untouched");
            k += 1;
        }
        // Every truncation of the live answer: no panic, no record.
        let b = body.as_bytes();
        let mut n = 0usize;
        while n < b.len() {
            let mut out = sentinel;
            assert!(!parse_clob_market(&b[..n], CID.as_bytes(), &mut out), "prefix {n}");
            assert_eq!(out, sentinel);
            n += 1;
        }
    }

    #[test]
    fn the_fixed_point_rendering_trims_its_zeros() {
        assert_eq!(Fixed(70_000_000, 9).to_string(), "0.07");
        assert_eq!(Fixed(1_000_000_000, 9).to_string(), "1");
        assert_eq!(Fixed(1_000_000, 9).to_string(), "0.001");
        assert_eq!(Fixed(5_000_000, 6).to_string(), "5");
        assert_eq!(Fixed(5_500_001, 6).to_string(), "5.500001");
        assert_eq!(Fixed(-35_000_000, 9).to_string(), "-0.035");
        assert_eq!(Fixed(0, 9).to_string(), "0");
        assert_eq!(Fixed(i64::MIN, 9).to_string(), "-9223372036.854775808");
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    const CID: &[u8] = b"0x232a3351a0f7775a834b8c4deb6d9fe09f11b96226937e5268ede62cae817fe6";

    const MEMBERS: [&str; 12] = [
        r#""r":{"mi":50,"ma":4.5,"moas":30}"#,
        r#""t":[{"t":"9980","o":"Up"},{"t":"4937","o":"Down"}]"#,
        r#""c":"0x232a3351a0f7775a834b8c4deb6d9fe09f11b96226937e5268ede62cae817fe6""#,
        r#""mos":5"#,
        r#""mts":0.01"#,
        r#""mbf":1000"#,
        r#""tbf":1000"#,
        r#""ao":true"#,
        r#""aot":"2026-10-28T09:52:49Z""#,
        r#""itode":true"#,
        r#""fd":{"r":0.07,"e":1,"to":true}"#,
        r#""v":"v1""#,
    ];

    proptest! {
        /// Arbitrary bytes never panic the scanner or the check; a refusal
        /// leaves `out` untouched, and a record's check names a CLOB cause.
        #[test]
        fn arbitrary_bytes_never_panic(
            input in proptest::collection::vec(any::<u8>(), 0..1024),
            cid_len in 0usize..80,
        ) {
            let sentinel = PmClobLaw { mos_1e6: 3, ..PmClobLaw::EMPTY };
            let mut out = sentinel;
            let cid = &input[..cid_len.min(input.len())];
            if parse_clob_market(&input, cid, &mut out) {
                match check_clob_law(&out) {
                    Ok(t) => prop_assert!(t == 10_000_000 || t == 1_000_000),
                    Err(e) => prop_assert!(matches!(
                        e,
                        PmLawErr::ClobShape | PmLawErr::ClobHold | PmLawErr::ClobFee | PmLawErr::ClobTick
                    )),
                }
            } else {
                prop_assert_eq!(out, sentinel);
            }
            let _ = parse_clob_market(&input, CID, &mut out);
        }

        /// The live answer's members in ANY order are the same record.
        #[test]
        fn key_order_is_not_assumed(
            members in Just(MEMBERS.to_vec()).prop_shuffle(),
        ) {
            let canonical = format!("{{{}}}", MEMBERS.join(","));
            let shuffled = format!("{{{}}}", members.join(","));
            let mut a = PmClobLaw::EMPTY;
            let mut b = PmClobLaw::EMPTY;
            prop_assert!(parse_clob_market(canonical.as_bytes(), CID, &mut a));
            prop_assert!(parse_clob_market(shuffled.as_bytes(), CID, &mut b));
            prop_assert_eq!(a, b);
            prop_assert_eq!(check_clob_law(&b), Ok(10_000_000));
        }
    }
}
