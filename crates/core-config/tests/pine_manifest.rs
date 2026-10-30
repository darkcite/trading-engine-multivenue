// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-08 §7.1 (re-baselined to slot 8, pin:08-slot8-rename): the contract §3.14 manifest
//! grammar (T1, I6) and the `[exec.slot.8.pine]` section of `exec.toml` (T2, I42).

use core_config::exec;
use core_config::pine::{
    load_manifest, parse_manifest, require_live, BootExitPolicy, PineCfgError, PineInputRaw,
    PineLimitPrimitive, PineManifest, PineMode, PineProvenance, PineScriptCfg, PineSlotCfg,
    CODE_MISSING_KEY, CODE_OUT_OF_RANGE, CODE_PROVENANCE, CODE_UNKNOWN_KEY, SCRIPT_INT_RANGES,
    SCRIPT_KEYS,
};
use proptest::prelude::*;

const SHA: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

/// The contract §3.14 listing with real values and the provenance one key per line.
const GOLDEN: &str = r#"
[[script]]
sha256            = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
file              = "supertrend.pine"
dialect           = 6                 # 5 | 6; must equal //@version (6002)
enabled           = 1
mode              = "realtime"
symbol            = "hyperliquid:btc"
timeframe_s       = 3600
venue             = "hyperliquid"
max_bars_back     = 5000
coalesce_ms       = 250
coalesce_bps      = 0
insn_budget       = 2000000
qty_notional_1e6  = 12000000
limit_primitive   = "ioc"
cross_ticks       = 2
cap_instance_1e6  = 12000000
cap_day_1e6       = 48000000
max_open          = 4
collection_cap    = 4096
str_persist_bytes = 4096
str_cell_bytes    = 128
str_scratch_bytes = 8192
intent_cap        = 32

  [script.inputs]                     # overrides, keyed by InputDesc::name
  length = 14
  mult   = 3000000
  source = "hlc3"
  [script.symbols]
  "BINANCE:BTCUSDT.P" = "binance_usdm:btcusdt"
  [script.provenance]
  sig                 = "sig-1"
  split_id            = "split-a"
  ledger_head         = "head-9"
  trials_n            = 41822
  upstream_sha256     = "FFEEDDCCBBAA99887766554433221100FFEEDDCCBBAA99887766554433221100"
  shell_id            = "shell-3"
  pub_date_ms         = 0
  selected_at_ms      = 0
  select_tool_version = 1
"#;

fn script_block(over: &[(&str, &str)], drop: &[&str]) -> String {
    let base: [(&str, &str); 23] = [
        (
            "sha256",
            "\"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\"",
        ),
        ("file", "\"s.pine\""),
        ("dialect", "6"),
        ("enabled", "1"),
        ("mode", "\"bar_close\""),
        ("symbol", "\"hyperliquid:btc\""),
        ("timeframe_s", "3600"),
        ("venue", "\"hyperliquid\""),
        ("max_bars_back", "5000"),
        ("coalesce_ms", "250"),
        ("coalesce_bps", "0"),
        ("insn_budget", "2000000"),
        ("qty_notional_1e6", "12000000"),
        ("limit_primitive", "\"ioc\""),
        ("cross_ticks", "2"),
        ("cap_instance_1e6", "12000000"),
        ("cap_day_1e6", "48000000"),
        ("max_open", "4"),
        ("collection_cap", "4096"),
        ("str_persist_bytes", "4096"),
        ("str_cell_bytes", "128"),
        ("str_scratch_bytes", "8192"),
        ("intent_cap", "32"),
    ];
    let mut s = String::from("[[script]]\n");
    for (k, v) in base {
        if drop.contains(&k) {
            continue;
        }
        let v = over
            .iter()
            .find(|(ok, _)| *ok == k)
            .map_or(v, |(_, ov)| *ov);
        s.push_str(&format!("{k} = {v}\n"));
    }
    s
}

fn refused(src: &str) -> PineCfgError {
    parse_manifest(src).expect_err("must refuse")
}

