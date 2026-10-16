// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.6 `tests/layout.rs`: sema's layout is impl-03's `testkit::Tb::build` discipline fed
//! with the same demand (I5), in both modes.

mod common;
#[path = "common/gen.rs"]
mod gen;

use common::{compile_src, default_opts};
use pine_ir::{abi, ring_flag, Dialect, ExecMode, FrameLayout, PineProgram};
use pine_vm::testkit::Tb;

/// The `Tb` that describes `p`'s demand: slot counts, rings in sema's order, blocks, cells.
fn tb_of(p: &PineProgram) -> Tb {
    let l = &p.layout;
    let rf = u32::from(abi::RESERVED_F);
    let ri = u32::from(abi::RESERVED_I);
    let (nf, nfv) = (l.f64_varip_from - rf, l.f64_len - l.f64_varip_from);
    let (ni, niv) = (l.i64_varip_from - ri, l.i64_len - l.i64_varip_from);
    let mut t = Tb::new(p.mode, nf, ni, nfv, niv);
    t.dialect = p.dialect;
    for r in &l.rings {
        t.ring(r.slot, r.lane, r.flags, r.mask + 1);
    }
    for s in &l.states {
        t.state_with(s.kind, s.bytes, s.flags);
    }
    let cb = l.str_cell_bytes;
    let ring_cells: u32 = l
        .rings
        .iter()
        .filter(|r| r.flags & ring_flag::STRING != 0)
        .map(|r| r.mask + 1)
        .sum();
    if cb > 0 {
        t.cells = (l.str_persist_varip_off - l.str_persist_off) / cb - ring_cells;
        t.varip_cells = (l.str_persist_off + l.str_persist_bytes - l.str_persist_varip_off) / cb;
    } else {
        t.cells = 0;
        t.varip_cells = 0;
    }
    t.cell_bytes = cb;
    t.scratch = l.str_scratch_bytes;
    t.n_plots = l.n_plots;
    t.intent_cap = l.intent_cap;
    t.max_bars_back = p.meta.max_bars_back;
    t
}

fn check(src: &str) {
    for d in [Dialect::V5, Dialect::V6] {
        for mode in [ExecMode::BarClose, ExecMode::Realtime] {
            let src = format!("//@version={}\n{src}", d as u8);
            let p = match compile_src(&src, &default_opts(d, mode)) {
                Ok(p) => p,
                Err(e) => panic!("{e:#?}\n{src}"),
            };
            let want: FrameLayout = tb_of(&p).build().layout;
            assert_eq!(p.layout, want, "{d:?} {mode:?}\n{src}");
        }
    }
}

#[test]
fn layout_equals_testkit_builder() {
    // Blocks, intents, a plot.
    check(
        "strategy(\"x\")\nf = ta.sma(close, 10)\ns = ta.sma(close, 300)\n\
         if ta.crossover(f, s)\n    strategy.entry(\"L\", strategy.long)\nplot(f)\n",
    );
    // Rings (f64, i64 na-tracked), a string variable cell and a string ring.
    check(
        "indicator(\"t\")\nstring s = bar_index % 2 == 0 ? \"a\" : \"b\"\nint n = bar_index\n\
         plot(close[3] + n[2])\nplot(s[1] == \"a\" ? 1 : 0)\n",
    );
    // varip lanes (I-6 padding) and a varip string cell.
    check(
        "indicator(\"t\")\nvarip int k = 0\nk += 1\nvarip float z = 0.0\nz += close\n\
         varip string w = \"\"\nw := \"x\"\nplot(k + z)\n",
    );
}

#[test]
fn layout_empty_program_is_valid() {
    check("indicator(\"t\")\nplot(close)\n");
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig {
        cases: 256,
        ..proptest::prelude::ProptestConfig::default()
    })]

    /// I6 (§7.6): for every `Ok` program of the generator — both dialects, both modes — the
    /// layout passes the verifier (I-1…I-8, `compile_src`), sema's own audit recount accepted it
    /// (an `Ok` is only returned past `audit::recount`), and pine-vm's testkit builder fed the
    /// same demand builds the identical `FrameLayout` region by region.
    #[test]
    fn prop_layout_supply_equals_demand(
        parts in proptest::collection::vec(
            (0u8..gen::TEMPLATES, 0usize..8, 0usize..8, 2u32..40),
            1..8,
        )
    ) {
        for d in [Dialect::V5, Dialect::V6] {
            for mode in [ExecMode::BarClose, ExecMode::Realtime] {
                let src = gen::script(d, &parts);
                if let Ok(p) = compile_src(&src, &default_opts(d, mode)) {
                    let want: FrameLayout = tb_of(&p).build().layout;
                    proptest::prop_assert_eq!(&p.layout, &want, "{:?} {:?}\n{}", d, mode, src);
                }
            }
        }
    }
}
