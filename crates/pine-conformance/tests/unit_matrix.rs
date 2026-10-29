// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! CAR-R40-2 (r40, R-P34) — `matrix.*` end to end: pine-lang → pine-sema (tier 4 active) → verify →
//! pine-vm → pine-std, one chart bar, every value read from a plot. Reference values from numpy
//! (computed offline, hard-coded). No TradingView capture exists for these (uncaptured, CAR-R40-2).
//!
//! COPY-DOCTRINE: test support, may allocate.

use pine_conformance::chart::{compile_opts, spec, Syminfo};
use pine_conformance::host::{last_bar, step_chart_bar, AlignedBuf, SeriesStore, SubRun};
use pine_ir::{barflag, Bar, Dialect, ExecMode, FileId, PineProgram, SymTable};
use pine_vm::VmInstance;

fn syminfo() -> Syminfo {
    Syminfo {
        mintick_1e6: 100_000,
        pointvalue_1e6: 1_000_000,
        ticker: "BTCUSDT.P".to_string(),
        basecurrency: "BTC".to_string(),
        currency: "USDT".to_string(),
        type_: "crypto".to_string(),
        venue: 0,
        day_offset_s: 0,
    }
}

fn limits() -> pine_ir::CompileLimits {
    pine_ir::CompileLimits {
        max_bars_back: 5000,
        insn_budget: 2_000_000,
        collection_cap: 4096,
        str_persist_bytes: 4096,
        str_cell_bytes: 128,
        str_scratch_bytes: 8192,
        intent_cap: 32,
        max_frame_bytes: 64 << 20,
        active_tiers: 0b1111,
    }
}

fn try_compile(src: &str, d: Dialect) -> Result<PineProgram, Vec<(u16, String)>> {
    let opts = compile_opts(
        &syminfo(),
        d,
        ExecMode::BarClose,
        false,
        limits(),
        core_crypto::sha256(src.as_bytes()),
    );
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parse");
    let mut libs = pine_sema::LibCache::empty();
    pine_sema::compile(&ast, syms, &opts, &mut libs)
        .map_err(|ds| ds.iter().map(|d| (d.code.0, d.msg.clone())).collect())
}

fn compile(body: &str) -> PineProgram {
    let src = format!("//@version=6\nindicator(\"m\")\n{body}");
    try_compile(&src, Dialect::V6).unwrap_or_else(|d| panic!("compile: {d:?}\n{src}"))
}

/// Run `prog` over `n` hourly bars; the plots after the last one, or the fault.
fn run(prog: &PineProgram, n: u32) -> Result<Vec<f64>, String> {
    let rows: Vec<Bar> = (0..n)
        .map(|i| {
            let px = 100 * 1_000_000 + i64::from(i) * 1_000_000;
            Bar {
                open_1e6: px,
                high_1e6: px + 500_000,
                low_1e6: px - 500_000,
                close_1e6: px + 250_000,
                volume_1e6: 1_000_000,
                open_ms: spec::FROM_MS + i64::from(i) * 3_600_000,
                close_ms: spec::FROM_MS + i64::from(i + 1) * 3_600_000,
                index: i,
                flags: barflag::CONFIRMED,
            }
        })
        .collect();
    let v = pine_vm::verify(prog, &pine_std::BUILTINS).map_err(|e| format!("verify {e:?}"))?;
    let mut main = AlignedBuf::new(prog.layout.total_bytes);
    let mut vm = VmInstance::new(v, main.as_mut_slice());
    let mut store = SeriesStore::with_slots(&[]);
    let mut subs: Vec<SubRun<'_>> = Vec::new();
    let mut broker = None;
    let mut evals = 0u64;
    for (k, row) in rows.iter().enumerate() {
        let lb;
        let bar = if k + 1 == rows.len() {
            lb = last_bar(row);
            &lb
        } else {
            row
        };
        match step_chart_bar(bar, &mut subs, &mut store, &mut vm, &mut broker, &mut evals) {
            Ok(()) => {}
            Err(pine_conformance::host::HostFault::Script { kind, pc, .. }) => {
                return Err(format!("fault {kind:?} at pc {pc}, bar {k}"));
            }
            Err(_) => return Err(format!("broker fault at bar {k}")),
        }
    }
    Ok(vm.plots().to_vec())
}

