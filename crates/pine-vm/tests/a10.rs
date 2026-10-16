// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Amendment A10 — `Verified::static_charge`: the verified static worst-case budget charge of
//! one evaluation of an acyclic program. Unit tests (loop, branchy, builtin-heavy, overflow,
//! sub-programs excluded) and a property test over random acyclic testkit programs asserting
//! that every evaluation's charge is `<=` the bound, and `==` it on single-path programs.

use pine_ir::*;
use pine_vm::testkit::*;
use pine_vm::*;

fn nop(_fr: &mut Frame<'_>, _site: SiteId, _a: &[u16]) -> Result<(), FaultKind> {
    Ok(())
}

/// Six zero-input, zero-output bodies that never fault, at assorted costs.
const COSTS: [u16; 6] = [1, 2, 7, 64, 1000, 65535];
static ENTRIES: [BuiltinEntry; 6] = [
    BuiltinEntry {
        f: nop,
        cost: COSTS[0],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: nop,
        cost: COSTS[1],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: nop,
        cost: COSTS[2],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: nop,
        cost: COSTS[3],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: nop,
        cost: COSTS[4],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
    BuiltinEntry {
        f: nop,
        cost: COSTS[5],
        n_out: 0,
        flags: 0,
        state_bytes_min: 0,
    },
];
static ARITY: [[u8; 2]; 6] = [[0, 0]; 6];
static TABLE: BuiltinTable = BuiltinTable {
    spec_sha256: [7; 32],
    entries: &ENTRIES,
    arity: &ARITY,
};

fn call(t: &mut Tb, e: u16) {
    let w = t.win(&[]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, e, w);
}

fn budget(t: &mut Tb, n: u32) {
    t.emit(op::BUDGET, 0, 0, n as u16, (n >> 16) as u16);
}

/// The interpreter's charge for one executed instruction, written independently of the
/// verifier (impl-03 §4.5 item 5; backward edges never reach here — the oracle is acyclic).
fn insn_charge(i: Insn) -> u64 {
    match i.op {
        op::BUDGET => (((i.b as u32) << 16) | i.a as u32) as u64,
        op::CALL => 1,
        op::CALL_BUILTIN => COSTS[i.a as usize] as u64,
        _ => 0,
    }
}

/// Brute force: the largest charge over every path from `pc` (no memo, no shared code).
fn oracle(code: &[Insn], pc: usize) -> u64 {
    let i = code[pc];
    let c = insn_charge(i);
    match i.op {
        op::HALT | op::TRAP => 0,
        op::JMP => oracle(code, i.target() as usize),
        op::CALL => c + oracle(code, i.target() as usize),
        op::JZ | op::JNZ | op::JNA_F | op::JNA_I => {
            oracle(code, pc + 1).max(oracle(code, i.target() as usize))
        }
        _ => c + oracle(code, pc + 1),
    }
}

/// Steps `bars` bars (Realtime: a tick before each confirmed bar from the midpoint on) and
/// returns `max_insns` after each step and whether any step faulted `InsnBudget`.
fn drive(v: Verified<'_>, p: &PineProgram, closes: &[f64]) -> (Vec<u32>, bool) {
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = VmInstance::new(v, a.slice());
    let mut ctx = Ctx { now: 0 };
    let mut seen = Vec::new();
    let mut budget_fault = false;
    let half = closes.len() / 2;
    for (i, c) in closes.iter().enumerate() {
        let rt = p.mode == ExecMode::Realtime && i >= half;
        if rt {
            let o = vm.step_tick(&bar(i as u32, *c, barflag::REALTIME), &mut ctx);
            budget_fault |= o == StepOutcome::Fault(FaultKind::InsnBudget);
            seen.push(vm.counters().max_insns);
        }
        let fl = if rt {
            barflag::CONFIRMED | barflag::REALTIME
        } else {
            barflag::CONFIRMED
        };
        let o = vm.step_bar(&bar(i as u32, *c, fl), &mut ctx);
        budget_fault |= o == StepOutcome::Fault(FaultKind::InsnBudget);
        seen.push(vm.counters().max_insns);
    }
    (seen, budget_fault)
}

#[test]
fn a10_loops_ret_and_backward_call_are_none() {
    // 0: NOP ; 1: JMP 0 ; 2: HALT — a back-edge.
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.emit(op::NOP, 0, 0, 0, 0);
    t.jump(op::JMP, 0, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(
        verify(&p, &TABLE)
            .expect("A10 refuses nothing")
            .static_charge(),
        None
    );

    // 0: CALL 2 (link) ; 1: HALT ; 2: BUDGET 3 ; 3: RET link — a subroutine returns backward.
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    let l = t.i(0);
    t.jump(op::CALL, 2, l);
    t.emit(op::HALT, 0, 0, 0, 0);
    budget(&mut t, 3);
    t.emit(op::RET, 0, 0, 0, l);
    let p = t.build();
    assert_eq!(verify(&p, &TABLE).unwrap().static_charge(), None);

    // 0: JMP 2 ; 1: HALT ; 2: CALL 1 (link) ; 3: HALT — a backward CALL is a back-edge.
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    let l = t.i(0);
    t.jump(op::JMP, 2, 0);
    t.emit(op::HALT, 0, 0, 0, 0);
    t.jump(op::CALL, 1, l);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(verify(&p, &TABLE).unwrap().static_charge(), None);

    // A self-jump (target == pc) is a back-edge too.
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(verify(&p, &TABLE).unwrap().static_charge(), None);
}

#[test]
fn a10_branchy_program_is_the_max_of_its_arms() {
    // 0: JZ isfirst -> 4 ; 1: BUDGET 40 ; 2: builtin 7 ; 3: JMP 6
    // 4: builtin 64 ; 5: BUDGET 3 ; 6: BUDGET 5 ; 7: HALT
    // isfirst arm (bar 0): 40 + 7 + 5 = 52; the other (bars >= 1): 64 + 3 + 5 = 72.
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    let j = t.jump(op::JZ, 0, abi::I_BS_ISFIRST);
    budget(&mut t, 40);
    call(&mut t, 2);
    let k = t.jump(op::JMP, 0, 0);
    let other = t.code.len() as u32;
    t.patch(j, other);
    call(&mut t, 3);
    budget(&mut t, 3);
    let end = t.code.len() as u32;
    t.patch(k, end);
    budget(&mut t, 5);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    assert_eq!(v.static_charge(), Some(72));
    assert_eq!(oracle(&p.code, 0), 72);
    let (seen, fault) = drive(v, &p, &[1.0, 2.0, 3.0]);
    assert!(!fault);
    assert_eq!(
        seen,
        vec![52, 72, 72],
        "the light arm, then the heavy arm attains the bound"
    );
}

#[test]
fn a10_builtin_heavy_program_sums_every_cost_and_is_attained() {
    // 60 builtin calls over every cost, a forward CALL (1) and a BUDGET with a high half.
    let mut t = Tb::new(ExecMode::Realtime, 1, 1, 0, 0);
    let l = t.i(0);
    let mut want: u64 = 0;
    let mut k = 0u16;
    while k < 60 {
        call(&mut t, k % 6);
        want += COSTS[(k % 6) as usize] as u64;
        k += 1;
    }
    let c = t.jump(op::CALL, 0, l);
    let to = t.code.len() as u32;
    t.patch(c, to);
    want += 1;
    budget(&mut t, (2 << 16) | 5);
    want += (2 << 16) | 5;
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = verify(&p, &TABLE).unwrap();
    assert_eq!(want, 10 * (1 + 2 + 7 + 64 + 1000 + 65535) + 1 + 131_077);
    assert_eq!(v.static_charge(), Some(want as u32));
    let (seen, fault) = drive(v, &p, &[1.0, 2.0, 3.0, 4.0]);
    assert!(!fault, "{want} < the 1_000_000 default budget");
    let mut i = 0usize;
    while i < seen.len() {
        assert_eq!(
            seen[i], want as u32,
            "single path: every bar and tick charges the bound"
        );
        i += 1;
    }
}

#[test]
fn a10_overflow_past_u32_is_none() {
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    budget(&mut t, u32::MAX);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(verify(&p, &TABLE).unwrap().static_charge(), Some(u32::MAX));
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    budget(&mut t, u32::MAX);
    budget(&mut t, 1);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    assert_eq!(verify(&p, &TABLE).unwrap().static_charge(), None);
}

#[test]
fn a10_subprograms_carry_their_own_bound() {
    let mut c = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    c.n_plots = 1;
    budget(&mut c, 1000);
    c.emit(op::HALT, 0, 0, 0, 0);
    let child = c.build();
    let mut t = Tb::new(ExecMode::BarClose, 1, 1, 0, 0);
    budget(&mut t, 5);
    t.emit(op::HALT, 0, 0, 0, 0);
    let mut p = t.build();
    p.subprograms.push(SubProgram {
        key: 1,
        symbol_key: String::from("binance:btcusdt"),
        symbol_hash: symbol_hash(b"binance:btcusdt"),
        timeframe_s: 3600,
        lookahead: false,
        gaps: false,
        n_out: 1,
        history: 2,
        program: Box::new(child),
    });
    assert_eq!(
        verify(&p, &TABLE).unwrap().static_charge(),
        Some(5),
        "the parent's own code"
    );
    let cv = verify(&p.subprograms[0].program, &TABLE).unwrap();
    assert_eq!(cv.static_charge(), Some(1000), "the child's own bound");
}

/// xorshift64* — deterministic, no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// One random acyclic program: ALU on bar-dependent slots, BUDGET, builtin calls, forward
/// conditional jumps on those slots (at most 10), forward JMP / CALL, interior HALTs.
fn random_program(r: &mut Rng) -> Tb {
    let mode = if r.below(2) == 0 {
        ExecMode::BarClose
    } else {
        ExecMode::Realtime
    };
    let mut t = Tb::new(mode, 4, 6, 0, 0);
    let consts = [t.k(0), t.k(1), t.k(2), t.k(3)];
    let link = t.i(5); // written by CALL only (rule 12)
    let n = 2 + r.below(46) as u32;
    let mut branches = 0u32;
    let mut pc = 0u32;
    while pc + 1 < n {
        let isrc = |r: &mut Rng, t: &Tb| -> u16 {
            match r.below(6) {
                0 => abi::I_BAR_INDEX,
                1 => abi::I_BS_ISFIRST,
                2 => abi::I_BS_ISCONFIRMED,
                3 => abi::I_BS_ISNEW,
                _ => t.i(r.below(5) as u32),
            }
        };
        let fwd = |r: &mut Rng| -> u32 { pc + 1 + r.below((n - 1 - pc) as u64) as u32 };
        let roll = r.below(100);
        if roll < 30 {
            let d = t.i(r.below(5) as u32);
            match r.below(7) {
                0 => {
                    let k = consts[r.below(4) as usize];
                    t.emit(op::I_CONST, flg::VLD, d, k, 0);
                }
                1 => {
                    let (a, b) = (isrc(r, &t), isrc(r, &t));
                    t.emit(op::I_MOD, flg::VLD, d, a, b);
                }
                2 => {
                    let (a, b) = (isrc(r, &t), isrc(r, &t));
                    t.emit(op::B_AND, 0, d, a, b);
                }
                3 => {
                    let a = isrc(r, &t);
                    t.emit(op::B_NOT, 0, d, a, 0);
                }
                4 => {
                    let f = t.f(r.below(4) as u32);
                    let g = t.f(r.below(4) as u32);
                    t.emit(op::F_ADD, 0, f, abi::F_CLOSE, g);
                }
                5 => {
                    // 0 / 0 on cold slots: NaN for JNA_F to see.
                    let (f, g, h) = (t.f(r.below(4) as u32), t.f(r.below(4) as u32), t.f(0));
                    t.emit(op::F_DIV, 0, f, g, h);
                }
                _ => {
                    let (a, b) = (isrc(r, &t), isrc(r, &t));
                    t.emit(op::I_ADD, flg::VLD, d, a, b);
                }
            }
        } else if roll < 45 {
            let hi = if r.below(8) == 0 { 1 } else { 0 };
            budget(&mut t, (hi << 16) | r.below(300) as u32);
        } else if roll < 65 {
            call(&mut t, r.below(6) as u16);
        } else if roll < 85 && branches < 10 {
            branches += 1;
            let to = fwd(r);
            match r.below(4) {
                0 => t.jump(op::JZ, to, isrc(r, &t)),
                1 => t.jump(op::JNZ, to, isrc(r, &t)),
                2 => {
                    let f = t.f(r.below(4) as u32);
                    t.jump(op::JNA_F, to, f)
                }
                _ => {
                    let s = isrc(r, &t);
                    t.jump(op::JNA_I, to, t.vidx_i(s))
                }
            };
        } else if roll < 91 {
            let to = fwd(r);
            t.jump(op::JMP, to, 0);
        } else if roll < 96 {
            let to = fwd(r);
            t.jump(op::CALL, to, link);
        } else {
            t.emit(op::HALT, 0, 0, 0, 0);
        }
        pc += 1;
    }
    t.emit(op::HALT, 0, 0, 0, 0);
    t
}

#[test]
fn a10_property_every_evaluation_charges_at_most_the_static_bound() {
    let mut r = Rng(0x9E37_79B9_7F4A_7C15);
    let closes = [1.0, 0.0, 3.0, 4.0, 0.0, 6.0, 7.0, 8.0, 9.0, 10.0];
    let (mut evals, mut exact, mut faulted) = (0u32, 0u32, 0u32);
    let mut prog = 0;
    while prog < 1500 {
        let mut t = random_program(&mut r);
        let want = oracle(&t.code, 0);
        // A quarter of the programs run under a budget the bound may exceed.
        if r.below(4) == 0 {
            let lo = t.code.len() as u64;
            t.budget = (lo + r.below(2 * want + 2)).min(u32::MAX as u64) as u32;
        }
        let p = t.build();
        let v = match verify(&p, &TABLE) {
            Ok(v) => v,
            Err(e) => panic!(
                "program {prog}: the generator broke rule {} at {}",
                e.rule, e.pc
            ),
        };
        let s = v.static_charge().expect("acyclic, below u32::MAX");
        assert_eq!(
            s as u64, want,
            "program {prog}: the sweep equals the brute-force max"
        );
        let (seen, fault) = drive(v, &p, &closes);
        let mut i = 0usize;
        while i < seen.len() {
            assert!(
                seen[i] <= s,
                "program {prog} step {i}: charged {} > bound {s}",
                seen[i]
            );
            i += 1;
        }
        evals += seen.len() as u32;
        if s < p.meta.insn_budget_per_eval {
            assert!(
                !fault,
                "program {prog}: a bound below the budget can never fault InsnBudget"
            );
        }
        faulted += fault as u32;
        let single = !p
            .code
            .iter()
            .any(|i| matches!(i.op, op::JZ | op::JNZ | op::JNA_F | op::JNA_I));
        if single && !fault {
            assert_eq!(
                seen.last().copied(),
                Some(s),
                "program {prog}: one path attains it"
            );
            exact += 1;
        }
        prog += 1;
    }
    assert!(
        evals > 15_000 && exact > 50 && faulted > 20,
        "{evals} {exact} {faulted}"
    );
}