#[test]
fn golden_manifest_parses_every_key() {
    let m = parse_manifest(GOLDEN).expect("golden parses");
    assert_eq!(m.scripts.len(), 1);
    let s = &m.scripts[0];
    assert_eq!(s.line, 2);
    assert_eq!(s.sha256[0], 0x00);
    assert_eq!(s.sha256[1], 0x11);
    assert_eq!(s.sha256[31], 0xff);
    assert_eq!(s.file, "supertrend.pine");
    assert_eq!(s.dialect, 6);
    assert!(s.enabled);
    assert_eq!(s.mode, PineMode::Realtime);
    assert_eq!(s.symbol, "hyperliquid:btc");
    assert_eq!(s.timeframe_s, 3600);
    assert_eq!(s.venue, "hyperliquid");
    assert_eq!(s.max_bars_back, 5000);
    assert_eq!(s.coalesce_ms, 250);
    assert_eq!(s.coalesce_bps, 0);
    assert_eq!(s.insn_budget, 2_000_000);
    assert_eq!(s.qty_notional_1e6, 12_000_000);
    assert_eq!(s.limit_primitive, PineLimitPrimitive::Ioc);
    assert_eq!(s.cross_ticks, 2);
    assert_eq!(s.cap_instance_1e6, 12_000_000);
    assert_eq!(s.cap_day_1e6, 48_000_000);
    assert_eq!(s.max_open, 4);
    assert_eq!(s.collection_cap, 4096);
    assert_eq!(s.str_persist_bytes, 4096);
    assert_eq!(s.str_cell_bytes, 128);
    assert_eq!(s.str_scratch_bytes, 8192);
    assert_eq!(s.intent_cap, 32);
    assert_eq!(
        s.inputs,
        vec![
            (String::from("length"), PineInputRaw::Int(14)),
            (String::from("mult"), PineInputRaw::Int(3_000_000)),
            (
                String::from("source"),
                PineInputRaw::Str(String::from("hlc3"))
            ),
        ]
    );
    assert_eq!(
        s.symbols,
        vec![(
            String::from("BINANCE:BTCUSDT.P"),
            String::from("binance_usdm:btcusdt")
        )]
    );
    let p = s.provenance.as_ref().expect("provenance");
    assert_eq!(p.sig, "sig-1");
    assert_eq!(p.split_id, "split-a");
    assert_eq!(p.ledger_head, "head-9");
    assert_eq!(p.trials_n, 41_822);
    assert_eq!(p.upstream_sha256[0], 0xff);
    assert_eq!(p.upstream_sha256[31], 0x00);
    assert_eq!(p.shell_id, "shell-3");
    assert_eq!(
        (p.pub_date_ms, p.selected_at_ms, p.select_tool_version),
        (0, 0, 1)
    );
    // Two blocks, file order.
    let two = format!(
        "{}{}",
        script_block(&[], &[]),
        script_block(&[("file", "\"t.pine\"")], &[])
    );
    let m = parse_manifest(&two).unwrap();
    assert_eq!(m.scripts.len(), 2);
    assert_eq!(m.scripts[1].file, "t.pine");
    assert_eq!(m.scripts[1].line, 25);
}

#[test]
fn unknown_key_is_6004_naming_it() {
    for (src, key) in [
        (format!("{}bogus = 1\n", script_block(&[], &[])), "bogus"),
        (format!("{}Mode = 1\n", script_block(&[], &[])), "Mode"),
        (
            format!("{}[script.provenance]\nnope = 1\n", script_block(&[], &[])),
            "nope",
        ),
        (String::from("dialect = 6\n"), "dialect"),
        (
            String::from("[script.inputs]\nlength = 1\n"),
            "script.inputs",
        ),
        (
            format!("{}[script.other]\n", script_block(&[], &[])),
            "script.other",
        ),
        (
            format!("{}[script.inputs]\n9len = 1\n", script_block(&[], &[])),
            "9len",
        ),
    ] {
        let e = refused(&src);
        assert_eq!(
            (e.code, e.key.as_str()),
            (CODE_UNKNOWN_KEY, key),
            "{src}: {e}"
        );
    }
}

