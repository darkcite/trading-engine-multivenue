// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # `fx` — integer log2 / exp2 / ln / exp
//!
//! The transcendental floor the whole VRP forecast rests on. The lane
//! forbids floats on any path an order can be born from, and the
//! forecast needs a logarithm twice per expiry (forming `x` and `y`)
//! and an exponential three times (σ̂ and the two bounds). These four
//! functions are that arithmetic, in `i64`/`u64` with `i128`
//! intermediates and no `f64` anywhere outside `#[cfg(test)]`.
//!
//! ## The law
//!
//! * [`log2_1e9`] takes a RAW `u64` and returns `log2(x) × 1e9`.
//! * [`exp2_1e9`] is its inverse: it takes `v × 1e9` and returns the
//!   RAW `2^v`, saturating at [`u64::MAX`].
//! * [`ln_1e9`] / [`exp_1e9`] are the same pair through
//!   [`LN2_1E9`].
//!
//! Because the pair is raw-in / raw-out, a fixed-point SCALE is just an
//! additive constant in the exponent domain: `x × 1e9` in the value
//! domain is `+`[`LOG2_1E9_1E9`] in the `log2` domain. That identity is
//! how the engine gets a `×1e9` result out of [`exp2_1e9`] without a
//! second code path, and it is exact — no rounding is introduced by the
//! scaling itself.
//!
//! ## Accuracy, and why it is load-bearing
//!
//! Both directions are a 256-interval linear interpolation over
//! `[1, 2)` with the mantissa carried in Q32. Linear interpolation of
//! `log2` over an interval of width `h = 1/256` has a maximum absolute
//! error of `h²/(8 ln 2)` ≈ `2.75e-6` in `log2` units, i.e. `1.9e-6` in
//! natural-log units — and an absolute error `ε` in a natural log is a
//! RELATIVE error `ε` after exponentiating. So the round trip is good to
//! about `2e-6`, four orders inside the `±10 %` decision band the θ
//! rule opens, and two orders inside the `1e-4` the build card demands.
//! `fx_accuracy` proves it against `f64` across the working domain
//! rather than asserting it.
//!
//! ## Doctrine
//!
//! * No allocation, no panic in release, no `unsafe`.
//! * Tables are `const`, 257 entries each (the `+1` is the right-hand
//!   endpoint, so interpolation never wraps and never branches on the
//!   last interval).
//! * Every table entry is re-derived from `f64` in a test — a
//!   hand-edited table is the kind of defect that survives every other
//!   gate.

/// `ln(2) × 1e9`.
pub const LN2_1E9: i64 = 693_147_181;

/// `log2(1e9) × 1e9`. Adding this in the `log2` domain multiplies by
/// `1e9` in the value domain — the crate's only scaling primitive.
///
/// The product still has to fit a `u64`, so the identity is good for
/// values below about `1.8e10` before [`exp2_1e9`] saturates. Every
/// caller in this crate uses it on a QLIKE ratio, which sits at 1.
pub const LOG2_1E9_1E9: i64 = 29_897_352_854;

/// Returned by [`log2_1e9`] for an input of `0`, where the true value
/// is `−∞`. Callers must reject a non-positive input before they get
/// here; this exists so the function is total rather than panicking.
pub const LOG2_UNDEFINED: i64 = i64::MIN;

