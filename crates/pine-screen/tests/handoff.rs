// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-07 §7.8 — the tier-2 hand-off (§4.8): windows (I23), ids, files, cut plan, B'3 argv (I35).

use pine_ir::{ExecMode, InputOverride, InputValue};
use pine_screen::handoff::{
    b3_argv, cutplan_tsv, manifest_toml, null_trial_id, read_handoff, run_windows, t2_trial_id,
    validate_windows, write_handoff, Handoff, HandoffErr, ManifestErr, ManifestNums, Window,
    HANDOFF_SCHEMA, MAX_WINDOW_NS,
};
use pine_screen::ids::{fee_digest, hex32};
use std::path::PathBuf;

const H_NS: i64 = 3_600_000_000_000;
const EPOCH_A: i64 = 1_790_000_000_123_456_789;
const EPOCH_B: i64 = EPOCH_A + 10 * H_NS;

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pine-screen-handoff-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("tmp");
    d
}

fn runs() -> Vec<Window> {
    let mut w = Vec::new();
    run_windows(
        &format!("run-{EPOCH_A}"),
        7 * H_NS + H_NS / 2,
        0,
        true,
        &mut w,
    );
    run_windows(&format!("run-{EPOCH_B}"), 3 * H_NS, 1, false, &mut w);
    w
}

const SCRIPT: &[u8] = b"//@version=6\nstrategy(\"x\")\n";

fn handoff(dir: PathBuf) -> Handoff {
    let windows = runs();
    let t1 = [0x11u8; 32];
    let t2 = t2_trial_id(&t1, &windows);
    let flags = vec!["bn.perp:2:5".to_string(), "hl.perp:1.5:4.5".to_string()];
    let refs: Vec<&str> = flags.iter().map(String::as_str).collect();
    Handoff {
        dir: dir.clone(),
        t1_trial_id: t1,
        t2_trial_id: t2,
        null_trial_id: null_trial_id(&t2),
        script: "pvm7_supertrend.pine".into(),
        source_sha256: core_crypto::sha256(SCRIPT),
        manifest: "manifest.toml".into(),
        symbol_key: "binance_usdm:btcusdt".into(),
        timeframe_s: 3600,
        mode: ExecMode::BarClose,
        sizing_notional_1e6: 1_000_000_000,
        fee_digest: fee_digest(&refs),
        fee_flags: flags,
        warmup_bars_root: PathBuf::from("/bars/root"),
        warmup_to_ms: 1_789_999_200_000,
        score_from_ms: 1_790_035_200_000,
        score_to_ms: 1_790_121_600_000,
        rows_out: dir.join("rows.t2"),
        ret_out: dir.join("ret-t2"),
        prior_trades_per_1k_bars: 55,
        tape_bars_target: 546,
        windows,
    }
}

#[test]
fn handoff_windows_obey_the_two_hour_law() {
    let mut w = Vec::new();
    let run = format!("run-{EPOCH_A}");
    run_windows(&run, 7 * H_NS + H_NS / 2, 3, true, &mut w);
    assert_eq!(w.len(), 4);
    let lens: Vec<i64> = w.iter().map(|x| x.off_to_ns - x.off_from_ns).collect();
    assert_eq!(lens, vec![2 * H_NS, 2 * H_NS, 2 * H_NS, H_NS + H_NS / 2]);
    for i in 0..4 {
        assert!(w[i].off_to_ns - w[i].off_from_ns <= MAX_WINDOW_NS);
        assert_eq!(w[i].chain, 3);
        assert_eq!(w[i].run, run);
        if i > 0 {
            assert_eq!(w[i].off_from_ns, w[i - 1].off_to_ns);
        }
    }
    assert_eq!(w[0].off_from_ns, 0);
    assert_eq!(w[0].from_ms, EPOCH_A / 1_000_000);
    assert_eq!(
        w[3].to_ms,
        EPOCH_A / 1_000_000 + (7 * H_NS + H_NS / 2) / 1_000_000
    );
    assert_eq!(validate_windows(&w), Ok(()));
    assert_eq!(validate_windows(&runs()), Ok(()));
}

