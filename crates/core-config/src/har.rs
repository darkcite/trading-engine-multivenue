// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `har.toml` — the HAR H3 series list (vault plan `har-h3-plan-2026-09-26.md`
//! §13.1).
//!
//! OPTIONAL: an absent file is the pre-H3 boot, bit for bit. When present
//! it names up to [`HAR_MAX_SERIES`] long-tenor HAR series, one
//! `[[series]]` block each:
//!
//! ```toml
//! [[series]]
//! name     = "BTC"                    # the Hypercall underlying: 1..=12 of [A-Z0-9]
//! feed     = "binance-usdm:btcusdt"   # the live minute source: a boot-universe descriptor
//! fallback = ["binance:btcusdt"]      # the SEED's older sources, newest first (worker only)
//! ```
//!
//! The engine reads `name` and `feed`; `fallback` is the worker's
//! (`claude_worker.har_backfill` fetches it, `claude_worker.har_seed`
//! splices it into the seed) and is checked here, never used — a file one
//! side refuses, the other refuses too. `claude_worker.har_config` is this
//! parser line for line, message for message.
//!
//! TOML subset in the `icdp.rs` / `hyparb.rs` style, sharing their
//! primitives (`strip_comment`, `parse_value`). **Laws** (all naming
//! `har.toml` and the line): an unknown section or key refuses · a key
//! before any `[[series]]` refuses · a duplicate key refuses · strings
//! carry no escapes · arrays are ONE line · `name` is 1..=12 bytes of
//! `[A-Z0-9]` · a descriptor is `<venue>:<instrument>` in at most
//! [`HAR_DESCRIPTOR_MAX`] bytes ([`valid_descriptor`]) · at most
//! [`HAR_MAX_FALLBACKS`] fallbacks, none repeating the feed or another ·
//! names unique, feeds unique · 1..=[`HAR_MAX_SERIES`] series.
//!
//! Descriptors stay strings here; resolving `feed` against the boot
//! universe is `har_boot`'s job (only the cli knows the manifest), and a
//! feed that does not resolve refuses the HAR service, never the boot.

use std::path::Path;

use crate::icdp::{parse_value, strip_comment, Value};

/// Series one file may configure: the Hypercall underlyings (O-HC5). The
/// engine boxes one `LongVolEngine` per series.
pub const HAR_MAX_SERIES: usize = 12;
/// Older sources one series may splice in front of its feed.
pub const HAR_MAX_FALLBACKS: usize = 4;
/// `name` is at most this many bytes.
pub const HAR_NAME_MAX: usize = 12;
/// A descriptor is at most this many bytes.
pub const HAR_DESCRIPTOR_MAX: usize = 64;
/// The keys a `[[series]]` block may carry (`fallback` OPTIONAL: absent =
/// no fallback). `claude_worker.har_config.SERIES_KEYS` mirrors it.
pub const HAR_KEYS: [&str; 3] = ["name", "feed", "fallback"];

/// One `[[series]]` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarSeries {
    /// The Hypercall underlying the series serves (`BTC`, `SP500`).
    pub name: String,
    /// The live minute source: a tick descriptor of the boot universe.
    pub feed: String,
    /// The seed's older sources, newest first (the worker's; checked here).
    pub fallback: Vec<String>,
}

/// The parsed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarFile {
    /// The series in file order.
    pub series: Vec<HarSeries>,
}

/// A `har.toml` that could not be read or did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarError(pub String);

impl std::fmt::Display for HarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "har.toml: {}", self.0)
    }
}

impl std::error::Error for HarError {}

impl From<crate::icdp::IcdpError> for HarError {
    fn from(e: crate::icdp::IcdpError) -> Self {
        Self(e.0)
    }
}

fn err(msg: impl Into<String>) -> HarError {
    HarError(msg.into())
}

/// Default location beside `universe.toml`.
pub fn default_har_path() -> Result<String, super::ConfigError> {
    super::expand_tilde("~/multivenue/har.toml")
}

