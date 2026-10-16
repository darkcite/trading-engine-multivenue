// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-05 §7.10 `grep_gates.rs` — the source greps over `crates/pine-broker/src` with comments,
//! string literals and `#[cfg(test)]` items stripped:
//! * `no_float_no_eight_decimal_scale_no_clock_no_dyn` (I4, P5) — no `f64` / `f32` / float
//!   literal, no `100_000_000`, no `std::time` / `Instant` / `SystemTime`, no `dyn`;
//! * `allocation_only_in_closed_ring_new` (I4) — no `Vec` / `String` / `vec!` / `format!` /
//!   `Box::new` / `to_string` / `to_vec` / `to_owned` / `collect` outside `ClosedRing::new`, and
//!   no closure handed to an iterator loop;
//! * `book_mutated_from_the_walk_and_on_fill_only` (I25) — `Book`'s mutators (`apply_fill`,
//!   `mark`, `set_liquidation_price`, `reset`, a `&mut` loan of the book) are reached only from the TV
//!   walk and `NativeBroker::on_fill` / `open_bar` / `reconcile_boot`;
//! * `one_dependency_and_it_is_pine_ir` (I2).
//!
//! Each gate is a function over source text, proven live by a test that runs it over a scratch
//! string containing a violating line. Test code: allocation is fine here.

use std::fs;
use std::path::{Path, PathBuf};

const CRATE: &str = env!("CARGO_MANIFEST_DIR");

/// Every `.rs` file under `dir`, sorted (deterministic reports).
fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap_or_else(|e| panic!("{}: {e}", d.display())) {
            let p = e.unwrap_or_else(|e| panic!("{e}")).path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// `(path relative to src, raw text)` for every source file.
fn sources() -> Vec<(String, String)> {
    let src = Path::new(CRATE).join("src");
    rs_files(&src)
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&src)
                .unwrap_or_else(|e| panic!("{e}"))
                .to_string_lossy()
                .replace('\\', "/");
            (
                rel,
                fs::read_to_string(&p).unwrap_or_else(|e| panic!("{e}")),
            )
        })
        .collect()
}

/// Blank comments, string and char literals (newlines kept, so line numbers survive).
fn blank_comments_and_literals(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let blank = |ch: char| if ch == '\n' { '\n' } else { ' ' };
    while i < c.len() {
        if c[i] == '/' && c.get(i + 1) == Some(&'/') {
            while i < c.len() && c[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c[i] == '/' && c.get(i + 1) == Some(&'*') {
            let mut depth = 0;
            while i < c.len() {
                if c[i] == '/' && c.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push_str("  ");
                    i += 2;
                } else if c[i] == '*' && c.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push_str("  ");
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(c[i]));
                    i += 1;
                }
            }
        } else if c[i] == '"' {
            out.push('"');
            i += 1;
            while i < c.len() && c[i] != '"' {
                if c[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                if i < c.len() {
                    out.push(blank(c[i]));
                    i += 1;
                }
            }
            out.push('"');
            i += 1;
        } else if c[i] == '\'' && ((c.get(i + 2) == Some(&'\'')) || (c.get(i + 1) == Some(&'\\'))) {
            // A char literal ('x' or '\x…'); a lifetime has no closing quote two chars on.
            out.push('\'');
            i += 1;
            while i < c.len() && c[i] != '\'' {
                if c[i] == '\\' {
                    out.push(' ');
                    i += 1;
                }
                out.push(' ');
                i += 1;
            }
            out.push('\'');
            i += 1;
        } else {
            out.push(c[i]);
            i += 1;
        }
    }
    out
}

/// Blank the item after every `#[cfg(test)]` (a braced item, or up to its `;`).
fn blank_cfg_test(s: &str) -> String {
    let mut c: Vec<char> = s.chars().collect();
    let pat: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut i = 0;
    while i + pat.len() <= c.len() {
        if c[i..i + pat.len()] == pat[..] {
            let mut j = i + pat.len();
            while j < c.len() && c[j] != '{' && c[j] != ';' {
                j += 1;
            }
            if j < c.len() && c[j] == '{' {
                let mut depth = 0;
                while j < c.len() {
                    if c[j] == '{' {
                        depth += 1;
                    } else if c[j] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
            }
            let end = (j + 1).min(c.len());
            for ch in &mut c[i..end] {
                if *ch != '\n' {
                    *ch = ' ';
                }
            }
            i = end;
        } else {
            i += 1;
        }
    }
    c.into_iter().collect()
}

/// The text a gate reads: no comments, no literals, no test items.
fn code_of(raw: &str) -> String {
    blank_cfg_test(&blank_comments_and_literals(raw))
}

/// A token: identifier / number run, its byte offset and line.
struct Tok<'a> {
    text: &'a str,
    at: usize,
    line: usize,
}

