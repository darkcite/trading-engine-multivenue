// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Intent validation (impl-05 §4.4.1, REVIEW §5: "validate every intent field").
//!
//! `submit` / `apply_intents` call [`validate`] over the **whole** slice before committing any of
//! it, so a bad intent leaves no partial commit (I35). Two passes: every structural rule over the
//! whole slice first (a `pine-std` defect, `Internal(MalformedIntent)`), then the script values
//! (`Script(Unsized)`), so a defect of ours is never reported as a script fault (D13 would isolate
//! it instead of failing fast). Zero allocation; index loops.

use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
use pine_ir::{intent_flag as f, IntentKind, IntentSide, OcaType, OrderIntent, StrKey, Sym};

/// 100 % at the ×1e6 percent scale.
const PCT_100: i64 = 100 * 1_000_000;

/// `Entry`, `Order`: admissible flag bits.
const ENTRY_BITS: u16 = f::QTY | f::LIMIT | f::STOP | f::ID_DYNAMIC;
/// `Exit`: admissible flag bits.
const EXIT_BITS: u16 = f::QTY
    | f::QTY_PCT
    | f::LIMIT
    | f::STOP
    | f::PROFIT
    | f::LOSS
    | f::TRAIL_PRICE
    | f::TRAIL_POINTS
    | f::TRAIL_OFFSET
    | f::ID_DYNAMIC;
/// `Close`: admissible flag bits.
const CLOSE_BITS: u16 = f::QTY | f::QTY_PCT | f::IMMEDIATELY | f::ID_DYNAMIC;

/// The per-kind row of the §4.4.1 table.
struct KindRule {
    /// Admissible `intent_flag` bits.
    bits: u16,
    /// `oca_*` allowed.
    oca: bool,
    /// `side == Short` allowed.
    short: bool,
    /// `from_entry ≠ NONE` allowed.
    from_entry: bool,
}

/// The §4.4.1 row of `kind`.
const fn rule(kind: IntentKind) -> KindRule {
    match kind {
        IntentKind::Entry | IntentKind::Order => KindRule {
            bits: ENTRY_BITS,
            oca: true,
            short: true,
            from_entry: false,
        },
        IntentKind::Exit => KindRule {
            bits: EXIT_BITS,
            oca: true,
            short: false,
            from_entry: true,
        },
        IntentKind::Close => KindRule {
            bits: CLOSE_BITS,
            oca: false,
            short: false,
            from_entry: true,
        },
        IntentKind::CloseAll => KindRule {
            bits: f::IMMEDIATELY,
            oca: false,
            short: false,
            from_entry: false,
        },
        IntentKind::Cancel => KindRule {
            bits: f::ID_DYNAMIC,
            oca: false,
            short: false,
            from_entry: false,
        },
        IntentKind::CancelAll => KindRule {
            bits: 0,
            oca: false,
            short: false,
            from_entry: false,
        },
    }
}

/// The kind's id-field rule (§4.4.1 table, "id fields").
const fn ids_ok(it: &OrderIntent) -> bool {
    let none = StrKey::NONE.0;
    match it.kind {
        IntentKind::Entry | IntentKind::Order | IntentKind::Exit | IntentKind::Cancel => {
            it.id.0 != none
        }
        IntentKind::Close => it.from_entry.0 != none,
        IntentKind::CloseAll | IntentKind::CancelAll => it.id.0 == none,
    }
}

/// A field whose flag bit is clear is non-zero.
const fn field_without_bit(it: &OrderIntent) -> bool {
    let fl = it.flags;
    (fl & f::QTY == 0 && it.qty_1e6 != 0)
        || (fl & f::QTY_PCT == 0 && it.qty_pct_1e6 != 0)
        || (fl & f::LIMIT == 0 && it.limit_1e6 != 0)
        || (fl & f::STOP == 0 && it.stop_1e6 != 0)
        || (fl & f::PROFIT == 0 && it.profit_ticks_1e6 != 0)
        || (fl & f::LOSS == 0 && it.loss_ticks_1e6 != 0)
        || (fl & f::TRAIL_PRICE == 0 && it.trail_price_1e6 != 0)
        || (fl & f::TRAIL_POINTS == 0 && it.trail_points_1e6 != 0)
        || (fl & f::TRAIL_OFFSET == 0 && it.trail_offset_1e6 != 0)
}