/// Read + parse. Returns the file bytes too so the caller can hash the
/// EXACT file it booted with.
pub fn load(path: &Path) -> Result<(HarFile, Vec<u8>), HarError> {
    let bytes = std::fs::read(path).map_err(|e| err(format!("{}: {e}", path.display())))?;
    let src =
        std::str::from_utf8(&bytes).map_err(|_| err(format!("{}: not UTF-8", path.display())))?;
    let file = parse(src)?;
    Ok((file, bytes))
}

/// 1..=12 bytes of `[A-Z0-9]` — a Hypercall underlying's own spelling.
#[must_use]
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= HAR_NAME_MAX
        && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// `<venue>:<instrument>` in at most 64 bytes: the venue is
/// `[a-z][a-z0-9-]*`, the instrument one or more of `[A-Za-z0-9_.:/-]`
/// (`okx:MU-USDT-SWAP`, `hyperliquid:xyz:SP500`, `mexc-perp:SPY_USDT`).
#[must_use]
pub fn valid_descriptor(s: &str) -> bool {
    let Some((venue, inst)) = s.split_once(':') else {
        return false;
    };
    s.len() <= HAR_DESCRIPTOR_MAX
        && venue.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && venue
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !inst.is_empty()
        && inst
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'/' | b'-'))
}

/// One block: `(key, value, line)` in file order.
type Kv = Vec<(String, Value, usize)>;

/// A value as the file spells it, the way the worker's messages do.
fn show(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{s}\""),
        Value::Int(i) => i.to_string(),
        Value::Ints(_) | Value::Strs(_) => "an array".to_owned(),
    }
}

fn finish_series(kv: &Kv, ln: usize) -> Result<HarSeries, HarError> {
    let at = format!("[[series]] at line {ln}");
    for (k, _, l) in kv {
        if !HAR_KEYS.contains(&k.as_str()) {
            return Err(err(format!("line {l}: unknown [[series]] key `{k}`")));
        }
    }
    let get = |key: &str| kv.iter().find(|(k, _, _)| k == key).map(|(_, v, _)| v);
    let name = match get("name") {
        None => return Err(err(format!("{at}: `name` is required"))),
        Some(Value::Str(s)) if valid_name(s) => s.clone(),
        Some(v) => {
            return Err(err(format!(
                "{at}: `name` must be 1..=12 of [A-Z0-9] (got {})",
                show(v)
            )))
        }
    };
    let feed = match get("feed") {
        None => return Err(err(format!("{at}: `feed` is required"))),
        Some(Value::Str(s)) if valid_descriptor(s) => s.clone(),
        Some(v) => {
            return Err(err(format!(
                "{at}: `feed` {} is not a `<venue>:<instrument>` descriptor",
                show(v)
            )))
        }
    };
    let fallback = match get("fallback") {
        None => Vec::new(),
        Some(Value::Strs(v)) => v.clone(),
        // `[]` reads as an empty integer array: no fallback.
        Some(Value::Ints(v)) if v.is_empty() => Vec::new(),
        Some(_) => {
            return Err(err(format!(
                "{at}: `fallback` must be a one-line array of descriptors"
            )))
        }
    };
    if fallback.len() > HAR_MAX_FALLBACKS {
        return Err(err(format!(
            "{at}: at most {HAR_MAX_FALLBACKS} fallbacks (got {})",
            fallback.len()
        )));
    }
    let mut i = 0usize;
    while i < fallback.len() {
        let fb = &fallback[i];
        if !valid_descriptor(fb) {
            return Err(err(format!(
                "{at}: fallback \"{fb}\" is not a `<venue>:<instrument>` descriptor"
            )));
        }
        if *fb == feed || fallback[..i].contains(fb) {
            return Err(err(format!("{at}: `fallback` repeats \"{fb}\"")));
        }
        i += 1;
    }
    Ok(HarSeries {
        name,
        feed,
        fallback,
    })
}