fn tokens(code: &str) -> Vec<Tok<'_>> {
    let b = code.as_bytes();
    let mut out = Vec::new();
    let (mut i, mut line) = (0, 1);
    while i < b.len() {
        if b[i] == b'\n' {
            line += 1;
            i += 1;
        } else if b[i].is_ascii_alphanumeric() || b[i] == b'_' {
            let s = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok {
                text: &code[s..i],
                at: s,
                line,
            });
        } else {
            i += 1;
        }
    }
    out
}

/// The next non-blank character at or after byte `at`.
fn next_char(code: &str, at: usize) -> Option<char> {
    code[at..].chars().find(|c| !c.is_whitespace())
}

/// The previous non-blank character before byte `at`.
fn prev_char(code: &str, at: usize) -> Option<char> {
    code[..at].chars().rev().find(|c| !c.is_whitespace())
}

/// Whether the numeric token is a float literal, or the eight-decimal scale `100_000_000`.
fn bad_number(code: &str, t: &Tok<'_>) -> Option<&'static str> {
    let x = t.text;
    if !x.as_bytes()[0].is_ascii_digit() || prev_char(code, t.at) == Some('.') {
        return None;
    }
    let hex = x.starts_with("0x") || x.starts_with("0b") || x.starts_with("0o");
    let digits: String = x
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '_')
        .filter(|c| *c != '_')
        .collect();
    let rest = &x[x
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '_'))
        .map_or(x.len(), |(k, _)| k)..];
    if !hex && (rest.starts_with('e') || rest.starts_with('E') || rest.starts_with('f')) {
        return Some("float literal");
    }
    let after = &code[t.at + x.len()..];
    if !hex
        && after.starts_with('.')
        && after[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
    {
        return Some("float literal");
    }
    if !hex && digits == "100000000" {
        return Some("eight-decimal scale");
    }
    None
}

/// Gate I4 / P5 over one file's code: floats, the 1e8 scale, clocks, `dyn`.
fn gate_float_scale_clock_dyn(file: &str, raw: &str) -> Vec<String> {
    let code = code_of(raw);
    let mut v = Vec::new();
    for t in tokens(&code) {
        let hit = match t.text {
            "f64" | "f32" => Some("float type"),
            "Instant" | "SystemTime" | "UNIX_EPOCH" => Some("clock"),
            "dyn" => Some("dyn"),
            "time" if code[..t.at].trim_end().ends_with("std::") => Some("std::time"),
            _ => bad_number(&code, &t),
        };
        if let Some(h) = hit {
            v.push(format!("{file}:{}: {h} `{}`", t.line, t.text));
        }
    }
    v
}

/// Every fn in `code`: `(name, start, end)` — `start` at its `fn` (the signature belongs to it),
/// `end` past its body's `}`.
fn fn_bodies(code: &str) -> Vec<(String, usize, usize)> {
    let toks = tokens(code);
    let mut out = Vec::new();
    for k in 1..toks.len() {
        if toks[k - 1].text != "fn" {
            continue;
        }
        let t = &toks[k];
        let rest = &code[t.at..];
        let Some(open) = rest.find(['{', ';']) else {
            continue;
        };
        if rest.as_bytes()[open] == b';' {
            continue; // a bodiless declaration
        }
        let open = t.at + open;
        let mut depth = 0;
        for (j, ch) in code[open..].char_indices() {
            if ch == '{' {
                depth += 1;
            } else if ch == '}' {
                depth -= 1;
                if depth == 0 {
                    out.push((t.text.to_string(), toks[k - 1].at, open + j + 1));
                    break;
                }
            }
        }
    }
    out
}

/// The byte range of the first `fn <name>` (signature and body) inside `code`.
fn fn_body(code: &str, name: &str) -> Option<(usize, usize)> {
    fn_bodies(code)
        .into_iter()
        .find(|(n, _, _)| n == name)
        .map(|(_, s, e)| (s, e))
}

