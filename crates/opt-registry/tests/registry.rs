// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `OptRegistry` behaviour.
//!
//! The shapes exercised here are the ones the live capture actually
//! produces (VRP V0(d), 12 boots on 2026-09-09): a Deribit block of 64
//! options at ordinals 513..=576, allocated in selection order, sitting
//! above the static instruments at ordinals 1..=9 — and RESHUFFLING at
//! every boot, which is why the stale-symbol case below is a test and
//! not a comment.

use core_types::{make_symbol_id, symbol_ordinal, VenueId};
use opt_registry::{
    OptInstrument, OptRegistry, RegistryErr, OPT_REGISTRY_CAP, OPT_REGISTRY_SLOTS, RIGHT_CALL,
    RIGHT_PUT,
};

const CS: i64 = 1_000_000_000; // 1.0 coin, Deribit BTC/ETH
const OPT_BASE: u32 = 512; // core_config::universe::OPT_ORDINAL_BASE
const PERP: u32 = 1; // deribit:BTC-PERPETUAL — V0(e)

fn perp_sym() -> u32 {
    make_symbol_id(VenueId::Deribit, PERP)
}

/// The live shape: `n` options from ordinal 513 up, calls before puts.
fn deribit_chain(n: u32) -> Vec<OptInstrument> {
    let mut v = Vec::with_capacity(n as usize);
    for k in 0..n {
        let sym = make_symbol_id(VenueId::Deribit, OPT_BASE + 1 + k);
        v.push(OptInstrument::new(
            sym,
            perp_sym(),
            VenueId::Deribit as u8,
            1_789_027_200_000_000_000 + (k as u64 % 2) * 86_400_000_000_000,
            (78_000 + 500 * (k as i64 / 2)) * 1_000_000,
            if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
            CS,
        ));
    }
    v
}

#[test]
fn empty_registry_answers_nothing() {
    let r = OptRegistry::new();
    assert_eq!(r.len(), 0);
    assert!(r.is_empty());
    assert_eq!(r.venue(), 0);
    assert!(r.rows().is_empty());
    for ord in [0u32, 1, 512, 513, 576, 0x00FF_FFFF] {
        let sym = make_symbol_id(VenueId::Deribit, ord);
        assert!(r.get(sym).is_none());
        assert!(!r.is_option(sym));
    }
}

/// The V1 acceptance property: every discovered row round-trips through
/// insert/get, and any sym outside the inserted set is not an option.
#[test]
fn every_row_round_trips_and_outsiders_are_refused() {
    let chain = deribit_chain(64); // the measured live chain size
    let mut r = OptRegistry::new();
    for row in &chain {
        r.insert(*row).expect("insert");
    }
    assert_eq!(r.len(), 64);
    assert_eq!(r.venue(), VenueId::Deribit as u8);
    assert_eq!(r.rows().len(), 64);

    for row in &chain {
        let got = r.get(row.sym).expect("registered sym must resolve");
        assert_eq!(*got, *row);
        assert!(r.is_option(row.sym));
        assert_eq!(got.underlying_sym, perp_sym());
        assert_eq!(got.contract_size_1e9, CS);
    }

    // Outsiders: the statics below the block, the gap at the base, the
    // slot one past the end, and the far end of the ordinal space.
    let outsiders = [
        0u32,
        PERP,
        9,
        OPT_BASE,
        OPT_BASE + 1 + 64,
        OPT_BASE + 5_000,
        0x00FF_FFFF,
    ];
    for ord in outsiders {
        let sym = make_symbol_id(VenueId::Deribit, ord);
        assert!(r.get(sym).is_none(), "ordinal {ord} must not resolve");
        assert!(!r.is_option(sym));
    }
    // And the hedge leg itself is emphatically not an option.
    assert!(!r.is_option(perp_sym()));
}