/// `log2(1 + i/256) × 1e9`, `i` in `0..=256`. Re-derived by
/// `log2_table_is_the_rounded_truth`.
const LOG2_TAB: [i64; 257] = [
    0,          5624549,    11227255,   16808288,
    22367813,   27905997,   33423002,   38918989,
    44394119,   49848549,   55282436,   60695932,
    66089190,   71462363,   76815597,   82149041,
    87462841,   92757141,   98032083,   103287808,
    108524457,  113742166,  118941073,  124121312,
    129283017,  134426320,  139551352,  144658243,
    149747120,  154818109,  159871337,  164906927,
    169925001,  174925683,  179909090,  184875343,
    189824559,  194756854,  199672345,  204571144,
    209453366,  214319121,  219168520,  224001674,
    228818690,  233619677,  238404739,  243173983,
    247927513,  252665432,  257387843,  262094845,
    266786541,  271463028,  276124405,  280770770,
    285402219,  290018847,  294620749,  299208018,
    303780748,  308339030,  312882955,  317412614,
    321928095,  326429487,  330916878,  335390355,
    339850003,  344295908,  348728154,  353146825,
    357552005,  361943774,  366322214,  370687407,
    375039431,  379378367,  383704292,  388017285,
    392317423,  396604781,  400879436,  405141463,
    409390936,  413627929,  417852515,  422064766,
    426264755,  430452552,  434628228,  438791853,
    442943496,  447083226,  451211112,  455327220,
    459431619,  463524373,  467605550,  471675214,
    475733431,  479780264,  483815777,  487840034,
    491853096,  495855027,  499845887,  503825738,
    507794640,  511752654,  515699838,  519636253,
    523561956,  527477006,  531381461,  535275377,
    539158811,  543031820,  546894460,  550746785,
    554588852,  558420713,  562242424,  566054038,
    569855608,  573647187,  577428828,  581200582,
    584962501,  588714636,  592457037,  596189756,
    599912842,  603626345,  607330314,  611024797,
    614709844,  618385502,  622051819,  625708843,
    629356620,  632995197,  636624621,  640244936,
    643856190,  647458426,  651051691,  654636029,
    658211483,  661778098,  665335917,  668884984,
    672425342,  675957033,  679480100,  682994584,
    686500527,  689997971,  693486957,  696967526,
    700439718,  703903573,  707359132,  710806434,
    714245518,  717676423,  721099189,  724513853,
    727920455,  731319031,  734709620,  738092260,
    741466986,  744833837,  748192850,  751544059,
    754887502,  758223215,  761551232,  764871591,
    768184325,  771489470,  774787060,  778077130,
    781359714,  784634846,  787902559,  791162889,
    794415866,  797661526,  800899900,  804131021,
    807354922,  810571635,  813781191,  816983623,
    820178962,  823367240,  826548487,  829722735,
    832890014,  836050355,  839203788,  842350343,
    845490051,  848622940,  851749041,  854868383,
    857980995,  861086906,  864186145,  867278740,
    870364720,  873444113,  876516947,  879583250,
    882643049,  885696373,  888743249,  891783703,
    894817763,  897845456,  900866808,  903881846,
    906890596,  909893084,  912889336,  915879379,
    918863237,  921840937,  924812504,  927777962,
    930737338,  933690655,  936637939,  939579214,
    942514505,  945443836,  948367232,  951284715,
    954196310,  957102042,  960001932,  962896005,
    965784285,  968666793,  971543554,  974414590,
    977279923,  980139578,  982993575,  985841937,
    988684687,  991521846,  994353437,  997179481,
    1000000000,
];