/// Iterator adapters that take a closure — a closure "passed to a loop".
const LOOP_ADAPTERS: [&str; 18] = [
    "for_each",
    "map",
    "filter",
    "filter_map",
    "fold",
    "any",
    "all",
    "find",
    "position",
    "take_while",
    "skip_while",
    "map_while",
    "flat_map",
    "try_for_each",
    "retain",
    "sort_by",
    "sort_by_key",
    "inspect",
];

/// Gate I4 over one file: allocation outside `ClosedRing::new`, closures handed to loops.
fn gate_allocation(file: &str, raw: &str) -> Vec<String> {
    let mut code = code_of(raw);
    if file == "closed.rs" {
        // The crate's one allocation lives in `ClosedRing::new` (§4.17).
        if let Some((s, e)) = fn_body(&code, "new") {
            let blanked: String = code[s..e]
                .chars()
                .map(|c| if c == '\n' { '\n' } else { ' ' })
                .collect();
            code.replace_range(s..e, &blanked);
        }
    }
    let mut v = Vec::new();
    let toks = tokens(&code);
    for (k, t) in toks.iter().enumerate() {
        let after = &code[t.at + t.text.len()..];
        let bang = after.trim_start().starts_with('!');
        let call = after.trim_start().starts_with('(');
        let method = prev_char(&code, t.at) == Some('.');
        let hit = match t.text {
            "Vec" | "String" | "BTreeMap" | "HashMap" | "VecDeque" => Some("heap type"),
            "vec" | "format" if bang => Some("allocating macro"),
            "new" if k > 0 && toks[k - 1].text == "Box" => Some("Box::new"),
            "to_string" | "to_vec" | "to_owned" | "collect" | "into_boxed_slice"
                if method
                    && (after.trim_start().starts_with("()")
                        || after.trim_start().starts_with("::<")) =>
            {
                Some("allocating call")
            }
            a if method
                && call
                && LOOP_ADAPTERS.contains(&a)
                && after.trim_start()[1..].trim_start().starts_with(['|', 'm']) =>
            {
                Some("closure passed to a loop")
            }
            _ => None,
        };
        if let Some(h) = hit {
            v.push(format!("{file}:{}: {h} `{}`", t.line, t.text));
        }
    }
    v
}

/// Book mutators and where each may be reached (I25): `(file, enclosing fns)`.
const BOOK_MUTATORS: [&str; 4] = ["apply_fill", "mark", "set_liquidation_price", "reset"];
const BOOK_SITES: [(&str, &[&str]); 3] = [
    // The TV walk: the tick, the margin call, the risk flatten, an order fill, a leg fill.
    (
        "tv/mod.rs",
        &[
            "tick",
            "margin_check",
            "flatten",
            "execute_order",
            "fill_leg",
        ],
    ),
    // The leg fill the walk hands `&mut Book` to.
    ("exit.rs", &["fill_set"]),
    ("native/mod.rs", &["on_fill", "open_bar", "reconcile_boot"]),
];

/// Gate I25 over one file: every `book.<mutator>(`, `&mut self.book` and `&mut Book` site sits in
/// an allowed fn of an allowed file.
fn gate_book(file: &str, raw: &str) -> Vec<String> {
    if file == "book.rs" {
        return Vec::new();
    }
    let code = code_of(raw);
    let fns = fn_bodies(&code);
    let allowed: &[&str] = BOOK_SITES
        .iter()
        .find(|(f, _)| *f == file)
        .map_or(&[], |(_, a)| a);
    let mut v = Vec::new();
    let toks = tokens(&code);
    for (k, t) in toks.iter().enumerate() {
        let mutating_call = BOOK_MUTATORS.contains(&t.text)
            && k > 0
            && toks[k - 1].text == "book"
            && prev_char(&code, t.at) == Some('.')
            && next_char(&code, t.at + t.text.len()) == Some('(');
        let loan = (t.text == "book" || t.text == "Book")
            && code[..t.at]
                .trim_end()
                .trim_end_matches("self.")
                .trim_end()
                .ends_with("&mut");
        let assign = t.text == "book"
            && prev_char(&code, t.at) == Some('.')
            && code[t.at + 4..].trim_start().starts_with('=')
            && !code[t.at + 4..].trim_start().starts_with("==");
        if mutating_call || loan || assign {
            let inner = fns
                .iter()
                .filter(|(_, s, e)| *s <= t.at && t.at < *e)
                .min_by_key(|(_, s, e)| e - s)
                .map(|(n, _, _)| n.as_str());
            let ok = inner.is_some_and(|n| allowed.contains(&n));
            if !ok {
                v.push(format!(
                    "{file}:{}: book mutated in `{}`",
                    t.line,
                    inner.unwrap_or("<item>")
                ));
            }
        }
    }
    v
}

