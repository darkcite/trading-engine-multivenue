// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Sending: `POST https://api.telegram.org/bot<token>/sendMessage` through
//! the house blocking HTTPS client (`core_net::boot_http`, rustls with the
//! compiled-in webpki roots), one message at a time, at most one a second
//! with a burst of three (Telegram's per-chat limit). The sender runs on
//! its own thread over the outbox, so a slow or dark Telegram never delays
//! a poll. Notify-only: `getUpdates` is never called.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_net::boot_http::{self, BootHttpErr};
use rustls::client::ClientConfig;

use crate::envfile::Telegram as Keys;
use crate::json::{self, Json};
use crate::outbox::{next_batch, Outbox};

/// Telegram's host.
pub const HOST: &str = "api.telegram.org";
/// One request's deadline.
pub const TIMEOUT: Duration = Duration::from_secs(10);
/// A backlog longer than this is sent in joined batches.
pub const BACKLOG_FROM: usize = 10;

/// What became of one send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Telegram took it.
    Sent,
    /// Transient: try again after this many seconds.
    Retry(u64),
    /// This message can never go (Telegram refused its content): drop it.
    Drop(String),
    /// The token is wrong: stop sending for this many seconds.
    Pause(u64, String),
    /// This one chat refuses the bot (403: it blocked the bot, or never
    /// started it): skip it for this message; the other chats still get it.
    ChatRefused(String),
}

/// A way to deliver text to one or more chats.
pub trait Transport: Send {
    /// How many chats each message goes to.
    fn chats(&self) -> usize {
        1
    }

    /// Deliver one message to chat `chat` (`0..chats()`).
    fn send(&mut self, chat: usize, text: &str, loud: bool) -> Outcome;
}

/// `dry_run = true`: the message goes to the log, nowhere else.
#[derive(Debug, Default)]
pub struct DryRun;

impl Transport for DryRun {
    fn send(&mut self, _chat: usize, text: &str, loud: bool) -> Outcome {
        crate::run::log(&format!(
            "DRY {} {}",
            if loud { "LOUD" } else { "silent" },
            text.replace('\n', " ⏎ ")
        ));
        Outcome::Sent
    }
}

/// The real thing.
pub struct Telegram {
    tls: Arc<ClientConfig>,
    keys: Keys,
    out: Vec<u8>,
}

impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Telegram")
            .field("keys", &self.keys)
            .finish_non_exhaustive()
    }
}

impl Telegram {
    /// A sender with these keys.
    #[must_use]
    pub fn new(keys: Keys) -> Telegram {
        Telegram {
            tls: core_net::transport::TlsTransport::default_client_config(),
            keys,
            out: Vec::with_capacity(8 << 10),
        }
    }
}

/// The `sendMessage` JSON body.
#[must_use]
pub fn request_body(chat_id: &str, text: &str, loud: bool) -> String {
    let mut b = String::with_capacity(text.len() + 96);
    b.push_str("{\"chat_id\":");
    let numeric = {
        let d = chat_id.strip_prefix('-').unwrap_or(chat_id);
        !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit())
    };
    if numeric {
        b.push_str(chat_id);
    } else {
        json::escape_into(chat_id, &mut b);
    }
    b.push_str(",\"text\":");
    json::escape_into(text, &mut b);
    b.push_str(",\"disable_web_page_preview\":true,\"disable_notification\":");
    b.push_str(if loud { "false" } else { "true" });
    b.push('}');
    b
}

