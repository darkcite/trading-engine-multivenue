// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The bounded regex of `str.match` (impl-04 §4.13.4): a subset parser, Thompson construction
//! and a Pike VM — leftmost-first, greedy, `O(len × states)` work, no allocation.
//!
//! Subset: literals, `.`, `\d \D \w \W \s \S`, classes `[a-z…]` / `[^…]`, anchors `^ $`, groups
//! `( )` (group 0 only), alternation `|`, quantifiers `* + ?` and `{m}`, `{m,}`, `{m,n}` with
//! `m, n ≤ 32`. No backreferences, lookaround, lazy or possessive quantifiers. Matching is over
//! bytes (`.` and a negated class consume one byte; `.` does not match `\n`).

use super::{code, err, LoadErr};

/// Instruction capacity of a compiled pattern.
pub const REGEX_MAX_STATES: usize = 256;
const MAX_CLASSES: usize = 16;
const MAX_NODES: usize = 512;
const MAX_DEPTH: u32 = 16;
const MAX_REPEAT: u32 = 32;
const INF: u8 = u8::MAX;

const OP_CHAR: u8 = 0;
const OP_ANY: u8 = 1;
const OP_CLASS: u8 = 2;
const OP_SPLIT: u8 = 3;
const OP_JMP: u8 = 4;
const OP_BOL: u8 = 5;
const OP_EOL: u8 = 6;
const OP_MATCH: u8 = 7;

/// One instruction: `op`, a byte or class index `a`, targets `x` (preferred) and `y`.
#[derive(Clone, Copy, Default)]
#[repr(C)]
struct RxInst {
    op: u8,
    a: u8,
    x: u16,
    y: u16,
    _pad: u16,
}
const _: () = assert!(core::mem::size_of::<RxInst>() == 8 && core::mem::align_of::<RxInst>() == 2);

/// A compiled pattern of the supported subset: fixed capacity, `#[repr(C)]`, 2 568 bytes.
#[derive(Clone, Copy)]
#[repr(C)]
pub struct RegexProg {
    classes: [[u64; 4]; MAX_CLASSES],
    insts: [RxInst; REGEX_MAX_STATES],
    n: u16,
    n_classes: u16,
    _pad: u32,
}
const _: () = assert!(core::mem::size_of::<RegexProg>() == 2568);
const _: () = assert!(core::mem::align_of::<RegexProg>() == 8);

const K_CHAR: u8 = 0;
const K_ANY: u8 = 1;
const K_CLASS: u8 = 2;
const K_BOL: u8 = 3;
const K_EOL: u8 = 4;
const K_EMPTY: u8 = 5;
const K_CAT: u8 = 6;
const K_ALT: u8 = 7;
const K_REP: u8 = 8;

/// An AST node in a fixed arena: `kind`, byte/class/min `a`, max `b`, children `l`, `r`.
#[derive(Clone, Copy, Default)]
struct Node {
    kind: u8,
    a: u8,
    b: u8,
    l: u16,
    r: u16,
}

struct Parser<'p> {
    s: &'p [u8],
    i: usize,
    nodes: [Node; MAX_NODES],
    n: usize,
    prog: RegexProg,
}

const fn refuse(at: usize, why: &'static str) -> LoadErr {
    err(code::REGEX_UNSUPPORTED, at, why)
}

const fn class_set(set: &mut [u64; 4], c: u8) {
    set[(c >> 6) as usize] |= 1 << (c & 63);
}

const fn class_has(set: &[u64; 4], c: u8) -> bool {
    set[(c >> 6) as usize] & (1 << (c & 63)) != 0
}

/// Add the predefined class `\d`, `\w` or `\s` (lower-case letter) to `set`.
fn add_predef(set: &mut [u64; 4], k: u8) {
    let mut c = 0u16;
    while c < 256 {
        let b = c as u8;
        let hit = match k {
            b'd' => b.is_ascii_digit(),
            b'w' => b.is_ascii_alphanumeric() || b == b'_',
            _ => matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C),
        };
        if hit {
            class_set(set, b);
        }
        c += 1;
    }
}