/// The `[section]` names of a manifest's dependency keys.
fn deps(manifest: &str, section: &str) -> Vec<String> {
    let mut on = false;
    let mut out = Vec::new();
    for line in manifest.lines() {
        let l = line.split('#').next().unwrap_or("").trim();
        if l.starts_with('[') {
            on = l == format!("[{section}]");
        } else if on && l.contains('=') {
            out.push(l.split('=').next().unwrap_or("").trim().to_string());
        }
    }
    out
}

/// Gate I2: exactly one dependency, `pine-ir`; no other workspace crate as a dev-dependency;
/// no build-dependencies.
fn gate_dependencies(manifest: &str, workspace: &[String]) -> Vec<String> {
    let mut v = Vec::new();
    let d = deps(manifest, "dependencies");
    if d != ["pine-ir"] {
        v.push(format!("[dependencies] is {d:?}, not [\"pine-ir\"]"));
    }
    for x in deps(manifest, "dev-dependencies") {
        if workspace.contains(&x) {
            v.push(format!("dev-dependency on workspace crate {x}"));
        }
    }
    let b = deps(manifest, "build-dependencies");
    if !b.is_empty() {
        v.push(format!("build-dependencies {b:?}"));
    }
    v
}

/// The workspace member crate names (the root manifest's `members`, by directory name).
fn workspace_members() -> Vec<String> {
    let root = Path::new(CRATE).join("../../Cargo.toml");
    let m = fs::read_to_string(&root).unwrap_or_else(|e| panic!("{}: {e}", root.display()));
    let s = m.find("members").unwrap_or_else(|| panic!("no members"));
    let open = s + m[s..].find('[').unwrap_or(0);
    let close = open + m[open..].find(']').unwrap_or(0);
    m[open + 1..close]
        .split(',')
        .map(|x| x.trim().trim_matches('"'))
        .filter(|x| !x.is_empty())
        .map(|x| x.rsplit('/').next().unwrap_or(x).to_string())
        .collect()
}

fn run_gate(g: fn(&str, &str) -> Vec<String>) -> Vec<String> {
    let mut v = Vec::new();
    for (f, raw) in sources() {
        v.extend(g(&f, &raw));
    }
    v
}

