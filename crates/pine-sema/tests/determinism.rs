// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! impl-02 §7.11.3 `tests/determinism.rs` (§4.19, I4): the same source, options and library
//! sources give byte-identical programs, whatever cache instance serves the libraries.

mod common;

use common::{compile_with_libs, default_opts};
use pine_ir::{Dialect, ExecMode};
use pine_sema::{canonical_bytes, LibCache, LibKey};

const LIB: &str = "//@version=5\nlibrary(\"l\")\nexport type P\n    float v = 0.0\n    \
                   string s = \"x\"\nexport f(float x) => ta.sma(x, 5) * 2\n";

const ROOT: &str = "//@version=5\nindicator(\"t\")\nimport u/l/1\nimport u/m/1 as m\n\
                    p = l.P.new(close)\np.s := close > open ? \"u\" : \"d\"\n\
                    plot(l.f(close) + m.g(p.v))\n";

const LIB_M: &str = "//@version=5\nlibrary(\"m\")\nexport g(float x) => x + 1\n";

fn key(lib: &str) -> LibKey {
    LibKey {
        user: "u".to_owned(),
        lib: lib.to_owned(),
        ver: 1,
    }
}

fn cache(order: bool) -> LibCache {
    let mut v = vec![(key("l"), LIB.to_owned()), (key("m"), LIB_M.to_owned())];
    if !order {
        v.reverse();
    }
    LibCache::in_memory(v)
}

/// Two caches built separately (their entries listed in opposite orders), and one cache reused
/// after it has already served a compilation: equal `canonical_bytes` in both modes.
#[test]
fn sema_is_deterministic_over_two_lib_caches() {
    for mode in [ExecMode::BarClose, ExecMode::Realtime] {
        let o = default_opts(Dialect::V5, mode);
        let mut a = cache(true);
        let mut b = cache(false);
        let pa = compile_with_libs(ROOT, &o, &mut a).expect("compiles");
        let pb = compile_with_libs(ROOT, &o, &mut b).expect("compiles");
        let pa2 = compile_with_libs(ROOT, &o, &mut a).expect("compiles again");
        let (ba, bb, ba2) = (
            canonical_bytes(&pa),
            canonical_bytes(&pb),
            canonical_bytes(&pa2),
        );
        assert_eq!(ba, bb, "{mode:?}: two caches");
        assert_eq!(ba, ba2, "{mode:?}: a warm cache");
    }
}

/// I18 / law P4 (§4.11.5, §4.11.8–§4.11.9): the same `request.security` in two different scripts
/// — other declarations (`format`, `precision`, title), other code around it, other interning
/// order — gives equal keys and byte-identical children, for the chart's own symbol and another.
#[test]
fn subprogram_key_is_shared_across_scripts() {
    let o = default_opts(Dialect::V6, ExecMode::BarClose);
    for target in ["syminfo.tickerid", "\"BINANCE:ETHUSDT.P\""] {
        let req = format!("request.security({target}, \"D\", ta.sma(close, 5)[1])");
        let a = format!(
            "//@version=6\nindicator(\"a\", format = format.volume, precision = 2)\n\
             x = ta.ema(close, 3)\nplot(x)\nplot({req})\n"
        );
        let b = format!(
            "//@version=6\nindicator(\"bee\", overlay = true)\nlabel_txt = \"zzz\"\n\
             y = {req}\nplot(y * 2, title = label_txt)\n"
        );
        let pa = common::compile_src(&a, &o).expect("a compiles");
        let pb = common::compile_src(&b, &o).expect("b compiles");
        assert_eq!(pa.subprograms.len(), 1, "{target}");
        assert_eq!(pb.subprograms.len(), 1, "{target}");
        let (sa, sb) = (&pa.subprograms[0], &pb.subprograms[0]);
        assert_eq!(
            canonical_bytes(&sa.program),
            canonical_bytes(&sb.program),
            "{target}: byte-identical children"
        );
        assert_eq!(sa.key, sb.key, "{target}: one key");
    }
}
