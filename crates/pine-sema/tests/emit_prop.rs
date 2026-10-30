// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.6 / §7.7: `prop_emitted_programs_verify` — every program sema returns `Ok` for
//! passes `pine_vm::verify` against pine-std's table, in both dialects and both modes, and runs
//! 64 bars on the VM without a fault. The generator composes small strategies from statement
//! templates over the earlier variables (T28 grows it into `arb_script`).

mod common;
#[path = "common/gen.rs"]
mod gen;

use common::{compile_src, default_opts};
use gen::{script, TEMPLATES};
use pine_ir::{barflag, Dialect, ExecMode, StepOutcome};
use pine_vm::testkit::{bar, Arena, Ctx};
use proptest::prelude::*;

fn run(src: &str, d: Dialect, mode: ExecMode) -> bool {
    let Ok(p) = compile_src(src, &default_opts(d, mode)) else {
        return false;
    };
    // compile_src already asserted verify; run it.
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut arena = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, arena.slice());
    let mut ctx = Ctx { now: 0 };
    for i in 0..64u32 {
        let c = 100.0 + 10.0 * (f64::from(i) * 0.37).sin() + f64::from(i % 7);
        let out = vm.step_bar(&bar(i, c, barflag::CONFIRMED), &mut ctx);
        assert_eq!(out, StepOutcome::Ok, "bar {i} {:?}\n{src}", vm.last_fault());
    }
    true
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    #[test]
    fn prop_emitted_programs_verify(
        parts in prop::collection::vec((0u8..TEMPLATES, 0usize..8, 0usize..8, 2u32..40), 1..8)
    ) {
        for d in [Dialect::V5, Dialect::V6] {
            for mode in [ExecMode::BarClose, ExecMode::Realtime] {
                run(&script(d, &parts), d, mode);
            }
        }
    }
}

/// The generator reaches emission: every template compiles to an `Ok` program on its own.
#[test]
fn prop_templates_each_compile() {
    for t in 0u8..TEMPLATES {
        for d in [Dialect::V5, Dialect::V6] {
            let src = script(d, &[(t, 0, 0, 5)]);
            assert!(
                run(&src, d, ExecMode::BarClose),
                "template {t} v{}:\n{src}\n{:?}",
                d as u8,
                compile_src(&src, &default_opts(d, ExecMode::BarClose)).err()
            );
        }
    }
}
