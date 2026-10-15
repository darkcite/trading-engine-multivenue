// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hashes, the dependency set, `src/` hygiene, codegen determinism and the chart (impl-00 §7:
//! I11–I14, I33–I35, I37).

#[path = "../src/bf_lowering.rs"]
mod bf_lowering;
#[path = "../build/chart.rs"]
mod chart;
#[path = "../build/emit.rs"]
mod emit;
#[path = "../build/json.rs"]
mod json;
#[path = "../build/mph.rs"]
mod mph;
#[path = "../build/schema.rs"]
mod schema;
#[path = "../build/validate.rs"]
mod validate;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

fn read(rel: &str) -> Vec<u8> {
    std::fs::read(format!("{MANIFEST}/{rel}")).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

#[test]
fn spec_sha256_matches_file() {
    assert_eq!(
        pine_spec::SPEC_SHA256,
        core_crypto::sha256(&read("spec/builtins.json"))
    );
}

#[test]
fn spec_captured_date_is_pinned() {
    assert!(chart::midnight_ms(pine_spec::SPEC_CAPTURED_UTC).is_some());
}

/// The dependency names of one `[section]` of the manifest.
fn deps(toml: &str, section: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            inside = t == section;
            continue;
        }
        if inside && !t.is_empty() && !t.starts_with('#') {
            if let Some((k, _)) = t.split_once('=') {
                out.push(k.trim().to_string());
            }
        }
    }
    out.sort();
    out
}

#[test]
fn dependency_sets_are_exact() {
    let toml = String::from_utf8(read("Cargo.toml")).expect("utf-8");
    assert_eq!(deps(&toml, "[dependencies]"), ["pine-ir"]);
    let b = deps(&toml, "[build-dependencies]");
    assert!(
        b.iter().all(|d| d == "pine-ir" || d == "core-crypto"),
        "{b:?}"
    );
    let d = deps(&toml, "[dev-dependencies]");
    assert!(
        d.iter().all(|x| x == "core-crypto" || x == "proptest"),
        "{d:?}"
    );
}

#[test]
fn src_is_const_only() {
    let lib = String::from_utf8(read("src/lib.rs")).expect("utf-8");
    assert!(lib.contains("#![no_std]") && lib.contains("#![forbid(unsafe_code)]"));
    let banned = [
        "extern crate",
        "static mut",
        "OnceLock",
        "lazy_static",
        "thread_local!",
        "Cell<",
        "RefCell",
        "Atomic",
    ];
    for f in [
        "lib.rs",
        "model.rs",
        "lookup.rs",
        "bf_lowering.rs",
        "chart.rs",
    ] {
        let src = String::from_utf8(read(&format!("src/{f}"))).expect("utf-8");
        for b in banned {
            assert!(!src.contains(b), "src/{f} contains {b}");
        }
    }
}

fn generate() -> String {
    let src = read("spec/builtins.json");
    let dom = json::parse(&src).expect("parses");
    let m = schema::lower(&dom).expect("lowers");
    validate::check(&m).expect("validates");
    let keys: Vec<&[u8]> = m.entries.iter().map(|e| e.full.as_bytes()).collect();
    let t = mph::build(&keys).expect("mph");
    emit::spec(&m, &t, &core_crypto::sha256(&src))
}

#[test]
fn codegen_is_deterministic() {
    let a = generate();
    let b = generate();
    assert_eq!(a, b);
    let built = std::fs::read_to_string(concat!(env!("OUT_DIR"), "/spec_gen.rs")).expect("out");
    assert_eq!(a, built);
    assert!(a.starts_with(emit::HEADER));
    let csrc = read(CHART);
    let c = chart::read(&csrc).expect("chart");
    let cg = emit::chart(&c, &core_crypto::sha256(&csrc));
    let cbuilt = std::fs::read_to_string(concat!(env!("OUT_DIR"), "/chart_gen.rs")).expect("out");
    assert_eq!(cg, cbuilt);
}

const CHART: &str = "../../tests/conformance/chart.json";

// Independent re-check of CH4/CH5 on the emitted constants (CH7 needs the date: below).
const _: () = {
    use pine_spec::chart as k;
    assert!(k::FROM_MS > 0 && k::FROM_MS % 3_600_000 == 0);
    assert!(k::BARS % 24 == 0 && k::BARS >= 2160 && k::BARS <= 4320);
    assert!(k::WARMUP_BARS % 24 == 0 && k::WARMUP_BARS >= 720);
};