fn negate(set: &mut [u64; 4]) {
    let mut k = 0usize;
    while k < 4 {
        set[k] = !set[k];
        k += 1;
    }
}

/// A literal escape (`\n`, `\t`, `\r`, or an escaped punctuation byte); `None` otherwise.
fn literal_escape(c: u8) -> Option<u8> {
    match c {
        b'n' => Some(b'\n'),
        b't' => Some(b'\t'),
        b'r' => Some(b'\r'),
        b'.' | b'\\' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'*' | b'+' | b'?' | b'|'
        | b'^' | b'$' | b'/' | b'-' => Some(c),
        _ => None,
    }
}

impl Parser<'_> {
    fn node(&mut self, kind: u8, a: u8, b: u8, l: u16, r: u16) -> Result<u16, LoadErr> {
        if self.n == MAX_NODES {
            return Err(refuse(self.i, "pattern exceeds REGEX_MAX_STATES"));
        }
        self.nodes[self.n] = Node { kind, a, b, l, r };
        self.n += 1;
        Ok((self.n - 1) as u16)
    }

    fn new_class(&mut self, set: [u64; 4]) -> Result<u8, LoadErr> {
        let k = self.prog.n_classes as usize;
        if k == MAX_CLASSES {
            return Err(refuse(self.i, "more than 16 character classes"));
        }
        self.prog.classes[k] = set;
        self.prog.n_classes += 1;
        Ok(k as u8)
    }

    fn alt(&mut self, depth: u32) -> Result<u16, LoadErr> {
        let mut l = self.cat(depth)?;
        while self.s.get(self.i) == Some(&b'|') {
            self.i += 1;
            let r = self.cat(depth)?;
            l = self.node(K_ALT, 0, 0, l, r)?;
        }
        Ok(l)
    }

    fn cat(&mut self, depth: u32) -> Result<u16, LoadErr> {
        let mut acc: Option<u16> = None;
        while let Some(&c) = self.s.get(self.i) {
            if c == b'|' || c == b')' {
                break;
            }
            let a = self.atom(depth)?;
            let q = self.quant(a)?;
            acc = Some(match acc {
                None => q,
                Some(l) => self.node(K_CAT, 0, 0, l, q)?,
            });
        }
        match acc {
            Some(n) => Ok(n),
            None => self.node(K_EMPTY, 0, 0, 0, 0),
        }
    }

    fn num(&mut self) -> Option<u32> {
        let st = self.i;
        let mut v = 0u32;
        while let Some(&c) = self.s.get(self.i) {
            if !c.is_ascii_digit() || v > MAX_REPEAT {
                break;
            }
            v = v * 10 + (c - b'0') as u32;
            self.i += 1;
        }
        if self.i == st {
            None
        } else {
            Some(v)
        }
    }

    fn quant(&mut self, a: u16) -> Result<u16, LoadErr> {
        let at = self.i;
        let (min, max) = match self.s.get(self.i) {
            Some(b'*') => (0, INF),
            Some(b'+') => (1, INF),
            Some(b'?') => (0, 1),
            Some(b'{') => {
                self.i += 1;
                let m = self
                    .num()
                    .ok_or(refuse(self.i, "expected a repeat count"))?;
                let n = if self.s.get(self.i) == Some(&b',') {
                    self.i += 1;
                    match self.num() {
                        Some(n) => n,
                        None => INF as u32,
                    }
                } else {
                    m
                };
                if self.s.get(self.i) != Some(&b'}') {
                    return Err(refuse(self.i, "expected '}'"));
                }
                if m > MAX_REPEAT || (n != INF as u32 && (n > MAX_REPEAT || n < m)) {
                    return Err(refuse(at, "repeat bounds must satisfy m <= n <= 32"));
                }
                (m as u8, n as u8)
            }
            _ => return Ok(a),
        };
        self.i += 1;
        if matches!(self.s.get(self.i), Some(b'?' | b'+' | b'*' | b'{')) {
            return Err(refuse(
                self.i,
                "lazy, possessive or stacked quantifiers are unsupported",
            ));
        }
        self.node(K_REP, min, max, a, 0)
    }

    fn atom(&mut self, depth: u32) -> Result<u16, LoadErr> {
        let at = self.i;
        let c = self.s[self.i];
        self.i += 1;
        match c {
            b'.' => self.node(K_ANY, 0, 0, 0, 0),
            b'^' => self.node(K_BOL, 0, 0, 0, 0),
            b'$' => self.node(K_EOL, 0, 0, 0, 0),
            b'(' => {
                if self.s.get(self.i) == Some(&b'?') {
                    return Err(refuse(
                        self.i,
                        "group modifiers and lookaround are unsupported",
                    ));
                }
                if depth == MAX_DEPTH {
                    return Err(refuse(at, "groups nested deeper than 16"));
                }
                let n = self.alt(depth + 1)?;
                if self.s.get(self.i) != Some(&b')') {
                    return Err(refuse(at, "unbalanced '('"));
                }
                self.i += 1;
                Ok(n)
            }
            b'[' => self.class(),
            b'\\' => {
                let e = *self.s.get(self.i).ok_or(refuse(at, "trailing backslash"))?;
                self.i += 1;
                if let Some(l) = literal_escape(e) {
                    return self.node(K_CHAR, l, 0, 0, 0);
                }
                let lower = e.to_ascii_lowercase();
                if !matches!(lower, b'd' | b'w' | b's') {
                    return Err(refuse(at, "unsupported escape"));
                }
                let mut set = [0u64; 4];
                add_predef(&mut set, lower);
                if e.is_ascii_uppercase() {
                    negate(&mut set);
                }
                let k = self.new_class(set)?;
                self.node(K_CLASS, k, 0, 0, 0)
            }
            b')' => Err(refuse(at, "unbalanced ')'")),
            b'*' | b'+' | b'?' | b'{' => Err(refuse(at, "quantifier without an operand")),
            _ => self.node(K_CHAR, c, 0, 0, 0),
        }
    }

    /// One class member byte (after `[`), resolving escapes; `Err` on a predefined class.
    fn class_byte(&mut self) -> Result<Option<u8>, LoadErr> {
        let at = self.i;
        let c = *self.s.get(self.i).ok_or(refuse(at, "unterminated '['"))?;
        self.i += 1;
        if c != b'\\' {
            return Ok(Some(c));
        }
        let e = *self.s.get(self.i).ok_or(refuse(at, "trailing backslash"))?;
        self.i += 1;
        match literal_escape(e) {
            Some(l) => Ok(Some(l)),
            None if matches!(e, b'd' | b'w' | b's') => Ok(None),
            None => Err(refuse(at, "unsupported escape in a class")),
        }
    }

    fn class(&mut self) -> Result<u16, LoadErr> {
        let open = self.i - 1;
        let mut set = [0u64; 4];
        let neg = self.s.get(self.i) == Some(&b'^');
        if neg {
            self.i += 1;
        }
        let mut first = true;
        loop {
            match self.s.get(self.i) {
                None => return Err(refuse(open, "unterminated '['")),
                Some(b']') if !first => {
                    self.i += 1;
                    break;
                }
                _ => {}
            }
            first = false;
            let esc = self.s[self.i] == b'\\';
            let pre = if esc {
                self.s.get(self.i + 1).copied()
            } else {
                None
            };
            let Some(lo) = self.class_byte()? else {
                let mut p = [0u64; 4];
                add_predef(&mut p, pre.unwrap_or(b's'));
                let mut k = 0usize;
                while k < 4 {
                    set[k] |= p[k];
                    k += 1;
                }
                continue;
            };
            if self.s.get(self.i) == Some(&b'-')
                && self.s.get(self.i + 1).is_some_and(|&c| c != b']')
            {
                self.i += 1;
                let at = self.i;
                let hi = self
                    .class_byte()?
                    .ok_or(refuse(at, "a range cannot end in a class"))?;
                if hi < lo {
                    return Err(refuse(at, "inverted class range"));
                }
                let mut c = lo as u16;
                while c <= hi as u16 {
                    class_set(&mut set, c as u8);
                    c += 1;
                }
            } else {
                class_set(&mut set, lo);
            }
        }
        if neg {
            negate(&mut set);
        }
        let k = self.new_class(set)?;
        self.node(K_CLASS, k, 0, 0, 0)
    }

    fn emit(&mut self, op: u8, a: u8, x: u16, y: u16) -> Result<u16, LoadErr> {
        let pc = self.prog.n as usize;
        if pc == REGEX_MAX_STATES {
            return Err(refuse(0, "pattern exceeds REGEX_MAX_STATES"));
        }
        self.prog.insts[pc] = RxInst {
            op,
            a,
            x,
            y,
            _pad: 0,
        };
        self.prog.n += 1;
        Ok(pc as u16)
    }

    fn gen(&mut self, id: u16) -> Result<(), LoadErr> {
        let nd = self.nodes[id as usize];
        match nd.kind {
            K_CHAR => self.emit(OP_CHAR, nd.a, 0, 0).map(drop),
            K_ANY => self.emit(OP_ANY, 0, 0, 0).map(drop),
            K_CLASS => self.emit(OP_CLASS, nd.a, 0, 0).map(drop),
            K_BOL => self.emit(OP_BOL, 0, 0, 0).map(drop),
            K_EOL => self.emit(OP_EOL, 0, 0, 0).map(drop),
            K_EMPTY => Ok(()),
            K_CAT => {
                self.gen(nd.l)?;
                self.gen(nd.r)
            }
            K_ALT => {
                let sp = self.emit(OP_SPLIT, 0, 0, 0)?;
                self.prog.insts[sp as usize].x = sp + 1;
                self.gen(nd.l)?;
                let j = self.emit(OP_JMP, 0, 0, 0)?;
                self.prog.insts[sp as usize].y = self.prog.n;
                self.gen(nd.r)?;
                self.prog.insts[j as usize].x = self.prog.n;
                Ok(())
            }
            _ => self.gen_rep(nd),
        }
    }

    fn gen_rep(&mut self, nd: Node) -> Result<(), LoadErr> {
        let mut k = 0u8;
        while k < nd.a {
            self.gen(nd.l)?;
            k += 1;
        }
        if nd.b == INF {
            let sp = self.emit(OP_SPLIT, 0, 0, 0)?;
            self.prog.insts[sp as usize].x = sp + 1;
            self.gen(nd.l)?;
            self.emit(OP_JMP, 0, sp, 0)?;
            self.prog.insts[sp as usize].y = self.prog.n;
            return Ok(());
        }
        // Optional copies: each SPLIT prefers the copy and skips to the common end.
        let mut sps = [0u16; MAX_REPEAT as usize];
        let mut n = 0usize;
        while k < nd.b {
            let sp = self.emit(OP_SPLIT, 0, 0, 0)?;
            self.prog.insts[sp as usize].x = sp + 1;
            sps[n] = sp;
            n += 1;
            self.gen(nd.l)?;
            k += 1;
        }
        let end = self.prog.n;
        let mut j = 0usize;
        while j < n {
            self.prog.insts[sps[j] as usize].y = end;
            j += 1;
        }
        Ok(())
    }
}

