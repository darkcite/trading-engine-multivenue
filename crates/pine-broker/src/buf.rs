// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `EngineIntentBuf` — the fixed-capacity answer of one `reconcile` / `flatten` call (impl-05 §3.3,
//! CAR-05-3).

use crate::fault::{BrokerFault, ScriptRefusal};
use pine_ir::{EngineIntent, EngineVerb, Primitive, StrKey};

/// A zeroed instruction, usable in `const` context (`EngineIntent::default` is not `const`).
const ZERO_INTENT: EngineIntent = EngineIntent {
    verb: EngineVerb::Submit,
    primitive: Primitive::TakerIoc,
    side: 0,
    flags: 0,
    client_oid: 0,
    prev_client_oid: 0,
    _pad0: 0,
    px_1e6: 0,
    qty_1e6: 0,
    ttl_ns: 0,
    id: StrKey::NONE,
    _pad1: [0; 2],
};

/// The instructions one `reconcile` / `flatten` call produced. Fixed capacity; 2 112 B.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
pub struct EngineIntentBuf {
    items: [EngineIntent; EngineIntentBuf::CAP],
    n: u32,
}

impl EngineIntentBuf {
    /// Capacity. `reconcile` emits at most one `Cancel` per resting maker, one IoC and one
    /// `Submit` or `Modify` per maker slot — at most 15 — so 32 is never reached (§4.12.6).
    pub const CAP: usize = 32;

    /// An empty buffer. The host owns one per script, built once at boot.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: [ZERO_INTENT; Self::CAP],
            n: 0,
        }
    }

    /// The instructions, in emission order: every `Cancel` first, then `Modify`, then `Submit`.
    #[must_use]
    pub fn as_slice(&self) -> &[EngineIntent] {
        &self.items[..self.len()]
    }

    /// Drop every instruction. `reconcile` and `flatten` call it on entry.
    pub fn clear(&mut self) {
        self.n = 0;
    }

    /// Number of instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        // `n ≤ CAP = 32`: the widening is lossless on every target.
        self.n as usize
    }

    /// `len() == 0`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    /// Append `e` (written in place); `TooManyIntents` at `CAP` — unreachable by construction
    /// (§4.12.6), a defence.
    pub(crate) fn push(&mut self, e: EngineIntent) -> Result<(), BrokerFault> {
        let i = self.len();
        if i >= Self::CAP {
            return Err(BrokerFault::Script(ScriptRefusal::TooManyIntents));
        }
        self.items[i] = e;
        self.n += 1;
        Ok(())
    }
}

impl Default for EngineIntentBuf {
    fn default() -> Self {
        Self::new()
    }
}

const _: () = assert!(core::mem::size_of::<EngineIntentBuf>() == 32 * 64 + 64);
const _: () = assert!(core::mem::align_of::<EngineIntentBuf>() == 64);

#[cfg(test)]
mod tests {
    use super::EngineIntentBuf;

    #[test]
    fn engine_intent_buf_is_2112_bytes() {
        assert_eq!(core::mem::size_of::<EngineIntentBuf>(), 2112);
        assert_eq!(core::mem::align_of::<EngineIntentBuf>(), 64);
    }

    #[test]
    fn new_is_empty_and_clear_empties() {
        let mut b = EngineIntentBuf::default();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert!(b.as_slice().is_empty());
        b.clear();
        assert!(b.is_empty());
    }
}
