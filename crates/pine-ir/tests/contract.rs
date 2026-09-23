// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Contract v4 pins: hash test vectors, encodings, the interner, the scale
//! helpers. Sizes and alignments are `const` assertions in the crate
//! itself (a drift is a compile error); these are the value-level laws.

use pine_ir::*;

// ---------------------------------------------------------------- hashes

#[test]
fn fnv1a64_matches_the_published_vectors() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn fnv1a64_streams_like_the_joined_string() {
    let h = fnv1a64_continue(fnv1a64(b"binance:"), b"btcusdt");
    assert_eq!(h, fnv1a64(b"binance:btcusdt"));
}

#[test]
fn symbol_hash_test_vectors_are_frozen() {
    // Contract §3.1. Changing either value orphans every prepared bar file
    // and every shared series key.
    assert_eq!(symbol_hash(b"binance:btcusdt"), 0xeb02_3695_eea7_d980);
    assert_eq!(symbol_hash(b"hyperliquid:btc"), 0xe6c1_7809_e356_29f4);
    assert_eq!(symbol_hash(b"okx:btc-usdt-swap"), 0x8643_e515_86d1_5379);
    assert_eq!(symbol_hash(b"binance_usdm:btcusdt"), 0xb5b3_78ac_bc13_e5b4);
}

#[test]
fn canonical_symbol_keys_are_recognised() {
    for ok in [
        &b"binance:btcusdt"[..],
        b"hyperliquid:btc",
        b"okx:btc-usdt-swap",
        b"deribit:btc-perpetual",
        b"polymarket:1234567890",
        b"hyperliquid:kpepe",
    ] {
        assert!(is_canonical_symbol_key(ok), "{:?}", core::str::from_utf8(ok));
    }
    for bad in [
        &b""[..],
        b"binance",
        b"binance:",
        b":btcusdt",
        b"BINANCE:BTCUSDT.P",
        b"binance:BTCUSDT",
        b"binance: btcusdt",
        b"bin ance:btcusdt",
        b"binance:btc:usdt",
        b"1binance:btcusdt",
        b"binance:-btc",
    ] {
        assert!(!is_canonical_symbol_key(bad), "{:?}", core::str::from_utf8(bad));
    }
    let long = [b'a'; SYMBOL_KEY_MAX_LEN + 1];
    assert!(!is_canonical_symbol_key(&long));
}

#[test]
fn str_key_never_returns_none_and_matches_fnv() {
    assert_eq!(str_key(b"Long"), StrKey(0x4efb_cab4_8a43_3013));
    assert_eq!(str_key(b"").0, 0xcbf2_9ce4_8422_2325);
    assert_ne!(str_key(b""), StrKey::NONE);
}

// ---------------------------------------------------------------- interner

#[test]
fn symtable_reserves_sym0_as_the_empty_string() {
    let t = SymTable::new();
    assert_eq!(t.len(), 1);
    assert!(!t.is_empty());
    assert_eq!(t.resolve(Sym::NONE), "");
    assert_eq!(t.get(""), Some(Sym::NONE));
    let d = SymTable::default();
    assert_eq!(d.resolve(Sym(0)), "");
}

#[test]
fn symtable_interns_once_and_resolves_without_copy() {
    let mut t = SymTable::new();
    let a = t.intern("close");
    let b = t.intern("ta.sma");
    let a2 = t.intern("close");
    assert_eq!(a, a2);
    assert_ne!(a, b);
    assert_eq!(t.resolve(a), "close");
    assert_eq!(t.resolve(b), "ta.sma");
    assert_eq!(t.get("ta.sma"), Some(b));
    assert_eq!(t.get("ta.ema"), None);
    // The CONST string region view.
    let (off, len) = t.span_of(b);
    assert_eq!(t.str_at(off, len), b"ta.sma");
    assert_eq!(&t.bytes()[off as usize..(off + len) as usize], b"ta.sma");
}

#[test]
fn symtable_survives_index_growth_and_is_deterministic() {
    let mut t1 = SymTable::new();
    let mut t2 = SymTable::new();
    let mut i = 0u32;
    while i < 5_000 {
        let s = format!("sym_{i}");
        assert_eq!(t1.intern(&s), t2.intern(&s));
        i += 1;
    }
    let mut j = 0u32;
    while j < 5_000 {
        let s = format!("sym_{j}");
        let g = t1.get(&s).expect("present");
        assert_eq!(t1.resolve(g), s);
        j += 1;
    }
    assert_eq!(t1.len(), 5_001);
    assert_eq!(t1.bytes(), t2.bytes());
}