/// `2^(i/256) × 2^32`, `i` in `0..=256`. Re-derived by
/// `exp2_table_is_the_rounded_truth`.
const EXP2_TAB: [u64; 257] = [
    4294967296, 4306612134, 4318288544, 4329996612,
    4341736423, 4353508065, 4365311623, 4377147183,
    4389014833, 4400914660, 4412846750, 4424811191,
    4436808071, 4448837478, 4460899500, 4472994226,
    4485121744, 4497282142, 4509475511, 4521701940,
    4533961517, 4546254334, 4558580480, 4570940045,
    4583333121, 4595759798, 4608220167, 4620714319,
    4633242347, 4645804341, 4658400394, 4671030599,
    4683695048, 4696393833, 4709127049, 4721894787,
    4734697143, 4747534209, 4760406080, 4773312851,
    4786254615, 4799231467, 4812243504, 4825290820,
    4838373510, 4851491672, 4864645400, 4877834792,
    4891059943, 4904320952, 4917617915, 4930950930,
    4944320094, 4957725506, 4971167263, 4984645465,
    4998160210, 5011711597, 5025299726, 5038924695,
    5052586606, 5066285558, 5080021652, 5093794988,
    5107605667, 5121453791, 5135339461, 5149262779,
    5163223846, 5177222766, 5191259641, 5205334574,
    5219447668, 5233599026, 5247788752, 5262016951,
    5276283726, 5290589183, 5304933425, 5319316559,
    5333738689, 5348199922, 5362700363, 5377240118,
    5391819295, 5406438001, 5421096341, 5435794424,
    5450532358, 5465310250, 5480128210, 5494986345,
    5509884764, 5524823577, 5539802893, 5554822823,
    5569883475, 5584984961, 5600127392, 5615310878,
    5630535530, 5645801460, 5661108781, 5676457604,
    5691848042, 5707280207, 5722754214, 5738270175,
    5753828203, 5769428414, 5785070921, 5800755840,
    5816483285, 5832253371, 5848066214, 5863921930,
    5879820635, 5895762446, 5911747479, 5927775853,
    5943847684, 5959963090, 5976122189, 5992325100,
    6008571941, 6024862833, 6041197893, 6057577242,
    6074001000, 6090469287, 6106982225, 6123539933,
    6140142534, 6156790150, 6173482901, 6190220911,
    6207004303, 6223833199, 6240707722, 6257627997,
    6274594148, 6291606299, 6308664574, 6325769099,
    6342919999, 6360117399, 6377361427, 6394652208,
    6411989869, 6429374537, 6446806340, 6464285405,
    6481811861, 6499385836, 6517007458, 6534676858,
    6552394164, 6570159507, 6587973017, 6605834824,
    6623745059, 6641703853, 6659711339, 6677767649,
    6695872913, 6714027267, 6732230841, 6750483771,
    6768786189, 6787138230, 6805540029, 6823991719,
    6842493438, 6861045320, 6879647501, 6898300117,
    6917003306, 6935757205, 6954561950, 6973417680,
    6992324534, 7011282649, 7030292165, 7049353220,
    7068465956, 7087630511, 7106847027, 7126115644,
    7145436504, 7164809747, 7184235517, 7203713956,
    7223245206, 7242829410, 7262466713, 7282157258,
    7301901189, 7321698651, 7341549790, 7361454751,
    7381413680, 7401426722, 7421494026, 7441615738,
    7461792005, 7482022975, 7502308797, 7522649620,
    7543045592, 7563496864, 7584003584, 7604565904,
    7625183973, 7645857945, 7666587968, 7687374197,
    7708216783, 7729115879, 7750071638, 7771084214,
    7792153760, 7813280433, 7834464385, 7855705773,
    7877004752, 7898361478, 7919776109, 7941248800,
    7962779710, 7984368996, 8006016816, 8027723330,
    8049488696, 8071313074, 8093196623, 8115139505,
    8137141881, 8159203910, 8181325756, 8203507581,
    8225749546, 8248051816, 8270414553, 8292837922,
    8315322086, 8337867211, 8360473463, 8383141006,
    8405870007, 8428660633, 8451513050, 8474427426,
    8497403930, 8520442729, 8543543993, 8566707891,
    8589934592,
];

/// `log2(x) × 1e9` for a raw `u64`.
///
/// `x == 0` returns [`LOG2_UNDEFINED`]; every other input is exact to
/// the interpolation bound in the module docs.
#[inline]
pub fn log2_1e9(x: u64) -> i64 {
    if x == 0 {
        return LOG2_UNDEFINED;
    }
    // e = floor(log2 x); shifting the leading 1 to bit 63 normalizes
    // the mantissa without a division.
    let e = 63 - x.leading_zeros() as i64;
    let norm = x << x.leading_zeros(); // [2^63, 2^64)
    // Drop the implicit leading 1 and keep 32 fractional bits.
    let frac_q32 = (norm >> 31) - (1u64 << 32); // [0, 2^32)
    let idx = (frac_q32 >> 24) as usize; // 0..=255
    let rem = frac_q32 & 0x00FF_FFFF; // [0, 2^24)
    let lo = LOG2_TAB[idx];
    let hi = LOG2_TAB[idx + 1];
    let interp = lo + (((hi - lo) as i128 * rem as i128) >> 24) as i64;
    e * 1_000_000_000 + interp
}