/// Compile `pat`: 5004 for anything outside the subset or over `REGEX_MAX_STATES`.
pub fn regex_compile(pat: &[u8]) -> Result<RegexProg, LoadErr> {
    let mut p = Parser {
        s: pat,
        i: 0,
        nodes: [Node::default(); MAX_NODES],
        n: 0,
        prog: RegexProg {
            classes: [[0; 4]; MAX_CLASSES],
            insts: [RxInst::default(); REGEX_MAX_STATES],
            n: 0,
            n_classes: 0,
            _pad: 0,
        },
    };
    let root = p.alt(0)?;
    if p.i != pat.len() {
        return Err(refuse(p.i, "unbalanced ')'"));
    }
    p.gen(root)?;
    p.emit(OP_MATCH, 0, 0, 0)?;
    Ok(p.prog)
}

/// A Pike-VM thread list: program counters in priority order with each thread's match start,
/// and the set of pcs already on the list this step.
struct Threads {
    pc: [u16; REGEX_MAX_STATES],
    start: [u32; REGEX_MAX_STATES],
    n: usize,
    on: [u64; REGEX_MAX_STATES / 64],
}

impl Threads {
    const fn new() -> Self {
        Self {
            pc: [0; REGEX_MAX_STATES],
            start: [0; REGEX_MAX_STATES],
            n: 0,
            on: [0; 4],
        }
    }
    fn clear(&mut self) {
        self.n = 0;
        self.on = [0; 4];
    }
}