#[test]
fn symtable_is_clone_and_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SymTable>();
    assert_send_sync::<PineProgram>();
    let mut t = SymTable::new();
    let s = t.intern("x");
    let c = t.clone();
    assert_eq!(c.resolve(s), "x");
}

// ---------------------------------------------------------------- encodings

#[test]
fn strref_round_trips_every_region() {
    let k = StrRef::konst(12, 5);
    assert_eq!((k.region(), k.off(), k.len()), (str_region::CONST, 12, 5));
    let s = StrRef::scratch(StrRef::MAX_OFF, u32::MAX);
    assert_eq!((s.region(), s.off(), s.len()), (str_region::SCRATCH, StrRef::MAX_OFF, u32::MAX));
    let p = StrRef::persist(4096, 0);
    assert_eq!((p.region(), p.off(), p.len()), (str_region::PERSIST, 4096, 0));
    assert!(p.is_empty());
    assert!(k.is_valid_encoding() && s.is_valid_encoding() && p.is_valid_encoding());
    assert!(!StrRef(NA_I64 as u64).is_valid_encoding());
}

#[test]
fn collref_round_trips_and_never_decodes_na() {
    let r = CollRef::new(SiteId(513), 7);
    assert_eq!(r.site(), SiteId(513));
    assert_eq!(r.gen(), 7);
    assert!(r.is_valid_encoding());
    assert!(!CollRef(NA_I64 as u64).is_valid_encoding());
    assert!(!CollRef(0).is_valid_encoding(), "gen 0 is never live");
    assert_eq!(CollRef::next_gen(CollRef::GEN_MASK), 1, "31-bit wrap skips 0");
    assert_eq!(CollRef::next_gen(1), 2);
}

#[test]
fn colour_packing_is_bgr_plus_transparency() {
    let c = color::pack(0x12, 0x34, 0x56, 80);
    assert_eq!(c, 0x5012_3456);
    assert_eq!((color::r(c), color::g(c), color::b(c), color::t(c)), (0x12, 0x34, 0x56, 80));
    assert_eq!(color::t(color::pack(0, 0, 0, 250)), 100, "transparency clamps to 100");
}

#[test]
fn elem_encoding_separates_scalars_enums_and_udts() {
    assert_eq!(elem::as_scalar(elem::scalar(Ty::Float)), Some(Ty::Float));
    assert_eq!(elem::as_scalar(elem::enumeration(3)), None);
    assert_eq!(elem::as_scalar(elem::udt(3)), None);
    assert_ne!(elem::enumeration(3), elem::udt(3));
    let t = TypeQ::map(elem::scalar(Ty::String), elem::scalar(Ty::Int), Qual::Series);
    assert_eq!(t.ty, Ty::Map);
    assert_eq!(elem::as_scalar(t.a), Some(Ty::String));
    assert_eq!(elem::as_scalar(t.b), Some(Ty::Int));
    assert!(Qual::Const < Qual::Input && Qual::Input < Qual::Simple && Qual::Simple < Qual::Series);
}

// ---------------------------------------------------------------- diagnostics

#[test]
fn diag_code_ranges_are_frozen() {
    assert_eq!(DiagCode::sema(16), DiagCode(3016));
    assert_eq!(DiagCode::verify(8), DiagCode(4008));
    assert_eq!(DiagCode(1500).range(), DiagRange::Lexer);
    assert_eq!(DiagCode(2001).range(), DiagRange::Parser);
    assert_eq!(DiagCode(3041).range(), DiagRange::Sema);
    assert_eq!(DiagCode(4021).range(), DiagRange::Verifier);
    assert_eq!(DiagCode(5000).range(), DiagRange::Std);
    assert_eq!(DiagCode(5107).range(), DiagRange::Broker);
    assert_eq!(DiagCode(6001).range(), DiagRange::Host);
    assert_eq!(DiagCode(7001).range(), DiagRange::Harness);
    assert_eq!(DiagCode(5500).range(), DiagRange::Unassigned);
}

// ---------------------------------------------------------------- ISA

