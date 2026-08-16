//! §11 proptests (1) + (2) for the §4.2 ruleset validator (they land
//! WITH the strategy-vm crate per §12 item 5, alongside the
//! `ruleset_json` fuzz target).
//!
//! (1) roundtrip: a generator builds arbitrary VALID rulesets, a
//!     test-only serializer renders §4.1 JSON, the validator must
//!     accept and every table field must round-trip.
//! (2) robustness: arbitrary mutations/truncations of valid bytes
//!     never panic and never leave a partially staged table
//!     (discard-on-reject: `len == 0` on ANY failure).

use core_types::{fnv1a_64, RuleRow, RuleTable, SYMBOL_ID_NONE};
use ingress_ai::validate_ruleset;
use proptest::prelude::*;

/// Sorted boot-universe fixture: three action syms + the reference.
const UNIVERSE: [u32; 4] = [3, 6, 9, 1_000];
const ACTION_SYMS: [u32; 3] = [3, 6, 9];
const REF_SYM: u32 = 1_000;

const FAMILIES: [&str; 5] = ["crypto", "politics", "sports", "macro", "other"];

fn hash128_of(bytes: &[u8]) -> [u8; 16] {
    let digest = core_crypto::sha256(bytes);
    let mut h = [0u8; 16];
    h.copy_from_slice(&digest[..16]);
    h
}

/// One generated §4.1 row (validator-legal by construction, except
/// rule-8 duplicates which the caller dedups).
#[derive(Clone, Debug)]
struct GenRow {
    sym_idx: usize,
    cross: bool,
    side: u8, // 0 bid / 1 ask / 2 both
    family_idx: usize,
    edge_bps: u32,
    horizon_ms: u32,
    level_1e6: i64,
    risk_1e6: i64,
}

impl GenRow {
    /// Rule-8 identity `(sym, trigger, side, ref/level)` — `ref` is
    /// the constant [`REF_SYM`] for every cross row here.
    fn identity(&self) -> (u32, bool, u8, i64) {
        (
            ACTION_SYMS[self.sym_idx],
            self.cross,
            self.side,
            if self.cross { 0 } else { self.level_1e6 },
        )
    }
}

fn gen_row() -> impl Strategy<Value = GenRow> {
    (
        0usize..ACTION_SYMS.len(),
        any::<bool>(),
        0u8..=2,
        0usize..FAMILIES.len(),
        0u32..=10_000,
        10u32..=86_400_000,
        0i64..=1_000_000,
        // ≤ $3 per row keeps 1..=6 rows inside every rule-7 cap.
        1i64..=3_000_000,
    )
        .prop_map(
            |(sym_idx, cross, side, family_idx, edge_bps, horizon_ms, level_1e6, risk_1e6)| {
                GenRow {
                    sym_idx,
                    cross,
                    side,
                    family_idx,
                    edge_bps,
                    horizon_ms,
                    level_1e6,
                    risk_1e6,
                }
            },
        )
}

fn money(v: i64) -> String {
    format!("{}.{:06}", v / 1_000_000, v % 1_000_000)
}

fn side_str(side: u8) -> &'static str {
    match side {
        0 => "bid",
        1 => "ask",
        _ => "both",
    }
}

