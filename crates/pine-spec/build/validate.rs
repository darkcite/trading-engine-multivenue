// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: build script — runs once in cargo's build step and is linked into no binary;
//! every copy here is cold.
//!
//! Rules R1–R20 over the lowered model (impl-00 §4.6). Rules whose input only exists in the
//! JSON (R2, R6, R8, R9, R13, R20 and the resolution halves of R3, R7, R17) are refused during
//! lowering (`build/schema.rs`) with the same `R<nn>` prefix; this pass checks the rest.

use crate::schema::{fail, DefVal, Model, SpecErr, BF_LOWERING, SX_NAMES};

const PF_OPTIONAL: u8 = 1 << 0;
const PF_VARIADIC: u8 = 1 << 1;
const PF_NAMED_ONLY: u8 = 1 << 2;
const PF_SYNTHETIC: u8 = 1 << 5;

fn bf(name: &str) -> u8 {
    BF_LOWERING
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
        .unwrap_or(0)
}

fn sx(name: &str) -> u8 {
    SX_NAMES
        .iter()
        .position(|n| *n == name)
        .map(|i| 1u8 << i)
        .unwrap_or(0)
}

/// Check every rule; the first violation is returned.
pub fn check(m: &Model) -> Result<(), SpecErr> {
    ascii(m)?;
    entries(m)?;
    overloads(m)?;
    values(m)?;
    switches(m)?;
    deltas(m)
}

/// R10: every emitted string is ASCII.
fn ascii(m: &Model) -> Result<(), SpecErr> {
    let bad = |s: &str| !s.is_ascii();
    for e in &m.entries {
        for s in [&e.full, &e.ns, &e.name, &e.decline, &e.doc] {
            if bad(s) {
                return fail(e.off, 10, format!("non-ASCII byte in \"{s}\""));
            }
        }
    }
    for o in &m.overloads {
        if bad(&o.sym) {
            return fail(o.off, 10, "non-ASCII byte in an overload id");
        }
    }
    for p in &m.params {
        if bad(&p.name) || matches!(&p.default, DefVal::Str(s) if bad(s)) {
            return fail(p.off, 10, "non-ASCII byte in a parameter");
        }
    }
    for v in &m.values {
        if matches!(&v.bits, crate::schema::VBits::Str(s) if bad(s)) {
            return fail(v.off, 10, "non-ASCII byte in a string value");
        }
    }
    for s in &m.switches {
        let strs = [
            &s.id, &s.row, &s.owner, &s.v5, &s.v6, &s.source, &s.pin, &s.doc,
        ];
        if strs.iter().any(|x| bad(x)) || s.readings.iter().any(|x| bad(x)) {
            return fail(s.off, 10, format!("non-ASCII byte in switch \"{}\"", s.id));
        }
    }
    for d in &m.deltas {
        let strs = [&d.id, &d.what, &d.switch, &d.source, &d.pin];
        if strs.iter().any(|x| bad(x)) || d.subjects.iter().any(|x| bad(&x.1)) {
            return fail(d.off, 10, format!("non-ASCII byte in delta \"{}\"", d.id));
        }
    }
    Ok(())
}

/// R1, R11, R19.
fn entries(m: &Model) -> Result<(), SpecErr> {
    let mut i = 1usize;
    while i < m.entries.len() {
        if m.entries[i - 1].full.as_bytes() >= m.entries[i].full.as_bytes() {
            return fail(
                m.entries[i].off,
                1,
                format!("duplicate entry \"{}\"", m.entries[i].full),
            );
        }
        i += 1;
    }
    for e in &m.entries {
        if e.declined && e.decline.is_empty() {
            return fail(
                e.off,
                11,
                format!("\"{}\" is declined without a reason", e.full),
            );
        }
        if !e.declined && !e.decline.is_empty() {
            return fail(
                e.off,
                11,
                format!("\"{}\" is in scope but has a decline", e.full),
            );
        }
        if !(1..=4).contains(&e.tier) {
            return fail(e.off, 19, format!("tier {} outside 1..=4", e.tier));
        }
        let rowless = e.ov_lo == e.ov_hi && e.val_lo == e.val_hi;
        if e.tier < 4 && rowless && !e.is_type {
            return fail(
                e.off,
                19,
                format!("T{} entry \"{}\" has no row", e.tier, e.full),
            );
        }
    }
    Ok(())
}