#[test]
fn missing_key_is_6005_naming_it() {
    for key in SCRIPT_KEYS {
        let e = refused(&script_block(&[], &[key]));
        assert_eq!(
            (e.code, e.key.as_str(), e.line),
            (CODE_MISSING_KEY, key, 1),
            "{e}"
        );
    }
    let e = refused("# nothing\n");
    assert_eq!((e.code, e.key.as_str()), (CODE_MISSING_KEY, "script"));
}

#[test]
fn out_of_range_is_6006_naming_it() {
    // One out-of-range value per key, at each end for the integers.
    let mut cases: Vec<(&str, String)> = vec![
        ("sha256", String::from("\"00\"")),
        ("sha256", format!("\"{}\"", SHA.to_uppercase())),
        ("file", String::from("\"a/b.pine\"")),
        ("file", String::from("\"..x.pine\"")),
        ("file", String::from("\"s.txt\"")),
        ("mode", String::from("\"tick\"")),
        ("symbol", String::from("\"HYPERLIQUID:btc\"")),
        ("symbol", String::from("\"hyperliquid_btc\"")),
        ("symbol", String::from("\"a:b:c\"")),
        ("symbol", format!("\"a:{}\"", "b".repeat(63))),
        ("venue", String::from("\"nasdaq\"")),
        ("limit_primitive", String::from("\"fok\"")),
        ("mode", String::from("1")),
        ("dialect", String::from("\"6\"")),
        ("enabled", String::from("[1]")),
        ("insn_budget", String::from("2_000_000")),
        ("insn_budget", String::from("02000000")),
    ];
    for (k, lo, hi) in SCRIPT_INT_RANGES {
        cases.push((k, (lo - 1).to_string()));
        cases.push((k, (hi + 1).to_string()));
    }
    for (key, v) in &cases {
        let e = refused(&script_block(&[(key, v.as_str())], &[]));
        assert_eq!(
            (e.code, e.key.as_str(), e.line),
            (
                CODE_OUT_OF_RANGE,
                *key,
                1 + SCRIPT_KEYS.iter().position(|k| k == key).unwrap() + 1
            ),
            "{key} = {v}: {e}"
        );
    }
    // impl-08 §3.7: a maker script needs max_open ≥ 2.
    let e = refused(&script_block(
        &[("limit_primitive", "\"maker\""), ("max_open", "1")],
        &[],
    ));
    assert_eq!((e.code, e.key.as_str()), (CODE_OUT_OF_RANGE, "max_open"));
    assert!(parse_manifest(&script_block(
        &[("limit_primitive", "\"maker\""), ("max_open", "2")],
        &[]
    ))
    .is_ok());
    // Duplicates are 6006 too: a key, an input, a ticker, a sub-table.
    let b = script_block(&[], &[]);
    for (src, key) in [
        (format!("{b}dialect = 6\n"), "dialect"),
        (format!("{b}[script.inputs]\na = 1\na = 2\n"), "a"),
        (
            format!("{b}[script.symbols]\n\"X:Y\" = \"a:b\"\n\"X:Y\" = \"a:c\"\n"),
            "X:Y",
        ),
        (
            format!("{b}[script.inputs]\n[script.inputs]\n"),
            "script.inputs",
        ),
        (format!("{b}[script.inputs]\nlen = [1, 2]\n"), "len"),
        (format!("{b}[script.symbols]\n\"X:Y\" = 3\n"), "X:Y"),
    ] {
        let e = refused(&src);
        assert_eq!(
            (e.code, e.key.as_str()),
            (CODE_OUT_OF_RANGE, key),
            "{src}: {e}"
        );
    }
}

