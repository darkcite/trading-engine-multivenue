// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The printer and the round-trip property (§4.10, §7.10).

use std::fs;
use std::path::Path;

use pine_ir::{FileId, SymTable};
use pine_lang::{parse, print, structurally_eq};

/// Every `tests/scripts/*.pine`, with its name.
fn scripts() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scripts");
    let mut out = Vec::new();
    for e in fs::read_dir(&dir).expect("scripts dir") {
        let p = e.expect("entry").path();
        if p.extension().is_some_and(|x| x == "pine") {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push((name, fs::read_to_string(&p).expect("read script")));
        }
    }
    out.sort();
    out
}

/// `parse → print → parse` is structurally identical; returns the printed text.
fn round_trip(name: &str, src: &str) -> String {
    let mut s1 = SymTable::new();
    let a = parse(src, FileId::ROOT, &mut s1).unwrap_or_else(|d| panic!("{name}: {d:?}"));
    let mut text = String::new();
    print(&a, &s1, &mut text).expect("print");
    let mut s2 = SymTable::new();
    let b = parse(&text, FileId::ROOT, &mut s2)
        .unwrap_or_else(|d| panic!("{name}: reparse of\n{text}\nfailed: {d:?}"));
    assert!(
        structurally_eq(&a, &s1, &b, &s2),
        "{name}: not structurally equal\n{text}"
    );
    text
}

#[test]
fn scripts_round_trip() {
    let all = scripts();
    assert!(all.len() >= 3);
    for (name, src) in &all {
        round_trip(name, src);
    }
}

#[test]
fn printing_is_idempotent() {
    for (name, src) in scripts() {
        let once = round_trip(&name, &src);
        let twice = round_trip(&name, &once);
        assert_eq!(once, twice, "{name}");
    }
}

#[test]
fn canonical_forms() {
    let cases = [
        ("a + b * c", "(a + (b * c))"),
        ("-a[1]", "(-a[1])"),
        ("not a == b", "((not a) == b)"),
        ("a ? b : c ? d : e", "(a ? b : c ? d : e)"),
        ("(1).abs()", "(1).abs()"),
        ("x = 1.", "x = 1.0"),
        ("c = #ff0000a0", "c = #FF0000A0"),
        ("s = 'a\"b'", "s = \"a\\\"b\""),
        ("f(x) => x", "f(x) =>\n    x"),
    ];
    for (src, want) in cases {
        let text = round_trip(src, &format!("//@version=6\n{src}\n"));
        assert_eq!(text, format!("//@version=6\n{want}\n"), "{src}");
    }
}

#[test]
fn structurally_eq_sees_differences() {
    let parse6 = |s: &str| {
        let mut syms = SymTable::new();
        let a = parse(&format!("//@version=6\n{s}\n"), FileId::ROOT, &mut syms).expect("parses");
        (a, syms)
    };
    let (a, sa) = parse6("x = a + b");
    let (b, sb) = parse6("x = a - b");
    let (c, sc) = parse6("x = a + c");
    let (d, sd) = parse6("x = (a + b)");
    assert!(!structurally_eq(&a, &sa, &b, &sb));
    assert!(!structurally_eq(&a, &sa, &c, &sc));
    assert!(structurally_eq(&a, &sa, &d, &sd));
    let (e, se) = parse6("//@variable doc\nx = a + b");
    assert!(!structurally_eq(&a, &sa, &e, &se));
}

#[test]
fn structural_eq_ignores_spans_and_forms() {
    let p = |s: &str| {
        let mut syms = SymTable::new();
        let a = parse(&format!("//@version=6\n{s}\n"), FileId::ROOT, &mut syms).expect("parses");
        (a, syms)
    };
    let pairs = [
        ("x = 'a'", "x = \"a\""),
        ("f(x) => x", "f(x) =>\n    x"),
        ("y = ((a)) + (b)", "y   =   a + b"),
    ];
    for (l, r) in pairs {
        let ((a, sa), (b, sb)) = (p(l), p(r));
        assert!(structurally_eq(&a, &sa, &b, &sb), "{l:?} vs {r:?}");
    }
}