fn rewrite_windows(dir: &std::path::Path, w: &[Window]) {
    std::fs::write(
        dir.join("windows.tsv"),
        pine_screen::handoff::windows_tsv(w),
    )
    .expect("w");
}

#[test]
fn handoff_read_refuses_a_bad_window() {
    let d = tmp("bad");
    let h = handoff(d.clone());
    write_handoff(&h, SCRIPT, "m").expect("write");
    // A 2 h 0.001 s window.
    let mut w = h.windows.clone();
    w[1].off_to_ns += 1_000_000;
    w[1].to_ms += 1;
    rewrite_windows(&d, &w);
    assert_eq!(read_handoff(&d), Err(HandoffErr::Window(1)));
    // An overlap: window 2 starts inside window 1.
    let mut w = h.windows.clone();
    w[2].off_from_ns -= 1_000_000;
    w[2].from_ms -= 1;
    rewrite_windows(&d, &w);
    assert_eq!(read_handoff(&d), Err(HandoffErr::Window(2)));
    // Past the run: run A's last window runs into run B's start.
    // Run B starts at A + 7 h: window 3 of run A ([6 h, 7.5 h)) runs past A's end.
    let mut w = h.windows.clone();
    let early_b = EPOCH_A + 7 * H_NS;
    for x in w.iter_mut().skip(4) {
        x.run = format!("run-{early_b}");
        x.from_ms = early_b / 1_000_000 + x.off_from_ns / 1_000_000;
        x.to_ms = early_b / 1_000_000 + x.off_to_ns / 1_000_000;
    }
    assert_eq!(validate_windows(&w), Err(HandoffErr::Window(3)));
    rewrite_windows(&d, &w);
    assert_eq!(read_handoff(&d), Err(HandoffErr::Window(3)));
    // A from_ms that is not its run's, an empty window, a bad run name.
    let mut w = h.windows.clone();
    w[0].from_ms += 1;
    assert_eq!(validate_windows(&w), Err(HandoffErr::Window(0)));
    let mut w = h.windows.clone();
    w[0].off_to_ns = w[0].off_from_ns;
    assert_eq!(validate_windows(&w), Err(HandoffErr::Window(0)));
    let mut w = h.windows.clone();
    w[0].run = "run-x".into();
    assert_eq!(validate_windows(&w), Err(HandoffErr::Window(0)));
    // write_handoff validates too.
    let mut bad = h.clone();
    bad.windows[1].off_to_ns += 1;
    assert_eq!(write_handoff(&bad, SCRIPT, "m"), Err(HandoffErr::Window(1)));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn handoff_round_trips() {
    let d = tmp("rt");
    let h = handoff(d.clone());
    write_handoff(&h, SCRIPT, "[[script]]\n").expect("write");
    let r = read_handoff(&d).expect("read");
    assert_eq!(r, h);
    assert_eq!(r.fee_flags, vec!["bn.perp:2:5", "hl.perp:1.5:4.5"]);
    for f in [
        "handoff.tsv",
        "windows.tsv",
        "cutplan.tsv",
        "b3.argv",
        "manifest.toml",
        "pvm7_supertrend.pine",
    ] {
        assert!(d.join(f).exists(), "{f}");
    }
    assert_eq!(
        std::fs::read(d.join("pvm7_supertrend.pine")).expect("r"),
        SCRIPT
    );
    let tsv = std::fs::read_to_string(d.join("handoff.tsv")).expect("r");
    assert!(tsv.starts_with(&format!("schema\t{HANDOFF_SCHEMA}\n")));
    let argv = std::fs::read_to_string(d.join("b3.argv")).expect("r");
    assert_eq!(argv.lines().collect::<Vec<_>>(), b3_argv(&h));
    // Another schema, a stray key, a tampered script.
    std::fs::write(
        d.join("handoff.tsv"),
        tsv.replace(HANDOFF_SCHEMA, "pvm7-handoff-1"),
    )
    .expect("w");
    assert_eq!(read_handoff(&d), Err(HandoffErr::Schema));
    std::fs::write(d.join("handoff.tsv"), format!("{tsv}bogus\t1\n")).expect("w");
    let n = tsv.lines().count() as u32 + 1;
    assert_eq!(read_handoff(&d), Err(HandoffErr::Parse(n)));
    std::fs::write(d.join("handoff.tsv"), &tsv).expect("w");
    std::fs::write(d.join("pvm7_supertrend.pine"), b"tampered").expect("w");
    assert!(matches!(read_handoff(&d), Err(HandoffErr::Parse(_))));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn handoff_trial_ids_are_derived() {
    let w = runs();
    let t1 = [0x11u8; 32];
    let base = t2_trial_id(&t1, &w);
    assert_ne!(t2_trial_id(&[0x12; 32], &w), base);
    let mut w2 = w.clone();
    w2[4].local = true;
    assert_ne!(t2_trial_id(&t1, &w2), base);
    let mut w3 = w.clone();
    w3[2].off_to_ns -= 1;
    assert_ne!(t2_trial_id(&t1, &w3), base);
    assert_ne!(t2_trial_id(&t1, &w[..5]), base);
    assert_eq!(t2_trial_id(&t1, &runs()), base);
    let n = null_trial_id(&base);
    assert_eq!(n, null_trial_id(&base));
    assert_ne!(n, base);
    assert_ne!(null_trial_id(&t2_trial_id(&t1, &w2)), n);
    // A hand-off whose ids are not the derived ones is refused.
    let d = tmp("ids");
    let mut h = handoff(d.clone());
    h.null_trial_id[0] ^= 1;
    assert_eq!(write_handoff(&h, SCRIPT, "m"), Err(HandoffErr::Parse(0)));
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn handoff_manifest_names_every_contract_key() {
    let h = handoff(PathBuf::from("/x"));
    let nums = ManifestNums {
        pyramiding: 2,
        t1_max_concurrent_1e6: 1_500_000_000,
        t1_turnover_day_p99_1e6: 1_000_000_000,
    };
    let inputs = vec![
        InputOverride {
            name: "length".into(),
            value: InputValue::Int(14),
        },
        InputOverride {
            name: "mult".into(),
            value: InputValue::Float1e6(3_000_000),
        },
        InputOverride {
            name: "src".into(),
            value: InputValue::Str("hl c3".into()),
        },
        InputOverride {
            name: "useAtr".into(),
            value: InputValue::Bool(true),
        },
    ];
    let tm = vec![(
        "BINANCE:BTCUSDT.P".to_string(),
        "binance_usdm:btcusdt".to_string(),
    )];
    let m = manifest_toml(&h, 6, "binance", &nums, &inputs, &tm).expect("escape-free");
    let keys = [
        "sha256",
        "file",
        "dialect",
        "enabled",
        "mode",
        "symbol",
        "timeframe_s",
        "venue",
        "max_bars_back",
        "coalesce_ms",
        "coalesce_bps",
        "insn_budget",
        "qty_notional_1e6",
        "limit_primitive",
        "cross_ticks",
        "cap_instance_1e6",
        "cap_day_1e6",
        "max_open",
        "collection_cap",
        "str_persist_bytes",
        "str_cell_bytes",
        "str_scratch_bytes",
        "intent_cap",
    ];
    let lines: Vec<&str> = m.lines().collect();
    assert_eq!(lines[0], "[[script]]");
    for k in keys {
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.split('=').next().map(str::trim) == Some(k))
                .count(),
            1,
            "{k}"
        );
    }
    let val = |k: &str| -> String {
        let l = lines
            .iter()
            .find(|l| l.split('=').next().map(str::trim) == Some(k))
            .expect(k);
        l.split_once('=').expect("=").1.trim().to_string()
    };
    let hexs = String::from_utf8(hex32(&h.source_sha256).to_vec()).expect("hex");
    assert_eq!(val("sha256"), format!("\"{hexs}\""));
    assert_eq!(val("file"), "\"pvm7_supertrend.pine\"");
    assert_eq!(val("dialect"), "6");
    assert_eq!(val("enabled"), "1");
    assert_eq!(val("mode"), "\"bar_close\"");
    assert_eq!(val("symbol"), "\"binance_usdm:btcusdt\"");
    assert_eq!(val("venue"), "\"binance\"");
    assert_eq!(val("max_bars_back"), "5000");
    assert_eq!(val("insn_budget"), "2000000");
    assert_eq!(val("qty_notional_1e6"), "1000000000");
    assert_eq!(val("limit_primitive"), "\"ioc\"");
    assert_eq!(val("cross_ticks"), "2");
    assert_eq!(val("cap_instance_1e6"), "3000000000"); // max(2 × N, 2 × 1.5 N)
    assert_eq!(val("cap_day_1e6"), "4000000000"); // max(4 N, 2 × N)
    assert_eq!(val("max_open"), "8");
    assert_eq!(
        (val("coalesce_ms").as_str(), val("coalesce_bps").as_str()),
        ("250", "0")
    );
    // Every value outside the quoted strings is an integer.
    for k in keys {
        let v = val(k);
        assert!(v.starts_with('"') || v.parse::<i64>().is_ok(), "{k} = {v}");
    }
    let inp = m.find("[script.inputs]").expect("inputs");
    let sym = m.find("[script.symbols]").expect("symbols");
    assert!(inp < sym);
    let inputs_block = &m[inp..sym];
    assert!(inputs_block.contains("length = 14\n"));
    assert!(inputs_block.contains("mult = 3000000\n"));
    assert!(inputs_block.contains("src = \"hl c3\"\n"));
    assert!(inputs_block.contains("useAtr = 1\n"));
    assert!(m[sym..].contains("\"BINANCE:BTCUSDT.P\" = \"binance_usdm:btcusdt\"\n"));
}

/// `pin:07-manifest-no-escapes` (lead ruling 2026-10-16): the impl-08 manifest grammar allows no
/// escape inside a quoted string (6006), so a value or quoted key that would need one — `"`, `\`,
/// a control character U+0000–U+001F, U+007F — is refused with the field named, never escaped.
#[test]
fn handoff_manifest_refuses_a_value_that_needs_an_escape() {
    let nums = ManifestNums {
        pyramiding: 1,
        t1_max_concurrent_1e6: 0,
        t1_turnover_day_p99_1e6: 0,
    };
    let ok_tm = vec![("BINANCE:BTCUSDT.P".to_string(), "k".to_string())];
    let input = |name: &str, v: &str| {
        vec![InputOverride {
            name: name.into(),
            value: InputValue::Str(v.into()),
        }]
    };
    let h = handoff(PathBuf::from("/x"));
    let refused = |e: &str| Err(ManifestErr::NeedsEscape(e.to_string()));
    // Every escape-needing character, in an input value.
    for bad in [
        "a\"b", "a\\b", "a\nb", "a\tb", "a\rb", "a\u{1}b", "a\u{1f}b", "a\u{7f}b",
    ] {
        assert_eq!(
            manifest_toml(&h, 6, "binance", &nums, &input("src", bad), &ok_tm),
            refused("script.inputs.src"),
            "{bad:?}"
        );
    }
    // A quoted input key, the venue, the script file and symbol, a ticker-map key and value.
    let key = "my \"src\"";
    assert_eq!(
        manifest_toml(&h, 6, "binance", &nums, &input(key, "x"), &ok_tm),
        refused(&format!("script.inputs.{key}"))
    );
    assert_eq!(
        manifest_toml(&h, 6, "bin\"ance", &nums, &[], &ok_tm),
        refused("venue")
    );
    let hf = Handoff {
        script: "a\\b.pine".into(),
        ..handoff(PathBuf::from("/x"))
    };
    assert_eq!(
        manifest_toml(&hf, 6, "binance", &nums, &[], &ok_tm),
        refused("file")
    );
    let hs = Handoff {
        symbol_key: "binance_usdm:btc\nusdt".into(),
        ..handoff(PathBuf::from("/x"))
    };
    assert_eq!(
        manifest_toml(&hs, 6, "binance", &nums, &[], &ok_tm),
        refused("symbol")
    );
    for tm in [
        vec![("BINANCE:\"X".to_string(), "k".to_string())],
        vec![("BINANCE:X".to_string(), "k\\".to_string())],
    ] {
        let f = format!("script.symbols.{}", tm[0].0);
        assert_eq!(
            manifest_toml(&h, 6, "binance", &nums, &[], &tm),
            refused(&f)
        );
    }
    // Everything else passes through verbatim: spaces, ':', '.', non-ASCII.
    let m = manifest_toml(&h, 6, "binance", &nums, &input("src", "hl c3 · é"), &ok_tm)
        .expect("escape-free");
    assert!(m.contains("  src = \"hl c3 · é\"\n"));
    assert!(!m.contains('\\'));
}

#[test]
fn handoff_cutplan_is_impl_bp_format() {
    let w = runs();
    let c = cutplan_tsv(&w);
    let lines: Vec<&str> = c.lines().collect();
    assert_eq!(lines[0], "# pvm-cutplan-1");
    assert_eq!(
        lines[1],
        "cut\trun_id\trun_epoch_ns\toff_from_ns\toff_to_ns\troot"
    );
    assert_eq!(lines.len(), 2 + w.len());
    for (i, x) in w.iter().enumerate() {
        let f: Vec<&str> = lines[2 + i].split('\t').collect();
        let ep = if i < 4 { EPOCH_A } else { EPOCH_B };
        assert_eq!(
            f,
            vec![
                i.to_string(),
                x.run.clone(),
                ep.to_string(),
                x.off_from_ns.to_string(),
                x.off_to_ns.to_string(),
                format!("spool:{i}"),
            ]
        );
    }
}

#[test]
fn handoff_b3_argv_is_impl_bp_surface() {
    let h = handoff(PathBuf::from("/h/d"));
    let a = b3_argv(&h);
    let hex_t2 = String::from_utf8(hex32(&h.t2_trial_id).to_vec()).expect("hex");
    let seed = u64::from_le_bytes(h.null_trial_id[..8].try_into().expect("8"));
    let want: Vec<String> = [
        "backtest",
        "--strategy",
        "pine",
        "--pine-manifest",
        "/h/d/manifest.toml",
        "--trial-id",
        &hex_t2,
        "--cut-plan",
        "/h/d/cutplan.tsv",
        "--cut-spool",
        "/h/d/spool",
        "--window-ms",
        "1790035200000:1790121600000",
        "--split-at-ms",
        "1790035200000",
        "--warmup-bars-root",
        "/bars/root",
        "--warmup-to-ms",
        "1789999200000",
        "--fee-bps",
        "bn.perp:2:5",
        "--fee-bps",
        "hl.perp:1.5:4.5",
        "--null-arm",
        "sign-flip",
        "--null-seed",
        &seed.to_string(),
        "--rows-out",
        "/h/d/rows.t2",
        "--ret-dir",
        "/h/d/ret-t2",
        "--report-out",
        "/h/d/host.jsonl",
        "--out-dir",
        "/h/d/t2",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(a, want);
    for banned in ["--hold-ms", "--fee-measured", "--require-measured-fees"] {
        assert!(!a.iter().any(|x| x == banned), "{banned}");
    }
}
