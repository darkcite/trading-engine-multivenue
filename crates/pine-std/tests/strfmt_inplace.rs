// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-04 §4.13.3 — number rendering writes straight into the lent range (the zero-copy audit's
//! string-formatting findings): results past the old 1 KiB render buffer, the integer-digit cap
//! at run time, ±Infinity under a pattern, and `str.format`'s referenced-only string lends.
//! Driven through pine-vm's program builder on a large scratch arena (the harness's is 256 B).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_ir::*;
use pine_spec::ov;
use pine_vm::testkit::{bar, Arena, Ctx, Tb};

/// One input of the call under test.
enum In {
    /// A CONST string.
    S(String),
    /// An na string (the slot is left invalid).
    NaS,
    I(i64),
    F(f64),
}

fn s(x: &str) -> In {
    In::S(x.to_string())
}

/// Run `id` once on `ins` with a `scratch`-byte arena, then `$op.eq_str(out, expect)`:
/// `Ok(true)` when the result is exactly `expect`, `Err(kind)` on a fault.
fn run(id: OverloadId, ins: &[In], expect: &str, scratch: u32) -> Result<bool, FaultKind> {
    let mut t = Tb::new(ExecMode::BarClose, 24, 24, 0, 0);
    t.scratch = scratch;
    t.spec = pine_spec::SPEC_SHA256;
    let (out, eq) = (t.i(20), t.i(21));
    let mut win = Vec::new();
    for (j, x) in ins.iter().enumerate() {
        let j = j as u32;
        match x {
            In::S(v) => {
                let c = t.kstr(v);
                t.emit(op::I_CONST, flg::VLD, t.i(j), c, 0);
                win.push(argwin::i64(t.i(j)));
            }
            In::NaS => {
                let c = t.k(0);
                t.emit(op::I_CONST, 0, t.i(j), c, 0);
                win.push(argwin::i64(t.i(j)));
            }
            In::I(v) => {
                let c = t.k(*v as u64);
                t.emit(op::I_CONST, flg::VLD, t.i(j), c, 0);
                win.push(argwin::i64(t.i(j)));
            }
            In::F(v) => {
                let c = t.kf(*v);
                t.emit(op::F_CONST, 0, t.f(j), c, 0);
                win.push(argwin::f64(t.f(j)));
            }
        }
    }
    win.push(argwin::i64(out));
    let w = t.win(&win);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, id.0, w);
    let e = t.kstr(expect);
    t.emit(op::I_CONST, flg::VLD, t.i(22), e, 0);
    let w = t.win(&[argwin::i64(out), argwin::i64(t.i(22)), argwin::i64(eq)]);
    t.emit(op::CALL_BUILTIN, 0, SiteId::NONE.0, ov::OP_EQ_STR.0, w);
    t.emit(op::HALT, 0, 0, 0, 0);
    let p = t.build();
    let v = pine_vm::verify(&p, &pine_std::BUILTINS).expect("verifies");
    let mut a = Arena::new(p.layout.total_bytes as usize);
    let mut vm = pine_vm::VmInstance::new(v, a.slice());
    match vm.step_bar(&bar(0, 1.0, barflag::CONFIRMED), &mut Ctx { now: 0 }) {
        StepOutcome::Ok => {}
        StepOutcome::Fault(k) => return Err(k),
    }
    let img = pine_vm::testkit::arena(&vm);
    let at = p.layout.i64_off as usize + 8 * eq as usize;
    Ok(u64::from_le_bytes(img[at..at + 8].try_into().unwrap()) == 1)
}

/// The exact decimal digits of `x` (Rust's exact fixed rendering) grouped by three.
fn grouped(x: f64) -> String {
    let d = format!("{:.0}", x.abs());
    let mut g = String::new();
    for (k, c) in d.chars().enumerate() {
        if k > 0 && (d.len() - k) % 3 == 0 {
            g.push(',');
        }
        g.push(c);
    }
    g
}

/// Audit item 3: `str.tostring(x, pattern)` sizes, takes the exact range and renders sign,
/// unquoted prefix, digits and suffix in place — so a result past the old 1 KiB render buffer
/// (which refused it) renders, and one past the arena is `StrArenaFull`, never truncated.
#[test]
fn tostring_pattern_renders_past_one_kib_in_place() {
    let pre = "x".repeat(800);
    let pat = format!("'{pre}'#,##0.00' end'");
    let want = format!("-{pre}{}.00 end", grouped(1e300));
    assert!(want.len() > 1024, "{}", want.len());
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(-1e300), In::S(pat.clone())],
            &want,
            4096
        ),
        Ok(true)
    );
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(-1e300), In::S(pat)],
            &want,
            1024
        ),
        Err(FaultKind::StrArenaFull)
    );
    // The default and keyword renderings take the exact range too.
    assert_eq!(
        run(
            ov::STR_TOSTRING_F,
            &[In::F(1e300)],
            &format!("{:.0}", 1e300),
            512
        ),
        Ok(true)
    );
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(0.5), s("percent")],
            "0.5%",
            512
        ),
        Ok(true)
    );
    assert_eq!(
        run(
            ov::STR_TOSTRING_IS,
            &[In::I(i64::MIN), s("#,###")],
            "-9,223,372,036,854,775,808",
            512
        ),
        Ok(true)
    );
}

