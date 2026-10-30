// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! The licence gate (7105): the class table of AMEND-harvest §2.6 under OH-6 (R-P33, 2026-10-29),
//! the §4.7 step 6 patterns written by hand (the crate's dependency set has no regex engine), and
//! the agreement check `import` runs between the operator's `licence_line` / `licence_class` and
//! the source's own comment lines.
//!
//! - **Admitted** (OH-6): `mpl2`, `permissive`, `copyleft`.
//! - **Refused**: `nc`, `nd`, `arr`, `custom` (until the operator classifies the row), `unknown`.
//! - OH-6(c) "do not republish" / "no redistribution" alone is admitted (we never publish): such a
//!   line implies `mpl2`, the default terms that remain.
//! - The operator may always choose a *refused* class over an implied admitted one: a refusal is
//!   never a false admission.

/// Every class of the table.
pub const CLASSES: [&str; 8] = [
    "mpl2",
    "permissive",
    "copyleft",
    "nc",
    "nd",
    "arr",
    "custom",
    "unknown",
];

/// The admitted classes (OH-6).
pub const ADMITTED: [&str; 3] = ["mpl2", "permissive", "copyleft"];

/// Whether `class` is admitted.
#[must_use]
pub fn admitted(class: &str) -> bool {
    ADMITTED.contains(&class)
}

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `needle` occurs in `hay` (both lowercase), with a word boundary before it when `wb`.
fn find(hay: &str, needle: &str, wb: bool) -> Option<usize> {
    let h = hay.as_bytes();
    let mut from = 0;
    while let Some(p) = hay[from..].find(needle) {
        let at = from + p;
        if !wb || at == 0 || !is_word(h[at - 1]) {
            return Some(at);
        }
        from = at + 1;
    }
    None
}

/// §4.7 step 6 `LICENCE`:
/// `licen[cs]e|all rights reserved|creative commons|\bcc[- ]by|\bgpl|non-?commercial|proprietary`,
/// case-insensitive.
#[must_use]
pub fn licence_match(line: &str) -> bool {
    let l = lower(line);
    find(&l, "licence", false).is_some()
        || find(&l, "license", false).is_some()
        || find(&l, "all rights reserved", false).is_some()
        || find(&l, "creative commons", false).is_some()
        || find(&l, "cc-by", true).is_some()
        || find(&l, "cc by", true).is_some()
        || find(&l, "gpl", true).is_some()
        || find(&l, "noncommercial", false).is_some()
        || find(&l, "non-commercial", false).is_some()
        || find(&l, "proprietary", false).is_some()
}

/// §4.7 step 6 `MPL`: `mozilla public license|\bmpl[- ]?2(\.0)?\b`, case-insensitive.
#[must_use]
pub fn mpl_match(line: &str) -> bool {
    let l = lower(line);
    if l.contains("mozilla public license") {
        return true;
    }
    let b = l.as_bytes();
    let mut from = 0;
    while let Some(at) = find(&l[from..], "mpl", true).map(|p| p + from) {
        let mut i = at + 3;
        if i < b.len() && (b[i] == b'-' || b[i] == b' ') {
            i += 1;
        }
        if i < b.len() && b[i] == b'2' {
            i += 1;
            if i + 1 < b.len() && b[i] == b'.' && b[i + 1] == b'0' {
                let j = i + 2;
                if j >= b.len() || !is_word(b[j]) {
                    return true;
                }
            }
            if i >= b.len() || !is_word(b[i]) {
                return true;
            }
        }
        from = at + 1;
    }
    false
}

/// The first `//` comment line of `src` that matches `LICENCE` and not `MPL`, as written
/// (the `//` and surrounding whitespace stripped).
#[must_use]
pub fn source_licence_line(src: &str) -> Option<String> {
    for line in src.lines() {
        let t = line.trim_start();
        if let Some(c) = t.strip_prefix("//") {
            if licence_match(c) && !mpl_match(c) {
                return Some(c.trim().to_string());
            }
        }
    }
    None
}

/// Strip a leading `//` and surrounding whitespace.
#[must_use]
pub fn norm_line(s: &str) -> String {
    let t = s.trim();
    t.strip_prefix("//").unwrap_or(t).trim().to_string()
}

/// The class a licence line implies, most restrictive first; `None` when no keyword decides.
#[must_use]
pub fn implied_class(line: &str) -> Option<&'static str> {
    let l = lower(line);
    let has = |n: &str| l.contains(n);
    let w = |n: &str| find(&l, n, true).is_some();
    if has("non-commercial") || has("noncommercial") || has("by-nc") || has("by nc") {
        return Some("nc");
    }
    if has("no derivative")
        || has("noderivative")
        || has("by-nd")
        || has("by nd")
        || has("no modification")
    {
        return Some("nd");
    }
    if has("all rights reserved") || has("proprietary") {
        return Some("arr");
    }
    if w("gpl")
        || w("lgpl")
        || w("agpl")
        || has("by-sa")
        || has("by sa")
        || has("sharealike")
        || has("share-alike")
    {
        return Some("copyleft");
    }
    if w("mit")
        || w("bsd")
        || has("apache")
        || w("isc")
        || w("cc0")
        || has("unlicense")
        || has("cc by 4.0")
        || has("cc-by 4.0")
        || has("cc-by-4.0")
    {
        return Some("permissive");
    }
    if mpl_match(&l) {
        return Some("mpl2");
    }
    if has("republish") || has("redistribut") {
        return Some("mpl2"); // OH-6(c)
    }
    None
}

/// The agreement check `import` runs (AMEND-harvest §2.3 `licence_line` / `licence_class`).
/// `Ok(licence_override)` (`None` for `-`); `Err(reason)` refuses the row.
pub fn check(src: &str, licence_line: &str, class: &str) -> Result<Option<String>, String> {
    if !CLASSES.contains(&class) {
        return Err(format!("licence_class `{class}` is not in the class table"));
    }
    let in_src = source_licence_line(src);
    if licence_line == "-" {
        if let Some(l) = in_src {
            return Err(format!(
                "licence_line is `-` but the source states a licence: `{l}`"
            ));
        }
        if class != "mpl2" && admitted(class) {
            return Err(format!(
                "licence_line is `-` (MPL-2.0 by default) but licence_class is `{class}`"
            ));
        }
        return Ok(None);
    }
    let line = norm_line(licence_line);
    if line.is_empty() {
        return Err("licence_line is empty (write `-` for none)".to_string());
    }
    if let Some(l) = &in_src {
        if norm_line(l) != line {
            return Err(format!(
                "licence_line differs from the source's own licence line `{l}`"
            ));
        }
    }
    match implied_class(&line) {
        Some(imp) if imp == class => {}
        Some(_) if !admitted(class) => {}
        Some(imp) => {
            return Err(format!(
                "licence_line implies `{imp}`, licence_class says `{class}`"
            ))
        }
        None if !admitted(class) => {}
        None => {
            return Err(format!(
                "licence_line names no class; `{class}` needs a matching statement (or classify `custom` / `unknown`)"
            ))
        }
    }
    Ok(Some(licence_line.to_string()))
}