#[test]
fn provenance_semicolon_line_is_6012() {
    let b = script_block(&[], &[]);
    // The contract's own listing joins keys with `;` — not TOML.
    let src =
        format!("{b}[script.provenance]\nsig = \"…\"; split_id = \"…\"; ledger_head = \"…\"\n");
    let e = refused(&src);
    assert_eq!((e.code, e.line), (CODE_PROVENANCE, 26), "{e}");
    // Every key required when present.
    let full = "sig = \"s\"\nsplit_id = \"a\"\nledger_head = \"h\"\ntrials_n = 1\n\
                upstream_sha256 = \"00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff\"\n\
                shell_id = \"x\"\npub_date_ms = 0\nselected_at_ms = 0\nselect_tool_version = 1\n";
    assert!(parse_manifest(&format!("{b}[script.provenance]\n{full}")).is_ok());
    for (drop_key, bad) in [
        ("sig", None),
        ("select_tool_version", Some("select_tool_version = 0")),
        ("trials_n", Some("trials_n = -1")),
        ("upstream_sha256", Some("upstream_sha256 = \"abc\"")),
        ("pub_date_ms", Some("pub_date_ms = \"0\"")),
    ] {
        let mut body = String::new();
        for l in full.lines() {
            if l.starts_with(drop_key) {
                if let Some(b2) = bad {
                    body.push_str(b2);
                    body.push('\n');
                }
            } else {
                body.push_str(l);
                body.push('\n');
            }
        }
        let e = refused(&format!("{b}[script.provenance]\n{body}"));
        assert_eq!((e.code, e.key.as_str()), (CODE_PROVENANCE, drop_key), "{e}");
    }
}

#[test]
fn quoted_keys_only_in_symbols() {
    let b = script_block(&[], &[]);
    let e = refused(&format!("{b}[script.inputs]\n\"length\" = 14\n"));
    assert_eq!((e.code, e.key.as_str()), (CODE_UNKNOWN_KEY, "length"));
    let e = refused(
        &script_block(&[], &["dialect"]).replace("[[script]]\n", "[[script]]\n\"dialect\" = 6\n"),
    );
    assert_eq!((e.code, e.key.as_str()), (CODE_UNKNOWN_KEY, "dialect"));
    let e = refused(&format!("{b}[script.provenance]\n\"sig\" = \"s\"\n"));
    assert_eq!(e.code, CODE_UNKNOWN_KEY);
    // Inside [script.symbols], quoted and bare (`[a-z0-9_]+`) both parse.
    let m = parse_manifest(&format!("{b}[script.symbols]\n\"BINANCE:BTCUSDT.P\" = \"binance_usdm:btcusdt\"\nbtc = \"hyperliquid:btc\"\n")).unwrap();
    assert_eq!(m.scripts[0].symbols.len(), 2);
    assert_eq!(
        m.scripts[0].symbols[1],
        (String::from("btc"), String::from("hyperliquid:btc"))
    );
}