/// R4, R5, R7, R12, R18.
fn overloads(m: &Model) -> Result<(), SpecErr> {
    for o in &m.overloads {
        if o.versions == 0 {
            return fail(o.off, 4, format!("{}: empty versions", o.sym));
        }
        let ps = &m.params[o.p_lo as usize..o.p_hi as usize];
        for p in ps {
            if p.versions == 0 || p.versions & !o.versions != 0 {
                return fail(
                    p.off,
                    4,
                    format!("{}.{}: versions not within the overload's", o.sym, p.name),
                );
            }
        }
        // R5, per dialect.
        for d in [1u8, 2u8] {
            if o.versions & d == 0 {
                continue;
            }
            let vis: Vec<_> = ps.iter().filter(|p| p.versions & d != 0).collect();
            let mut seen_plain = false;
            for (k, p) in vis.iter().enumerate() {
                if vis[..k].iter().any(|q| q.name == p.name) {
                    return fail(
                        p.off,
                        5,
                        format!("{}: parameter \"{}\" twice", o.sym, p.name),
                    );
                }
                if p.flags & PF_VARIADIC != 0 && k + 1 != vis.len() {
                    return fail(
                        p.off,
                        5,
                        format!("{}: variadic \"{}\" is not last", o.sym, p.name),
                    );
                }
                let has_default = p.default != DefVal::None;
                if (p.flags & PF_OPTIONAL != 0) != has_default {
                    return fail(
                        p.off,
                        5,
                        format!("{}: \"{}\" optional <=> default", o.sym, p.name),
                    );
                }
                if p.flags & PF_VARIADIC != 0 && p.flags & PF_NAMED_ONLY != 0 {
                    return fail(
                        p.off,
                        5,
                        format!("{}: \"{}\" variadic and named_only", o.sym, p.name),
                    );
                }
                let synth = p.flags & PF_SYNTHETIC != 0;
                let dunder = p.name.starts_with("__");
                if synth != dunder {
                    return fail(
                        p.off,
                        5,
                        format!("{}: \"{}\" synthetic <=> __ name", o.sym, p.name),
                    );
                }
                if synth {
                    let ok = p.flags == PF_SYNTHETIC
                        && p.default == DefVal::None
                        && p.sig.tv == 0
                        && p.sig.ty.ty == pine_ir::Ty::Int
                        && p.sig.ty.qual == pine_ir::Qual::Const;
                    if !ok {
                        return fail(
                            p.off,
                            5,
                            format!(
                                "{}: synthetic \"{}\" is not a required const int",
                                o.sym, p.name
                            ),
                        );
                    }
                    if seen_plain {
                        return fail(
                            p.off,
                            5,
                            format!("{}: synthetic \"{}\" is not leading", o.sym, p.name),
                        );
                    }
                } else {
                    seen_plain = true;
                }
            }
        }
        // R7.
        let stateful = o.bf & bf("stateful") != 0;
        let coll = o.bf & bf("collection") != 0;
        let blk = matches!(o.state.shape, "Fixed" | "Param");
        let cst = matches!(o.state.shape, "Array" | "Map");
        if stateful && coll {
            return fail(o.off, 7, format!("{}: both stateful and collection", o.sym));
        }
        if stateful != blk {
            return fail(
                o.off,
                7,
                format!("{}: stateful <=> fixed/param state violated", o.sym),
            );
        }
        if coll != cst {
            return fail(
                o.off,
                7,
                format!("{}: collection <=> array/map state violated", o.sym),
            );
        }
        // R12.
        let wl = o.sx & sx("work_linear") != 0;
        let ib = o.sx & sx("implicit_bar") != 0;
        if o.abi == "Folded" && (o.bf != 0 || o.state.shape != "None" || wl || ib) {
            return fail(
                o.off,
                12,
                format!(
                    "{}: folded with bf, state, work_linear or implicit_bar",
                    o.sym
                ),
            );
        }
        if o.abi == "Lowered" && o.sx & sx("request") == 0 {
            return fail(o.off, 12, format!("{}: lowered without sx request", o.sym));
        }
        if wl && o.state.shape != "Param" {
            return fail(
                o.off,
                12,
                format!("{}: work_linear without a param state", o.sym),
            );
        }
        // R18.
        if o.plot.is_some() && o.bf & bf("capture") == 0 {
            return fail(o.off, 18, format!("{}: plot kind without capture", o.sym));
        }
        let ns = &m.entries[o.entry as usize].ns;
        if o.bf & bf("intent") != 0 && ns != "strategy" && !ns.starts_with("strategy.") {
            return fail(
                o.off,
                18,
                format!("{}: intent outside namespace strategy", o.sym),
            );
        }
        if o.input.is_some() && o.abi != "Folded" {
            return fail(
                o.off,
                18,
                format!("{}: input kind without abi folded", o.sym),
            );
        }
        if o.bf & bf("needs_ctx") != 0 && !ib {
            return fail(
                o.off,
                18,
                format!("{}: needs_ctx without implicit_bar", o.sym),
            );
        }
    }
    Ok(())
}

