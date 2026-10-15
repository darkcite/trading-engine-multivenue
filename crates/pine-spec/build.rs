// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! The pipeline of impl-00 §4.3: read → hash → parse → lower → validate → number → MPH → emit,
//! then the chart. Any failure is a `panic!` naming the file, the byte offset and the rule; a
//! malformed spec never produces a half-table.

#[path = "src/bf_lowering.rs"]
mod bf_lowering;
#[path = "build/chart.rs"]
mod chart;
#[path = "build/emit.rs"]
mod emit;
#[path = "build/json.rs"]
mod json;
#[path = "build/mph.rs"]
mod mph;
#[path = "build/schema.rs"]
mod schema;
#[path = "build/validate.rs"]
mod validate;

const SPEC: &str = "spec/builtins.json";
const CHART: &str = "../../tests/conformance/chart.json";

fn main() {
    for p in [SPEC, CHART, "build", "src/bf_lowering.rs"] {
        println!("cargo:rerun-if-changed={p}");
    }
    let out = match std::env::var("OUT_DIR") {
        Ok(o) => std::path::PathBuf::from(o),
        Err(e) => panic!("OUT_DIR: {e}"),
    };

    let src = std::fs::read(SPEC).unwrap_or_else(|e| panic!("{SPEC}: {e}"));
    let sha = core_crypto::sha256(&src);
    let dom = json::parse(&src).unwrap_or_else(|e| panic!("{SPEC}:{}: R02: {}", e.off, e.what));
    let model = schema::lower(&dom).unwrap_or_else(|e| panic!("{SPEC}:{e}"));
    validate::check(&model).unwrap_or_else(|e| panic!("{SPEC}:{e}"));
    let mut keys: Vec<&[u8]> = Vec::with_capacity(model.entries.len());
    for e in &model.entries {
        keys.push(e.full.as_bytes());
    }
    let table = mph::build(&keys).unwrap_or_else(|e| panic!("{SPEC}:0: {e}"));
    let gen = emit::spec(&model, &table, &sha);
    std::fs::write(out.join("spec_gen.rs"), gen).unwrap_or_else(|e| panic!("spec_gen.rs: {e}"));

    let csrc = std::fs::read(CHART).unwrap_or_else(|e| panic!("tests/conformance/chart.json: {e}"));
    let csha = core_crypto::sha256(&csrc);
    let c = chart::read(&csrc).unwrap_or_else(|e| panic!("tests/conformance/chart.json:{e}"));
    let cgen = emit::chart(&c, &csha);
    std::fs::write(out.join("chart_gen.rs"), cgen).unwrap_or_else(|e| panic!("chart_gen.rs: {e}"));
}
