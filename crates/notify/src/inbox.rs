// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The AI-session inbox. Any process on the Mac — a Claude Code session,
//! the RustRover terminal, the AIRES controller, a worker job — or a Cowork
//! session's Linux shell (which sees only the repo folder) drops a `*.msg`
//! file into an inbox directory; the notifier that owns the directory
//! relays it and removes it. Only the running notifier holds the bot token.
//!
//! A message file is plain text. Optional header lines `level=loud` /
//! `level=silent` and `source=<name>` may come first, followed by a blank
//! line; without them the whole file is the body. Writers create
//! `.<name>.tmp` and rename it to `<name>.msg`, so a half-written file is
//! never read. Symlinks are refused; bodies are clipped to the text budget.

use std::path::{Path, PathBuf};

use crate::clock;

/// The most read of one file.
pub const MAX_FILE_BYTES: u64 = 64 << 10;
/// Files taken from one directory per poll.
pub const MAX_FILES_PER_POLL: usize = 50;

/// A parsed inbox message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxMsg {
    /// `level=loud`.
    pub loud: bool,
    /// `source=…`, sanitized.
    pub source: Option<String>,
    /// The text.
    pub body: String,
}

fn sanitize(src: &str) -> Option<String> {
    let s: String = src
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(24)
        .collect();
    (!s.is_empty()).then_some(s)
}

/// Parse a message file's text.
#[must_use]
pub fn parse_message(text: &str) -> InboxMsg {
    let mut loud = false;
    let mut source = None;
    if let Some((head, rest)) = text.split_once("\n\n") {
        let lines: Vec<&str> = head.lines().collect();
        let all_headers = !lines.is_empty()
            && lines.iter().all(|l| {
                let l = l.trim();
                l.starts_with("level=") || l.starts_with("source=")
            });
        if all_headers {
            for l in lines {
                let l = l.trim();
                if let Some(v) = l.strip_prefix("level=") {
                    loud = v.trim().eq_ignore_ascii_case("loud");
                } else if let Some(v) = l.strip_prefix("source=") {
                    source = sanitize(v.trim());
                }
            }
            return InboxMsg {
                loud,
                source,
                body: rest.trim_end().to_string(),
            };
        }
    }
    InboxMsg {
        loud,
        source,
        body: text.trim_end().to_string(),
    }
}

/// The relayed text.
#[must_use]
pub fn relay_text(m: &InboxMsg) -> String {
    match &m.source {
        Some(s) => format!("[AI:{s}] {}", m.body),
        None => format!("[AI] {}", m.body),
    }
}

/// Write a message atomically into `dir` (created 0700 if missing).
///
/// # Errors
/// I/O.
pub fn write_message(
    dir: &Path,
    loud: bool,
    source: Option<&str>,
    body: &str,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let name = format!("{:020}-{}.msg", clock::now_wall_ns(), std::process::id());
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut text = String::new();
    text.push_str(if loud {
        "level=loud\n"
    } else {
        "level=silent\n"
    });
    if let Some(s) = source.and_then(sanitize) {
        text.push_str(&format!("source={s}\n"));
    }
    text.push('\n');
    text.push_str(body);
    text.push('\n');
    std::fs::write(&tmp, text)?;
    let path = dir.join(name);
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Take the waiting messages of `dirs` (oldest name first). Each returned
/// path is the caller's to remove once the message is safely queued.
/// Unreadable or oversized entries are returned with `None`, to be removed.
#[must_use]
pub fn collect(dirs: &[PathBuf]) -> Vec<(PathBuf, Option<InboxMsg>)> {
    let mut out = Vec::new();
    for d in dirs {
        let Ok(rd) = std::fs::read_dir(d) else {
            continue;
        };
        let mut names: Vec<String> = rd
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".msg") && !n.starts_with('.'))
            .collect();
        names.sort();
        for n in names.into_iter().take(MAX_FILES_PER_POLL) {
            let p = d.join(&n);
            let Ok(md) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            if !md.file_type().is_file() || md.len() > MAX_FILE_BYTES {
                out.push((p, None));
                continue;
            }
            let msg = std::fs::read(&p)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
                .map(|t| parse_message(&t));
            let msg = msg.filter(|m| !m.body.trim().is_empty());
            out.push((p, msg));
        }
    }
    out
}

/// The hourly acceptance gate (`inbox_max_per_hour`).
#[derive(Debug, Clone, Default)]
pub struct Gate {
    cap: u32,
    hour: i64,
    accepted: u32,
    suppressed: u32,
}

impl Gate {
    /// A gate of `cap` per hour.
    #[must_use]
    pub fn new(cap: u32) -> Gate {
        Gate {
            cap,
            ..Gate::default()
        }
    }

    /// Should this message be relayed? The return's second value, when
    /// `Some`, is a count of messages suppressed in the hour that ended.
    pub fn admit(&mut self, now_s: i64) -> (bool, Option<u32>) {
        let h = now_s.div_euclid(3600);
        let mut ended = None;
        if h != self.hour {
            if self.suppressed > 0 {
                ended = Some(self.suppressed);
            }
            self.hour = h;
            self.accepted = 0;
            self.suppressed = 0;
        }
        if self.accepted < self.cap {
            self.accepted += 1;
            (true, ended)
        } else {
            self.suppressed += 1;
            (false, ended)
        }
    }

    /// Suppressed so far this hour.
    #[must_use]
    pub fn suppressed(&self) -> u32 {
        self.suppressed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notify-inbox-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn headers_are_optional() {
        let m = parse_message("level=loud\nsource=cowork session!\n\nBacktest done: +$12\n");
        assert_eq!(
            m,
            InboxMsg {
                loud: true,
                source: Some("coworksession".into()),
                body: "Backtest done: +$12".into()
            }
        );
        assert_eq!(relay_text(&m), "[AI:coworksession] Backtest done: +$12");
        let m = parse_message("just text\n\nwith a paragraph\n");
        assert_eq!(
            m,
            InboxMsg {
                loud: false,
                source: None,
                body: "just text\n\nwith a paragraph".into()
            }
        );
        assert_eq!(relay_text(&m), "[AI] just text\n\nwith a paragraph");
    }

    #[test]
    fn writes_atomically_and_collects_oldest_first() {
        let d = dir("rw");
        let a = write_message(&d, false, Some("aires"), "first").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        write_message(&d, true, None, "second").unwrap();
        std::fs::write(d.join(".half.msg.tmp"), "x").unwrap();
        std::fs::write(d.join("empty.msg"), "level=loud\n\n   \n").unwrap();
        let got = collect(&[d.clone(), d.join("absent")]);
        let bodies: Vec<Option<String>> = got
            .iter()
            .map(|(_, m)| m.as_ref().map(|m| m.body.clone()))
            .collect();
        assert_eq!(bodies, [Some("first".into()), Some("second".into()), None]);
        assert_eq!(got[0].0, a);
        assert!(got[1].1.as_ref().unwrap().loud);
    }

    #[test]
    fn the_gate_caps_per_hour_and_reports_the_suppressed() {
        let mut g = Gate::new(2);
        assert_eq!(g.admit(3600), (true, None));
        assert_eq!(g.admit(3601), (true, None));
        assert_eq!(g.admit(3602), (false, None));
        assert_eq!(g.admit(3603), (false, None));
        assert_eq!(g.suppressed(), 2);
        assert_eq!(g.admit(7200), (true, Some(2)));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused() {
        let d = dir("ln");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("target.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(d.join("target.txt"), d.join("evil.msg")).unwrap();
        let got = collect(&[d]);
        assert_eq!(got.len(), 1);
        assert!(got[0].1.is_none());
    }
}