/// R4 (values), R17 (`call` targets).
fn values(m: &Model) -> Result<(), SpecErr> {
    for v in &m.values {
        if v.versions == 0 {
            return fail(v.off, 4, "value with empty versions");
        }
        if v.src == "Call" {
            if let crate::schema::VBits::Bits(id) = v.bits {
                let o = &m.overloads[id as usize];
                let ps = &m.params[o.p_lo as usize..o.p_hi as usize];
                if ps.iter().any(|p| p.default == DefVal::None) {
                    return fail(
                        v.off,
                        17,
                        format!("call to {} which has a required parameter", o.sym),
                    );
                }
            }
        }
    }
    Ok(())
}

/// R15.
fn switches(m: &Model) -> Result<(), SpecErr> {
    for (i, s) in m.switches.iter().enumerate() {
        if m.switches[..i].iter().any(|q| q.id == s.id) {
            return fail(s.off, 15, format!("switch \"{}\" twice", s.id));
        }
        if s.mech == "Switch" {
            if s.readings.len() < 2 {
                return fail(
                    s.off,
                    15,
                    format!("switch \"{}\" has fewer than 2 readings", s.id),
                );
            }
            for d in [&s.v5, &s.v6] {
                if !s.readings.iter().any(|r| r == d) {
                    return fail(
                        s.off,
                        15,
                        format!("switch \"{}\": default \"{d}\" is not a reading", s.id),
                    );
                }
            }
            if s.readings.len() > 255 {
                return fail(
                    s.off,
                    15,
                    format!("switch \"{}\" has more than 255 readings", s.id),
                );
            }
        } else if !s.readings.is_empty() || !s.v5.is_empty() || !s.v6.is_empty() {
            return fail(
                s.off,
                15,
                format!("non-switch row \"{}\" has readings", s.id),
            );
        }
        if !s.pin.starts_with("pin:") {
            return fail(
                s.off,
                15,
                format!("switch \"{}\": pin does not start pin:", s.id),
            );
        }
        if s.normative && s.source.is_empty() {
            return fail(
                s.off,
                15,
                format!("switch \"{}\" is normative without a source", s.id),
            );
        }
    }
    if m.switches.len() > 254 {
        return fail(0, 15, "more than 254 switch rows");
    }
    Ok(())
}

/// R16.
fn deltas(m: &Model) -> Result<(), SpecErr> {
    for (i, d) in m.deltas.iter().enumerate() {
        if m.deltas[..i].iter().any(|q| q.id == d.id) {
            return fail(d.off, 16, format!("delta \"{}\" twice", d.id));
        }
        if (d.mech == "Switch" || !d.switch.is_empty())
            && !m.switches.iter().any(|s| s.id == d.switch)
        {
            return fail(
                d.switch_off,
                16,
                format!("delta \"{}\" names no switch row", d.id),
            );
        }
        for (off, s) in &d.subjects {
            let hit = m
                .entries
                .binary_search_by(|e| e.full.as_bytes().cmp(s.as_bytes()));
            if hit.is_err() {
                return fail(
                    *off,
                    16,
                    format!("delta \"{}\": subject \"{s}\" names no entry", d.id),
                );
            }
        }
    }
    Ok(())
}
