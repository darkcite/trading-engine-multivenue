// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

use pine_ir::*;
use pine_vm::*;
use pine_vm::testkit::stub::*;
use pine_vm::testkit::*;

/// A Realtime reference program touching every checked feature.
fn reference() -> Tb {
    let mut t = Tb::new(ExecMode::Realtime, 3, 6, 1, 0);
    t.n_plots = 1;
    let rf = t.ring(abi::F_CLOSE, Lane::F64, 0, 4);
    let _ri = t.ring(t.i(0), Lane::I64, ring_flag::NA_TRACKED, 2);
    let _rs = t.ring(t.i(4), Lane::I64, ring_flag::NA_TRACKED | ring_flag::STRING, 2);
    let acc = t.state(StateKind::Builtin, 8);
    let arr = t.state(StateKind::Collection, 32 + 8 * 4);
    let obj = t.state(StateKind::Object, 32 + 16);
    let cell = t.cell();
    let (f0, f1, i0, i1, i2, i3, s) = (t.f(0), t.f(1), t.i(0), t.i(1), t.i(2), t.i(3), t.i(4));
    let ka = t.kstr("hi");
    let w = t.win(&[argwin::f64(abi::F_CLOSE), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, acc, ACC, w); // 0
    t.emit(op::HIST_F, 0, f1, rf, 1); // 1
    t.emit(op::O_NEW, 0, i1, obj, 2); // 2
    t.emit(op::O_SET_F, 0, i1, 0, f1); // 3
    t.emit(op::I_CONST, flg::VLD, i2, ka, 0); // 4
    t.emit(op::S_PERSIST, 0, s, i2, cell); // 5
    let kz = t.k(0);
    t.emit(op::I_CONST, flg::VLD, i3, kz, 0); // 6
    let wa = t.win(&[argwin::i64(i3), argwin::i64(i0)]);
    t.emit(op::CALL_BUILTIN, 0, arr, ARR_NEW, wa); // 7
    let wp = t.win(&[argwin::i64(i3), argwin::f64(f0)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, PLOT, wp); // 8
    t.emit(op::HALT, 0, 0, 0, 0); // 9
    t
}

fn rule_of(p: &PineProgram) -> u16 {
    verify(p, &TABLE).err().map(|e| e.rule).unwrap_or(0)
}

#[test]
fn verify_accepts_the_reference_program() {
    let p = reference().build();
    assert!(verify(&p, &TABLE).is_ok(), "{:?}", verify(&p, &TABLE).err());
    assert_eq!(VerifyError { rule: 8, pc: 0, detail: 0 }.code(), DiagCode(4008));
}

#[test]
fn rule_01_regions_overlap_and_empty_region_offset() {
    let mut p = reference().build();
    p.layout.f64_off = 0;
    assert_eq!(rule_of(&p), rule::REGIONS);
    let mut p = reference().build();
    p.layout.plots_off += 64; // plots region is non-empty; now past intents_off
    assert_eq!(rule_of(&p), rule::REGIONS);
    let mut t = reference();
    t.n_plots = 1;
    let mut p = t.build();
    p.layout.intent_cap = 0; // already empty: move it off the next region's offset
    p.layout.intents_off += 64;
    assert_eq!(rule_of(&p), rule::REGIONS);
}

#[test]
fn rule_02_valid_words() {
    let mut p = reference().build();
    p.layout.valid_words += 1;
    assert_eq!(rule_of(&p), rule::VALID_WORDS);
}

#[test]
fn rule_03_lane_minima() {
    let mut p = reference().build();
    p.layout.i64_len = 9;
    p.layout.i64_varip_from = 9;
    p.layout.valid_words = (p.layout.f64_len + 9).div_ceil(64);
    assert_eq!(rule_of(&p), rule::LANE_MINIMA);
}

#[test]
fn rule_04_ring_shape() {
    let mut p = reference().build();
    p.layout.rings[0].mask = 2; // capacity 3
    assert_eq!(rule_of(&p), rule::RING_SHAPE);
    let mut p = reference().build();
    p.layout.rings[0].flags = ring_flag::NA_TRACKED; // f64 rings carry no flags
    assert_eq!(rule_of(&p), rule::RING_SHAPE);
}

#[test]
fn rule_05_ring_overlap() {
    let mut p = reference().build();
    p.layout.rings[1].off = p.layout.rings[0].off;
    assert_eq!(rule_of(&p), rule::RING_OVERLAP);
}

#[test]
fn rule_06_state_dense() {
    let mut p = reference().build();
    p.layout.states[0].site = SiteId(1);
    assert_eq!(rule_of(&p), rule::STATE_DENSE);
}

#[test]
fn rule_07_opcode_retired_numbers_are_rejected() {
    let mut p = reference().build();
    p.code[1].op = 0x3C; // retired S_EQ
    assert_eq!(rule_of(&p), rule::OPCODE);
    // §3.6.1's prose says 0x85–0x98, but 0x90 is S_PERSIST (op::defined is the truth; CAR-03-3).
    let mut retired: Vec<u8> = vec![0x3C, 0x3D, 0x52, 0x53, 0x54, 0xA0, 0xA1, 0xB0, 0xB1];
    retired.extend(0x85..=0x8Fu8);
    retired.extend(0x91..=0x98u8);
    retired.extend(0x9A..=0x9Fu8);
    retired.extend(0xC0..=0xC6u8);
    assert!(pine_ir::op::defined(0x90), "0x90 is S_PERSIST");
    for r in retired {
        let mut p = reference().build();
        p.code[1].op = r;
        assert_eq!(rule_of(&p), rule::OPCODE, "{r:#04x}");
    }
}

#[test]
fn rule_08_operand_range_reserved_write_and_flags() {
    let mut p = reference().build();
    p.code[1].dst = p.layout.f64_len as u16;
    assert_eq!(rule_of(&p), rule::OPERAND);
    let mut p = reference().build();
    p.code[1].dst = abi::F_CLOSE; // a write into the reserved ABI
    assert_eq!(rule_of(&p), rule::OPERAND);
    let mut p = reference().build();
    p.code[1].flags = flg::K_CONST | flg::K_DYNAMIC;
    assert_eq!(rule_of(&p), rule::OPERAND);
}

#[test]
fn rule_09_jump_target() {
    let mut t = reference();
    let end = t.code.len() as u32 + 5;
    t.code.insert(0, Insn::new(op::JMP, 0, (end >> 16) as u16, end as u16, 0));
    let p = t.build();
    assert_eq!(rule_of(&p), rule::JUMP_TARGET);
}

#[test]
fn rule_10_termination() {
    let mut t = reference();
    t.emit(op::NOP, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(rule_of(&p), rule::TERMINATION);
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.jump(op::JMP, 0, 0); // ends in JMP, but no HALT exists
    assert_eq!(rule_of(&t.build()), rule::TERMINATION);
}

#[test]
fn rule_11_code_budget() {
    let mut t = reference();
    t.budget = t.code.len() as u32 - 1;
    assert_eq!(rule_of(&t.build()), rule::CODE_BUDGET);
}

#[test]
fn rule_12_link_slot() {
    let mut t = reference();
    let halt = t.code.pop().unwrap();
    t.emit(op::RET, 0, 0, 0, t.i(5)); // never written by a CALL
    t.code.push(halt);
    assert_eq!(rule_of(&t.build()), rule::LINK_SLOT);
    let mut t = Tb::new(ExecMode::BarClose, 1, 3, 0, 0);
    let l = t.i(0);
    t.jump(op::CALL, 3, l); // 0
    t.emit(op::I_MOV, 0, l, t.i(1), 0); // 1: a non-CALL write into the link slot
    t.emit(op::HALT, 0, 0, 0, 0); // 2
    t.emit(op::RET, 0, 0, 0, l); // 3
    assert_eq!(rule_of(&t.build()), rule::LINK_SLOT);
}

#[test]
fn rule_13_const_hist_depth() {
    let mut p = reference().build();
    p.code[1].b = 4; // ring 0 has mask 3
    assert_eq!(rule_of(&p), rule::CONST_HIST_DEPTH);
    let mut p = reference().build();
    p.consts.push((-1i64) as u64);
    p.code[1].flags = flg::K_CONST;
    p.code[1].b = (p.consts.len() - 1) as u16;
    assert_eq!(rule_of(&p), rule::CONST_HIST_DEPTH);
}

#[test]
fn rule_15_object_ops() {
    let mut p = reference().build();
    p.code[2].b = 3; // 32 + 24 > 48-byte block
    assert_eq!(rule_of(&p), rule::OBJECT_OPS);
    let mut p = reference().build();
    p.code[3].a = 2; // field 2 >= the 2 fields O_NEW creates
    assert_eq!(rule_of(&p), rule::OBJECT_OPS);
    let mut p = reference().build();
    let dup = p.code[2];
    p.code.insert(3, dup); // a second creating instruction for the same Object site
    assert_eq!(rule_of(&p), rule::OBJECT_OPS);
}

#[test]
fn rule_16_varip_word_boundary() {
    let mut t = Tb::new(ExecMode::Realtime, 1, 1, 0, 2);
    t.emit(op::HALT, 0, 0, 0, 0);
    let mut p = t.build();
    p.layout.i64_varip_from -= 1; // breaks I-6
    assert_eq!(rule_of(&p), rule::VARIP);
}

#[test]
fn rule_17_out_regions() {
    let mut p = reference().build();
    p.meta.plots.clear();
    assert_eq!(rule_of(&p), rule::OUT_REGIONS);
    let mut t = reference();
    t.intent_cap = 1; // no INTENT builtin is called
    assert_eq!(rule_of(&t.build()), rule::OUT_REGIONS);
}

#[test]
fn rule_18_journal_bound_and_barclose_zero() {
    let mut p = reference().build();
    p.layout.jrnl_bytes -= 8;
    assert_eq!(rule_of(&p), rule::JOURNAL);
    let mut t = reference();
    t.mode = ExecMode::BarClose;
    let mut p = t.build();
    p.layout.jrnl_bytes = 8;
    p.layout.total_bytes += 64;
    assert_eq!(rule_of(&p), rule::JOURNAL);
}

#[test]
fn rule_19_subprogram() {
    let mut p = reference().build();
    let child = reference().build();
    p.subprograms.push(SubProgram {
        key: 1,
        symbol_key: String::from("binance:btcusdt"),
        symbol_hash: symbol_hash(b"binance:btcusdt"),
        timeframe_s: 3600,
        lookahead: false,
        gaps: false,
        n_out: 0,
        history: 2,
        program: Box::new(child),
    });
    assert_eq!(rule_of(&p), rule::SUBPROGRAM);
    let sub = |child: PineProgram| SubProgram {
        key: 2,
        symbol_key: String::from("binance:btcusdt"),
        symbol_hash: symbol_hash(b"binance:btcusdt"),
        timeframe_s: 3600,
        lookahead: false,
        gaps: false,
        n_out: 1,
        history: 2,
        program: Box::new(child),
    };
    let mut t = reference();
    t.mode = ExecMode::BarClose;
    let mut p = reference().build();
    p.subprograms.push(sub(t.build())); // a BarClose child under a Realtime parent
    assert_eq!(rule_of(&p), rule::SUBPROGRAM);
    let mut bad = reference().build();
    bad.code[1].dst = bad.layout.f64_len as u16; // the child breaks rule 8 at pc 1
    let mut p = reference().build();
    p.subprograms.push(sub(bad));
    let e = verify(&p, &TABLE).unwrap_err();
    assert_eq!((e.rule, e.pc, e.detail), (rule::SUBPROGRAM, 1, rule::OPERAND as u32));
}

#[test]
fn rule_20_ring_cap() {
    let mut p = reference().build();
    p.meta.max_bars_back = 1; // bound next_pow2(2) = 2 < ring 0's capacity 4 (CAR-03-2)
    assert_eq!(rule_of(&p), rule::RING_CAP);
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.max_bars_back = 1;
    let r = t.ring(abi::F_CLOSE, Lane::F64, 0, 2); // x[1] needs capacity 2
    t.emit(op::HIST_F, 0, t.f(0), r, 1);
    t.emit(op::HALT, 0, 0, 0, 0);
    assert!(verify(&t.build(), &TABLE).is_ok(), "the literal rule (1.next_power_of_two() = 1) would refuse x[1]");
}

#[test]
fn rule_21_operand_width() {
    let mut p = reference().build();
    p.consts.resize(65_537, 0);
    assert_eq!(rule_of(&p), rule::OPERAND_WIDTH);
}

#[test]
fn rule_22_arg_win() {
    let mut p = reference().build();
    p.code[0].b = p.argwin.len() as u16;
    assert_eq!(rule_of(&p), rule::ARG_WIN);
    let mut p = reference().build();
    let w = p.code[0].b as usize;
    p.argwin[w + 2] = argwin::f64(abi::F_CLOSE); // the output entry names a reserved slot
    assert_eq!(rule_of(&p), rule::ARG_WIN);
}

#[test]
fn rule_23_site_kind() {
    let mut p = reference().build();
    p.code[0].dst = SiteId::NONE.0; // a STATEFUL entry without a site
    assert_eq!(rule_of(&p), rule::SITE_KIND);
    let mut p = reference().build();
    p.code[8].dst = 0; // a stateless entry with a site
    assert_eq!(rule_of(&p), rule::SITE_KIND);
    let mut p = reference().build();
    let dup = p.code[0];
    p.code.insert(1, dup); // two call instructions share one Builtin site
    assert_eq!(rule_of(&p), rule::SITE_KIND);
}

#[test]
fn rule_24_spec_hash() {
    let mut p = reference().build();
    p.meta.spec_sha256 = [0; 32];
    assert_eq!(rule_of(&p), rule::SPEC_HASH);
}

#[test]
fn rule_25_str_cells() {
    let mut p = reference().build();
    p.code[5].b = 1; // cell 1 is the first STRING ring cell
    assert_eq!(rule_of(&p), rule::STR_CELLS);
    let mut p = reference().build();
    p.code[5].b = 50; // outside str_persist
    assert_eq!(rule_of(&p), rule::STR_CELLS);
    let mut t = Tb::new(ExecMode::Realtime, 0, 3, 0, 1);
    let rel = t.varip_cell();
    let vc = t.varip_cell_index(rel);
    let ka = t.kstr("a");
    t.emit(op::I_CONST, flg::VLD, t.i(0), ka, 0);
    t.emit(op::S_PERSIST, 0, t.i(1), t.i(0), vc); // a non-varip dst into a varip cell
    t.emit(op::HALT, 0, 0, 0, 0);
    assert_eq!(rule_of(&t.build()), rule::STR_CELLS);
    let mut t = Tb::new(ExecMode::Realtime, 0, 3, 0, 0);
    let c = t.cell();
    let ka = t.kstr("a");
    t.emit(op::I_CONST, flg::VLD, t.i(0), ka, 0);
    t.emit(op::S_PERSIST, 0, t.i(1), t.i(0), c);
    t.emit(op::S_PERSIST, 0, t.i(2), t.i(0), c); // two variables share one cell
    t.emit(op::HALT, 0, 0, 0, 0);
    assert_eq!(rule_of(&t.build()), rule::STR_CELLS);
}

#[test]
fn rule_numbers_are_the_frozen_set() {
    let all = [
        rule::REGIONS, rule::VALID_WORDS, rule::LANE_MINIMA, rule::RING_SHAPE, rule::RING_OVERLAP,
        rule::STATE_DENSE, rule::OPCODE, rule::OPERAND, rule::JUMP_TARGET, rule::TERMINATION,
        rule::CODE_BUDGET, rule::LINK_SLOT, rule::CONST_HIST_DEPTH, rule::OBJECT_OPS, rule::VARIP,
        rule::OUT_REGIONS, rule::JOURNAL, rule::SUBPROGRAM, rule::RING_CAP, rule::OPERAND_WIDTH,
        rule::ARG_WIN, rule::SITE_KIND, rule::SPEC_HASH, rule::STR_CELLS,
    ];
    let want: Vec<u16> = (1..=25).filter(|n| *n != 14).collect();
    assert_eq!(all.to_vec(), want, "rules 1..=25, 14 retired and never reused");
}