/// `2^(v_1e9 / 1e9)` as a raw `u64`, saturating at [`u64::MAX`].
///
/// A negative exponent whose result would be below 1 returns `0` — the
/// engine never asks for one (every value it exponentiates is a scaled
/// vol, far above 1), and returning 0 keeps the function total.
#[inline]
pub fn exp2_1e9(v_1e9: i64) -> u64 {
    if v_1e9 < 0 {
        return 0;
    }
    let e = v_1e9 / 1_000_000_000;
    if e >= 64 {
        return u64::MAX;
    }
    let f = v_1e9 - e * 1_000_000_000; // [0, 1e9)
    // 256 intervals over [0, 1): idx = floor(f × 256 / 1e9), and the
    // remainder is carried exactly rather than re-derived.
    let scaled = f as i128 * 256;
    let idx = (scaled / 1_000_000_000) as usize; // 0..=255
    let rem = scaled - idx as i128 * 1_000_000_000; // [0, 1e9)
    let lo = EXP2_TAB[idx];
    let hi = EXP2_TAB[idx + 1];
    let mant_q32 = lo as i128 + ((hi - lo) as i128 * rem) / 1_000_000_000; // [2^32, 2^33]
    let out = (mant_q32 as u128) << e as u32;
    let out = out >> 32;
    if out > u64::MAX as u128 {
        u64::MAX
    } else {
        out as u64
    }
}

/// `ln(x) × 1e9` for a raw `u64`; `x == 0` gives [`LOG2_UNDEFINED`].
#[inline]
pub fn ln_1e9(x: u64) -> i64 {
    let l2 = log2_1e9(x);
    if l2 == LOG2_UNDEFINED {
        return LOG2_UNDEFINED;
    }
    ((l2 as i128 * LN2_1E9 as i128) / 1_000_000_000) as i64
}