/// Map a transport result to an [`Outcome`].
#[must_use]
pub fn classify(res: Result<Range<usize>, BootHttpErr>, out: &[u8]) -> Outcome {
    match res {
        Ok(r) => {
            let ok = Json::parse(&out[r]).ok().and_then(|j| j.get("ok").cloned())
                == Some(Json::Bool(true));
            if ok {
                Outcome::Sent
            } else {
                Outcome::Retry(30)
            }
        }
        Err(BootHttpErr::Status(429)) => Outcome::Retry(30),
        Err(BootHttpErr::Status(400)) => Outcome::Drop("Telegram refused the message (400)".into()),
        Err(BootHttpErr::Status(403)) => Outcome::ChatRefused(
            "Telegram answered 403: this chat blocked the bot or never started it".into(),
        ),
        Err(BootHttpErr::Status(s @ (401 | 404))) => Outcome::Pause(
            300,
            format!("Telegram answered {s}: check TELEGRAM_BOT_TOKEN"),
        ),
        Err(BootHttpErr::RequestTooLarge) => {
            Outcome::Drop("message too large for one request".into())
        }
        Err(_) => Outcome::Retry(10),
    }
}

impl Transport for Telegram {
    fn chats(&self) -> usize {
        self.keys.chat_ids.len()
    }

    fn send(&mut self, chat: usize, text: &str, loud: bool) -> Outcome {
        let Some(chat_id) = self.keys.chat_ids.get(chat) else {
            return Outcome::Drop(format!("no chat #{chat}"));
        };
        let body = request_body(chat_id, text, loud);
        let path = format!("/bot{}/sendMessage", self.keys.token.expose());
        let res = boot_http::https_post(
            &self.tls,
            HOST,
            443,
            &path,
            b"multivenue-notify",
            b"application/json",
            body.as_bytes(),
            &mut self.out,
            64 << 10,
            TIMEOUT,
        );
        // The path holds the token: overwrite it before it is dropped.
        let mut p = path.into_bytes();
        p.fill(0);
        classify(res, &self.out)
    }
}

/// One message a second, a burst of three.
#[derive(Debug)]
pub struct Bucket {
    tokens_ms: u64,
    last: Instant,
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket {
            tokens_ms: 3000,
            last: Instant::now(),
        }
    }
}

impl Bucket {
    /// Take a token if one is there.
    pub fn take(&mut self) -> bool {
        let now = Instant::now();
        let add = u64::try_from(now.duration_since(self.last).as_millis()).unwrap_or(u64::MAX);
        self.tokens_ms = (self.tokens_ms.saturating_add(add)).min(3000);
        self.last = now;
        if self.tokens_ms >= 1000 {
            self.tokens_ms -= 1000;
            true
        } else {
            false
        }
    }
}

/// How long an idle sender waits for a wake before it looks again.
pub const IDLE_WAIT: Duration = Duration::from_secs(5);

/// A batch on its way to several chats: pinned until every chat has it, so
/// a backlog growing meanwhile never changes what one chat got.
struct InFlight {
    paths: Vec<PathBuf>,
    loud: bool,
    text: String,
    done: Vec<bool>,
}

