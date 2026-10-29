// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The durable outbox: every message is a file in `<state_dir>/outbox`
//! before anything tries to send it, so a Telegram outage, a sleeping Mac
//! or a restart of the notifier loses nothing. The poll loop writes
//! (temporary file, then rename); the sender thread reads oldest first and
//! removes a file once Telegram has taken it. Delivery is at-least-once.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::clock;
use crate::model::Msg;

/// Beyond this many pending files, the oldest silent ones are dropped
/// (counted in the log); loud ones are never dropped.
pub const OUTBOX_CAP: usize = 5000;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A pending message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// Its file.
    pub path: PathBuf,
    /// Ring or silent.
    pub loud: bool,
    /// Text.
    pub text: String,
}

/// The outbox directory.
#[derive(Debug, Clone)]
pub struct Outbox {
    dir: PathBuf,
}

impl Outbox {
    /// `<state_dir>/outbox`, created.
    ///
    /// # Errors
    /// The directory cannot be created.
    pub fn open(state_dir: &Path) -> std::io::Result<Outbox> {
        let dir = state_dir.join("outbox");
        std::fs::create_dir_all(&dir)?;
        Ok(Outbox { dir })
    }

    /// The directory.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Queue a message.
    ///
    /// # Errors
    /// I/O.
    pub fn push(&self, m: &Msg) -> std::io::Result<()> {
        let n = SEQ.fetch_add(1, Ordering::Relaxed) % 10_000;
        let name = format!(
            "{:020}-{:04}-{}.txt",
            clock::now_wall_ns(),
            n,
            if m.loud { 'l' } else { 's' }
        );
        let tmp = self.dir.join(format!(".{name}.tmp"));
        std::fs::write(&tmp, crate::clip(&m.text, crate::MAX_TEXT_BYTES))?;
        std::fs::rename(&tmp, self.dir.join(name))
    }

    /// Pending messages, oldest first.
    ///
    /// # Errors
    /// The directory cannot be listed.
    pub fn pending(&self) -> std::io::Result<Vec<Pending>> {
        let mut names: Vec<String> = std::fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".txt") && !n.starts_with('.'))
            .collect();
        names.sort();
        let mut out = Vec::with_capacity(names.len());
        for n in names {
            let path = self.dir.join(&n);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            out.push(Pending {
                loud: n.ends_with("-l.txt"),
                path,
                text,
            });
        }
        Ok(out)
    }

    /// Drop the oldest silent messages beyond [`OUTBOX_CAP`]; returns how
    /// many were dropped.
    ///
    /// # Errors
    /// I/O.
    pub fn enforce_cap(&self) -> std::io::Result<usize> {
        let p = self.pending()?;
        if p.len() <= OUTBOX_CAP {
            return Ok(0);
        }
        let mut excess = p.len() - OUTBOX_CAP;
        let mut dropped = 0;
        for m in p.iter().filter(|m| !m.loud) {
            if excess == 0 {
                break;
            }
            if std::fs::remove_file(&m.path).is_ok() {
                dropped += 1;
                excess -= 1;
            }
        }
        Ok(dropped)
    }
}

/// The next send: one message, or — when the backlog is long — a run of
/// consecutive silent ones joined up to the text budget. A loud message
/// always goes alone, so it keeps its ring.
#[must_use]
pub fn next_batch(
    pending: &[Pending],
    backlog_from: usize,
) -> Option<(Vec<PathBuf>, bool, String)> {
    let first = pending.first()?;
    if first.loud || pending.len() <= backlog_from {
        return Some((vec![first.path.clone()], first.loud, first.text.clone()));
    }
    let mut paths = Vec::new();
    let mut text = String::new();
    for m in pending {
        if m.loud {
            break;
        }
        let add = if text.is_empty() {
            m.text.len()
        } else {
            m.text.len() + 2
        };
        if !text.is_empty() && text.len() + add > crate::MAX_TEXT_BYTES {
            break;
        }
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&m.text);
        paths.push(m.path.clone());
    }
    Some((paths, false, crate::clip(&text, crate::MAX_TEXT_BYTES)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notify-out-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn queues_in_order_and_batches_a_silent_backlog() {
        let o = Outbox::open(&dir("q")).unwrap();
        for i in 0..5 {
            o.push(&Msg {
                loud: i == 3,
                text: format!("m{i}"),
            })
            .unwrap();
        }
        let p = o.pending().unwrap();
        assert_eq!(
            p.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["m0", "m1", "m2", "m3", "m4"]
        );
        assert!(p[3].loud && !p[0].loud);
        // Short queue: one at a time.
        let (paths, loud, text) = next_batch(&p, 10).unwrap();
        assert_eq!((paths.len(), loud, text.as_str()), (1, false, "m0"));
        // Backlog: silent ones join, stopping at the loud one.
        let (paths, loud, text) = next_batch(&p, 2).unwrap();
        assert_eq!((paths.len(), loud), (3, false));
        assert_eq!(text, "m0\n\nm1\n\nm2");
        // A loud head goes alone.
        let (paths, loud, _) = next_batch(&p[3..], 0).unwrap();
        assert_eq!((paths.len(), loud), (1, true));
        assert!(next_batch(&[], 0).is_none());
    }

    #[test]
    fn the_cap_drops_only_silent_messages() {
        let o = Outbox::open(&dir("cap")).unwrap();
        assert_eq!(o.enforce_cap().unwrap(), 0);
        let long = "x".repeat(10_000);
        o.push(&Msg {
            loud: true,
            text: long,
        })
        .unwrap();
        let p = o.pending().unwrap();
        assert!(
            p[0].text.len() <= crate::MAX_TEXT_BYTES + 8,
            "clipped on the way in"
        );
    }
}
