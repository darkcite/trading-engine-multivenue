// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//
// PMJ J16 — slot 11's gate lines, parsed in ONE place: `build.rs`
// `include!`s this file (so it carries no inner attribute and no `//!`
// doc), and the library compiles it as `cli::gate_line` for its tests.
// COPY-DOCTRINE: build-time and test-time text scanning; nothing here is
// reachable from the engine loop.

/// The dust gate's grammar: a line that BEGINS with this, then a date
/// `YYYY-MM-DD` (the operator's ruling, written into the risk policy).
pub const DUST_GATE: &str = "PMJ-DUST-GATE: ruled ";

/// The pilot's gate (plan-J8), the same grammar.
pub const LIVE_GATE: &str = "PMJ-LIVE-GATE: ruled ";

/// Whether `line` begins with `prefix` and then a date `YYYY-MM-DD`
/// (digits and dashes only — the ruling's day; what follows is free).
pub fn is_gate_line(line: &str, prefix: &str) -> bool {
    let Some(rest) = line.strip_prefix(prefix) else {
        return false;
    };
    let d = rest.as_bytes();
    if d.len() < 10 {
        return false;
    }
    let mut i = 0usize;
    while i < 10 {
        let ok = if i == 4 || i == 7 { d[i] == b'-' } else { d[i].is_ascii_digit() };
        if !ok {
            return false;
        }
        i += 1;
    }
    true
}

/// `(dust, live)`: whether `text` (a risk policy) carries each gate line
/// OUTSIDE fenced code blocks — a line beginning (after optional
/// indentation) with three backticks or three tildes opens a fence, and
/// only a line beginning with the same three closes it; a gate word quoted
/// inside one, or mid-line anywhere, opens nothing.
pub fn gate_lines_in(text: &str) -> (bool, bool) {
    let mut fence: Option<u8> = None;
    let mut dust = false;
    let mut live = false;
    for line in text.lines() {
        let t = line.trim_start();
        let marker = if t.starts_with("```") {
            Some(b'`')
        } else if t.starts_with("~~~") {
            Some(b'~')
        } else {
            None
        };
        if let Some(c) = marker {
            match fence {
                None => fence = Some(c),
                Some(f) if f == c => fence = None,
                Some(_) => {}
            }
            continue;
        }
        if fence.is_some() {
            continue;
        }
        dust |= is_gate_line(line, DUST_GATE);
        live |= is_gate_line(line, LIVE_GATE);
    }
    (dust, live)
}