/// The sender loop: drain the outbox through `t`, every message to every
/// chat, until `stop` is set or the poll loop's end of `wake` is dropped.
/// It blocks on `wake` while idle instead of polling the outbox on a short
/// timer (a timer wakeup a few times a second measurably shifts the house's
/// ns-scale benches). A chat that refuses the bot (403) or a message
/// Telegram refuses (400) is skipped for that chat only; a network error or
/// a 429 backs everything off and resumes with the chats still owed.
pub fn run_sender(
    outbox: Outbox,
    mut t: Box<dyn Transport>,
    stop: Arc<AtomicBool>,
    wake: Receiver<()>,
) {
    let mut bucket = Bucket::default();
    let mut paused_until: Option<Instant> = None;
    let mut backoff = 0u64;
    let mut inflight: Option<InFlight> = None;
    let mut refused_logged: Vec<Option<Instant>> = vec![None; t.chats()];
    while !stop.load(Ordering::Relaxed) {
        if let Some(p) = paused_until {
            let now = Instant::now();
            if now < p {
                if let Err(RecvTimeoutError::Disconnected) =
                    wake.recv_timeout((p - now).min(IDLE_WAIT))
                {
                    return;
                }
                continue;
            }
            paused_until = None;
        }
        if inflight.is_none() {
            let pending = match outbox.pending() {
                Ok(p) => p,
                Err(e) => {
                    crate::run::log(&format!("outbox unreadable: {e}"));
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            };
            let Some((paths, loud, text)) = next_batch(&pending, BACKLOG_FROM) else {
                if let Err(RecvTimeoutError::Disconnected) = wake.recv_timeout(IDLE_WAIT) {
                    return;
                }
                continue;
            };
            inflight = Some(InFlight {
                paths,
                loud,
                text,
                done: vec![false; t.chats()],
            });
        }
        let Some(f) = inflight.as_mut() else {
            continue;
        };
        let Some(chat) = f.done.iter().position(|d| !d) else {
            remove_all(&f.paths);
            inflight = None;
            continue;
        };
        if !bucket.take() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        match t.send(chat, &f.text, f.loud) {
            Outcome::Sent => {
                backoff = 0;
                f.done[chat] = true;
            }
            Outcome::Drop(why) => {
                crate::run::log(&format!("chat #{chat}: dropped a message: {why}"));
                f.done[chat] = true;
            }
            Outcome::ChatRefused(why) => {
                // Logged at most once an hour per chat.
                let quiet = refused_logged
                    .get(chat)
                    .copied()
                    .flatten()
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(3600));
                if !quiet {
                    crate::run::log(&format!("chat #{chat} skipped: {why}"));
                    if let Some(slot) = refused_logged.get_mut(chat) {
                        *slot = Some(Instant::now());
                    }
                }
                f.done[chat] = true;
            }
            Outcome::Retry(s) => {
                backoff = if backoff == 0 {
                    s
                } else {
                    (backoff * 2).min(300)
                };
                crate::run::log(&format!(
                    "chat #{chat}: send failed, retrying in {backoff} s"
                ));
                paused_until = Some(Instant::now() + Duration::from_secs(backoff));
            }
            Outcome::Pause(s, why) => {
                crate::run::log(&format!("sending paused {s} s: {why}"));
                paused_until = Some(Instant::now() + Duration::from_secs(s));
            }
        }
    }
}