/// `e^(v_1e9 / 1e9)` as a raw `u64`, saturating at [`u64::MAX`].
#[inline]
pub fn exp_1e9(v_1e9: i64) -> u64 {
    if v_1e9 < 0 {
        return 0;
    }
    // v / ln2, in i128 so the ×1e9 never overflows.
    let l2 = (v_1e9 as i128 * 1_000_000_000) / LN2_1E9 as i128;
    if l2 > i64::MAX as i128 {
        return u64::MAX;
    }
    exp2_1e9(l2 as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Test-only floats: the point of these tests is to prove the
    // integer path against the reference the integer path replaced.

    #[test]
    fn log2_table_is_the_rounded_truth() {
        let mut i = 0usize;
        while i <= 256 {
            let want = ((1.0f64 + i as f64 / 256.0).log2() * 1e9).round() as i64;
            assert_eq!(LOG2_TAB[i], want, "LOG2_TAB[{i}] hand-edited");
            i += 1;
        }
        assert_eq!(LOG2_TAB[0], 0);
        assert_eq!(LOG2_TAB[256], 1_000_000_000);
    }

    #[test]
    fn exp2_table_is_the_rounded_truth() {
        let mut i = 0usize;
        while i <= 256 {
            let want = (2.0f64.powf(i as f64 / 256.0) * (1u64 << 32) as f64).round() as u64;
            assert_eq!(EXP2_TAB[i], want, "EXP2_TAB[{i}] hand-edited");
            i += 1;
        }
        assert_eq!(EXP2_TAB[0], 1u64 << 32);
        assert_eq!(EXP2_TAB[256], 1u64 << 33);
    }

    #[test]
    fn constants_are_the_rounded_truth() {
        assert_eq!(LN2_1E9, (2.0f64.ln() * 1e9).round() as i64);
        assert_eq!(LOG2_1E9_1E9, (1e9f64.log2() * 1e9).round() as i64);
    }

    #[test]
    fn log2_is_exact_on_powers_of_two() {
        let mut e = 0u32;
        while e < 64 {
            assert_eq!(log2_1e9(1u64 << e), e as i64 * 1_000_000_000, "2^{e}");
            e += 1;
        }
        assert_eq!(log2_1e9(0), LOG2_UNDEFINED);
        assert_eq!(ln_1e9(0), LOG2_UNDEFINED);
    }

    #[test]
    fn exp2_is_exact_on_whole_exponents() {
        let mut e = 0i64;
        while e < 64 {
            assert_eq!(exp2_1e9(e * 1_000_000_000), 1u64 << e as u32, "2^{e}");
            e += 1;
        }
        assert_eq!(exp2_1e9(64 * 1_000_000_000), u64::MAX, "saturates");
        assert_eq!(exp2_1e9(-1), 0);
        assert_eq!(exp_1e9(-1), 0);
    }

    /// The build card's acceptance: relative error ≤ 1e-4 across the
    /// working domain — realised vol from 1 % to 500 % over the hold,
    /// carried as bps ×1e9 (`frac × 1e13`), which is every value the
    /// HAR pipeline can hand to `ln`.
    #[test]
    fn fx_accuracy_holds_across_the_working_domain() {
        let mut worst_ln = 0.0f64;
        let mut worst_rt = 0.0f64;
        // 1 % .. 500 % of the underlying, 1e13 per unit of fraction.
        let mut pct = 1u64;
        while pct <= 500 {
            // A few points inside each percent, so the sweep lands off
            // the table knots as well as on them.
            let mut k = 0u64;
            while k < 7 {
                let v = pct * 100_000_000_000 + k * 13_717_421_000;
                let got = ln_1e9(v) as f64 / 1e9;
                let want = (v as f64).ln();
                let err = (got - want).abs() / want.abs();
                if err > worst_ln {
                    worst_ln = err;
                }
                // Round trip: exp(ln(v)) must return v itself.
                let back = exp_1e9(ln_1e9(v)) as f64;
                let rt = (back - v as f64).abs() / v as f64;
                if rt > worst_rt {
                    worst_rt = rt;
                }
                k += 1;
            }
            pct += 1;
        }
        assert!(worst_ln <= 1e-4, "ln rel err {worst_ln:e} > 1e-4");
        assert!(worst_rt <= 1e-4, "round-trip rel err {worst_rt:e} > 1e-4");
        // And it is not merely inside the bound — it is three orders
        // inside it. If this ever tightens to a fail, the table or the
        // interpolation changed, not the bound.
        assert!(worst_rt <= 1e-5, "round trip should be ~2e-6: {worst_rt:e}");
    }

    /// The scaling identity the engine leans on: `+LOG2_1E9_1E9` in the
    /// exponent domain is `×1e9` in the value domain.
    #[test]
    fn adding_log2_of_1e9_scales_by_1e9() {
        // ln(v) for v = 2.5, carried at ×1e9, then exp back at ×1e9
        // scale — v × 1e9 must still fit a u64, which is the identity's
        // only limit (see LOG2_1E9_1E9).
        let v: u64 = 2_500_000_000;
        let lv = ln_1e9(v);
        let scaled = exp2_1e9((lv as i128 * 1_000_000_000 / LN2_1E9 as i128) as i64 + LOG2_1E9_1E9);
        let want = v as u128 * 1_000_000_000;
        let err = (scaled as i128 - want as i128).unsigned_abs();
        assert!(
            err * 1_000_000 < want,
            "scaled {scaled} vs {want} (rel > 1e-6)"
        );
    }
}