impl RegexProg {
    /// Follow the epsilon closure of `pc0` at byte position `pos`, appending consuming and
    /// `MATCH` threads to `t` in priority order (an explicit stack, `x` before `y`).
    fn add(&self, t: &mut Threads, pc0: u16, start: u32, pos: usize, s: &[u8]) {
        // Each pc is expanded once and pushes at most two successors.
        let mut stack = [0u16; 2 * REGEX_MAX_STATES + 1];
        let mut sp = 1usize;
        stack[0] = pc0;
        while sp > 0 {
            sp -= 1;
            let pc = stack[sp];
            let (w, b) = ((pc >> 6) as usize, 1u64 << (pc & 63));
            if t.on[w] & b != 0 {
                continue;
            }
            t.on[w] |= b;
            let i = self.insts[pc as usize];
            match i.op {
                OP_JMP => {
                    stack[sp] = i.x;
                    sp += 1;
                }
                OP_SPLIT => {
                    stack[sp] = i.y;
                    stack[sp + 1] = i.x;
                    sp += 2;
                }
                OP_BOL => {
                    if pos == 0 {
                        stack[sp] = pc + 1;
                        sp += 1;
                    }
                }
                OP_EOL => {
                    if pos == s.len() {
                        stack[sp] = pc + 1;
                        sp += 1;
                    }
                }
                _ => {
                    t.pc[t.n] = pc;
                    t.start[t.n] = start;
                    t.n += 1;
                }
            }
        }
    }

