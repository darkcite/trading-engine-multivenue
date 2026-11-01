// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Venue JSON → term-proto, without a JSON library (CLAUDE.md bans
//! `serde_json`). A small, bounded walker: `get` finds a member of an object
//! at its own depth (never inside a nested value or a string), `items` walks
//! an array; both return raw value slices and never allocate or panic.
//! Decimal strings go through `term_proto::dec` (T-L8: more than 6 decimals
//! is refused, never rounded).

use term_proto::dec::parse_1e6;
use term_proto::Side;

const MAX_DEPTH: usize = 32;

#[inline]
fn ws(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// End (exclusive) of the string whose opening quote is at `i`.
fn str_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// End (exclusive) of the value starting at `i`.
pub fn value_end(b: &[u8], i: usize) -> Option<usize> {
    let i = ws(b, i);
    match *b.get(i)? {
        b'"' => str_end(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = str_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => {
                        depth += 1;
                        if depth > MAX_DEPTH {
                            return None;
                        }
                    }
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// Strip the quotes of a string value; other values pass through.
#[inline]
pub fn unquote(v: &[u8]) -> &[u8] {
    if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// The raw value of member `key` of the object `obj` (at its own depth).
pub fn get<'a>(obj: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let mut i = ws(obj, 0);
    if *obj.get(i)? != b'{' {
        return None;
    }
    i += 1;
    loop {
        i = ws(obj, i);
        match *obj.get(i)? {
            b'}' => return None,
            b',' => {
                i += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let k_end = str_end(obj, i)?;
        let k = &obj[i + 1..k_end - 1];
        i = ws(obj, k_end);
        if *obj.get(i)? != b':' {
            return None;
        }
        let v0 = ws(obj, i + 1);
        let v1 = value_end(obj, v0)?;
        if k == key {
            return Some(&obj[v0..v1]);
        }
        i = v1;
    }
}

/// The elements of the array `arr`.
pub fn items(arr: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut i = ws(arr, 0);
    let mut ok = arr.get(i) == Some(&b'[');
    i += 1;
    core::iter::from_fn(move || {
        if !ok {
            return None;
        }
        i = ws(arr, i);
        if arr.get(i) == Some(&b',') {
            i = ws(arr, i + 1);
        }
        match arr.get(i) {
            None | Some(b']') => {
                ok = false;
                None
            }
            Some(_) => match value_end(arr, i) {
                Some(e) => {
                    let v = &arr[i..e];
                    i = e;
                    Some(v)
                }
                None => {
                    ok = false;
                    None
                }
            },
        }
    })
}

pub fn dec(v: &[u8]) -> Option<i64> {
    parse_1e6(unquote(v)).ok()
}

pub fn int(v: &[u8]) -> Option<u64> {
    let s = core::str::from_utf8(unquote(v)).ok()?;
    s.parse().ok()
}

// ---- Hyperliquid ---------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Hl<'a> {
    /// A full `l2Book` snapshot: `levels` is `[[bids…],[asks…]]`.
    L2 { coin: &'a [u8], time_ms: u64, levels: &'a [u8] },
    Trades { data: &'a [u8] },
    Pong,
    Other,
}

pub fn hl(msg: &[u8]) -> Option<Hl<'_>> {
    let ch = unquote(get(msg, b"channel")?);
    let data = get(msg, b"data");
    Some(match ch {
        b"l2Book" => {
            let d = data?;
            Hl::L2 { coin: unquote(get(d, b"coin")?), time_ms: int(get(d, b"time")?)?, levels: get(d, b"levels")? }
        }
        b"trades" => Hl::Trades { data: data? },
        b"pong" => Hl::Pong,
        _ => Hl::Other,
    })
}

/// Each level of an `l2Book` side: `(px_1e6, sz_1e6)`. Any malformed level
/// fails the whole book (a partial book is worse than none).
pub fn hl_levels(levels: &[u8], mut f: impl FnMut(Side, i64, i64)) -> Option<()> {
    let mut sides = items(levels);
    for side in [Side::Buy, Side::Sell] {
        for lv in items(sides.next()?) {
            let px = dec(get(lv, b"px")?)?;
            let sz = dec(get(lv, b"sz")?)?;
            if px <= 0 || sz < 0 {
                return None;
            }
            f(side, px, sz);
        }
    }
    Some(())
}

/// One HL print: coin, aggressor side ("B" buy / "A" sell), px, sz, time.
pub fn hl_trade(t: &[u8]) -> Option<(&[u8], Side, i64, i64, u64)> {
    let side = match unquote(get(t, b"side")?) {
        b"B" => Side::Buy,
        b"A" => Side::Sell,
        _ => return None,
    };
    let px = dec(get(t, b"px")?)?;
    let sz = dec(get(t, b"sz")?)?;
    (px > 0 && sz > 0).then_some((unquote(get(t, b"coin")?), side, px, sz, int(get(t, b"time")?)?))
}

// ---- Binance USDⓈ-M combined stream ----------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum Bn<'a> {
    Depth { sym: &'a [u8], first: u64, last: u64, prev: u64, bids: &'a [u8], asks: &'a [u8] },
    /// `buyer_maker` ⇒ the aggressor sold.
    Agg { sym: &'a [u8], px: i64, qty: i64, buyer_maker: bool, t_ms: u64 },
    Other,
}

pub fn bn(msg: &[u8]) -> Option<Bn<'_>> {
    let d = get(msg, b"data")?;
    Some(match unquote(get(d, b"e")?) {
        b"depthUpdate" => Bn::Depth {
            sym: unquote(get(d, b"s")?),
            first: int(get(d, b"U")?)?,
            last: int(get(d, b"u")?)?,
            prev: int(get(d, b"pu")?)?,
            bids: get(d, b"b")?,
            asks: get(d, b"a")?,
        },
        b"aggTrade" => {
            let px = dec(get(d, b"p")?)?;
            let qty = dec(get(d, b"q")?)?;
            let buyer_maker = match get(d, b"m")? {
                b"true" => true,
                b"false" => false,
                _ => return None,
            };
            if px <= 0 || qty <= 0 {
                return None;
            }
            Bn::Agg { sym: unquote(get(d, b"s")?), px, qty, buyer_maker, t_ms: int(get(d, b"T")?)? }
        }
        _ => Bn::Other,
    })
}

/// `[["px","qty"], …]` pairs (depth events and the REST snapshot).
pub fn bn_pairs(arr: &[u8], mut f: impl FnMut(i64, i64)) -> Option<()> {
    for p in items(arr) {
        let mut it = items(p);
        let px = dec(it.next()?)?;
        let q = dec(it.next()?)?;
        if px <= 0 || q < 0 || it.next().is_some() {
            return None;
        }
        f(px, q);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn scanners_never_panic(b in prop::collection::vec(any::<u8>(), 0..400)) {
            let _ = hl(&b);
            let _ = bn(&b);
            let _ = hl_levels(&b, |_, _, _| {});
            let _ = bn_pairs(&b, |_, _| {});
            for it in items(&b) { let _ = get(it, b"px"); }
        }

        #[test]
        fn scanners_never_panic_on_jsonish(parts in prop::collection::vec(prop_oneof![
            Just("{".to_string()), Just("}".to_string()), Just("[".to_string()), Just("]".to_string()),
            Just(",".to_string()), Just(":".to_string()), Just("\"".to_string()), Just("\\".to_string()),
            Just("\"channel\"".to_string()), Just("\"data\"".to_string()), Just("\"l2Book\"".to_string()),
            Just("\"levels\"".to_string()), Just("\"px\"".to_string()), Just("\"1.5\"".to_string()), Just("7".to_string())
        ], 0..80)) {
            let s = parts.concat();
            let b = s.as_bytes();
            let _ = hl(b);
            let _ = bn(b);
            if let Some(Hl::L2 { levels, .. }) = hl(b) { let _ = hl_levels(levels, |_, _, _| {}); }
        }
    }

    const HL_L2: &[u8] = br#"{"channel":"l2Book","data":{"coin":"BTC","time":1761980000123,"levels":[[{"px":"67012.0","sz":"1.234","n":5},{"px":"67011.0","sz":"0.5","n":1}],[{"px":"67013.0","sz":"2.0","n":3}]]}}"#;
    const HL_TR: &[u8] = br#"{"channel":"trades","data":[{"coin":"BTC","side":"A","px":"67012.0","sz":"0.01","time":1761980000200,"hash":"0xab","tid":7,"users":["0x1","0x2"]}]}"#;
    const BN_D: &[u8] = br#"{"stream":"btcusdt@depth@100ms","data":{"e":"depthUpdate","E":1761980000300,"T":1761980000299,"s":"BTCUSDT","U":100,"u":105,"pu":99,"b":[["67010.10","1.500"],["67009.00","0"]],"a":[["67010.20","0.004"]]}}"#;
    const BN_A: &[u8] = br#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","E":1,"a":2,"s":"BTCUSDT","p":"67010.20","q":"0.010","f":3,"l":3,"T":1761980000301,"m":true}}"#;

    #[test]
    fn hyperliquid() {
        let Some(Hl::L2 { coin, time_ms, levels }) = hl(HL_L2) else { panic!() };
        assert_eq!((coin, time_ms), (&b"BTC"[..], 1761980000123));
        let mut v = Vec::new();
        hl_levels(levels, |s, p, q| v.push((s, p, q))).unwrap();
        assert_eq!(v, vec![(Side::Buy, 67_012_000_000, 1_234_000), (Side::Buy, 67_011_000_000, 500_000), (Side::Sell, 67_013_000_000, 2_000_000)]);
        let Some(Hl::Trades { data }) = hl(HL_TR) else { panic!() };
        let t = items(data).next().unwrap();
        assert_eq!(hl_trade(t), Some((&b"BTC"[..], Side::Sell, 67_012_000_000, 10_000, 1761980000200)));
    }

    #[test]
    fn binance() {
        let Some(Bn::Depth { sym, first, last, prev, bids, asks }) = bn(BN_D) else { panic!() };
        assert_eq!((sym, first, last, prev), (&b"BTCUSDT"[..], 100, 105, 99));
        let mut b = Vec::new();
        bn_pairs(bids, |p, q| b.push((p, q))).unwrap();
        assert_eq!(b, vec![(67_010_100_000, 1_500_000), (67_009_000_000, 0)]);
        let mut a = Vec::new();
        bn_pairs(asks, |p, q| a.push((p, q))).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(bn(BN_A), Some(Bn::Agg { sym: b"BTCUSDT", px: 67_010_200_000, qty: 10_000, buyer_maker: true, t_ms: 1761980000301 }));
    }

    #[test]
    fn keys_inside_strings_or_nested_values_do_not_match() {
        let j = br#"{"x":"\"channel\":\"trades\"","nested":{"channel":"l2Book"},"channel":"pong"}"#;
        assert_eq!(hl(j), Some(Hl::Pong));
        assert_eq!(get(br#"{"a":[1,{"b":2}],"b":3}"#, b"b"), Some(&b"3"[..]));
    }

    #[test]
    fn refuses_more_than_six_decimals() {
        let j = br#"{"stream":"s","data":{"e":"aggTrade","s":"X","p":"0.0000001","q":"1","m":false,"T":1}}"#;
        assert_eq!(bn(j), None);
    }
}
