// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! COPY-DOCTRINE: offline corpus tool, cold — see the crate root.
//!
//! The corpus `DiagCode`s 7101–7105 (CAR-09-2, impl-09 §3.7; 7105's details amended by
//! AMEND-harvest 2026-10-29: `<class>` for the script's own licence class, `lib:<class>` for a
//! library's). Under the manual intake 7104 is never raised (every record is `parsed`); the
//! code stays, so a future `acq` that can carry other statuses keeps its bucket.

use pine_ir::DiagCode;

/// A referenced built-in is outside the active tiers, or declined by pine-spec.
pub const BUILTIN_UNAVAILABLE: DiagCode = DiagCode(7101);
/// A name that no pine-spec entry covers in the script's dialect.
pub const BUILTIN_UNKNOWN: DiagCode = DiagCode(7102);
/// An import with no cached library and a permanent outcome, or a cycle, or beyond the depth bound.
pub const IMPORT_UNRESOLVED: DiagCode = DiagCode(7103);
/// No source: the record's status is not `parsed`.
pub const SOURCE_UNAVAILABLE: DiagCode = DiagCode(7104);
/// The licence class of the script or of a transitive library is not admitted (OH-6).
pub const LICENCE_RESTRICTED: DiagCode = DiagCode(7105);
/// Every corpus code, ascending.
pub const ALL: [DiagCode; 5] = [
    BUILTIN_UNAVAILABLE,
    BUILTIN_UNKNOWN,
    IMPORT_UNRESOLVED,
    SOURCE_UNAVAILABLE,
    LICENCE_RESTRICTED,
];
/// The corpus's sub-range of the harness range (CAR-09-2).
pub const RANGE: core::ops::RangeInclusive<u16> = 7100..=7199;
