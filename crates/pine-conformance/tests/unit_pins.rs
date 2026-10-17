// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-06 T12 — the pin table (§4.9, I15) and `manifest.tsv` (§4.4.6, I10) against the disk.

use std::collections::BTreeSet;
use std::path::PathBuf;

use pine_conformance::fixture::{Fixture, Group};
use pine_conformance::manifest::{Manifest, ManifestRow, MANIFEST_TSV};
use pine_conformance::pins::{self, Decision, FOREIGN_SWITCH_PINS, PINS};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every `.fix` the generator owns, parsed from disk.
fn disk() -> Vec<Fixture> {
    let root = workspace_root();
    pine_conformance::gen::generate()
        .iter()
        .map(|g| {
            let t = std::fs::read_to_string(root.join(&g.fix_path)).unwrap();
            Fixture::parse(&t, &g.fix_path).unwrap()
        })
        .collect()
}

#[test]
fn every_pin_has_fixtures() {
    // §4.9.8: 26 + 9 + 21 + 38 = 94 names (the 9th `pin:06-*` is R-P2's
    // `pin:06-na-object-field`), each with ≥ 1 fixture; r29 (2026-10-17) adds the capture
    // session's ten — `pin:U17-hist-close-isnew`, five `pin:05-hist-tick-*`, four `pin:04-*` — 104.
    assert_eq!(PINS.len(), 104);
    let names: BTreeSet<&str> = PINS.iter().map(|p| p.name).collect();
    assert_eq!(names.len(), 104, "unique");
    let fx = disk();
    for p in PINS {
        assert!(p.ids().count() >= 1, "{} has no fixture", p.name);
        let ids: Vec<&str> = p.ids().collect();
        let uniq: BTreeSet<&str> = ids.iter().copied().collect();
        assert_eq!(uniq.len(), ids.len(), "{}: duplicate id", p.name);
        for (list, want) in [(p.v5, "5"), (p.v6, "6"), (p.both, "56")] {
            for id in list {
                let f = fx
                    .iter()
                    .find(|f| f.id.as_str() == *id)
                    .unwrap_or_else(|| panic!("{}: {id} not on disk", p.name));
                assert_eq!(f.dialects.text(), want, "{}: {id}'s dialects", p.name);
            }
        }
        for r in p.readings {
            assert!(
                !r.is_empty() && !r.contains(' '),
                "{}: reading `{r}`",
                p.name
            );
        }
    }
    // Before capture every decision is pending: the generator's pre-capture view, not the disk (a
    // working tree after a capture round holds captured fixtures).
    let pre: Vec<Fixture> = pine_conformance::gen::generate()
        .into_iter()
        .map(|g| g.fix)
        .collect();
    for p in PINS {
        assert_eq!(pins::decide(p, &pre), Decision::Pending, "{}", p.name);
    }
    let tsv = pins::tsv(&pre);
    assert_eq!(tsv.lines().count(), 104);
    assert!(tsv
        .lines()
        .all(|l| l.split('\t').count() == 6 && l.ends_with("\tpending\tpending\tpending")));
}

#[test]
fn pins_table_matches_fix_headers() {
    let fx = disk();
    let table: BTreeSet<(&str, &str)> = PINS
        .iter()
        .flat_map(|p| p.ids().map(move |id| (p.name, id)))
        .collect();
    let mut seen = BTreeSet::new();
    for f in &fx {
        for p in &f.pins {
            assert!(
                table.contains(&(p.as_str(), f.id.as_str())),
                "{} names {p}, which §4.9 does not assign to it",
                f.id.as_str()
            );
            seen.insert((p.clone(), f.id.as_str().to_string()));
        }
        // Reading names are the switch owner's: each is a reading of one of the fixture's pins.
        let allowed: BTreeSet<&str> = PINS
            .iter()
            .filter(|p| f.pins.iter().any(|x| x == p.name))
            .flat_map(|p| p.readings.iter().copied())
            .collect();
        for r in &f.readings {
            assert!(
                allowed.contains(r.name.as_str()),
                "{}: reading `{}` is no reading of its pins",
                f.id.as_str(),
                r.name
            );
        }
        // I15: data-derived ids (B-wide, B-chart) carry no pin line.
        if f.id.group() == Group::B && f.id.number() > 76 {
            assert!(f.pins.is_empty(), "{}", f.id.as_str());
        }
    }
    for (p, id) in &table {
        assert!(
            seen.contains(&(p.to_string(), id.to_string())),
            "{id}.fix does not name {p}"
        );
    }
}