fn plots(body: &str) -> Vec<f64> {
    run(&compile(body), 3).unwrap_or_else(|e| panic!("run: {e}\n{body}"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-9 * (1.0 + b.abs())
}

#[track_caller]
fn assert_plots(got: &[f64], want: &[f64]) {
    assert_eq!(got.len(), want.len(), "{got:?}");
    for (i, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(
            (g.is_nan() && w.is_nan()) || close(*g, *w),
            "plot {i}: got {g}, want {w}; all {got:?}"
        );
    }
}

#[test]
fn matrix_new_get_set_shape() {
    let p = plots(
        "m = matrix.new<float>(2, 3, 1.5)\nm.set(1, 2, close)\n\
         plot(m.rows())\nplot(m.columns())\nplot(m.elements_count())\n\
         plot(m.get(0, 0))\nplot(matrix.get(m, 1, 2))\nplot(m.is_square() ? 1 : 0)\n",
    );
    assert_plots(&p, &[2.0, 3.0, 6.0, 1.5, 102.25, 0.0]);
}

/// `name = matrix.new<T>(r, c, 0)` filled row-major from `v`.
fn fill(name: &str, ty: &str, r: usize, c: usize, v: &[f64]) -> String {
    let mut s = format!("{name} = matrix.new<{ty}>({r}, {c}, 0)\n");
    for i in 0..r {
        for j in 0..c {
            s.push_str(&format!("{name}.set({i}, {j}, {})\n", v[i * c + j]));
        }
    }
    s
}

/// Plot every element of matrix `name` (`r × c`), row-major.
fn plot_all(name: &str, r: usize, c: usize) -> String {
    let mut s = String::new();
    for i in 0..r {
        for j in 0..c {
            s.push_str(&format!("plot({name}.get({i}, {j}))\n"));
        }
    }
    s
}

const A: [f64; 9] = [4.0, 7.0, 2.0, 3.0, 6.0, 1.0, 2.0, 5.0, 3.0];

#[test]
fn matrix_det_inv_mult_pow() {
    let body = format!(
        "{}d = a.det()\ni = a.inv()\np = matrix.mult(a, a)\nq = a.pow(3)\nplot(d)\n{}{}{}",
        fill("a", "float", 3, 3, &A),
        plot_all("i", 3, 3),
        plot_all("p", 3, 3),
        plot_all("q", 3, 3)
    );
    let p = plots(&body);
    let mut want = vec![9.0];
    want.extend([
        1.4444444444444442,
        -1.222222222222222,
        -0.5555555555555556,
        -0.7777777777777777,
        0.8888888888888888,
        0.22222222222222224,
        0.3333333333333333,
        -0.6666666666666666,
        0.3333333333333333,
    ]);
    want.extend([41.0, 80.0, 21.0, 32.0, 62.0, 15.0, 29.0, 59.0, 18.0]);
    want.extend([
        446.0, 872.0, 225.0, 344.0, 671.0, 171.0, 329.0, 647.0, 171.0,
    ]);
    assert_plots(&p, &want);
}

#[test]
fn matrix_singular_inv_is_na_and_det_zero() {
    let body = format!(
        "{}plot(s.det())\ni = s.inv()\n{}",
        fill("s", "float", 2, 2, &[1.0, 2.0, 2.0, 4.0]),
        plot_all("i", 2, 2)
    );
    let p = plots(&body);
    assert_eq!(p[0], 0.0);
    assert!(p[1..].iter().all(|x| x.is_nan()), "{p:?}");
}

#[test]
fn matrix_pinv_tall_and_wide_and_rank() {
    let b = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
    let body = format!(
        "{}{}x = b.pinv()\ny = w.pinv()\n{}{}{}plot(r.rank())\nplot(b.rank())\n",
        fill("b", "float", 3, 2, &b),
        fill("w", "float", 2, 3, &[1.0, 3.0, 5.0, 2.0, 4.0, 6.0]),
        plot_all("x", 2, 3),
        plot_all("y", 3, 2),
        fill(
            "r",
            "float",
            3,
            3,
            &[1.0, 2.0, 3.0, 2.0, 4.0, 6.0, 1.0, 0.0, 1.0]
        ),
    );
    let p = plots(&body);
    let mut want = vec![
        -1.3333333333333337,
        -0.3333333333333329,
        0.6666666666666666,
        1.083333333333334,
        0.33333333333333304,
        -0.4166666666666667,
    ];
    want.extend([
        -1.3333333333333344,
        1.0833333333333346,
        -0.33333333333333404,
        0.33333333333333404,
        0.6666666666666672,
        -0.4166666666666673,
    ]);
    want.extend([2.0, 2.0]);
    assert_plots(&p, &want);
}

#[test]
fn matrix_eigen_symmetric() {
    let s = [2.0, 1.0, 0.0, 1.0, 3.0, 1.0, 0.0, 1.0, 4.0];
    let body = format!(
        "{}e = s.eigenvalues()\nv = s.eigenvectors()\nplot(e.get(0))\nplot(e.get(1))\nplot(e.get(2))\n{}",
        fill("s", "float", 3, 3, &s),
        plot_all("v", 3, 3)
    );
    let p = plots(&body);
    let lam = [1.2679491924311228, 3.0, 4.7320508075688785];
    assert_plots(&p[..3], &lam);
    // Each column is a unit eigenvector: S·v = λ·v.
    for (j, l) in lam.iter().enumerate() {
        let v: Vec<f64> = (0..3).map(|i| p[3 + i * 3 + j]).collect();
        let norm: f64 = v.iter().map(|x| x * x).sum::<f64>().sqrt();
        assert!(close(norm, 1.0), "norm {norm}");
        for i in 0..3 {
            let sv: f64 = (0..3).map(|k| s[i * 3 + k] * v[k]).sum();
            assert!(
                (sv - l * v[i]).abs() < 1e-12,
                "col {j} row {i}: {sv} vs {}",
                l * v[i]
            );
        }
    }
}

#[test]
fn matrix_structure_ops() {
    let body = format!(
        "{}t = a.transpose()\nu = a.submatrix(1, 3, 0, 2)\nr = a.row(1)\nc = a.col(2)\n\
         k = matrix.kron(x, y)\nplot(t.get(0, 1))\nplot(t.get(2, 0))\nplot(u.rows())\nplot(u.get(1, 1))\n\
         plot(r.size())\nplot(r.get(2))\nplot(c.get(0))\nplot(k.rows())\nplot(k.get(1, 3))\nplot(k.get(3, 2))\n",
        fill("a", "float", 3, 3, &A) + &fill("x", "float", 2, 2, &[1.0, 2.0, 3.0, 4.0])
            + &fill("y", "float", 2, 2, &[0.0, 5.0, 6.0, 7.0]),
    );
    let p = plots(&body);
    assert_plots(&p, &[3.0, 2.0, 2.0, 5.0, 3.0, 1.0, 2.0, 4.0, 14.0, 24.0]);
}

#[test]
fn matrix_mutators() {
    let body = format!(
        "{}m.add_row(1, array.from(9.0, 9.0, 9.0))\nm.add_col(0)\nplot(m.rows())\nplot(m.columns())\n\
         plot(m.get(1, 1))\nplot(na(m.get(0, 0)) ? 1 : 0)\nx = m.remove_row(1)\nplot(x.get(0))\n\
         y = m.remove_col(0)\nplot(y.size())\nplot(m.columns())\nm.swap_rows(0, 2)\nplot(m.get(0, 0))\n\
         m.reverse()\nplot(m.get(2, 2))\nm.reshape(1, 9)\nplot(m.get(0, 4))\nm.fill(0.5, 0, 1, 2, 4)\n\
         plot(m.get(0, 3))\nplot(m.get(0, 4))\n",
        fill("m", "float", 3, 3, &A)
    );
    let p = plots(&body);
    // After add_row(1): rows 4; add_col(0): cols 4 with a na column first.
    // remove_row(1) returns [na, 9, 9, 9]; remove_col(0) returns 3 elements (rows 3).
    // m is A again; swap_rows(0, 2) → first row [2, 5, 3]; reverse → last element = 2.
    // reshape(1, 9): row-major of the reversed swapped A.
    assert_plots(&p[..6], &[4.0, 4.0, 9.0, 1.0, f64::NAN, 3.0]);
    assert_eq!(p[6], 3.0);
    assert_eq!(p[7], 2.0);
    assert_eq!(p[8], 2.0);
    // swapped A rows: [2,5,3],[3,6,1],[4,7,2]; reversed: [2,7,4,1,6,3,3,5,2]; element 4 = 6.
    assert_eq!(p[9], 6.0);
    assert_eq!(p[10], 0.5);
    assert_eq!(p[11], 6.0);
}

#[test]
fn matrix_reductions_and_predicates() {
    let body = format!(
        "{}{}{}plot(a.min())\nplot(a.max())\nplot(a.avg())\nplot(a.median())\nplot(a.mode())\n\
         plot(a.trace())\nplot(i.is_identity() ? 1 : 0)\nplot(s.is_symmetric() ? 1 : 0)\n\
         plot(a.is_symmetric() ? 1 : 0)\nplot(i.is_diagonal() ? 1 : 0)\nplot(i.is_binary() ? 1 : 0)\n\
         plot(a.is_triangular() ? 1 : 0)\nplot(st.is_stochastic() ? 1 : 0)\nplot(z.is_zero() ? 1 : 0)\n",
        fill("a", "float", 3, 3, &A),
        fill("i", "float", 2, 2, &[1.0, 0.0, 0.0, 1.0])
            + &fill("s", "float", 2, 2, &[1.0, 2.0, 2.0, 1.0]),
        fill("st", "float", 2, 2, &[0.25, 0.75, 0.5, 0.5])
            + "z = matrix.new<float>(2, 2, 0)\n",
    );
    let p = plots(&body);
    // A sorted: 1 2 2 3 3 4 5 6 7 → median 3, mode 2 (2 and 3 tie → smallest), avg 33/9.
    assert_plots(
        &p,
        &[
            1.0,
            7.0,
            33.0 / 9.0,
            3.0,
            2.0,
            13.0,
            1.0,
            1.0,
            0.0,
            1.0,
            1.0,
            0.0,
            1.0,
            1.0,
        ],
    );
}

#[test]
fn matrix_int_ops_and_sort_and_concat() {
    let body = format!(
        "{}{}s = matrix.sum(a, b)\nd = matrix.diff(a, 1)\nm = matrix.mult(a, 2)\nv = matrix.mult(a, array.from(1, 2, 3))\n\
         plot(s.get(2, 2))\nplot(d.get(0, 0))\nplot(m.get(1, 1))\nplot(v.get(0))\nplot(a.det())\n\
         a.sort(1, order.descending)\nplot(a.get(0, 1))\nplot(a.get(2, 1))\nc = matrix.concat(a, b)\nplot(c.rows())\nplot(a.rows())\n",
        fill("a", "int", 3, 3, &A),
        fill("b", "int", 3, 3, &[1.0; 9]),
    );
    let p = plots(&body);
    assert_plots(&p, &[4.0, 3.0, 12.0, 24.0, 9.0, 7.0, 5.0, 6.0, 6.0]);
}

#[test]
fn matrix_bounds_and_shape_faults() {
    for (body, want) in [
        ("m = matrix.new<float>(2, 2, 0)\nplot(m.get(2, 0))\n", "CollectionBounds"),
        ("m = matrix.new<float>(2, 3, 0)\nplot(m.det())\n", "ScriptRefused"),
        ("m = matrix.new<float>(-1, 3, 0)\nplot(m.rows())\n", "ScriptRefused"),
        ("m = matrix.new<float>(100, 100, 0)\nplot(m.rows())\n", "PoolFull"),
        (
            "a = matrix.new<float>(2, 3, 0)\nb = matrix.new<float>(2, 3, 0)\nc = matrix.mult(a, b)\nplot(c.rows())\n",
            "ScriptRefused",
        ),
    ] {
        let e = run(&compile(body), 1).expect_err(body);
        assert!(e.contains(want), "{body}: {e}");
    }
}

#[test]
fn matrix_created_each_bar_cannot_be_kept_in_a_var() {
    // As every collection creator (`3047`): `m := matrix.mult(m, x)` into a `var` is refused, so
    // a creator never reads its own previous block through a kept reference.
    let src = format!(
        "//@version=6\nindicator(\"m\")\nvar m = matrix.new<float>(2, 2, 1)\n{}m := matrix.mult(m, x)\nplot(m.get(0, 0))\n",
        fill("x", "float", 2, 2, &[2.0, 0.0, 0.0, 3.0])
    );
    let codes: Vec<u16> = try_compile(&src, Dialect::V6)
        .err()
        .unwrap_or_default()
        .iter()
        .map(|d| d.0)
        .collect();
    assert_eq!(codes, vec![3047]);
}

#[test]
fn matrix_tier4_off_is_refused() {
    let src = "//@version=6\nindicator(\"m\")\nm = matrix.new<float>(2, 2, 0)\nplot(m.rows())\n";
    let opts = compile_opts(
        &syminfo(),
        Dialect::V6,
        ExecMode::BarClose,
        false,
        pine_ir::CompileLimits {
            active_tiers: 0b0111,
            ..limits()
        },
        core_crypto::sha256(src.as_bytes()),
    );
    let mut syms = SymTable::new();
    let ast = pine_lang::parse(src, FileId::ROOT, &mut syms).expect("parse");
    let r = pine_sema::compile(&ast, syms, &opts, &mut pine_sema::LibCache::empty());
    let codes: Vec<u16> = r
        .err()
        .unwrap_or_default()
        .iter()
        .map(|d| d.code.0)
        .collect();
    assert_eq!(codes, vec![3024]);
}