/// Ordinal spaces overlap across venues — OKX allocates options from the
/// same base 512. A lookup must key on the FULL sym, never the ordinal.
#[test]
fn another_venues_identical_ordinal_does_not_resolve() {
    let chain = deribit_chain(8);
    let mut r = OptRegistry::new();
    for row in &chain {
        r.insert(*row).expect("insert");
    }
    for row in &chain {
        let ord = symbol_ordinal(row.sym);
        for v in [VenueId::Okx, VenueId::Binance, VenueId::Bybit] {
            let foreign = make_symbol_id(v, ord);
            assert!(
                r.get(foreign).is_none(),
                "venue {v:?} ordinal {ord} must not resolve against a Deribit table"
            );
        }
    }
}

/// Ordinals reshuffle every boot, so a registry from the previous boot
/// must answer `None` for a symbol whose instrument has rolled off —
/// never the instrument that inherited the ordinal.
#[test]
fn a_stale_symbol_resolves_to_nothing_not_to_a_foreign_row() {
    let mut r = OptRegistry::new();
    let sym = make_symbol_id(VenueId::Deribit, OPT_BASE + 1);
    r.insert(OptInstrument::new(
        sym,
        perp_sym(),
        VenueId::Deribit as u8,
        1_789_027_200_000_000_000,
        79_000_000_000,
        RIGHT_CALL,
        CS,
    ))
    .expect("insert");
    // Same ordinal, same venue: this IS the sym, so it resolves.
    assert!(r.is_option(sym));
    // A sym from a different venue byte at the same ordinal does not.
    assert!(!r.is_option(make_symbol_id(VenueId::Okx, OPT_BASE + 1)));
    // The row we get back is the one we put in — never a neighbour.
    assert_eq!(r.get(sym).unwrap().strike_1e6, 79_000_000_000);
}

#[test]
fn insert_is_order_independent_via_rebase() {
    let chain = deribit_chain(32);
    let mut ascending = OptRegistry::new();
    for row in &chain {
        ascending.insert(*row).expect("ascending insert");
    }
    // Same rows, reversed: the base must be rebased down as we go.
    let mut descending = OptRegistry::new();
    for row in chain.iter().rev() {
        descending.insert(*row).expect("descending insert");
    }
    assert_eq!(ascending.len(), descending.len());
    for row in &chain {
        assert_eq!(
            ascending.get(row.sym).expect("asc"),
            descending.get(row.sym).expect("desc")
        );
    }
}

#[test]
fn duplicate_sym_is_refused() {
    let chain = deribit_chain(2);
    let mut r = OptRegistry::new();
    r.insert(chain[0]).expect("first");
    assert_eq!(r.insert(chain[0]), Err(RegistryErr::Duplicate));
    assert_eq!(r.len(), 1);
    // A different row at the same ordinal is equally refused.
    let mut clash = chain[1];
    clash.sym = chain[0].sym;
    assert_eq!(r.insert(clash), Err(RegistryErr::Duplicate));
}

#[test]
fn foreign_venue_row_is_refused() {
    let mut r = OptRegistry::new();
    r.insert(deribit_chain(1)[0]).expect("first");
    let foreign = OptInstrument::new(
        make_symbol_id(VenueId::Okx, OPT_BASE + 2),
        make_symbol_id(VenueId::Okx, 1),
        VenueId::Okx as u8,
        1_789_027_200_000_000_000,
        79_000_000_000,
        RIGHT_CALL,
        CS,
    );
    assert_eq!(r.insert(foreign), Err(RegistryErr::VenueMismatch));
    assert_eq!(r.len(), 1);
}

#[test]
fn out_of_window_and_full_are_refused() {
    let mut r = OptRegistry::new();
    r.insert(deribit_chain(1)[0]).expect("first");
    let far = OptInstrument::new(
        make_symbol_id(VenueId::Deribit, OPT_BASE + 1 + OPT_REGISTRY_SLOTS as u32),
        perp_sym(),
        VenueId::Deribit as u8,
        1_789_027_200_000_000_000,
        79_000_000_000,
        RIGHT_CALL,
        CS,
    );
    assert_eq!(r.insert(far), Err(RegistryErr::OutOfWindow));

    let mut full = OptRegistry::new();
    for row in &deribit_chain(OPT_REGISTRY_CAP as u32) {
        full.insert(*row).expect("within cap");
    }
    assert_eq!(full.len(), OPT_REGISTRY_CAP);
    let one_more = OptInstrument::new(
        make_symbol_id(VenueId::Deribit, OPT_BASE + 1 + OPT_REGISTRY_CAP as u32),
        perp_sym(),
        VenueId::Deribit as u8,
        1_789_027_200_000_000_000,
        79_000_000_000,
        RIGHT_CALL,
        CS,
    );
    assert_eq!(full.insert(one_more), Err(RegistryErr::Full));
}

