// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `runtime.error(message)` — the one body that stops a script on purpose. Stateless, no
//! allocation: the message is not read (a fault carries only its kind, contract §3.6).
//!
//! pine-sema also calls it as the run-time check of a `for … by s` whose step does not fold
//! (CI-004 / F085, `pin:02-for-step` `RuntimeFault`): TradingView's "Error on bar 0: 'step' in
//! loop must be greater than zero." is a plain runtime error, so the fault kind is
//! `ScriptRefused` (the r27 lead answers, broker item 2 / item 14).

use pine_ir::{FaultKind, SiteId};
use pine_vm::Frame;

/// `runtime.error(message)`: always `Err(ScriptRefused)`; the window is the message slot only.
pub fn bi_runtime_error(_: &mut Frame<'_>, _: SiteId, args: &[u16]) -> Result<(), FaultKind> {
    if args.len() != 1 {
        return Err(FaultKind::Internal);
    }
    Err(FaultKind::ScriptRefused)
}
