// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The terminal's `/state` reader (TERM plan §4.6): a side thread polls the
//! engine's `/state` every 2 s with `notify`'s reader; the server turns the
//! latest view into the UI's `EngineStatus` (PAPER/LIVE per slot is read from
//! `/state`, never from config) and into the door's boot epoch
//! (`boot.boot_wall_ns`, T-L11).

use std::sync::mpsc;
use std::time::Duration;

use notify::config::StateUrl;
use notify::state::StateView;
use term_proto::exec::{EngineStatus, SlotMode, SlotState};
use term_proto::Ascii;

pub const POLL: Duration = Duration::from_secs(2);

pub struct StatePoller {
    rx: mpsc::Receiver<Option<StateView>>,
    /// The latest view; `None` while the engine does not answer.
    pub last: Option<StateView>,
}

impl StatePoller {
    pub fn spawn(url: StateUrl) -> StatePoller {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("term-state".into())
            .spawn(move || loop {
                let v = notify::state::fetch(&url, Duration::from_secs(1)).ok();
                if tx.send(v).is_err() {
                    return;
                }
                std::thread::sleep(POLL);
            })
            .ok();
        StatePoller { rx, last: None }
    }

    /// Drain the poller; true when the engine's status changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(v) = self.rx.try_recv() {
            let before = status_key(self.last.as_ref());
            self.last = v;
            changed |= status_key(self.last.as_ref()) != before;
        }
        changed
    }
}

/// What the UI can see change: up/down, boot, mode, arming, halts.
fn status_key(v: Option<&StateView>) -> Option<(u64, bool, u64, Vec<u8>, bool)> {
    v.map(|v| (v.boot_wall_ns, v.paper, v.configured_mask, v.exec.arm_slots.clone(), v.exec.halt_file_present))
}

/// `/state` → the UI's engine status. An unanswered `/state` is "engine
/// down", and the UI shows order entry off.
pub fn engine_status(v: Option<&StateView>) -> EngineStatus {
    let mut slots = [SlotState::default(); 16];
    let Some(v) = v else {
        return EngineStatus { boot_epoch: 0, git_sha: Ascii::new(b"0").unwrap_or_else(|_| unreachable!()), engine_up: false, n: 0, slots };
    };
    let sha: Vec<u8> = v.git_sha.bytes().filter(u8::is_ascii_hexdigit).take(12).collect();
    let sha = if sha.is_empty() { b"0".to_vec() } else { sha };
    let mut n = 0usize;
    for slot in 0..16u8 {
        if v.configured_mask & (1 << slot) == 0 {
            continue;
        }
        let mode = if v.slot_live(slot) { SlotMode::Live } else { SlotMode::Paper };
        slots[n] = SlotState { slot, mode, halted: v.exec.halt_file_present };
        n += 1;
    }
    EngineStatus { boot_epoch: v.boot_wall_ns, git_sha: Ascii::new(&sha).unwrap_or_else(|_| unreachable!()), engine_up: true, n: n as u8, slots }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_and_up() {
        let s = engine_status(None);
        assert!(!s.engine_up);
        let v = StateView { boot_wall_ns: 42, git_sha: "6bf205d6e410".into(), paper: true, configured_mask: (1 << 3) | (1 << 12), ..StateView::default() };
        let s = engine_status(Some(&v));
        assert!(s.engine_up);
        assert_eq!((s.boot_epoch, s.n), (42, 2));
        assert_eq!((s.slots[1].slot, s.slots[1].mode), (12, SlotMode::Paper));
        let mut b = [0u8; 256];
        let k = s.encode(0, 0, &mut b).unwrap();
        assert!(term_proto::decode(&b[..k]).is_ok());
    }
}