// ---------------------------------------------------------------
// constructors
// ---------------------------------------------------------------

#[test]
fn from_discovery_converts_units_once() {
    // The live boot path: ms and x1e9 straight off the venue's REST row.
    let row = OptInstrument::from_discovery(
        make_symbol_id(VenueId::Deribit, OPT_BASE + 1),
        perp_sym(),
        VenueId::Deribit as u8,
        1_789_027_200_000,   // expiration_ts_ms
        79_000_000_000_000,  // strike_1e9 = $79,000
        true,
        CS,
    )
    .expect("valid discovery row");
    assert_eq!(row.expiry_ns, 1_789_027_200_000_000_000);
    assert_eq!(row.strike_1e6, 79_000_000_000);
    assert!(row.is_call());
    assert_eq!(row.contract_size_1e9, CS);

    // Refusals: a boot must not round or guess.
    let bad = |exp, strike, cs| {
        OptInstrument::from_discovery(
            make_symbol_id(VenueId::Deribit, OPT_BASE + 1),
            perp_sym(),
            VenueId::Deribit as u8,
            exp,
            strike,
            true,
            cs,
        )
    };
    assert!(bad(0, 79_000_000_000_000, CS).is_none());
    assert!(bad(-1, 79_000_000_000_000, CS).is_none());
    assert!(bad(1_789_027_200_000, 0, CS).is_none());
    assert!(bad(1_789_027_200_000, 79_000_000_000_000, 0).is_none());
    // A strike finer than the x1e6 field would truncate: refused.
    assert!(bad(1_789_027_200_000, 79_000_000_000_001, CS).is_none());
}

#[test]
fn from_descriptor_matches_from_discovery() {
    let sym = make_symbol_id(VenueId::Deribit, OPT_BASE + 5);
    let from_name = OptInstrument::from_descriptor(
        sym,
        perp_sym(),
        VenueId::Deribit as u8,
        b"deribit:BTC-10SEP26-79000-C",
        CS,
    )
    .expect("descriptor");
    let from_row = OptInstrument::from_discovery(
        sym,
        perp_sym(),
        VenueId::Deribit as u8,
        1_789_027_200_000,
        79_000_000_000_000,
        true,
        CS,
    )
    .expect("discovery");
    // THE property that matters: the harness path and the boot path
    // produce the same instrument for the same instrument.
    assert_eq!(from_name, from_row);

    // A put, and the bare-name form.
    let p = OptInstrument::from_descriptor(
        sym,
        perp_sym(),
        VenueId::Deribit as u8,
        b"BTC-10SEP26-78000-P",
        CS,
    )
    .expect("bare put");
    assert!(!p.is_call());
    assert_eq!(p.strike_1e6, 78_000_000_000);

    // Non-options and foreign grammars produce no instrument at all.
    for d in [
        &b"deribit:BTC-PERPETUAL"[..],
        b"okx:BTC-USD-260910-79000-C",
        b"binance-opt:BTC-260910-79000-C",
        b"",
    ] {
        assert!(
            OptInstrument::from_descriptor(sym, perp_sym(), VenueId::Deribit as u8, d, CS)
                .is_none(),
            "{:?} must not become an instrument",
            core::str::from_utf8(d)
        );
    }
}