/// A splitmix64 stream: the grammar-directed generator's only randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// `format!` then `Gen::line`, so the arguments may draw from the generator first.
macro_rules! emit {
    ($g:expr, $lvl:expr, $($t:tt)*) => {{
        let s = format!($($t)*);
        $g.line($lvl, &s);
    }};
}

/// The grammar-directed program generator (§4.10): well-formed statements with valid layout.
struct Gen {
    rng: Rng,
    v6: bool,
    out: String,
}

const NAMES: [&str; 6] = ["a", "b", "c", "x", "y", "z"];

impl Gen {
    fn expr(&mut self, d: u32) -> String {
        let leaf = d == 0 || self.rng.below(3) == 0;
        if leaf {
            return match self.rng.below(8) {
                0 => self.rng.pick(&NAMES).to_string(),
                1 => self.rng.below(1000).to_string(),
                2 => self.rng.pick(&["1.5", ".5", "2e3", "3."]).to_string(),
                3 => self.rng.pick(&["true", "false"]).to_string(),
                4 => self
                    .rng
                    .pick(&["\"s\"", "'t'", "\"a\\nb\"", "'q\"'"])
                    .to_string(),
                5 => self.rng.pick(&["#FF0000", "#00ff0080"]).to_string(),
                6 => format!(
                    "{}.{}",
                    self.rng.pick(&NAMES),
                    self.rng.pick(&["p", "q", "if"])
                ),
                _ => format!("{}[{}]", self.rng.pick(&NAMES), self.rng.below(5)),
            };
        }
        let d = d - 1;
        match self.rng.below(9) {
            // Operator runs (the pine_lex_parse fuzz finding): up to 150 prefix operators.
            7 => {
                let run = self
                    .rng
                    .pick(&["-", "+", "not ", "- "])
                    .repeat(1 + self.rng.below(150) as usize);
                format!("({} - {run}{})", self.expr(d), self.rng.pick(&NAMES))
            }
            0 => format!(
                "({} {} {})",
                self.expr(d),
                self.rng.pick(&[
                    "+", "-", "*", "/", "%", "<", "<=", ">", ">=", "==", "!=", "and", "or"
                ]),
                self.expr(d)
            ),
            1 => format!("({}{})", self.rng.pick(&["-", "+", "not "]), self.expr(d)),
            2 => format!("({} ? {} : {})", self.expr(d), self.expr(d), self.expr(d)),
            3 => format!("f({}, n = {})", self.expr(d), self.expr(d)),
            4 => format!(
                "array.new<{}>({})",
                self.rng.pick(&["float", "int[]", "array<float>"]),
                self.expr(d)
            ),
            5 => format!("[{}, {}]", self.expr(d), self.expr(d)),
            6 => format!("{}[{}]", self.rng.pick(&NAMES), self.expr(d)),
            _ => format!("g({}).r", self.expr(d)),
        }
    }