/// Any padding byte non-zero.
fn pad_set(it: &OrderIntent) -> bool {
    let mut set = false;
    let mut i = 0;
    while i < it._pad0.len() {
        set |= it._pad0[i] != 0;
        i += 1;
    }
    let mut i = 0;
    while i < it._pad1.len() {
        set |= it._pad1[i] != 0;
        i += 1;
    }
    set
}

/// Every structural rule of §4.4.1 for one intent (the slice-wide `seq` rule is the caller's).
fn structurally_sound(it: &OrderIntent) -> bool {
    let r = rule(it.kind);
    let has_oca = it.oca_type != OcaType::None || it.oca_name != StrKey::NONE;
    !(it.flags & !f::DEFINED != 0
        || pad_set(it)
        || field_without_bit(it)
        || it.flags & !r.bits != 0
        || (it.side == IntentSide::Short && !r.short)
        || !ids_ok(it)
        || (it.flags & f::ID_DYNAMIC != 0 && it.id_sym != Sym::NONE)
        || ((it.oca_type == OcaType::None) != (it.oca_name == StrKey::NONE))
        || (has_oca && !r.oca)
        || (it.from_entry != StrKey::NONE && !r.from_entry))
}

/// The script asked for something unsizable (`pin:05-bad-qty`).
const fn unsizable(it: &OrderIntent) -> bool {
    (it.flags & f::QTY != 0 && it.qty_1e6 <= 0)
        || (it.flags & f::QTY_PCT != 0 && (it.qty_pct_1e6 <= 0 || it.qty_pct_1e6 > PCT_100))
}