#[test]
fn opcode_table_defines_exactly_the_v4_set() {
    let mut n = 0u32;
    let mut op = 0u32;
    while op < 256 {
        if pine_ir::op::defined(op as u8) {
            n += 1;
        }
        op += 1;
    }
    // 4 control + 16 f64 + 16 i64 + 12 compare + 8 na + 2 history
    // + 7 flow + 1 builtin + 5 object + 1 string.
    assert_eq!(n, 72);
    for retired in [0x3Cu8, 0x3D, 0x52, 0x53, 0x54, 0x85, 0x98, 0x9A, 0x9F, 0xA0, 0xA1, 0xB0, 0xC0, 0xC6] {
        assert!(!pine_ir::op::defined(retired), "{retired:#04x} is retired");
    }
    assert_eq!(Insn::new(pine_ir::op::JMP, 0, 0x0001, 0x0002, 0).target(), 0x0001_0002);
    assert_eq!(argwin::slot(argwin::i64(513)), 513);
    assert!(argwin::is_i64(argwin::i64(0)));
    assert!(!argwin::is_i64(argwin::f64(0x7FFF)));
    assert_eq!(argwin::f64(0x8001), 1, "an f64 entry can never carry the lane bit");
    assert_eq!(abi::RESERVED_F, 5);
    assert_eq!(abi::RESERVED_I, 10);
}

#[test]
fn barflag_bits_are_the_frozen_ones() {
    assert_eq!(barflag::CONFIRMED, 1);
    assert_eq!(barflag::GAP_BEFORE, 2);
    assert_eq!(barflag::REALTIME, 4);
    assert_eq!(barflag::LAST, 8);
    assert_eq!(barflag::LAST_CONFIRMED_HISTORY, 16);
    assert_eq!(barflag::VOL_SAT, 32);
    assert_eq!(barflag::DEFINED, 63);
}

// ---------------------------------------------------------------- scale

#[test]
fn to_1e6_rounds_half_away_from_zero_and_saturates() {
    assert_eq!(scale::to_1e6(1.0), Some(1_000_000));
    assert_eq!(scale::to_1e6(0.1), Some(100_000));
    assert_eq!(scale::to_1e6(0.000_000_5), Some(1));
    assert_eq!(scale::to_1e6(-0.000_000_5), Some(-1));
    assert_eq!(scale::to_1e6(43_210.123_456), Some(43_210_123_456));
    assert_eq!(scale::to_1e6(f64::NAN), None);
    assert_eq!(scale::to_1e6(f64::INFINITY), Some(i64::MAX));
    assert_eq!(scale::to_1e6(f64::NEG_INFINITY), Some(i64::MIN));
    assert_eq!(scale::to_1e6(1e300), Some(i64::MAX));
    assert_eq!(scale::from_1e6(43_210_123_456), 43_210.123_456);
}

#[test]
fn money_renders_like_the_fill_ledger() {
    // The same law as cli::backtest::fill::usd_1e12_to_1e6_floor / _ceil.
    assert_eq!(scale::e12_to_1e6_floor(1_500_000), 1);
    assert_eq!(scale::e12_to_1e6_floor(-1_500_000), -2);
    assert_eq!(scale::e12_to_1e6_ceil(1_500_000), 2);
    assert_eq!(scale::e12_to_1e6_ceil(-1_500_000), -1);
    assert_eq!(scale::e12_to_1e6_floor(3_000_000), 3);
    assert_eq!(scale::e12_to_1e6_ceil(3_000_000), 3);
    assert_eq!(scale::e12_to_1e6_floor(i128::MAX), i64::MAX);
    assert_eq!(scale::e12_to_1e6_ceil(i128::MIN), i64::MIN);
    assert_eq!(scale::notional_1e12(100_000_000_000, 2_500_000), 250_000_000_000_000_000);
    assert_eq!(scale::usd_1e6_to_1e12(2_000_000), 2_000_000_000_000);
    assert_eq!(scale::ms_from_ns(-1), -1, "floor, not truncation");
    assert_eq!(scale::ms_from_ns(1_999_999), 1);
    assert_eq!(scale::ns_from_ms(i64::MAX), i64::MAX);
}

// ---------------------------------------------------------------- defaults

#[test]
fn default_pods_are_all_zero_and_none() {
    let i = OrderIntent::default();
    assert_eq!(i.kind, IntentKind::Entry);
    assert_eq!(i.id, StrKey::NONE);
    assert_eq!(i.flags, 0);
    let t = TradeRec::default();
    assert_eq!(t.flags, 0);
    let b = Bar::default();
    assert_eq!(b.flags, 0);
    assert_eq!(StepOutcome::Ok, StepOutcome::Ok);
    assert_eq!(CONTRACT_VERSION, 4);
}
