// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Group C — `var` / `varip` / scope / UDT / realtime (impl-06 §4.2.3). GENERATOR-TIME ONLY.
//!
//! C-decl (C001–C021), C-udt (C022–C034), C-blk (C035–C036, `pin:U16-block-local-carry`, with
//! readings), C-scope (C037–C038), C-rt (C039–C044, `Driver::Rt`, `mode realtime`), C-bs (C045)
//! and C-once (C046, `pin:02-once-flag`, with relation readings).
//!
//! Row notation as group F: `@` stands for `_<ID>`; each leading `>` is one block level.

use pine_ir::{DiagCode, ExecMode};

use super::emit::{Line, Prelude, Script};
use super::readings::{pair, row as arow, NA};
use super::{Cell, Run};
use crate::fixture::{
    Dialects, Driver, Expect, FixtureId, Group, Oracle, Reading, ReadingBody, RtSpec, Tol, Window,
};
use crate::syn::SynSeq;

/// SynTape's defaults (§4.1.6): `history 64`, `realtime 16`, `ticks 3`, no HTF.
pub const RT_DEFAULT: RtSpec = RtSpec {
    history: 64,
    realtime: 16,
    ticks: 3,
    htf_mult: 0,
    open: false,
};

/// C045's drive: 128 history steps, then one forming realtime bar (ticks, no confirm).
///
/// `pin:06-c045-window` (r27b, CI-028 fixture defect) — the compared window is the last 65 rows
/// (`C045_TAIL`) of a 129-row run, keys 64–128: no compared row is the chart's first bar on either
/// side (TradingView's export of 469 history rows + the open bar, distilled by position, and our
/// 128 history steps), so `isfirst` reads 0 on every compared row of both.
pub const RT_C045: RtSpec = RtSpec {
    history: 128,
    realtime: 1,
    ticks: 3,
    htf_mult: 0,
    open: true,
};

const B: Dialects = Dialects::BOTH;

/// `S` of C-decl, in id order.
const S: [(&str, &str); 3] = [("", "plain"), ("var ", "var"), ("varip ", "varip")];
/// `L` of C-decl, in id order.
const L: [&str; 7] = [
    "global",
    "if_block",
    "for_block",
    "fn_body",
    "fn_body_called_twice",
    "switch_arm",
    "method_body",
];

fn line(s: &str, id: FixtureId) -> Line {
    let sfx = format!("_{}", id.as_str());
    let depth = s.bytes().take_while(|&c| c == b'>').count();
    Line {
        depth: depth as u8,
        text: s[depth..].replace('@', &sfx),
    }
}

fn lines(src: &[String], id: FixtureId) -> Vec<Line> {
    src.iter().map(|s| line(s, id)).collect()
}

fn isna_val(e: &str, suffix: &str) -> Vec<(String, String)> {
    let (i, v) = if suffix.is_empty() {
        ("isna".to_string(), "val".to_string())
    } else {
        (format!("{suffix}_isna"), format!("{suffix}_val"))
    };
    vec![
        (format!("na({e}) ? 1 : 0"), i),
        (format!("na({e}) ? na : {e}"), v),
    ]
}

/// One group-C row before emission.
struct Row {
    n: u16,
    ds: Dialects,
    title: String,
    about: Vec<String>,
    pins: Vec<String>,
    body: Vec<String>,
    plots: Vec<(String, String)>,
    window: Window,
    oracle: Oracle,
    expect: Expect,
    tier: u8,
    readings: Vec<Reading>,
    run: Run,
}

fn base(n: u16, title: &str, body: Vec<String>, plots: Vec<(String, String)>) -> Row {
    Row {
        n,
        ds: B,
        title: title.to_string(),
        about: vec![title.to_string()],
        pins: Vec::new(),
        body,
        plots,
        window: Window::Head(64),
        oracle: Oracle::Tv,
        expect: Expect::Pending,
        tier: 1,
        readings: Vec::new(),
        run: Run::default(),
    }
}