    /// The leftmost-first match of the pattern in `s` as a byte range `(start, end)`, greedy
    /// within the subset's priorities; `None` without a match. Work ≤ `(len + 1) × states`.
    #[must_use]
    pub fn find(&self, s: &[u8]) -> Option<(usize, usize)> {
        let mut cur = Threads::new();
        let mut nxt = Threads::new();
        let mut found: Option<(usize, usize)> = None;
        let mut pos = 0usize;
        loop {
            if found.is_none() {
                self.add(&mut cur, 0, pos as u32, pos, s);
            }
            if cur.n == 0 && found.is_some() {
                break;
            }
            nxt.clear();
            let mut k = 0usize;
            while k < cur.n {
                let (pc, st) = (cur.pc[k], cur.start[k]);
                let i = self.insts[pc as usize];
                let hit = match (i.op, s.get(pos)) {
                    (OP_MATCH, _) => {
                        found = Some((st as usize, pos));
                        break;
                    }
                    (OP_CHAR, Some(&c)) => c == i.a,
                    (OP_ANY, Some(&c)) => c != b'\n',
                    (OP_CLASS, Some(&c)) => class_has(&self.classes[i.a as usize], c),
                    _ => false,
                };
                if hit {
                    self.add(&mut nxt, pc + 1, st, pos + 1, s);
                }
                k += 1;
            }
            if pos == s.len() {
                break;
            }
            pos += 1;
            core::mem::swap(&mut cur, &mut nxt);
        }
        found
    }
}