#[test]
fn a_manifest_block_builds_a_registry_end_to_end() {
    // Verbatim rows from a live `instrument-manifest.tsv` (2026-09-09,
    // run-1788942689239338000), statics included so the filter is
    // exercised the way the harness will exercise it.
    let manifest: &[(u32, &[u8])] = &[
        (50_331_649, b"deribit:BTC-PERPETUAL"),
        (50_331_650, b"deribit:BTC_USDC"),
        (50_332_161, b"deribit:BTC-10SEP26-78000-C"),
        (50_332_162, b"deribit:BTC-10SEP26-78000-P"),
        (50_332_163, b"deribit:BTC-10SEP26-78500-C"),
        (50_332_164, b"deribit:BTC-10SEP26-78500-P"),
        (50_332_165, b"deribit:BTC-10SEP26-79000-C"),
        (50_332_166, b"deribit:BTC-10SEP26-79000-P"),
    ];
    let mut r = OptRegistry::new();
    let mut skipped = 0usize;
    for (sym, desc) in manifest {
        match OptInstrument::from_descriptor(*sym, perp_sym(), VenueId::Deribit as u8, desc, CS) {
            Some(row) => r.insert(row).expect("insert"),
            None => skipped += 1,
        }
    }
    assert_eq!(skipped, 2, "the two statics are not options");
    assert_eq!(r.len(), 6);
    // The perp is the hedge leg, present in the manifest, never an option.
    assert!(!r.is_option(50_331_649));
    let c = r.get(50_332_165).expect("the 79000 call");
    assert!(c.is_call());
    assert_eq!(c.strike_1e6, 79_000_000_000);
    assert_eq!(c.expiry_ns, 1_789_027_200_000_000_000);
    assert_eq!(c.underlying_sym, 50_331_649);
    let p = r.get(50_332_166).expect("the 79000 put");
    assert!(!p.is_call());
    assert_eq!(p.strike_1e6, c.strike_1e6);
    assert_eq!(p.expiry_ns, c.expiry_ns);
}

#[test]
fn layout_is_one_cache_line() {
    assert_eq!(core::mem::size_of::<OptInstrument>(), 64);
    assert_eq!(core::mem::align_of::<OptInstrument>(), 64);
    assert_eq!(core::mem::align_of::<OptRegistry>(), 64);
}

proptest::proptest! {
    /// Round-trip over arbitrary in-window chains: everything inserted
    /// resolves to itself, and nothing else resolves at all.
    #[test]
    fn arbitrary_chains_round_trip(
        base in 0u32..1_000_000,
        n in 1usize..=OPT_REGISTRY_CAP,
        gap in 0u32..3,
    ) {
        // CLAMP the chain length to what the window holds rather than
        // rejecting the case. `prop_assume!` here threw away so many
        // inputs that proptest aborted on its global-reject cap once the
        // case count was raised (found at PROPTEST_CASES=200000); a
        // generator that constructs valid inputs tests strictly more.
        let step = gap + 1;
        let max_n = (((OPT_REGISTRY_SLOTS as u32 - 1) / step + 1) as usize).min(OPT_REGISTRY_CAP);
        let n = n.min(max_n);
        let mut r = OptRegistry::new();
        let mut syms = Vec::with_capacity(n);
        for k in 0..n as u32 {
            let sym = make_symbol_id(VenueId::Deribit, base + k * step);
            syms.push(sym);
            r.insert(OptInstrument::new(
                sym,
                perp_sym(),
                VenueId::Deribit as u8,
                1_789_027_200_000_000_000,
                (79_000 + k as i64) * 1_000_000,
                if k % 2 == 0 { RIGHT_CALL } else { RIGHT_PUT },
                CS,
            )).expect("in-window insert");
        }
        proptest::prop_assert_eq!(r.len(), n);
        for (k, sym) in syms.iter().enumerate() {
            let got = r.get(*sym).expect("inserted sym resolves");
            proptest::prop_assert_eq!(got.sym, *sym);
            proptest::prop_assert_eq!(got.strike_1e6, (79_000 + k as i64) * 1_000_000);
        }
        // The gaps between entries, when there are any, hold nothing.
        if step > 1 {
            for k in 0..n as u32 - 1 {
                let hole = make_symbol_id(VenueId::Deribit, base + k * step + 1);
                proptest::prop_assert!(!r.is_option(hole));
            }
        }
    }
}