#[test]
fn every_switch_pin_is_published() {
    let names: BTreeSet<&str> = PINS.iter().map(|p| p.name).collect();
    for s in pine_spec::SWITCHES.iter() {
        let p = s.pin.as_str();
        if !p.is_empty() {
            assert!(names.contains(p), "pine_spec switch pin {p} unpublished");
        }
    }
    // §4.9.6–§4.9.7's `pin:NN-*` (NN ≠ 06) names: 21 + 38 = 59, owner `impl-NN`; r29 adds nine
    // (five `pin:05-hist-tick-*`, four `pin:04-*`) ahead of the linkage pass: 68.
    assert_eq!(FOREIGN_SWITCH_PINS.len(), 68);
    for (p, owner) in FOREIGN_SWITCH_PINS {
        assert!(names.contains(p), "{p} unpublished");
        let nn = &p["pin:".len().."pin:".len() + 2];
        assert_ne!(nn, "06");
        assert_eq!(*owner, format!("impl-{nn}"), "{p}");
    }
    // Every `pin:NN-*` row of PINS other than 06 is foreign.
    for p in PINS {
        let tail = &p.name["pin:".len()..];
        if tail.as_bytes()[0].is_ascii_digit() && !tail.starts_with("06-") {
            assert!(
                FOREIGN_SWITCH_PINS.iter().any(|(n, _)| *n == p.name),
                "{} missing from FOREIGN_SWITCH_PINS",
                p.name
            );
        }
    }
}

#[test]
fn manifest_counts_match_disk() {
    let root = workspace_root();
    let text = std::fs::read_to_string(root.join(MANIFEST_TSV)).unwrap();
    let m = Manifest::parse(&text).unwrap();
    assert_eq!(m.render(), text, "render ∘ parse");
    let c = m.counts();
    // .fix files per group directory on disk.
    for g in Group::ALL {
        let dir = root.join(format!(
            "{}/fixtures/{}",
            pine_conformance::FIXTURE_ROOT,
            g.letter()
        ));
        let n = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "fix")
                })
                .count()
            })
            .unwrap_or(0);
        assert_eq!(c[g as usize] as usize, n, "group {}", g.letter());
    }
    // I10: the fixed blocks; B = 76 fixed + the derived counts.
    let (w, ch) = pine_conformance::gen::group_b::derived_counts();
    // r29 (2026-10-17): E122–E132 and F099–F119 appended for the next capture session.
    let want = [96, 76 + w + ch, 46, 62, 132, 119, 19];
    for (g, n) in Group::ALL.iter().zip(want) {
        assert_eq!(c[*g as usize] as usize, n, "group {}", g.letter());
    }
    assert!(w > 0 && ch > 0);
    // Every row is its .fix.
    let fx = disk();
    assert_eq!(m.rows.len(), fx.len());
    for (r, f) in m.rows.iter().zip(&fx) {
        assert_eq!(*r, ManifestRow::of(f));
    }
    // The license gate's positive guard (§4.10): ≥ 300 fixture scripts under the fixture tree.
    let mut pine = 0;
    for g in Group::ALL {
        let dir = root.join(format!(
            "{}/fixtures/{}",
            pine_conformance::FIXTURE_ROOT,
            g.letter()
        ));
        if let Ok(rd) = std::fs::read_dir(&dir) {
            pine += rd
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|x| x == "pine")
                })
                .count();
        }
    }
    assert!(pine >= 300, "{pine} fixture scripts");
}