#[test]
fn load_manifest_returns_the_bytes_and_refuses_a_missing_file() {
    let dir = std::env::temp_dir().join(format!("pine-manifest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("m.toml");
    std::fs::write(&p, GOLDEN).unwrap();
    let (m, bytes) = load_manifest(&p).unwrap();
    assert_eq!(bytes, GOLDEN.as_bytes());
    assert_eq!(m, parse_manifest(GOLDEN).unwrap());
    let e = load_manifest(&dir.join("absent.toml")).unwrap_err();
    assert_eq!((e.code, e.key.as_str()), (CODE_MISSING_KEY, "manifest"));
    std::fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------------------------------------------------------
// the property round trip (I6)
// ---------------------------------------------------------------------------------------------

fn hex(b: &[u8; 32]) -> String {
    let mut s = String::new();
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn render(m: &PineManifest) -> String {
    let mut s = String::new();
    for c in &m.scripts {
        s.push_str("[[script]]\n");
        let q = |v: &str| format!("\"{v}\"");
        let rows: [(&str, String); 23] = [
            ("sha256", q(&hex(&c.sha256))),
            ("file", q(&c.file)),
            ("dialect", c.dialect.to_string()),
            ("enabled", u8::from(c.enabled).to_string()),
            (
                "mode",
                q(match c.mode {
                    PineMode::BarClose => "bar_close",
                    PineMode::Realtime => "realtime",
                }),
            ),
            ("symbol", q(&c.symbol)),
            ("timeframe_s", c.timeframe_s.to_string()),
            ("venue", q(&c.venue)),
            ("max_bars_back", c.max_bars_back.to_string()),
            ("coalesce_ms", c.coalesce_ms.to_string()),
            ("coalesce_bps", c.coalesce_bps.to_string()),
            ("insn_budget", c.insn_budget.to_string()),
            ("qty_notional_1e6", c.qty_notional_1e6.to_string()),
            (
                "limit_primitive",
                q(match c.limit_primitive {
                    PineLimitPrimitive::Ioc => "ioc",
                    PineLimitPrimitive::Maker => "maker",
                }),
            ),
            ("cross_ticks", c.cross_ticks.to_string()),
            ("cap_instance_1e6", c.cap_instance_1e6.to_string()),
            ("cap_day_1e6", c.cap_day_1e6.to_string()),
            ("max_open", c.max_open.to_string()),
            ("collection_cap", c.collection_cap.to_string()),
            ("str_persist_bytes", c.str_persist_bytes.to_string()),
            ("str_cell_bytes", c.str_cell_bytes.to_string()),
            ("str_scratch_bytes", c.str_scratch_bytes.to_string()),
            ("intent_cap", c.intent_cap.to_string()),
        ];
        for (k, v) in rows {
            s.push_str(&format!("{k} = {v}\n"));
        }
        s.push_str("  [script.inputs]\n");
        for (k, v) in &c.inputs {
            match v {
                PineInputRaw::Int(i) => s.push_str(&format!("  {k} = {i}\n")),
                PineInputRaw::Str(t) => s.push_str(&format!("  {k} = \"{t}\"\n")),
            }
        }
        s.push_str("  [script.symbols]\n");
        for (k, v) in &c.symbols {
            s.push_str(&format!("  \"{k}\" = \"{v}\"\n"));
        }
        if let Some(p) = &c.provenance {
            s.push_str("  [script.provenance]\n");
            s.push_str(&format!(
                "  sig = \"{}\"\n  split_id = \"{}\"\n  ledger_head = \"{}\"\n",
                p.sig, p.split_id, p.ledger_head
            ));
            s.push_str(&format!(
                "  trials_n = {}\n  upstream_sha256 = \"{}\"\n  shell_id = \"{}\"\n",
                p.trials_n,
                hex(&p.upstream_sha256),
                p.shell_id
            ));
            s.push_str(&format!(
                "  pub_date_ms = {}\n  selected_at_ms = {}\n  select_tool_version = {}\n",
                p.pub_date_ms, p.selected_at_ms, p.select_tool_version
            ));
        }
    }
    s
}

fn r(key: &str) -> std::ops::RangeInclusive<i64> {
    let (_, lo, hi) = SCRIPT_INT_RANGES
        .iter()
        .find(|(k, _, _)| *k == key)
        .copied()
        .unwrap();
    lo..=hi
}

fn u(key: &str) -> impl Strategy<Value = u32> {
    r(key).prop_map(|v| v as u32)
}

fn arb_prov() -> impl Strategy<Value = PineProvenance> {
    (
        (
            "[a-z0-9-]{0,8}",
            "[a-z0-9-]{0,8}",
            "[a-z0-9-]{0,8}",
            0i64..1_000_000,
        ),
        (
            any::<[u8; 32]>(),
            "[a-z0-9-]{0,8}",
            0i64..i64::MAX,
            0i64..i64::MAX,
            1i64..1000,
        ),
    )
        .prop_map(
            |((sig, split_id, ledger_head, trials_n), (up, shell_id, pd, sa, v))| PineProvenance {
                sig,
                split_id,
                ledger_head,
                trials_n,
                upstream_sha256: up,
                shell_id,
                pub_date_ms: pd,
                selected_at_ms: sa,
                select_tool_version: v,
            },
        )
}

fn arb_script() -> impl Strategy<Value = PineScriptCfg> {
    let a = (
        any::<[u8; 32]>(),
        "[a-z][a-z0-9_]{0,7}\\.pine",
        5u8..=6,
        any::<bool>(),
        any::<bool>(),
        "[a-z][a-z0-9_]{0,7}:[a-z0-9]{1,8}",
        u("timeframe_s"),
        prop::sample::select(vec!["hyperliquid", "binance", "okx", "deribit", "bybit"]),
    );
    let b = (
        u("max_bars_back"),
        u("coalesce_ms"),
        u("coalesce_bps"),
        u("insn_budget"),
        r("qty_notional_1e6"),
        any::<bool>(),
        u("cross_ticks"),
        r("cap_instance_1e6"),
        r("cap_day_1e6"),
        2u32..=8,
    );
    let c = (
        u("collection_cap"),
        u("str_persist_bytes"),
        u("str_cell_bytes"),
        u("str_scratch_bytes"),
        u("intent_cap"),
        prop::collection::btree_map(
            "[a-z_][a-zA-Z0-9_]{0,6}",
            prop_oneof![
                (i64::MIN + 1..=i64::MAX).prop_map(PineInputRaw::Int),
                "[a-z ]{0,6}".prop_map(PineInputRaw::Str)
            ],
            0..4,
        ),
        prop::collection::btree_map(
            "[A-Z]{1,6}:[A-Z0-9.]{1,8}",
            "[a-z_]{1,8}:[a-z0-9]{1,8}",
            0..3,
        ),
        prop::option::of(arb_prov()),
    );
    (a, b, c).prop_map(|(a, b, c)| PineScriptCfg {
        line: 0,
        sha256: a.0,
        file: a.1,
        dialect: a.2,
        enabled: a.3,
        mode: if a.4 {
            PineMode::Realtime
        } else {
            PineMode::BarClose
        },
        symbol: a.5,
        timeframe_s: a.6,
        venue: a.7.to_owned(),
        max_bars_back: b.0,
        coalesce_ms: b.1,
        coalesce_bps: b.2,
        insn_budget: b.3,
        qty_notional_1e6: b.4,
        limit_primitive: if b.5 {
            PineLimitPrimitive::Maker
        } else {
            PineLimitPrimitive::Ioc
        },
        cross_ticks: b.6,
        cap_instance_1e6: b.7,
        cap_day_1e6: b.8,
        max_open: b.9,
        collection_cap: c.0,
        str_persist_bytes: c.1,
        str_cell_bytes: c.2,
        str_scratch_bytes: c.3,
        intent_cap: c.4,
        inputs: c.5.into_iter().collect(),
        symbols: c.6.into_iter().collect(),
        provenance: c.7,
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Generated manifests render and re-parse equal (the header line aside); a random
    /// single-key mutation refuses naming that key — out of range (6006) or deleted (6005).
    #[test]
    fn manifest_property_roundtrip(
        scripts in prop::collection::vec(arb_script(), 1..3),
        which in 0usize..23,
        delete in any::<bool>(),
    ) {
        let m = PineManifest { scripts };
        let text = render(&m);
        let back = parse_manifest(&text).map_err(|e| TestCaseError::fail(format!("{e}\n{text}")))?;
        prop_assert_eq!(back.scripts.len(), m.scripts.len());
        for (a, b) in m.scripts.iter().zip(back.scripts.iter()) {
            let mut a = a.clone();
            a.line = b.line;
            prop_assert_eq!(&a, b);
        }
        prop_assert_eq!(render(&back), text.clone());

        // Mutate one key of the first block.
        let key = SCRIPT_KEYS[which];
        let bad = match SCRIPT_INT_RANGES.iter().find(|(k, _, _)| *k == key) {
            Some((_, lo, _)) => (lo - 1).to_string(),
            None => String::from("\"\""),
        };
        let mut out = String::new();
        let mut done = false;
        for l in text.lines() {
            if !done && l.starts_with(&format!("{key} = ")) {
                done = true;
                if !delete {
                    out.push_str(&format!("{key} = {bad}\n"));
                }
                continue;
            }
            out.push_str(l);
            out.push('\n');
        }
        let e = parse_manifest(&out).expect_err("a mutated key refuses");
        prop_assert_eq!(e.key.as_str(), key);
        prop_assert_eq!(e.code, if delete { CODE_MISSING_KEY } else { CODE_OUT_OF_RANGE });
    }
}

// ---------------------------------------------------------------------------------------------
// `[exec.slot.8.pine]` (I42)
// ---------------------------------------------------------------------------------------------

const PINE_FULL: &str = "[exec.slot.8.pine]\nmin_coalesce_ms = 250\nevent_insn_budget = 4000000\n\
    boot_exit_policy = \"protective\"\nmax_frame_bytes = 16777216\nquote_stale_ms = 700\n\
    live_settle_ms = 2000\nrefusal_streak_max = 16\nfee_taker_ppm = 0\nfee_maker_ppm = 0\n\
    max_warm_lag_bars = 1\n";

fn exec_err(src: &str) -> String {
    exec::parse(src).expect_err("must refuse").0
}

#[test]
fn slot8_section_defaults_and_ranges() {
    // An empty section is the DEFAULT; every key at its table value parses.
    let f = exec::parse("[exec]\n[exec.slot.8.pine]\n").unwrap();
    assert_eq!(f.pine8, Some(PineSlotCfg::DEFAULT));
    assert_eq!(PineSlotCfg::DEFAULT.max_frame_bytes, 16_777_216);
    assert_eq!(PineSlotCfg::DEFAULT.live_settle_ms, 2000);
    assert_eq!(PineSlotCfg::DEFAULT.refusal_streak_max, 16);
    assert_eq!(PineSlotCfg::DEFAULT.max_warm_lag_bars, 1);
    assert_eq!(core_config::pine::EVENT_INSN_BUDGET_DEFAULT, 4_000_000);
    let f = exec::parse(&format!("[exec]\n{PINE_FULL}")).unwrap();
    let p = f.pine8.unwrap();
    assert_eq!(p.min_coalesce_ms, Some(250));
    assert_eq!(p.event_insn_budget, Some(4_000_000));
    assert_eq!(p.boot_exit_policy, Some(BootExitPolicy::Protective));
    assert_eq!(p.quote_stale_ms, Some(700));
    for (w, v) in [
        ("accept", BootExitPolicy::Accept),
        ("standing", BootExitPolicy::Standing),
        ("flatten", BootExitPolicy::Flatten),
    ] {
        let f = exec::parse(&format!(
            "[exec]\n[exec.slot.8.pine]\nboot_exit_policy = \"{w}\"\n"
        ))
        .unwrap();
        assert_eq!(f.pine8.unwrap().boot_exit_policy, Some(v));
    }
    // Every bound, both ends; each refusal begins with its code and names the key.
    for (k, lo, hi) in core_config::pine::SLOT_INT_RANGES {
        for v in [lo, hi] {
            assert!(
                exec::parse(&format!("[exec]\n[exec.slot.8.pine]\n{k} = {v}\n")).is_ok(),
                "{k} = {v}"
            );
        }
        for v in [lo - 1, hi + 1] {
            let e = exec_err(&format!("[exec]\n[exec.slot.8.pine]\n{k} = {v}\n"));
            assert!(
                e.starts_with("6006 [exec.slot.8.pine] line 3:") && e.contains(k),
                "{e}"
            );
        }
    }
    let e = exec_err("[exec]\n[exec.slot.8.pine]\nboot_exit_policy = \"later\"\n");
    assert!(
        e.starts_with("6006 ") && e.contains("boot_exit_policy"),
        "{e}"
    );
    let e = exec_err("[exec]\n[exec.slot.8.pine]\nhalt_on_ws_gap_ms = 1\n");
    assert!(
        e.starts_with("6004 [exec.slot.8.pine] line 3:") && e.contains("halt_on_ws_gap_ms"),
        "{e}"
    );
    let e = exec_err("[exec]\n[exec.slot.8.pine]\nlive_settle_ms = 200\nlive_settle_ms = 300\n");
    assert!(e.starts_with("6006 ") && e.contains("duplicate"), "{e}");
    let e = exec_err("[exec]\n[exec.slot.8.pine]\n[exec.slot.8.pine]\n");
    assert!(
        e.starts_with("6006 ") && e.contains("duplicate section"),
        "{e}"
    );
    let e = exec_err("[exec.slot.8.pine]\n[exec]\n");
    assert!(e.starts_with("6004 ") && e.contains("after [exec]"), "{e}");
    let e = exec_err("[exec]\n[exec.slot.8.pine]\nmax_frame_bytes = \"big\"\n");
    assert!(
        e.starts_with("6006 ") && e.contains("max_frame_bytes"),
        "{e}"
    );
    // Recognised only at slot 8.
    assert!(exec::parse("[exec]\n[exec.slot.7.pine]\n").is_err());
    assert!(exec::parse("[exec]\n[exec.slot.3.pine]\n").is_err());
}

#[test]
fn slot8_live_requires_three_keys() {
    let full = exec::parse(&format!("[exec]\n{PINE_FULL}"))
        .unwrap()
        .pine8
        .unwrap();
    assert!(require_live(&full, 2).is_ok());
    for key in ["min_coalesce_ms", "event_insn_budget", "boot_exit_policy"] {
        let mut c = full;
        match key {
            "min_coalesce_ms" => c.min_coalesce_ms = None,
            "event_insn_budget" => c.event_insn_budget = None,
            _ => c.boot_exit_policy = None,
        }
        let e = require_live(&c, 212).unwrap_err();
        assert_eq!(
            (e.code, e.key.as_str(), e.line),
            (CODE_MISSING_KEY, key, 212)
        );
    }
    assert_eq!(
        require_live(&PineSlotCfg::DEFAULT, 1).unwrap_err().key,
        "min_coalesce_ms"
    );
}

/// Ruling 2026-10-16: slot 8 is PAPER ONLY — a live `[exec.slot.8]` is refused at parse,
/// whatever else the artifact holds; paper and off parse.
#[test]
fn slot8_live_section_is_refused_paper_only() {
    let live8 = "[exec]\n[exec.slot.8]\nmode = \"live\"\nname = \"pine\"\n\
         venues = [\"hyperliquid\"]\nmax_order_usd_1e6 = 30000000\n\
         cap_instance_usd_1e6 = 12000000\ncap_day_usd_1e6 = 96000000\n\
         max_open_orders = 8\nrequest_budget_floor = 2000\n\
         halt_on_reject_streak = 5\nhalt_on_recon_drift_usd_1e6 = 5000000\n\
         halt_on_ws_gap_ms = 30000\nhalt_on_asset_refusal_streak = 3\n\
         halt_on_recon_stale_ms = 300000\n";
    for src in [
        live8.to_owned(),
        format!("{live8}{PINE_FULL}"),
        String::from("[exec]\n[exec.slot.8]\nmode = \"live\"\n"),
    ] {
        let e = exec_err(&src);
        assert!(e.contains("slot 8") && e.contains("PAPER ONLY"), "{e}");
    }
    for mode in ["paper", "off"] {
        let f = exec::parse(&format!(
            "[exec]\n[exec.slot.8]\nmode = \"{mode}\"\n{PINE_FULL}"
        ))
        .unwrap();
        assert_eq!(f.live_mask(), 0);
        assert!(f.pine8.is_some());
    }
}

#[test]
fn exec_toml_without_slot8_section_is_unchanged() {
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../exec.toml.example");
    let src = std::fs::read_to_string(&example).unwrap();
    let f = exec::parse(&src).unwrap();
    assert_eq!(
        f.pine8, None,
        "the example carries the section commented out"
    );
    // Appending the section changes `pine8` alone.
    let g = exec::parse(&format!("{src}\n{PINE_FULL}")).unwrap();
    assert!(g.pine8.is_some());
    let mut g2 = g.clone();
    g2.pine8 = None;
    assert_eq!(g2, f);
    // A minimal artifact likewise.
    let f = exec::parse("[exec]\nenabled = 1\n[exec.slot.3]\nmode = \"paper\"\n").unwrap();
    assert_eq!(f.pine8, None);
    assert_eq!(f.slots.len(), 1);
}
