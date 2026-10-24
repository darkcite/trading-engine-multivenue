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
    /// The token or chat is wrong: stop sending for this many seconds.
    Pause(u64, String),
}

/// A way to deliver text.
pub trait Transport: Send {
    /// Deliver one message.
    fn send(&mut self, text: &str, loud: bool) -> Outcome;
}

/// `dry_run = true`: the message goes to the log, nowhere else.
#[derive(Debug, Default)]
pub struct DryRun;

impl Transport for DryRun {
    fn send(&mut self, text: &str, loud: bool) -> Outcome {
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
            let ok = Json::parse(&out[r]).ok().and_then(|j| j.get("ok").cloned()) == Some(Json::Bool(true));
            if ok {
                Outcome::Sent
            } else {
                Outcome::Retry(30)
            }
        }
        Err(BootHttpErr::Status(429)) => Outcome::Retry(30),
        Err(BootHttpErr::Status(400)) => Outcome::Drop("Telegram refused the message (400)".into()),
        Err(BootHttpErr::Status(s @ (401 | 403 | 404))) => {
            Outcome::Pause(300, format!("Telegram answered {s}: check TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID (and that the bot may post to the chat)"))
        }
        Err(BootHttpErr::RequestTooLarge) => Outcome::Drop("message too large for one request".into()),
        Err(_) => Outcome::Retry(10),
    }
}

impl Transport for Telegram {
    fn send(&mut self, text: &str, loud: bool) -> Outcome {
        let body = request_body(&self.keys.chat_id, text, loud);
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

/// The sender loop: drain the outbox through `t` until `stop` is set or
/// the poll loop's end of `wake` is dropped. It blocks on `wake` while
/// idle instead of polling the outbox on a short timer (a timer wakeup a
/// few times a second measurably shifts the house's ns-scale benches).
pub fn run_sender(
    outbox: Outbox,
    mut t: Box<dyn Transport>,
    stop: Arc<AtomicBool>,
    wake: Receiver<()>,
) {
    let mut bucket = Bucket::default();
    let mut paused_until: Option<Instant> = None;
    let mut backoff = 0u64;
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
        if !bucket.take() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        match t.send(&text, loud) {
            Outcome::Sent => {
                backoff = 0;
                remove_all(&paths);
            }
            Outcome::Drop(why) => {
                crate::run::log(&format!("dropped a message: {why}"));
                remove_all(&paths);
            }
            Outcome::Retry(s) => {
                backoff = if backoff == 0 {
                    s
                } else {
                    (backoff * 2).min(300)
                };
                crate::run::log(&format!(
                    "send failed, retrying in {backoff} s ({} pending)",
                    pending.len()
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
            classify(Err(BootHttpErr::Status(401)), b""),
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

    struct Fake(Arc<Mutex<Vec<(String, bool)>>>);
    impl Transport for Fake {
        fn send(&mut self, text: &str, loud: bool) -> Outcome {
            self.0.lock().unwrap().push((text.to_string(), loud));
            Outcome::Sent
        }
    }

    #[test]
    fn the_sender_drains_the_outbox() {
        let d = std::env::temp_dir().join(format!("notify-send-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let o = Outbox::open(&d).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let (o2, s2, g2) = (o.clone(), stop.clone(), got.clone());
        let h = std::thread::spawn(move || run_sender(o2, Box::new(Fake(g2)), s2, rx));
        // Idle first: it blocks on the wake, then drains what was queued.
        std::thread::sleep(Duration::from_millis(50));
        o.push(&Msg {
            loud: true,
            text: "a".into(),
        })
        .unwrap();
        o.push(&Msg {
            loud: false,
            text: "b".into(),
        })
        .unwrap();
        tx.send(()).unwrap();
        for _ in 0..100 {
            if o.pending().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        drop(tx); // unblocks the idle wait
        h.join().unwrap();
        assert_eq!(
            *got.lock().unwrap(),
            vec![("a".to_string(), true), ("b".to_string(), false)]
        );
        assert!(o.pending().unwrap().is_empty());
    }
}