#[test]
fn chart_sha256_matches_file() {
    assert_eq!(pine_spec::chart::SHA256, core_crypto::sha256(&read(CHART)));
}

#[test]
fn chart_definition_is_valid() {
    let c = chart::read(&read(CHART)).expect("CH1-CH7");
    use pine_spec::chart as k;
    assert_eq!(c.frozen_utc, k::FROZEN_UTC);
    assert_eq!(
        (c.tv_symbol.as_str(), c.symbol_key.as_str()),
        (k::SYMBOL, k::SYMBOL_KEY)
    );
    assert_eq!(c.candles_descriptor, k::CANDLES_DESCRIPTOR);
    assert_eq!(
        (c.timeframe_tv.as_str(), c.timeframe_s),
        (k::TIMEFRAME_TV, k::TIMEFRAME_S)
    );
    assert_eq!(
        (c.timezone.as_str(), c.chart_type.as_str()),
        (k::TIMEZONE, k::CHART_TYPE)
    );
    assert_eq!((c.bar_magnifier, c.from_ms), (k::BAR_MAGNIFIER, k::FROM_MS));
    assert_eq!((c.bars, c.warmup_bars), (k::BARS, k::WARMUP_BARS));
    assert_eq!(
        k::SYMBOL_HASH,
        pine_ir::symbol_hash(k::SYMBOL_KEY.as_bytes())
    );
    let fz = chart::midnight_ms(k::FROZEN_UTC).expect("date");
    assert!(k::TO_MS <= fz - 48 * 3_600_000);
    assert_eq!(k::REPLAY_FROM_MS, k::FROM_MS - 720 * 3_600_000);
}

#[test]
fn chart_has_no_price_data() {
    let src = read(CHART);
    let root = json::parse(&src).expect("parses");
    let json::Val::Obj(fields) = &root.v else {
        panic!("object")
    };
    let keys: Vec<&str> = fields.iter().map(|(k, _)| k.as_ref()).collect();
    assert_eq!(keys, chart::KEYS);
    for (k, v) in fields {
        assert!(
            !matches!(
                v.v,
                json::Val::Arr(_) | json::Val::Obj(_) | json::Val::Num(_)
            ),
            "{k}"
        );
    }
}

#[test]
fn each_invalid_chart_names_its_rule() {
    let dir = format!("{MANIFEST}/tests/fixtures/chart_invalid");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("chart_invalid/")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names.len(), 7, "{names:?}");
    for (i, name) in names.iter().enumerate() {
        let rule: u8 = name[2..3].parse().expect("ch<N>_ prefix");
        assert_eq!(rule as usize, i + 1, "{name}");
        let src = std::fs::read(format!("{dir}/{name}")).expect("fixture");
        match chart::read(&src) {
            Ok(_) => panic!("{name}: accepted"),
            Err(e) => {
                let msg = e.to_string();
                assert_eq!(e.rule, rule, "{name}: {msg}");
                assert!(
                    msg.starts_with(&format!("{}: CH{rule}: ", e.off)),
                    "{name}: {msg}"
                );
            }
        }
    }
}

/// Landing guard: the authored file carries no "TODO:" and no trace of the authoring lane's
/// validation merger (`author/tools/merge.py`). The merger substitutes ordinary-looking fallback
/// values in place (nothing in-band marks them) and records each substitution in a
/// `fallbacks.json` it writes beside any output named `builtins.json`; that sidecar, or its
/// note, next to or inside the spec means the file is a check artifact, not the authored one.
#[test]
fn builtins_has_no_merger_placeholder() {
    let src = String::from_utf8(read("spec/builtins.json")).expect("utf-8");
    assert!(!src.contains("TODO:"), "spec/builtins.json holds a TODO: string");
    assert!(
        !src.contains("validation-only fallback"),
        "spec/builtins.json holds the merger note"
    );
    let sidecar = std::path::Path::new(MANIFEST).join("spec/fallbacks.json");
    assert!(
        !sidecar.exists(),
        "spec/fallbacks.json: builtins.json is a merger check artifact"
    );
}