#[test]
fn no_float_no_eight_decimal_scale_no_clock_no_dyn() {
    let v = run_gate(gate_float_scale_clock_dyn);
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn allocation_only_in_closed_ring_new() {
    let v = run_gate(gate_allocation);
    assert!(v.is_empty(), "{v:#?}");
    // The one allocation is where the gate excuses it.
    let (_, closed) = sources()
        .into_iter()
        .find(|(f, _)| f == "closed.rs")
        .unwrap_or_else(|| panic!("no closed.rs"));
    let code = code_of(&closed);
    let (s, e) = fn_body(&code, "new").unwrap_or_else(|| panic!("no ClosedRing::new"));
    assert!(
        code[s..e].contains("vec!"),
        "ClosedRing::new allocates the ring"
    );
}

#[test]
fn book_mutated_from_the_walk_and_on_fill_only() {
    let v = run_gate(gate_book);
    assert!(v.is_empty(), "{v:#?}");
}

#[test]
fn one_dependency_and_it_is_pine_ir() {
    let m =
        fs::read_to_string(Path::new(CRATE).join("Cargo.toml")).unwrap_or_else(|e| panic!("{e}"));
    let ws = workspace_members();
    assert!(ws.iter().any(|x| x == "pine-ir"), "{ws:?}");
    let v = gate_dependencies(&m, &ws);
    assert!(v.is_empty(), "{v:#?}");
}

// ------------------------------------------------- the gates are live ----

#[test]
fn the_float_scale_clock_dyn_gate_is_live() {
    let ok = "// f64 in a comment\nfn a() -> i64 { let s = \"f32 dyn\"; 100 * 1_000_000 }\n\
              #[cfg(test)]\nmod tests { fn b() -> f64 { 1.5 } }\nfn c(x: (i64, i64)) -> i64 { x.0 }\n";
    assert!(gate_float_scale_clock_dyn("ok.rs", ok).is_empty());
    for bad in [
        "fn a(x: f64) {}",
        "fn a() -> f32 { 0 }",
        "const S: i64 = 100_000_000;",
        "const X: i64 = 1.5;",
        "const Y: i64 = 1e6;",
        "use std::time::Duration;",
        "fn a() { let t = Instant::now(); }",
        "fn a() { SystemTime::now(); }",
        "fn a(x: &dyn Fn()) {}",
    ] {
        assert_eq!(
            gate_float_scale_clock_dyn("bad.rs", bad).len(),
            1,
            "missed: {bad}"
        );
    }
}

#[test]
fn the_allocation_gate_is_live() {
    let ok = "fn a(x: Box<[u8]>) -> usize { let mut i = 0; while i < 3 { i += 1; } i }\n\
              fn b(o: Option<u8>) -> u8 { o.unwrap_or(0) }\n\
              #[cfg(test)]\nmod tests { fn c() -> Vec<u8> { vec![1].iter().map(|x| *x).collect() } }\n";
    assert!(gate_allocation("x.rs", ok).is_empty());
    let ring = "impl R { fn new() -> Self { Self { r: vec![0; 9].into_boxed_slice() } } }";
    assert!(gate_allocation("closed.rs", ring).is_empty());
    assert!(!gate_allocation("other.rs", ring).is_empty());
    for bad in [
        "fn a() -> Vec<u8> { todo() }",
        "fn a() { let s = String::new(); }",
        "fn a() { let v = vec![1]; }",
        "fn a(x: u8) { let s = format!(\"{x}\"); }",
        "fn a() { let b = Box::new(1); }",
        "fn a(x: u8) { let s = x.to_string(); }",
        "fn a(x: &[u8]) { x.iter().for_each(|b| f(b)); }",
        "fn a(x: &[u8]) -> bool { x.iter().any(|b| *b == 0) }",
    ] {
        assert!(!gate_allocation("bad.rs", bad).is_empty(), "missed: {bad}");
    }
}

#[test]
fn the_book_gate_is_live() {
    let ok = "impl N { fn on_fill(&mut self) { self.book\n  .apply_fill(1); }\n\
              fn view(&self) -> &V { self.book.view() } }";
    assert!(gate_book("native/mod.rs", ok).is_empty());
    for bad in [
        "impl N { fn reconcile(&mut self) { self.book.apply_fill(1); } }",
        "impl N { fn on_ack(&mut self) { self.book.mark(1); } }",
        "impl N { fn reconcile(&mut self) { f(&mut self.book); } }",
        "impl N { fn on_cancelled(&mut self) { self.book = Book::new(); } }",
    ] {
        assert_eq!(gate_book("native/mod.rs", bad).len(), 1, "missed: {bad}");
    }
    // The walk's fns in another file are not licensed.
    let walk = "impl T { fn tick(&mut self) { self.book.mark(1); } }";
    assert!(gate_book("tv/mod.rs", walk).is_empty());
    assert_eq!(gate_book("orders.rs", walk).len(), 1);
}

#[test]
fn the_dependency_gate_is_live() {
    let ws = vec!["pine-ir".to_string(), "core-alloc".to_string()];
    let ok = "[package]\nname = \"x\"\n[dependencies]\npine-ir = { workspace = true }\n\
              [dev-dependencies]\nproptest = { workspace = true }\n";
    assert!(gate_dependencies(ok, &ws).is_empty());
    let two = "[dependencies]\npine-ir = { workspace = true }\nlibc = \"0.2\"\n";
    assert_eq!(gate_dependencies(two, &ws).len(), 1);
    let dev = "[dependencies]\npine-ir = { workspace = true }\n[dev-dependencies]\ncore-alloc = { workspace = true }\n";
    assert_eq!(gate_dependencies(dev, &ws).len(), 1);
    let none = "[dependencies]\n";
    assert_eq!(gate_dependencies(none, &ws).len(), 1);
}
