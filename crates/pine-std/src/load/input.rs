// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Manifest input overrides (impl-04 §3.7, §4.8; contract §3.8, §3.14).

use super::{code, err, session_parse, tf_seconds, LoadErr};
use pine_ir::{color, input_flag, scale, InputDesc, InputKind, InputValue};

/// What an `input.*` site resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputResolved<'a> {
    /// No manifest override: the declared default stands.
    Default,
    /// The override, in `InputDesc::default_bits` encoding: `int`/`enum` ordinal/`time` ms as
    /// i64, `float`/`price` as f64 bits, `bool` 0/1, `color` as the `pine_ir::color` packing.
    Bits(u64),
    /// A string-kind override (string, text_area, symbol, source, timeframe, session),
    /// validated; the caller interns it.
    Str(&'a [u8]),
}

const TYPE_ERR: LoadErr = err(
    code::INPUT_TYPE,
    0,
    "override variant does not match the input kind",
);
const RANGE_ERR: LoadErr = err(
    code::INPUT_RANGE,
    0,
    "override outside minval/maxval/options",
);

fn in_options(options: &[InputValue], v: &InputValue) -> bool {
    if options.is_empty() {
        return true;
    }
    let mut i = 0usize;
    while i < options.len() {
        if options[i] == *v {
            return true;
        }
        i += 1;
    }
    false
}

/// Validate one manifest override (contract §3.14 `[script.inputs]`) against the call site.
///
/// Exactly one `InputValue` variant per kind: `Int` for `int`, `color`, `time`, `enum`;
/// `Float1e6` for `float` and `price`; `Bool` for `bool`; `Str` for the string kinds. Anything
/// else is 5007 — an `Int` for a float input included (impl-08 v4 §10.3 N3). A timeframe must
/// pass `tf_seconds`, a session `session_parse`; 5006 on `minval`/`maxval`/`options`/colour-range
/// violations. `step` is not enforced.
pub fn resolve_input<'a>(
    desc: &InputDesc,
    options: &[InputValue],
    ov: Option<&'a InputValue>,
) -> Result<InputResolved<'a>, LoadErr> {
    let Some(v) = ov else {
        return Ok(InputResolved::Default);
    };
    let has_min = desc.flags & input_flag::HAS_MIN != 0;
    let has_max = desc.flags & input_flag::HAS_MAX != 0;
    match desc.kind {
        InputKind::Int | InputKind::Time | InputKind::Enum => {
            let InputValue::Int(x) = *v else {
                return Err(TYPE_ERR);
            };
            if (has_min && x < desc.min_bits as i64) || (has_max && x > desc.max_bits as i64) {
                return Err(RANGE_ERR);
            }
            if !in_options(options, v) {
                return Err(RANGE_ERR);
            }
            Ok(InputResolved::Bits(x as u64))
        }
        InputKind::Float | InputKind::Price => {
            let InputValue::Float1e6(x) = *v else {
                return Err(TYPE_ERR);
            };
            let f = scale::from_1e6(x);
            if (has_min && f < f64::from_bits(desc.min_bits))
                || (has_max && f > f64::from_bits(desc.max_bits))
            {
                return Err(RANGE_ERR);
            }
            if !in_options(options, v) {
                return Err(RANGE_ERR);
            }
            Ok(InputResolved::Bits(f.to_bits()))
        }
        InputKind::Bool => {
            let InputValue::Bool(b) = *v else {
                return Err(TYPE_ERR);
            };
            Ok(InputResolved::Bits(b as u64))
        }
        InputKind::Color => {
            let InputValue::Int(c) = *v else {
                return Err(TYPE_ERR);
            };
            if !(0..1i64 << 31).contains(&c) || color::t(c) > 100 {
                return Err(RANGE_ERR);
            }
            if !in_options(options, v) {
                return Err(RANGE_ERR);
            }
            Ok(InputResolved::Bits(c as u64))
        }
        InputKind::String
        | InputKind::TextArea
        | InputKind::Symbol
        | InputKind::Source
        | InputKind::Timeframe
        | InputKind::Session => {
            let InputValue::Str(s) = v else {
                return Err(TYPE_ERR);
            };
            let b = s.as_bytes();
            if desc.kind == InputKind::Timeframe {
                tf_seconds(b)?;
            }
            if desc.kind == InputKind::Session {
                session_parse(b)?;
            }
            if !in_options(options, v) {
                return Err(RANGE_ERR);
            }
            Ok(InputResolved::Str(b))
        }
    }
}