/// Validate a whole evaluation's intents (§4.4.1). `Ok` only if every intent passes; the caller
/// commits nothing otherwise (I35).
pub(crate) fn validate(intents: &[OrderIntent]) -> Result<(), BrokerFault> {
    let n = intents.len();
    let mut i = 0;
    while i < n {
        let it = &intents[i];
        if !structurally_sound(it) || (i > 0 && it.seq <= intents[i - 1].seq) {
            return Err(BrokerFault::Internal(InternalDefect::MalformedIntent));
        }
        i += 1;
    }
    let mut i = 0;
    while i < n {
        if unsizable(&intents[i]) {
            return Err(BrokerFault::Script(ScriptRefusal::Unsized));
        }
        i += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! §7.3's validation cases (`tests/intents.rs` repeats them through `submit` /
    //! `apply_intents` once the brokers land, T11 / T14, with the commit-side cases).

    use super::validate;
    use crate::fault::{BrokerFault, InternalDefect, ScriptRefusal};
    use pine_ir::{intent_flag as f, IntentKind, IntentSide, OcaType, OrderIntent, StrKey, Sym};

    const MALFORMED: Result<(), BrokerFault> =
        Err(BrokerFault::Internal(InternalDefect::MalformedIntent));
    const UNSIZED: Result<(), BrokerFault> = Err(BrokerFault::Script(ScriptRefusal::Unsized));

    fn entry(seq: u32) -> OrderIntent {
        OrderIntent {
            kind: IntentKind::Entry,
            id: StrKey(7),
            seq,
            ..OrderIntent::default()
        }
    }

    fn one(it: OrderIntent) -> Result<(), BrokerFault> {
        validate(&[it])
    }

    #[test]
    fn well_formed_intents_of_every_kind_pass() {
        let e = OrderIntent {
            flags: f::QTY | f::LIMIT | f::STOP,
            qty_1e6: 1,
            limit_1e6: -5,
            stop_1e6: i64::MAX,
            side: IntentSide::Short,
            oca_type: OcaType::Reduce,
            oca_name: StrKey(9),
            ..entry(1)
        };
        let x = OrderIntent {
            kind: IntentKind::Exit,
            flags: f::QTY_PCT | f::PROFIT | f::LOSS | f::TRAIL_POINTS | f::TRAIL_OFFSET,
            qty_pct_1e6: 100 * 1_000_000,
            profit_ticks_1e6: 10_000_000,
            loss_ticks_1e6: 5_000_000,
            trail_points_1e6: 1,
            trail_offset_1e6: 1,
            from_entry: StrKey(7),
            ..entry(2)
        };
        let c = OrderIntent {
            kind: IntentKind::Close,
            id: StrKey::NONE,
            from_entry: StrKey(7),
            flags: f::IMMEDIATELY,
            ..entry(3)
        };
        let ca = OrderIntent {
            kind: IntentKind::CloseAll,
            id: StrKey::NONE,
            ..entry(4)
        };
        let k = OrderIntent {
            kind: IntentKind::Cancel,
            flags: f::ID_DYNAMIC,
            ..entry(5)
        };
        let ka = OrderIntent {
            kind: IntentKind::CancelAll,
            id: StrKey::NONE,
            ..entry(6)
        };
        let o = OrderIntent {
            kind: IntentKind::Order,
            id_sym: Sym(3),
            ..entry(7)
        };
        assert_eq!(validate(&[e, x, c, ca, k, ka, o]), Ok(()));
        assert_eq!(validate(&[]), Ok(()));
    }

    #[test]
    fn undefined_flag_bit_is_malformed() {
        assert_eq!(
            one(OrderIntent {
                flags: 1 << 11,
                ..entry(1)
            }),
            MALFORMED
        );
        assert_eq!(
            one(OrderIntent {
                flags: 1 << 15,
                ..entry(1)
            }),
            MALFORMED
        );
    }

    #[test]
    fn nonzero_padding_is_malformed() {
        assert_eq!(
            one(OrderIntent {
                _pad0: [0, 0, 1],
                ..entry(1)
            }),
            MALFORMED
        );
        let mut p = [0u8; 12];
        p[11] = 1;
        assert_eq!(
            one(OrderIntent {
                _pad1: p,
                ..entry(1)
            }),
            MALFORMED
        );
    }

    #[test]
    fn field_without_its_bit_is_malformed() {
        let cases: [fn(&mut OrderIntent); 9] = [
            |i| i.qty_1e6 = 1,
            |i| i.qty_pct_1e6 = 1,
            |i| i.limit_1e6 = 1,
            |i| i.stop_1e6 = 1,
            |i| i.profit_ticks_1e6 = 1,
            |i| i.loss_ticks_1e6 = 1,
            |i| i.trail_price_1e6 = 1,
            |i| i.trail_points_1e6 = 1,
            |i| i.trail_offset_1e6 = 1,
        ];
        let mut k = 0;
        while k < cases.len() {
            let mut it = OrderIntent {
                kind: IntentKind::Exit,
                ..entry(1)
            };
            cases[k](&mut it);
            assert_eq!(one(it), MALFORMED, "case {k}");
            k += 1;
        }
    }

    #[test]
    fn inadmissible_flag_for_the_kind_is_malformed() {
        let e = OrderIntent {
            flags: f::PROFIT,
            profit_ticks_1e6: 1,
            ..entry(1)
        };
        assert_eq!(one(e), MALFORMED);
        let e = OrderIntent {
            flags: f::IMMEDIATELY,
            ..entry(1)
        };
        assert_eq!(one(e), MALFORMED);
        let c = OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(7),
            flags: f::LIMIT,
            limit_1e6: 1,
            ..entry(1)
        };
        assert_eq!(one(c), MALFORMED);
        let ca = OrderIntent {
            kind: IntentKind::CloseAll,
            id: StrKey::NONE,
            flags: f::ID_DYNAMIC,
            ..entry(1)
        };
        assert_eq!(one(ca), MALFORMED);
        let ka = OrderIntent {
            kind: IntentKind::CancelAll,
            id: StrKey::NONE,
            flags: f::ID_DYNAMIC,
            ..entry(1)
        };
        assert_eq!(one(ka), MALFORMED);
        // Id-field rules.
        assert_eq!(
            one(OrderIntent {
                id: StrKey::NONE,
                ..entry(1)
            }),
            MALFORMED
        );
        assert_eq!(
            one(OrderIntent {
                kind: IntentKind::CloseAll,
                ..entry(1)
            }),
            MALFORMED
        );
        assert_eq!(
            one(OrderIntent {
                kind: IntentKind::Close,
                ..entry(1)
            }),
            MALFORMED,
            "a close without from_entry"
        );
    }

    #[test]
    fn short_side_on_a_close_is_malformed() {
        let c = OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(7),
            side: IntentSide::Short,
            ..entry(1)
        };
        assert_eq!(one(c), MALFORMED);
        let x = OrderIntent {
            kind: IntentKind::Exit,
            side: IntentSide::Short,
            ..entry(1)
        };
        assert_eq!(one(x), MALFORMED);
    }

    #[test]
    fn dynamic_id_with_a_sym_is_malformed() {
        assert_eq!(
            one(OrderIntent {
                flags: f::ID_DYNAMIC,
                id_sym: Sym(4),
                ..entry(1)
            }),
            MALFORMED
        );
        assert_eq!(
            one(OrderIntent {
                flags: f::ID_DYNAMIC,
                ..entry(1)
            }),
            Ok(())
        );
    }

    #[test]
    fn oca_on_a_close_is_malformed() {
        let c = OrderIntent {
            kind: IntentKind::Close,
            from_entry: StrKey(7),
            oca_type: OcaType::Cancel,
            oca_name: StrKey(9),
            ..entry(1)
        };
        assert_eq!(one(c), MALFORMED);
        // Half an OCA pair is malformed on any kind.
        assert_eq!(
            one(OrderIntent {
                oca_type: OcaType::Cancel,
                ..entry(1)
            }),
            MALFORMED
        );
        assert_eq!(
            one(OrderIntent {
                oca_name: StrKey(9),
                ..entry(1)
            }),
            MALFORMED
        );
    }

    #[test]
    fn from_entry_on_an_entry_is_malformed() {
        assert_eq!(
            one(OrderIntent {
                from_entry: StrKey(7),
                ..entry(1)
            }),
            MALFORMED
        );
        let k = OrderIntent {
            kind: IntentKind::Cancel,
            from_entry: StrKey(7),
            ..entry(1)
        };
        assert_eq!(one(k), MALFORMED);
    }

    #[test]
    fn non_increasing_seq_is_malformed() {
        assert_eq!(validate(&[entry(2), entry(2)]), MALFORMED);
        assert_eq!(validate(&[entry(3), entry(2)]), MALFORMED);
        assert_eq!(validate(&[entry(0), entry(9)]), Ok(()));
    }

    #[test]
    fn zero_or_negative_qty_is_unsized() {
        assert_eq!(
            one(OrderIntent {
                flags: f::QTY,
                qty_1e6: 0,
                ..entry(1)
            }),
            UNSIZED
        );
        assert_eq!(
            one(OrderIntent {
                flags: f::QTY,
                qty_1e6: -1,
                ..entry(1)
            }),
            UNSIZED
        );
        assert_eq!(
            one(OrderIntent {
                flags: f::QTY,
                qty_1e6: 1,
                ..entry(1)
            }),
            Ok(())
        );
    }

    #[test]
    fn qty_percent_0_or_101_is_unsized() {
        let x = |p: i64| OrderIntent {
            kind: IntentKind::Exit,
            flags: f::QTY_PCT,
            qty_pct_1e6: p,
            ..entry(1)
        };
        assert_eq!(one(x(0)), UNSIZED);
        assert_eq!(one(x(101_000_000)), UNSIZED);
        assert_eq!(one(x(100 * 1_000_000 + 1)), UNSIZED);
        assert_eq!(one(x(100 * 1_000_000)), Ok(()));
        assert_eq!(one(x(1)), Ok(()));
    }

    #[test]
    fn a_bad_intent_rejects_its_whole_slice() {
        // The bad intent is last: nothing before it may be taken as valid (I35).
        let bad = OrderIntent {
            flags: f::QTY,
            qty_1e6: 0,
            ..entry(3)
        };
        assert_eq!(validate(&[entry(1), entry(2), bad]), UNSIZED);
        // A structural defect anywhere outranks a script refusal earlier in the slice.
        let bad_qty = OrderIntent {
            flags: f::QTY,
            qty_1e6: 0,
            ..entry(1)
        };
        let malformed = OrderIntent {
            _pad0: [1, 0, 0],
            ..entry(2)
        };
        assert_eq!(validate(&[bad_qty, malformed]), MALFORMED);
    }
}