fn v(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

/// C-decl: `C(7·s + l + 1)`. The declaration sits inside `L` (§4.2.3's C012 example), and the
/// block yields `acc` where it is an expression.
///
/// `pin:06-c-decl-forms` — the template's `<open L>` / `<close L>` as written here: `if synb`,
/// `for i = 0 to 1` and `switch` / `synb =>` return `acc` as the structure's value; `fn_body`
/// returns it; `method_body` is a method on `int` called as `x.tick()`; `fn_body_called_twice` is
/// C012's example. The plotted value takes the isna/val pair (§4.2.0), except the two-site form,
/// which plots C012's `a`, `b_isna`, `b_val`.
fn c_decl(s: usize, l: usize) -> Row {
    let (kw, sname) = S[s];
    let n = (7 * s + l + 1) as u16;
    let decl = format!("{kw}int acc@ = 0");
    let inc = "acc@ := acc@ + 1".to_string();
    let (body, plots): (Vec<String>, Vec<(String, String)>) = match l {
        0 => (vec![decl, inc], isna_val("acc@", "")),
        1 => (
            vec![
                "a@ = if synb".into(),
                format!(">{decl}"),
                format!(">{inc}"),
                ">acc@".into(),
            ],
            isna_val("a@", ""),
        ),
        2 => (
            vec![
                "a@ = for i@ = 0 to 1".into(),
                format!(">{decl}"),
                format!(">{inc}"),
                ">acc@".into(),
            ],
            isna_val("a@", ""),
        ),
        3 => (
            vec![
                "tick@() =>".into(),
                format!(">{decl}"),
                format!(">{inc}"),
                ">acc@".into(),
                "a@ = tick@()".into(),
            ],
            isna_val("a@", ""),
        ),
        4 => {
            let mut p = vec![("a@".to_string(), "a".to_string())];
            p.extend(isna_val("b@", "b"));
            (
                vec![
                    "tick@() =>".into(),
                    format!(">{decl}"),
                    format!(">{inc}"),
                    ">acc@".into(),
                    "a@ = tick@()".into(),
                    "var int b@ = na".into(),
                    "if synb".into(),
                    ">b@ := tick@()".into(),
                ],
                p,
            )
        }
        5 => (
            vec![
                "a@ = switch".into(),
                ">synb =>".into(),
                format!(">>{decl}"),
                format!(">>{inc}"),
                ">>acc@".into(),
            ],
            isna_val("a@", ""),
        ),
        _ => (
            vec![
                "method tick@(int this@) =>".into(),
                format!(">{decl}"),
                format!(">{inc}"),
                ">acc@ + this@".into(),
                "x@ = 0".into(),
                "a@ = x@.tick@()".into(),
            ],
            isna_val("a@", ""),
        ),
    };
    let title = if n == 12 {
        "var inside a function called from two sites".to_string()
    } else {
        format!("{sname} inside {}", L[l])
    };
    let mut r = base(n, &title, body, plots);
    r.about.push("C-decl".into());
    if n == 12 {
        r.pins.push("pin:06-cond-call-state".into());
        r.readings = c012_readings();
    }
    r
}

/// C012's `pin:06-cond-call-state` readings over `head 64`: `a` (the unconditional site) is the step
/// count under both; `b` (the `if synb` site, held by its `var`) is the count of `synb` steps so far
/// (`executed_only`) or the step count at the last `synb` step (`every_bar`); `na` before the first.
fn c012_readings() -> Vec<Reading> {
    let mut q = SynSeq::new();
    let c: Vec<bool> = (0..64).map(|_| q.step().synb).collect();
    let run = |every: bool| {
        let (mut b, mut k) = (NA, 0.0);
        (0..64)
            .map(|i| {
                if c[i] {
                    k += 1.0;
                    b = if every { (i + 1) as f64 } else { k };
                }
                let [bi, bv] = pair(b);
                arow(i as i64, &[(i + 1) as f64, bi, bv])
            })
            .collect()
    };
    vec![
        Reading {
            name: "executed_only".into(),
            body: ReadingBody::Rows(run(false)),
        },
        Reading {
            name: "every_bar".into(),
            body: ReadingBody::Rows(run(true)),
        },
    ]
}

const PT: &[&str] = &["type Pt@", ">int x@"];

fn udt(n: u16, title: &str, pre: &[&str], body: &[&str], plot: &str) -> Row {
    let mut b = v(pre);
    b.extend(v(body));
    base(n, title, b, isna_val(plot, ""))
}

fn declined(mut r: Row, code: u16) -> Row {
    r.oracle = Oracle::Declined;
    r.expect = Expect::Refuse {
        code: DiagCode(code),
    };
    r
}

/// C-udt (C022–C034).
///
/// `pin:06-c-udt-plots` — each row plots its object's field through the isna/val pair; C030 plots
/// `o.x` guarded by `na(o)`, C027 and C043 the array's size. `pin:06-c029-captured-fault` — C029's
/// fault is TradingView's runtime behaviour, captured (`tv`, pending); not authored.
fn c_udt() -> Vec<Row> {
    let mut rows = vec![
        udt(
            22,
            "a fresh object each bar",
            PT,
            &["o@ = Pt@.new(0)", "o@.x@ := o@.x@ + 1"],
            "o@.x@",
        ),
        udt(
            23,
            "a var object",
            PT,
            &["var o@ = Pt@.new(0)", "o@.x@ := o@.x@ + 1"],
            "o@.x@",
        ),
        udt(
            24,
            "assignment copies the reference",
            PT,
            &["var a@ = Pt@.new(0)", "b@ = a@", "b@.x@ := b@.x@ + 1"],
            "a@.x@",
        ),
        udt(
            25,
            "a function mutates a var object",
            PT,
            &[
                "bump@(Pt@ p@) => p@.x@ := p@.x@ + 1",
                "var o@ = Pt@.new(0)",
                "bump@(o@)",
            ],
            "o@.x@",
        ),
        declined(
            udt(
                26,
                "a var object built by a function",
                PT,
                &["mk@() => Pt@.new(1)", "var o@ = mk@()"],
                "o@.x@",
            ),
            3047,
        ),
        declined(
            base(
                27,
                "objects pushed into a var array",
                {
                    let mut b = v(PT);
                    b.extend(v(&[
                        "var arr@ = array.new<Pt@>()",
                        "array.push(arr@, Pt@.new(_n))",
                    ]));
                    b
                },
                isna_val("array.size(arr@)", ""),
            ),
            3047,
        ),
        udt(
            28,
            "copy() is a copy",
            PT,
            &["var a@ = Pt@.new(0)", "b@ = a@.copy()", "b@.x@ := 9"],
            "a@.x@",
        ),
        udt(
            29,
            "a field of an na object",
            PT,
            &["Pt@ o@ = na", "e@ = o@.x@"],
            "e@",
        ),
        declined(
            base(
                30,
                "a var na object assigned later",
                {
                    let mut b = v(PT);
                    b.extend(v(&["var Pt@ o@ = na", "if _n == 3", ">o@ := Pt@.new(7)"]));
                    b
                },
                vec![
                    ("na(o@) ? 1 : 0".into(), "isna".into()),
                    ("na(o@) ? na : o@.x@".into(), "val".into()),
                ],
            ),
            3047,
        ),
        udt(
            31,
            "a nested object field",
            &["type In@", ">int v@", "type Out@", ">In@ inner@"],
            &["var o@ = Out@.new(In@.new(0))", "o@.inner@.v@ += 1"],
            "o@.inner@.v@",
        ),
        udt(
            32,
            "a UDT field of enum type",
            &[
                "enum Side@",
                ">long@",
                ">short@",
                "type Pt@",
                ">Side@ s@ = Side@.long@",
            ],
            &["o@ = Pt@.new()", "k@ = o@.s@ == Side@.long@ ? 1 : 0"],
            "k@",
        ),
        udt(
            33,
            "a method mutates its receiver",
            PT,
            &[
                "method bump@(Pt@ this@) => this@.x@ += 1",
                "var o@ = Pt@.new(0)",
                "o@.bump@()",
            ],
            "o@.x@",
        ),
        udt(
            34,
            "a declared field default",
            &["type Pt@", ">int x@ = 5"],
            &["o@ = Pt@.new()"],
            "o@.x@",
        ),
    ];
    rows[5].tier = 3;
    rows
}

/// C035/C036's readings over SYN-1's first 64 steps (`pin:U16-block-local-carry`): `carry` — the
/// block's `y[1]` is `syn` of the last step the block ran; `na` — `na` whenever the previous step
/// skipped it. `e` (a `var`) keeps its value on skipped steps.
fn blk_readings() -> Vec<Reading> {
    let mut q = SynSeq::new();
    let st: Vec<_> = (0..64).map(|_| q.step()).collect();
    let run = |carry: bool| {
        let mut e = NA;
        let mut last = NA;
        (0..64)
            .map(|i| {
                if st[i].synb {
                    let y1 = if carry {
                        last
                    } else if i > 0 && st[i - 1].synb {
                        st[i - 1].syn
                    } else {
                        NA
                    };
                    e = y1;
                    last = st[i].syn;
                }
                arow(i as i64, &pair(e))
            })
            .collect()
    };
    vec![
        Reading {
            name: "carry".into(),
            body: ReadingBody::Rows(run(true)),
        },
        Reading {
            name: "na".into(),
            body: ReadingBody::Rows(run(false)),
        },
    ]
}

fn c_blk() -> Vec<Row> {
    let mk = |n: u16, title: &str, body: Vec<String>| {
        let mut r = base(n, title, body, isna_val("e@", ""));
        r.pins.push("pin:U16-block-local-carry".into());
        r.readings = blk_readings();
        r
    };
    vec![
        mk(
            35,
            "block-local history across skipped bars",
            v(&[
                "var float e@ = na",
                "if synb",
                ">float y@ = syn",
                ">e@ := y@[1]",
            ]),
        ),
        mk(
            36,
            "block-local history across skipped switch arms",
            v(&[
                "var float e@ = na",
                "switch",
                ">synb =>",
                ">>float y@ = syn",
                ">>e@ := y@[1]",
            ]),
        ),
    ]
}

fn c_scope() -> Vec<Row> {
    let mut no_hoist = base(
        38,
        "no_hoist",
        v(&["y@ = x@ + 1", "x@ = 1"]),
        isna_val("y@", ""),
    );
    no_hoist.oracle = Oracle::Authored;
    no_hoist.expect = Expect::Refuse {
        code: DiagCode(3003),
    };
    vec![
        base(
            37,
            "shadow_global",
            v(&["int x@ = 1", "if synb", ">int x@ = 2"]),
            isna_val("x@", ""),
        ),
        no_hoist,
    ]
}

/// The per-bar update machinery every C-rt row shares (§4.2.3: counters reset on a new bar by a
/// `time` change): `upd` counts the bar's updates, `done` is 1 once `barstate.isconfirmed` was seen.
const RT_PRE: &[&str] = &[
    "varip int tl@ = 0",
    "varip int upd@ = 0",
    "varip int done@ = 0",
    "if time != tl@",
    ">tl@ := time",
    ">upd@ := 0",
    ">done@ := 0",
    "upd@ += 1",
    "if barstate.isconfirmed",
    ">done@ := 1",
];

fn rt_plots() -> Vec<(String, String)> {
    vec![
        ("barstate.isrealtime ? 1 : 0".into(), "rt".into()),
        ("done@".into(), "done".into()),
    ]
}

/// `pin:06-c-rt-window` — a `Driver::Rt` fixture compares `tail <realtime>` (its realtime rows; the
/// relation is checked on the rows with `rt = 1` and `done = 1`).
fn rt_row(n: u16, title: &str, body: &[&str], extra: &[(&str, &str)]) -> Row {
    let mut b = v(RT_PRE);
    b.extend(v(body));
    let mut p = rt_plots();
    p.extend(extra.iter().map(|(e, k)| (e.to_string(), k.to_string())));
    let mut r = base(n, title, b, p);
    r.window = Window::Tail(RT_DEFAULT.realtime);
    r.run = Run {
        driver: Driver::Rt,
        mode: ExecMode::Realtime,
        rt: Some(RT_DEFAULT),
        ..Run::default()
    };
    r
}

fn c_rt() -> Vec<Row> {
    let ok = "v@ - nz(v@[1]) == 1 and upd@ >= 2 ? 1 : 0";
    let mut v43 = rt_row(
        43,
        "a varip array",
        &["varip a@ = array.new<float>()"],
        &[("array.size(a@)", "val")],
    );
    v43 = declined(v43, 3048);
    v43.tier = 3;
    // pin:06-c044-relation (R-P3, 2026-10-17) — TradingView's isconfirmed COUNT was 1 or 2 per
    // bar (the confirming update runs twice), so C044 states tick-independent relations instead:
    // `isnew` = 1 when barstate.isnew held on the bar's first update and on no later one;
    // `confirmed` = barstate.isconfirmed on the bar's last update (the row is the closing update);
    // `confmono` = 1 when no update after an isconfirmed one read isconfirmed false; `ishistory`,
    // `notlast`, `notrt` count the updates with ishistory / not islast / not isrealtime (0 for
    // any number of updates).
    let mut v44 = rt_row(
        44,
        "barstate flags per realtime update, tick-independent",
        &[
            "varip int nw@ = 1",
            "varip int cm@ = 1",
            "varip int pc@ = 0",
            "varip int bh@ = 0",
            "varip int bl@ = 0",
            "varip int br@ = 0",
            "if upd@ == 1",
            ">nw@ := 1",
            ">cm@ := 1",
            ">pc@ := 0",
            ">bh@ := 0",
            ">bl@ := 0",
            ">br@ := 0",
            "if barstate.isnew != (upd@ == 1)",
            ">nw@ := 0",
            "if pc@ == 1 and not barstate.isconfirmed",
            ">cm@ := 0",
            "if barstate.isconfirmed",
            ">pc@ := 1",
            "bh@ += barstate.ishistory ? 1 : 0",
            "bl@ += not barstate.islast ? 1 : 0",
            "br@ += not barstate.isrealtime ? 1 : 0",
        ],
        &[
            ("nw@", "isnew"),
            ("barstate.isconfirmed ? 1 : 0", "confirmed"),
            ("cm@", "confmono"),
            ("bh@", "ishistory"),
            ("bl@", "notlast"),
            ("br@", "notrt"),
        ],
    );
    v44.pins.push("pin:U17-barstate-table".into());
    // U17's readings over C044's relation (§4.9.2): `contract` (A2: isnew on the first update
    // only) and `isnew_on_confirm` (v4's table: isnew also in the confirm step, so not first-only).
    let rel44 = |isnew: i64| {
        ReadingBody::Relation(
            [
                ("isnew", isnew),
                ("confirmed", 1),
                ("confmono", 1),
                ("ishistory", 0),
                ("notlast", 0),
                ("notrt", 0),
            ]
            .iter()
            .map(|(s, v)| (format!("f_C044_{s}"), *v))
            .collect(),
        )
    };
    v44.readings = vec![
        Reading {
            name: "contract".into(),
            body: rel44(1),
        },
        Reading {
            name: "isnew_on_confirm".into(),
            body: rel44(0),
        },
    ];
    vec![
        rt_row(
            39,
            "a var advances once per realtime bar",
            &["var int v@ = 0", "v@ += 1"],
            &[("v@ - nz(v@[1])", "val")],
        ),
        rt_row(
            40,
            "varip survives the rollback that resets var",
            &["var int v@ = 0", "v@ += 1"],
            &[("upd@", "upd"), (ok, "ok")],
        ),
        rt_row(
            41,
            "the varip update counter inside a function",
            &[
                "cnt@() =>",
                ">varip int u@ = 0",
                ">varip int t@ = 0",
                ">if time != t@",
                ">>t@ := time",
                ">>u@ := 0",
                ">u@ += 1",
                ">u@",
                "fu@ = cnt@()",
                "var int v@ = 0",
                "v@ += 1",
            ],
            &[
                ("fu@", "upd"),
                ("v@ - nz(v@[1]) == 1 and fu@ >= 2 ? 1 : 0", "ok"),
            ],
        ),
        rt_row(
            42,
            "a varip string survives the rollback",
            &[
                "varip string s@ = \"\"",
                "if upd@ == 1",
                ">s@ := \"\"",
                "s@ += \"x\"",
            ],
            &[("upd@", "upd"), ("str.length(s@) >= 2 ? 1 : 0", "ok")],
        ),
        v43,
        v44,
    ]
}

/// C045's compared rows: 64 history rows plus the open realtime row (`pin:06-c045-window`).
pub const C045_TAIL: u32 = 65;

/// C045: the seven `barstate.*` flags as 0/1 on every row.
///
/// `pin:06-c045-window` — `tail 65` of a 129-row run (CI-028): the last 64 history rows plus the
/// open realtime row, the chart's first bar 64 rows above the window.
fn c_bs() -> Row {
    let flags = [
        ("barstate.isconfirmed", "isconfirmed"),
        ("barstate.islast", "islast"),
        ("barstate.isnew", "isnew"),
        ("barstate.isrealtime", "isrealtime"),
        ("barstate.ishistory", "ishistory"),
        ("barstate.isfirst", "isfirst"),
        ("barstate.islastconfirmedhistory", "islch"),
    ];
    let plots = flags
        .iter()
        .map(|(f, k)| (format!("{f} ? 1 : 0"), k.to_string()))
        .collect();
    let mut r = base(
        45,
        "the barstate flags on history rows and an open bar",
        Vec::new(),
        plots,
    );
    r.pins.push("pin:U17-barstate-table".into());
    r.window = Window::Tail(C045_TAIL);
    r.run = Run {
        driver: Driver::Rt,
        mode: ExecMode::Realtime,
        rt: Some(RT_C045),
        ..Run::default()
    };
    r
}

/// C046 (`pin:02-once-flag`): relation readings `var_like` (impl-02's default, first) / `varip_like`.
fn c_once() -> Row {
    let mut r = rt_row(
        46,
        "once on the first realtime update",
        &[
            "varip int runs@ = 0",
            "once barstate.isrealtime",
            ">runs@ += 1",
        ],
        &[("runs@ == 1 ? 1 : 0", "ok")],
    );
    r.ds = Dialects::V6;
    r.pins.push("pin:02-once-flag".into());
    let rel = |k: i64| ReadingBody::Relation(vec![("f_C046_ok".to_string(), k)]);
    r.readings = vec![
        Reading {
            name: "var_like".into(),
            body: rel(0),
        },
        Reading {
            name: "varip_like".into(),
            body: rel(1),
        },
    ];
    r
}

fn cell(r: Row) -> Cell {
    let id = FixtureId::from_parts(Group::C, r.n);
    let sfx = format!("_{}", id.as_str());
    let plots: Vec<(String, String)> = r
        .plots
        .into_iter()
        .map(|(e, k)| (e.replace('@', &sfx), k))
        .collect();
    let tols = vec![Tol::Exact; plots.len()];
    let mut about = r.about;
    if let Some(t) = about.first_mut() {
        *t = r.title.clone();
    }
    let script = Script {
        id,
        dialects: r.ds,
        about,
        twin: None,
        pins: r.pins.clone(),
        decl: None,
        prelude: Prelude::Syn1,
        body: lines(&r.body, id),
        plots,
    };
    Cell {
        script,
        title: r.title,
        pins: r.pins,
        oracle: r.oracle,
        tier: r.tier,
        window: r.window,
        tols,
        expect: r.expect,
        readings: r.readings,
        run: r.run,
    }
}

/// Every group-C cell (C001–C046).
pub fn cells() -> Vec<Cell> {
    let mut rows: Vec<Row> = Vec::new();
    for s in 0..3 {
        for l in 0..7 {
            rows.push(c_decl(s, l));
        }
    }
    rows.extend(c_udt());
    rows.extend(c_blk());
    rows.extend(c_scope());
    rows.extend(c_rt());
    rows.push(c_bs());
    rows.push(c_once());
    rows.sort_by_key(|r| r.n);
    rows.into_iter().map(cell).collect()
}