/// Audit item 1 at run time (`pin:04-fmt-int-cap`): 255 integer zeros pad, a 256th / 257th is
/// refused (the u8 used to panic in debug and wrap in release, rendering "7"); a grouping size of
/// 256 is refused (it used to truncate to 0, no grouping).
#[test]
fn tostring_pattern_int_cap_at_run_time() {
    let z255 = "0".repeat(255);
    let want = format!("{}7", "0".repeat(254));
    assert_eq!(
        run(ov::STR_TOSTRING_IS, &[In::I(7), In::S(z255)], &want, 2048),
        Ok(true)
    );
    for n in [256, 257] {
        assert_eq!(
            run(
                ov::STR_TOSTRING_IS,
                &[In::I(7), In::S("0".repeat(n))],
                "7",
                2048
            ),
            Err(FaultKind::ScriptRefused),
            "{n} zeros"
        );
    }
    let g256 = format!("#,{}", "#".repeat(256));
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(1234.0), In::S(g256)],
            "1234",
            2048
        ),
        Err(FaultKind::ScriptRefused)
    );
    let g255 = format!("#,{}", "#".repeat(255));
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(1234.0), In::S(g255)],
            "1234",
            2048
        ),
        Ok(true)
    );
}

/// Audit item 4: ±Infinity is a word laid out like digits — under a pattern the sign still goes
/// before the prefix (`pin:04-num-nonfinite`); na is `"NaN"` with no affixes.
#[test]
fn tostring_nonfinite_words() {
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(f64::NEG_INFINITY), s("'<'#'>'")],
            "-<Infinity>",
            256
        ),
        Ok(true)
    );
    assert_eq!(
        run(ov::STR_TOSTRING_F, &[In::F(f64::INFINITY)], "Infinity", 256),
        Ok(true)
    );
    assert_eq!(
        run(
            ov::STR_TOSTRING_FS,
            &[In::F(f64::NAN), s("'<'#'>'")],
            "NaN",
            256
        ),
        Ok(true)
    );
    assert_eq!(run(ov::STR_TOSTRING_I, &[In::NaS], "NaN", 256), Ok(true));
}

/// Audit item 2: `str.format` counts in pass 1 and renders every number straight into the lent
/// range in pass 2 — long grouped numbers, padded ints, ±Infinity under a pattern, the four
/// keyword patterns (compiled at build time) — and lends only the string arguments a placeholder
/// names (an unreferenced na string is never resolved; a referenced na renders "NaN").
#[test]
fn str_format_renders_numbers_in_place() {
    use Ty::{Float as F, Int as I, String as S};
    let types = |ts: &[Ty]| {
        let mut v = 0i64;
        for (k, t) in ts.iter().enumerate() {
            v |= (*t as i64) << (4 * k);
        }
        In::I(v)
    };
    let tpl = "{0,number,#,##0.00}|{1}|{2,number,0000}|{5}|{3}|{6,number,'<'#'>'}|{0,number}";
    let want = format!("{g}.00|-2.5|0007|c|a|-<Infinity>|{g}", g = grouped(1e300));
    let ins = [
        types(&[F, F, I, S, S, S, F]),
        In::S(tpl.to_string()),
        In::F(1e300),
        In::F(-2.5),
        In::I(7),
        s("a"),
        In::NaS,
        s("c"),
        In::F(f64::NEG_INFINITY),
    ];
    assert!(want.len() > 512);
    assert_eq!(run(ov::STR_FORMAT_F, &ins, &want, 4096), Ok(true));
    // The sizing pass is exact: one byte short of the result is StrArenaFull, nothing written.
    assert_eq!(
        run(ov::STR_FORMAT_F, &ins, &want, want.len() as u32 - 1),
        Err(FaultKind::StrArenaFull)
    );
    // A referenced na string renders "NaN"; currency / integer / percent keywords.
    let ins = [
        types(&[S, S, F, F, F]),
        s("{1}/{0}/{2,number,currency}/{3,number,integer}/{4,number,percent}"),
        s("x"),
        In::NaS,
        In::F(-1234.5),
        In::F(2.5),
        In::F(0.256),
    ];
    assert_eq!(
        run(ov::STR_FORMAT_F, &ins, "NaN/x/-$1,234.50/2/26%", 512),
        Ok(true)
    );
    // Fifteen string arguments, all named: the widest lend (template + 15).
    let mut ins = vec![types(&[S; 15])];
    let mut tpl = String::new();
    let mut want = String::new();
    for k in 0..15 {
        tpl.push_str(&format!("{{{}}}", 14 - k));
    }
    ins.push(In::S(tpl));
    for k in 0..15 {
        ins.push(In::S(format!("{k},")));
    }
    for k in (0..15).rev() {
        want.push_str(&format!("{k},"));
    }
    assert_eq!(run(ov::STR_FORMAT_S, &ins, &want, 512), Ok(true));
}