fn remove_all(paths: &[PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Msg;
    use std::sync::Mutex;

    #[test]
    fn the_body_is_json_and_numeric_chats_stay_numbers() {
        let b = request_body("-1001234", "line \"1\"\nline 2", true);
        let j = Json::parse(b.as_bytes()).unwrap();
        assert_eq!(j.get("chat_id"), Some(&Json::Num("-1001234".into())));
        assert_eq!(
            j.get("text").and_then(Json::as_str),
            Some("line \"1\"\nline 2")
        );
        assert_eq!(j.get("disable_notification"), Some(&Json::Bool(false)));
        let b = request_body("@chan", "x", false);
        let j = Json::parse(b.as_bytes()).unwrap();
        assert_eq!(j.get("chat_id").and_then(Json::as_str), Some("@chan"));
        assert_eq!(j.get("disable_notification"), Some(&Json::Bool(true)));
    }

    #[test]
    fn answers_classify() {
        let ok = br#"{"ok":true,"result":{}}"#;
        assert_eq!(classify(Ok(0..ok.len()), ok), Outcome::Sent);
        let no = br#"{"ok":false}"#;
        assert_eq!(classify(Ok(0..no.len()), no), Outcome::Retry(30));
        assert_eq!(
            classify(Err(BootHttpErr::Status(429)), b""),
            Outcome::Retry(30)
        );
        assert!(matches!(
            classify(Err(BootHttpErr::Status(400)), b""),
            Outcome::Drop(_)
        ));
        assert!(matches!(
            classify(Err(BootHttpErr::Status(403)), b""),
            Outcome::ChatRefused(_)
        ));
        assert!(matches!(
            classify(Err(BootHttpErr::Status(401)), b""),
            Outcome::Pause(300, _)
        ));
        assert!(matches!(
            classify(Err(BootHttpErr::Status(404)), b""),
            Outcome::Pause(300, _)
        ));
        assert_eq!(classify(Err(BootHttpErr::Timeout), b""), Outcome::Retry(10));
    }

    #[test]
    fn the_bucket_allows_a_burst_of_three() {
        let mut b = Bucket::default();
        assert!(b.take() && b.take() && b.take());
        assert!(!b.take());
    }

    /// Scripted chats: each send pops the chat's next outcome (`Sent` once
    /// the script is empty) and records `(chat, text, loud)` when it is Sent.
    struct Fake {
        got: Arc<Mutex<Vec<(usize, String, bool)>>>,
        script: Vec<Vec<Outcome>>,
    }
    impl Transport for Fake {
        fn chats(&self) -> usize {
            self.script.len()
        }
        fn send(&mut self, chat: usize, text: &str, loud: bool) -> Outcome {
            let o = if self.script[chat].is_empty() {
                Outcome::Sent
            } else {
                self.script[chat].remove(0)
            };
            if o == Outcome::Sent {
                self.got
                    .lock()
                    .unwrap()
                    .push((chat, text.to_string(), loud));
            }
            o
        }
    }

    fn drive(
        name: &str,
        script: Vec<Vec<Outcome>>,
        msgs: &[(&str, bool)],
        wait_ms: u64,
    ) -> (Vec<(usize, String, bool)>, usize) {
        let d = std::env::temp_dir().join(format!("notify-send-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let o = Outbox::open(&d).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (o2, s2) = (o.clone(), stop.clone());
        let fake = Fake {
            got: got.clone(),
            script,
        };
        let h = std::thread::spawn(move || run_sender(o2, Box::new(fake), s2, rx));
        // Idle first: it blocks on the wake, then drains what was queued.
        std::thread::sleep(Duration::from_millis(50));
        for (t, l) in msgs {
            o.push(&Msg {
                loud: *l,
                text: (*t).into(),
            })
            .unwrap();
        }
        tx.send(()).unwrap();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(wait_ms) && !o.pending().unwrap().is_empty() {
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        drop(tx); // unblocks the idle wait
        h.join().unwrap();
        let left = o.pending().unwrap().len();
        let v = got.lock().unwrap().clone();
        (v, left)
    }

    #[test]
    fn the_sender_drains_the_outbox() {
        let (got, left) = drive("one", vec![vec![]], &[("a", true), ("b", false)], 2000);
        assert_eq!(
            got,
            vec![(0, "a".to_string(), true), (0, "b".to_string(), false)]
        );
        assert_eq!(left, 0);
    }

    #[test]
    fn every_chat_gets_every_message_in_order() {
        let (got, left) = drive(
            "two",
            vec![vec![], vec![]],
            &[("a", true), ("b", false)],
            4000,
        );
        assert_eq!(
            got,
            vec![
                (0, "a".to_string(), true),
                (1, "a".to_string(), true),
                (0, "b".to_string(), false),
                (1, "b".to_string(), false)
            ]
        );
        assert_eq!(left, 0);
    }

    #[test]
    fn a_refusing_chat_never_holds_the_others() {
        // Chat 1 blocked the bot (403) for the first message only.
        let script = vec![vec![], vec![Outcome::ChatRefused("blocked".into())]];
        let (got, left) = drive("refused", script, &[("a", false), ("b", false)], 4000);
        assert_eq!(
            got,
            vec![
                (0, "a".to_string(), false),
                (0, "b".to_string(), false),
                (1, "b".to_string(), false)
            ]
        );
        assert_eq!(left, 0);
    }

    #[test]
    fn a_retry_resumes_with_the_chats_still_owed() {
        // Chat 1 times out once: after the back-off only chat 1 gets "a" again.
        let script = vec![vec![], vec![Outcome::Retry(1)]];
        let (got, left) = drive("retry", script, &[("a", true)], 4000);
        assert_eq!(
            got,
            vec![(0, "a".to_string(), true), (1, "a".to_string(), true)],
            "chat 0 is not sent twice"
        );
        assert_eq!(left, 0);
    }
}