/// Test-only §4.1 serializer (fixed key order — the validator accepts
/// any order; this pins one).
fn serialize(rows: &[GenRow]) -> Vec<u8> {
    let mut json = String::from(r#"{"rows":["#);
    for (i, r) in rows.iter().enumerate() {
        if i > 0 {
            json.push(',');
        }
        let trigger = if r.cross {
            format!(r#"{{"type":"cross_deviation","ref":{REF_SYM}}}"#)
        } else {
            format!(r#"{{"type":"level_breach","level":{}}}"#, money(r.level_1e6))
        };
        json.push_str(&format!(
            r#"{{"name":"r{i:03}","family":"{}","trigger":{trigger},"sym":{},"side":"{}","edge_bps":{},"horizon_ms":{},"max_risk_usd":{}}}"#,
            FAMILIES[r.family_idx],
            ACTION_SYMS[r.sym_idx],
            side_str(r.side),
            r.edge_bps,
            r.horizon_ms,
            money(r.risk_1e6),
        ));
    }
    json.push_str("]}");
    json.into_bytes()
}

/// Drop rule-8 identity duplicates, keeping first occurrences (the
/// generator's only validator-illegal degree of freedom).
fn dedup(rows: Vec<GenRow>) -> Vec<GenRow> {
    let mut kept: Vec<GenRow> = Vec::new();
    for r in rows.into_iter() {
        if !kept.iter().any(|k| k.identity() == r.identity()) {
            kept.push(r);
        }
    }
    kept
}

proptest! {
    // ---- §11 proptest (1): valid rulesets roundtrip ----
    #[test]
    fn valid_rulesets_are_accepted_and_roundtrip(
        raw_rows in proptest::collection::vec(gen_row(), 1..=6),
    ) {
        let rows = dedup(raw_rows);
        let bytes = serialize(&rows);
        let hash = hash128_of(&bytes);
        let mut scratch = Box::new(RuleTable::EMPTY);
        scratch.epoch = 7; // must survive untouched (side-path state)

        let res = validate_ruleset(&bytes, &hash, &UNIVERSE, &mut scratch);
        prop_assert!(res.is_ok(), "generated ruleset rejected: {:?}", res);
        prop_assert_eq!(scratch.len as usize, rows.len());
        prop_assert_eq!(scratch.hash128, hash);
        prop_assert_eq!(scratch.epoch, 7, "validator must not touch epoch");

        for (i, r) in rows.iter().enumerate() {
            let row: &RuleRow = &scratch.rows[i];
            prop_assert_eq!(row.sym, ACTION_SYMS[r.sym_idx]);
            prop_assert_eq!(
                row.ref_sym,
                if r.cross { REF_SYM } else { SYMBOL_ID_NONE }
            );
            prop_assert_eq!(row.edge_bps, r.edge_bps);
            prop_assert_eq!(row.horizon_ms, r.horizon_ms);
            prop_assert_eq!(row.level_1e6, if r.cross { 0 } else { r.level_1e6 });
            prop_assert_eq!(row.max_risk_1e6, r.risk_1e6);
            prop_assert_eq!(
                row.trigger,
                if r.cross {
                    RuleRow::TRIGGER_CROSS_DEVIATION
                } else {
                    RuleRow::TRIGGER_LEVEL_BREACH
                }
            );
            prop_assert_eq!(
                row.side,
                match r.side {
                    0 => 0u8,
                    1 => 1u8,
                    _ => RuleRow::SIDE_BOTH,
                }
            );
            prop_assert_eq!(row.family, r.family_idx as u8);
            let name = format!("r{i:03}");
            prop_assert_eq!(row.name_h, fnv1a_64(name.as_bytes()));
        }
    }

    // ---- §11 proptest (2): mutations never panic / partially stage ----
    #[test]
    fn mutated_rulesets_never_panic_and_never_partially_stage(
        raw_rows in proptest::collection::vec(gen_row(), 1..=6),
        ops in proptest::collection::vec(
            (0u8..=3, any::<usize>(), any::<u8>()),
            1..=8
        ),
    ) {
        let rows = dedup(raw_rows);
        let valid = serialize(&rows);
        let valid_hash = hash128_of(&valid);

        let mut bytes = valid.clone();
        for (kind, pos, byte) in ops.iter() {
            match kind {
                // Flip: XOR a byte with a nonzero mask (always changes).
                0 => {
                    let i = pos % bytes.len();
                    bytes[i] ^= byte | 1;
                }
                // Insert an arbitrary byte.
                1 => {
                    let i = pos % (bytes.len() + 1);
                    bytes.insert(i, *byte);
                }
                // Delete a byte (keep at least one).
                2 => {
                    if bytes.len() > 1 {
                        let i = pos % bytes.len();
                        bytes.remove(i);
                    }
                }
                // Truncate (keep at least one byte).
                _ => {
                    let i = 1 + pos % bytes.len();
                    bytes.truncate(i);
                }
            }
        }

        // Path A: honest hash of the mutated bytes — drives rules 2–8.
        let mut scratch = Box::new(RuleTable::EMPTY);
        let h = hash128_of(&bytes);
        match validate_ruleset(&bytes, &h, &UNIVERSE, &mut scratch) {
            Ok(()) => {
                prop_assert!(scratch.len >= 1 && scratch.len <= 256);
                prop_assert_eq!(scratch.hash128, h);
            }
            Err(_) => {
                prop_assert_eq!(scratch.len, 0, "discard-on-reject (§11)");
            }
        }

        // Path B: the pre-mutation hash — rule 1 unless the mutation
        // chain happened to be byte-identical.
        let mut scratch_b = Box::new(RuleTable::EMPTY);
        match validate_ruleset(&bytes, &valid_hash, &UNIVERSE, &mut scratch_b) {
            Ok(()) => prop_assert_eq!(&bytes, &valid, "stale hash may only pass on identical bytes"),
            Err(_) => prop_assert_eq!(scratch_b.len, 0, "discard-on-reject (§11)"),
        }
    }
}