/// Parse the text of a `har.toml`.
pub fn parse(src: &str) -> Result<HarFile, HarError> {
    let mut blocks: Vec<(Kv, usize)> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = strip_comment(raw);
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            if line != "[[series]]" {
                return Err(err(format!("line {ln}: unknown section `{line}`")));
            }
            if blocks.len() >= HAR_MAX_SERIES {
                return Err(err(format!("line {ln}: more than {HAR_MAX_SERIES} series")));
            }
            blocks.push((Vec::new(), ln));
            continue;
        }
        let Some((cur, _)) = blocks.last_mut() else {
            return Err(err(format!("line {ln}: key before any section header")));
        };
        let (k, v) = line
            .split_once('=')
            .ok_or_else(|| err(format!("line {ln}: expected `key = value`")))?;
        let k = k.trim();
        if k.is_empty() || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return Err(err(format!("line {ln}: bad key `{k}`")));
        }
        if cur.iter().any(|(e, _, _)| e == k) {
            return Err(err(format!("line {ln}: duplicate key `{k}`")));
        }
        cur.push((k.to_owned(), parse_value(v, ln)?, ln));
    }
    let mut series: Vec<HarSeries> = Vec::with_capacity(blocks.len());
    for (kv, ln) in &blocks {
        series.push(finish_series(kv, *ln)?);
    }
    if series.is_empty() {
        return Err(err("at least one [[series]] is required"));
    }
    let mut i = 0usize;
    while i < series.len() {
        let mut j = 0usize;
        while j < i {
            if series[j].name == series[i].name {
                return Err(err(format!(
                    "series name \"{}\" appears twice",
                    series[i].name
                )));
            }
            if series[j].feed == series[i].feed {
                return Err(err(format!("feed \"{}\" appears twice", series[i].feed)));
            }
            j += 1;
        }
        i += 1;
    }
    Ok(HarFile { series })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../har.toml.example");

    fn refused(src: &str) -> String {
        parse(src).expect_err("must refuse").0
    }

    fn one(body: &str) -> String {
        refused(&format!("[[series]]\n{body}"))
    }

    #[test]
    fn the_example_parses_and_names_the_twelve_underlyings() {
        let f = parse(EXAMPLE).expect("har.toml.example parses");
        let names: Vec<&str> = f.series.iter().map(|s| s.name.as_str()).collect();
        // The order `universe.toml [hypercall] underlyings` names them in.
        assert_eq!(
            names,
            [
                "SP500", "SPCX", "MU", "NVDA", "MSFT", "META", "AAPL", "BABA", "SNDK", "BOT",
                "BTC", "ETH"
            ]
        );
        assert_eq!(f.series.len(), HAR_MAX_SERIES);
        assert!(f.series.iter().all(|s| s.feed.starts_with("binance-usdm:")));
        assert_eq!(f.series[0].fallback, ["okx:SPY-USDT-SWAP"]);
        assert!(f.series[7].fallback.is_empty(), "BABA: no OKX listing");
        assert_eq!(f.series[9].fallback, ["bybit-linear:BOTUSDT"]);
        assert_eq!(f.series[10].fallback, ["binance:btcusdt"]);
    }

    #[test]
    fn a_good_file_parses_in_file_order() {
        let f = parse(
            "# two\n[[series]]\nname = \"BTC\"\nfeed = \"binance-usdm:btcusdt\"\n\
             fallback = [\"binance:btcusdt\"]   # spot first\n\n[[series]]\n\
             name = \"SP500\"\nfeed = \"binance-usdm:spyusdt\"\n\
             fallback = [\"okx:SPY-USDT-SWAP\", \"hyperliquid:xyz:SP500\",]\n\
             [[series]]\nname = \"BABA\"\nfeed = \"binance-usdm:babausdt\"\nfallback = []\n",
        )
        .expect("parses");
        assert_eq!(
            f.series[1],
            HarSeries {
                name: "SP500".to_owned(),
                feed: "binance-usdm:spyusdt".to_owned(),
                fallback: vec!["okx:SPY-USDT-SWAP".to_owned(), "hyperliquid:xyz:SP500".to_owned()],
            }
        );
        assert!(f.series[2].fallback.is_empty());
    }

    #[test]
    fn the_grammar_refuses_what_standard_toml_accepts() {
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = [\n  \"x:z\",\n]\n"),
            "line 4: unterminated array"
        );
        assert_eq!(one("name = 'A'\n"), "line 2: bad integer `'A'`");
        assert_eq!(one("name = \"A\\\"B\"\n"), "line 2: strings carry no escapes");
        assert_eq!(
            refused("[[ series ]]\nname = \"A\"\n"),
            "line 1: unknown section `[[ series ]]`"
        );
        assert_eq!(
            refused("series = [{ name = \"A\" }]\n"),
            "line 1: key before any section header"
        );
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfeed = \"x:z\"\n"),
            "line 4: duplicate key `feed`"
        );
        assert_eq!(one("series.name = \"A\"\n"), "line 2: bad key `series.name`");
        assert_eq!(one("name \"A\"\n"), "line 2: expected `key = value`");
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = [\"x:z\", 1]\n"),
            "line 4: expected a quoted string"
        );
        assert_eq!(one("name = 01\n"), "line 2: leading zero in `01`");
        assert_eq!(
            one("name = 99999999999999999999\n"),
            "line 2: bad integer `99999999999999999999`"
        );
        assert_eq!(
            one("name = 9223372036854775808\n"),
            "line 2: integer overflow `9223372036854775808`"
        );
    }

    #[test]
    fn unknown_keys_and_sections_are_refused() {
        assert_eq!(
            one("name = \"BTC\"\nfeed = \"binance:btcusdt\"\ntau = 1\n"),
            "line 4: unknown [[series]] key `tau`"
        );
        assert_eq!(refused("[har]\nname = \"A\"\n"), "line 1: unknown section `[har]`");
        assert_eq!(
            refused("mode = 1\n[[series]]\nname = \"A\"\nfeed = \"x:y\"\n"),
            "line 1: key before any section header"
        );
    }

    #[test]
    fn a_name_is_one_to_twelve_of_upper_alnum() {
        for bad in ["\"\"", "\"btc\"", "\"SP-500\"", "\"ABCDEFGHIJKLM\"", "\"É\"", "7"] {
            assert!(
                one(&format!("name = {bad}\nfeed = \"x:y\"\n"))
                    .contains("`name` must be 1..=12 of [A-Z0-9]"),
                "{bad}"
            );
        }
        assert_eq!(one("feed = \"x:y\"\n"), "[[series]] at line 1: `name` is required");
        assert_eq!(
            one("name = \"btc\"\n"),
            "[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got \"btc\")"
        );
        assert_eq!(
            one("name = 7\n"),
            "[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got 7)"
        );
        assert_eq!(
            one("name = [\"A\"]\n"),
            "[[series]] at line 1: `name` must be 1..=12 of [A-Z0-9] (got an array)"
        );
        let ok = parse("[[series]]\nname = \"ABCDEFGHIJ12\"\nfeed = \"x:y\"\n").expect("ok");
        assert_eq!(ok.series[0].name, "ABCDEFGHIJ12");
    }

    #[test]
    fn feed_and_fallbacks_are_descriptors() {
        assert_eq!(one("name = \"A\"\n"), "[[series]] at line 1: `feed` is required");
        for bad in [
            "\"btcusdt\"",
            "\"Binance:btc\"",
            "\"binance:\"",
            "\":btc\"",
            "\"1x:btc\"",
            "\"x:a b\"",
            "1",
        ] {
            assert!(
                one(&format!("name = \"A\"\nfeed = {bad}\n"))
                    .contains("is not a `<venue>:<instrument>` descriptor"),
                "{bad}"
            );
        }
        let long = format!("x:{}", "a".repeat(63));
        assert!(one(&format!("name = \"A\"\nfeed = \"{long}\"\n")).contains("descriptor"));
        assert!(valid_descriptor(&format!("x:{}", "a".repeat(62))));
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = \"x:z\"\n"),
            "[[series]] at line 1: `fallback` must be a one-line array of descriptors"
        );
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = [1]\n"),
            "[[series]] at line 1: `fallback` must be a one-line array of descriptors"
        );
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = [\"z\"]\n"),
            "[[series]] at line 1: fallback \"z\" is not a `<venue>:<instrument>` descriptor"
        );
        assert_eq!(
            one("name = \"A\"\nfeed = \"Binance:btc\"\n"),
            "[[series]] at line 1: `feed` \"Binance:btc\" is not a `<venue>:<instrument>` \
             descriptor"
        );
        for good in [
            "okx:MU-USDT-SWAP",
            "hyperliquid:xyz:SP500",
            "mexc-perp:SPY_USDT",
            "a1-b:x.y/z",
        ] {
            assert!(valid_descriptor(good), "{good}");
        }
    }

    #[test]
    fn a_fallback_never_repeats_the_feed_or_itself() {
        assert_eq!(
            one("name = \"A\"\nfeed = \"x:y\"\nfallback = [\"x:y\"]\n"),
            "[[series]] at line 1: `fallback` repeats \"x:y\""
        );
        assert!(one("name = \"A\"\nfeed = \"x:y\"\nfallback = [\"x:z\", \"x:z\"]\n")
            .contains("repeats \"x:z\""));
        let many: Vec<String> = (0..=HAR_MAX_FALLBACKS).map(|i| format!("\"x:f{i}\"")).collect();
        assert_eq!(
            one(&format!(
                "name = \"A\"\nfeed = \"x:y\"\nfallback = [{}]\n",
                many.join(", ")
            )),
            "[[series]] at line 1: at most 4 fallbacks (got 5)"
        );
    }

    #[test]
    fn names_and_feeds_are_unique_and_the_count_bounded() {
        assert_eq!(
            refused(
                "[[series]]\nname = \"A\"\nfeed = \"x:y\"\n[[series]]\nname = \"A\"\nfeed = \"x:z\"\n"
            ),
            "series name \"A\" appears twice"
        );
        assert_eq!(
            refused(
                "[[series]]\nname = \"A\"\nfeed = \"x:y\"\n[[series]]\nname = \"B\"\nfeed = \"x:y\"\n"
            ),
            "feed \"x:y\" appears twice"
        );
        assert_eq!(refused("# nothing\n"), "at least one [[series]] is required");
        let blocks: String = (0..13)
            .map(|i| format!("[[series]]\nname = \"S{i}\"\nfeed = \"x:s{i}\"\n"))
            .collect();
        assert_eq!(refused(&blocks), "line 37: more than 12 series");
        let twelve: String = (0..12)
            .map(|i| format!("[[series]]\nname = \"S{i}\"\nfeed = \"x:s{i}\"\n"))
            .collect();
        assert_eq!(parse(&twelve).expect("twelve").series.len(), 12);
    }

    #[test]
    fn load_reads_the_bytes_and_names_the_path() {
        let dir = std::env::temp_dir().join(format!("har-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let p = dir.join("har.toml");
        std::fs::write(&p, EXAMPLE).expect("write");
        let (f, bytes) = load(&p).expect("loads");
        assert_eq!(f.series.len(), 12);
        assert_eq!(bytes, EXAMPLE.as_bytes());
        std::fs::write(&p, b"[[series]]\nname = \"\xff\"\n").expect("write");
        assert!(load(&p).expect_err("not utf-8").0.ends_with("not UTF-8"));
        let absent = dir.join("absent.toml");
        assert!(load(&absent).expect_err("absent").0.contains("absent.toml"));
        assert!(HarError("x".to_owned()).to_string().starts_with("har.toml: "));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