    fn line(&mut self, level: u32, s: &str) {
        self.out.push_str(&" ".repeat(4 * level as usize));
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn block(&mut self, level: u32, in_loop: bool, depth: u32) {
        let n = 1 + self.rng.below(2);
        for _ in 0..n {
            self.stmt(level, in_loop, false, depth);
        }
    }

    fn stmt(&mut self, level: u32, in_loop: bool, global: bool, depth: u32) {
        let structured = depth > 0;
        let k = self.rng.below(if structured { 16 } else { 6 });
        let n = self.rng.pick(&NAMES);
        match k {
            0 => {
                let e = self.expr(2);
                let m =
                    self.rng
                        .pick(&["", "var ", "varip ", "float ", "series float ", "var int "]);
                emit!(self, level, "{m}{n} = {e}");
            }
            1 => {
                let e = self.expr(2);
                let op = self.rng.pick(&[":=", "+=", "-=", "*=", "/=", "%="]);
                emit!(self, level, "{n} {op} {e}");
            }
            2 => emit!(self, level, "[{n}, w] = f({})", self.expr(1)),
            3 => emit!(self, level, "[{n}, w] := f({})", self.expr(1)),
            4 => {
                let e = self.expr(2);
                emit!(self, level, "f({e})");
            }
            5 if in_loop => {
                let s = self.rng.pick(&["break", "continue"]);
                self.line(level, s)
            }
            5 => emit!(self, level, "h({})", self.expr(1)),
            6 => {
                let c = self.expr(1);
                emit!(self, level, "if {c}");
                self.block(level + 1, in_loop, depth - 1);
                if self.rng.below(2) == 0 {
                    self.line(level, "else");
                    self.block(level + 1, in_loop, depth - 1);
                }
            }
            7 => {
                let by = if self.rng.below(2) == 0 { " by 2" } else { "" };
                let ty = if self.rng.below(2) == 0 { "int " } else { "" };
                emit!(self, level, "for {ty}i = 0 to {}{by}", self.expr(1));
                self.block(level + 1, true, depth - 1);
            }
            8 => {
                let b = self.rng.pick(&["v", "[k, v]"]);
                emit!(self, level, "for {b} in {}", self.expr(1));
                self.block(level + 1, true, depth - 1);
            }
            9 => {
                emit!(self, level, "while {}", self.expr(1));
                self.block(level + 1, true, depth - 1);
            }
            10 => {
                let subj = if self.rng.below(2) == 0 {
                    format!(" {}", self.expr(1))
                } else {
                    String::new()
                };
                emit!(self, level, "{n} = switch{subj}");
                emit!(self, level + 1, "{} => {}", self.expr(1), self.expr(1));
                emit!(self, level + 1, "{} =>", self.expr(1));
                self.block(level + 2, in_loop, depth - 1);
                emit!(self, level + 1, "=> {}", self.expr(1));
            }
            11 if self.v6 => {
                let c = if self.rng.below(2) == 0 {
                    format!(" {}", self.expr(1))
                } else {
                    String::new()
                };
                emit!(self, level, "once{c}");
                self.block(level + 1, in_loop, depth - 1);
            }
            12 if global => {
                let ex = self.rng.pick(&["", "export "]);
                emit!(
                    self,
                    level,
                    "{ex}fn{}(p, simple int q = 1, float[] r) =>",
                    self.rng.below(9)
                );
                self.block(level + 1, false, depth - 1);
                emit!(self, level, "m{}(p) => p + 1", self.rng.below(9));
                self.line(level, "method mm(T t, int k) => t.u + k");
            }
            13 if global => {
                emit!(self, level, "type T{}", self.rng.below(9));
                self.line(level + 1, "float u = 0.0");
                self.line(level + 1, "varip int w");
                self.line(level + 1, "array<float> xs");
                emit!(self, level, "enum E{}", self.rng.below(9));
                self.line(level + 1, "lo");
                self.line(level + 1, "hi = \"High\"");
            }
            14 if global => {
                emit!(
                    self,
                    level,
                    "import user/lib{}/{} as l{}",
                    self.rng.below(9),
                    1 + self.rng.below(99),
                    self.rng.below(9)
                );
                if self.v6 {
                    self.line(level, "export const float K = 1.0");
                    self.line(level, "s = \"\"\"\nmulti\n  line\n\"\"\"");
                }
            }
            _ => emit!(self, level, "{n} = {}", self.expr(3)),
        }
    }
}

#[test]
fn prop_parse_print_parse_identical() {
    use pine_lang::{Ast, NodeKind};
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<NodeKind> = BTreeSet::new();
    let mut seed_rng = Rng(0x5EED_0001_0EA5_E5ED); // fixed seed (D1-a)
    for case in 0..4096u32 {
        let v6 = case % 4 != 0;
        let mut g = Gen {
            rng: Rng(seed_rng.next()),
            v6,
            out: String::new(),
        };
        g.out.push_str(if v6 {
            "//@version=6\n"
        } else {
            "//@version=5\n"
        });
        let n = 1 + g.rng.below(4);
        for _ in 0..n {
            g.stmt(0, false, true, 2);
        }
        let src = g.out;
        let mut s1 = SymTable::new();
        let a: Ast = parse(&src, FileId::ROOT, &mut s1)
            .unwrap_or_else(|d| panic!("case {case}: generated source refused {d:?}\n{src}"));
        for i in 0..a.len() as u32 {
            seen.insert(a.kind(pine_ir::NodeId(i)));
        }
        let mut text = String::new();
        print(&a, &s1, &mut text).expect("print");
        let mut s2 = SymTable::new();
        let b = parse(&text, FileId::ROOT, &mut s2)
            .unwrap_or_else(|d| panic!("case {case}: reparse refused {d:?}\n{text}"));
        assert!(
            structurally_eq(&a, &s1, &b, &s2),
            "case {case}\n{src}\n---\n{text}"
        );
    }
    let missing: Vec<NodeKind> = NodeKind::ALL
        .iter()
        .copied()
        .filter(|k| *k != NodeKind::Error && !seen.contains(k))
        .collect();
    assert!(
        missing.is_empty(),
        "the generator never produced {missing:?}"
    );
}

/// Parse → print → parse → `structurally_eq`, for a source the parser accepts.
fn assert_round_trips(src: &str) {
    let mut s1 = SymTable::new();
    let Ok(a) = parse(src, FileId::ROOT, &mut s1) else {
        return;
    };
    let mut text = String::new();
    print(&a, &s1, &mut text).expect("print");
    let mut s2 = SymTable::new();
    let b = parse(&text, FileId::ROOT, &mut s2).unwrap_or_else(|d| {
        let codes: Vec<u16> = d.iter().map(|x| x.code.0).collect();
        panic!("accepted source's canonical print refused {codes:?}:\n{text}")
    });
    assert!(structurally_eq(&a, &s1, &b, &s2), "{text}");
}

/// The pine_lex_parse fuzz finding: operands joined by long runs of `-` (binary minus followed
/// by a chain of unary minuses) — the printer gave every unary its own parentheses, so the
/// canonical form needed ~3 recursion levels per operator and reparsed as 2026.
#[test]
fn minus_runs_round_trip() {
    let runs: [&[usize]; 5] = [
        &[1, 150, 30, 20, 18],
        &[100],
        &[220],
        &[1, 1, 1, 1, 1],
        &[60, 60, 60],
    ];
    let ops = ["e", "P", "P", "e", "P", "P"];
    for v in [5, 6] {
        for r in runs {
            let mut body = String::from(ops[0]);
            for (k, n) in r.iter().enumerate() {
                body.push_str(&"-".repeat(*n));
                body.push_str(ops[(k + 1) % ops.len()]);
            }
            let src = format!("//@version={v}\n{body}");
            let mut syms = SymTable::new();
            assert!(parse(&src, FileId::ROOT, &mut syms).is_ok(), "{src}");
            assert_round_trips(&src);
        }
    }
    for unit in ["+", "not ", "- ", "-+"] {
        assert_round_trips(&format!("//@version=6\nx = a - {}b\n", unit.repeat(120)));
    }
}

/// Every accepted program round-trips right up to the depth limit, for each chain shape.
#[test]
fn deep_accepted_programs_round_trip() {
    let run = || {
        let mut n = 100usize;
        while n <= 520 {
            assert_round_trips(&format!("//@version=6\nx = {}a\n", "-".repeat(n)));
            assert_round_trips(&format!("//@version=6\nx = {}a\n", "not ".repeat(n)));
            assert_round_trips(&format!("//@version=6\nx = {}c\n", "a ? b : ".repeat(n)));
            assert_round_trips(&format!(
                "//@version=6\nx = {}b{}\n",
                "a ? ".repeat(n),
                " : c".repeat(n)
            ));
            assert_round_trips(&format!("//@version=6\nx = {}a\n", "a - ".repeat(n)));
            assert_round_trips(&format!("//@version=6\nx = {}a\n", "a - -".repeat(n)));
            assert_round_trips(&format!(
                "//@version=6\nx = {}a{}\n",
                "(".repeat(n),
                ")".repeat(n)
            ));
            assert_round_trips(&format!(
                "//@version=6\nx = {}a{}\n",
                "a - (".repeat(n),
                ")".repeat(n)
            ));
            n += 7;
        }
    };
    let h = std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(run)
        .expect("spawn");
    h.join().expect("no overflow, no panic");
}
