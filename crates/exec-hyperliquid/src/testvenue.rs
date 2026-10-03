// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A scripted Hyperliquid over a real rustls server on `127.0.0.1` — the
//! loopback the order gateway's tests and allocation gate 96 run against
//! (XH4b). Feature `testvenue` only; never in a production build.
//!
//! It upgrades `/ws`, acknowledges the subscribes, answers the
//! application ping, answers every post by the test's [`Policy`] on the
//! socket it came on, pushes stream frames to every live socket (as the
//! venue does to both subscribed connections during a make-before-break
//! swap), and serves `POST /exchange` for the REST fallback and `POST
//! /info` for reads ([`REST_INFO`], XMM XH4g). Every connection gets its
//! own thread.
//!
//! COPY-DOCTRINE: test fixture; allocates and copies freely.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use core_net::{expected_accept, ws_read_frame, ws_unmask_in_place, WsOpcode, WsReadResult};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// One post the venue received.
#[derive(Clone, Debug)]
pub struct Seen {
    /// The WebSocket connection it came on (0, 1, … in accept order),
    /// `usize::MAX` for a REST post, or [`REST_INFO`] for a REST `/info`
    /// read.
    pub conn: usize,
    /// The post as sent: the WebSocket post, or the REST body.
    pub body: String,
}

/// [`Seen::conn`] of a REST `/info` read (XMM XH4g: the battery's reads;
/// its answer is the policy's payload, as the venue's `/info` answers the
/// data alone).
pub const REST_INFO: usize = usize::MAX - 1;

/// What the venue does with one post.
#[derive(Clone, Debug)]
pub enum Answer {
    /// Answer `{"type":"action","payload":<json>}` (REST: the body).
    Action(String),
    /// Answer `{"type":"error","payload":"<msg>"}` (REST: a status err).
    Error(String),
    /// Answer a read (XH4d): `{"type":"info","payload":<json>}`.
    Info(String),
    /// [`Answer::Action`], this much later — the socket keeps streaming
    /// meanwhile (pushes and other posts' answers go out first), as a real
    /// venue's stream rows can come before a post's answer (XMM XH4f).
    Later(Duration, String),
    /// Never answer.
    Silent,
    /// Close the socket without answering.
    Drop,
}

/// The test's answer to each post.
pub type Policy = Box<dyn FnMut(&Seen) -> Answer + Send>;

/// Stream frames to every live WebSocket — clonable into a [`Policy`].
#[derive(Clone)]
pub struct Pusher(Arc<Mutex<Vec<mpsc::Sender<Vec<u8>>>>>);

impl Pusher {
    /// Queue `json` as a text frame on every live WebSocket; each goes out
    /// as its own TLS record after anything its socket is writing now.
    pub fn push_all(&self, json: &str) {
        let mut v = self.0.lock().unwrap();
        v.retain(|tx| tx.send(json.as_bytes().to_vec()).is_ok());
    }
}

#[derive(Clone)]
struct Shared {
    posts: Arc<Mutex<Vec<Seen>>>,
    policy: Arc<Mutex<Policy>>,
    push: Pusher,
    refuse_ws: Arc<AtomicBool>,
    ws_conns: Arc<AtomicUsize>,
    snapshot: Arc<Mutex<Option<String>>>,
}

/// The running venue.
pub struct Venue {
    /// Its port on `127.0.0.1` (the host name is `localhost`).
    pub port: u16,
    /// A client configuration that trusts it.
    pub tls: Arc<ClientConfig>,
    /// Its self-signed certificate (for a client in another process).
    pub cert_der: Vec<u8>,
    sh: Shared,
}

impl Venue {
    /// Bind a port and serve until the process ends. The policy starts as
    /// [`Answer::Silent`].
    ///
    /// # Panics
    /// If the certificate or the listener cannot be made (a test fixture).
    #[must_use]
    pub fn start() -> Self {
        let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
        let cert: CertificateDer<'static> = c.cert.der().clone();
        let key = PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key DER");
        let server = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key)
                .expect("server cfg"),
        );
        let cert_der = cert.to_vec();
        let mut roots = RootCertStore::empty();
        roots.add(cert).expect("trust anchor");
        let tls = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let sh = Shared {
            posts: Arc::new(Mutex::new(Vec::new())),
            policy: Arc::new(Mutex::new(Box::new(|_: &Seen| Answer::Silent))),
            push: Pusher(Arc::new(Mutex::new(Vec::new()))),
            refuse_ws: Arc::new(AtomicBool::new(false)),
            ws_conns: Arc::new(AtomicUsize::new(0)),
            snapshot: Arc::new(Mutex::new(Some("[]".to_string()))),
        };
        let s2 = sh.clone();
        thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(sock) = sock else { return };
                let (cfg, sh) = (Arc::clone(&server), s2.clone());
                thread::spawn(move || serve(sock, cfg, &sh));
            }
        });
        Self {
            port,
            tls,
            cert_der,
            sh,
        }
    }

    /// Replace the policy.
    ///
    /// # Panics
    /// If a connection thread panicked holding it.
    pub fn policy(&self, p: impl FnMut(&Seen) -> Answer + Send + 'static) {
        *self.sh.policy.lock().unwrap() = Box::new(p);
    }

    /// Every post received so far, in arrival order.
    ///
    /// # Panics
    /// If a connection thread panicked holding the list.
    #[must_use]
    pub fn posts(&self) -> Vec<Seen> {
        self.sh.posts.lock().unwrap().clone()
    }

    /// A stream frame to every live WebSocket.
    pub fn push_all(&self, json: &str) {
        self.sh.push.push_all(json);
    }

    /// A pusher to move into a policy.
    #[must_use]
    pub fn pusher(&self) -> Pusher {
        self.sh.push.clone()
    }

    /// The `userFills` snapshot sent right after a socket subscribes to
    /// that channel, as the venue does: `Some(<the fills array>)` —
    /// `[]` by default — or `None` to send none.
    ///
    /// # Panics
    /// If a connection thread panicked holding it.
    pub fn snapshot(&self, fills: Option<&str>) {
        *self.sh.snapshot.lock().unwrap() = fills.map(str::to_string);
    }

    /// Refuse (403) every WebSocket upgrade while `on`.
    pub fn refuse_ws(&self, on: bool) {
        self.sh.refuse_ws.store(on, Ordering::SeqCst);
    }
}

/// `{"status":"ok",…}` with `n` resting statuses, oids from `first_oid`.
#[must_use]
pub fn resting(n: usize, first_oid: u64) -> String {
    let items: Vec<String> = (0..n)
        .map(|i| format!(r#"{{"resting":{{"oid":{}}}}}"#, first_oid + i as u64))
        .collect();
    format!(
        r#"{{"status":"ok","response":{{"type":"order","data":{{"statuses":[{}]}}}}}}"#,
        items.join(",")
    )
}

/// Order wires in a posted `order` or `batchModify` action (one `"a":`
/// each).
#[must_use]
pub fn items_in(body: &str) -> usize {
    body.matches(r#""a":"#).count()
}

/// Unmasked server frame: FIN + opcode + 7/16/64-bit length.
fn server_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(payload.len() + 10);
    v.push(0x80 | opcode);
    let n = payload.len();
    if n < 126 {
        v.push(n as u8);
    } else if n < 65_536 {
        v.push(126);
        v.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        v.push(127);
        v.extend_from_slice(&(n as u64).to_be_bytes());
    }
    v.extend_from_slice(payload);
    v
}

/// Read what is there: `Some(n)` (0 = closed), `None` = nothing yet.
fn read_some(conn: &mut ServerConnection, sock: &mut TcpStream, dst: &mut [u8]) -> Option<usize> {
    let mut s = Stream::new(conn, sock);
    match s.read(dst) {
        Ok(n) => Some(n),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            None
        }
        Err(_) => Some(0),
    }
}

fn write(conn: &mut ServerConnection, sock: &mut TcpStream, bytes: &[u8]) -> bool {
    let mut s = Stream::new(conn, sock);
    s.write_all(bytes).is_ok() && s.flush().is_ok()
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn serve(mut sock: TcpStream, cfg: Arc<ServerConfig>, sh: &Shared) {
    let mut conn = ServerConnection::new(cfg).expect("conn");
    sock.set_read_timeout(Some(Duration::from_millis(5)))
        .expect("timeout");
    let mut buf = vec![0u8; 1 << 20];
    let mut len = 0usize;
    loop {
        let head_end = loop {
            if let Some(i) = find(&buf[..len], b"\r\n\r\n") {
                break i + 4;
            }
            match read_some(&mut conn, &mut sock, &mut buf[len..]) {
                Some(0) => return,
                Some(n) => len += n,
                None => {}
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        if head.starts_with("GET /ws") {
            buf.copy_within(head_end..len, 0);
            serve_ws(&mut conn, &mut sock, &mut buf, len - head_end, &head, sh);
            return;
        }
        let info = head.starts_with("POST /info");
        assert!(
            info || head.starts_with("POST /exchange"),
            "request line: {head:?}"
        );
        let cl: usize = head
            .lines()
            .find_map(|l| {
                let l = l.to_ascii_lowercase();
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().parse().expect("content-length"))
            })
            .expect("content-length");
        while len < head_end + cl {
            match read_some(&mut conn, &mut sock, &mut buf[len..]) {
                Some(0) => return,
                Some(n) => len += n,
                None => {}
            }
        }
        let seen = Seen {
            conn: if info { REST_INFO } else { usize::MAX },
            body: String::from_utf8_lossy(&buf[head_end..head_end + cl]).to_string(),
        };
        sh.posts.lock().unwrap().push(seen.clone());
        let answer = (sh.policy.lock().unwrap())(&seen);
        let body = match answer {
            Answer::Action(p) | Answer::Later(_, p) | Answer::Info(p) => p,
            Answer::Error(m) => format!(r#"{{"status":"err","response":"{m}"}}"#),
            Answer::Silent | Answer::Drop => return,
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        if !write(&mut conn, &mut sock, resp.as_bytes()) {
            return;
        }
        buf.copy_within(head_end + cl..len, 0);
        len -= head_end + cl;
    }
}

fn post_frame(id: u64, kind: &str, payload: &str) -> String {
    format!(
        r#"{{"channel":"post","data":{{"id":{id},"response":{{"type":"{kind}","payload":{payload}}}}}}}"#
    )
}

fn serve_ws(
    conn: &mut ServerConnection,
    sock: &mut TcpStream,
    buf: &mut [u8],
    mut len: usize,
    head: &str,
    sh: &Shared,
) {
    if sh.refuse_ws.load(Ordering::SeqCst) {
        let _ = write(
            conn,
            sock,
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        );
        thread::sleep(Duration::from_millis(50));
        return;
    }
    let key = head
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("sec-websocket-key:"))
        .and_then(|l| l.split(':').nth(1))
        .expect("Sec-WebSocket-Key")
        .trim()
        .as_bytes()
        .to_vec();
    let mut key24 = [0u8; 24];
    key24.copy_from_slice(&key);
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&expected_accept(&key24));
    resp.extend_from_slice(b"\r\n\r\n");
    if !write(conn, sock, &resp) {
        return;
    }
    let idx = sh.ws_conns.fetch_add(1, Ordering::SeqCst);
    let (ptx, prx) = mpsc::channel::<Vec<u8>>();
    sh.push.0.lock().unwrap().push(ptx);
    // `Answer::Later` answers, written when due (the loop turns every 5 ms).
    let mut later: Vec<(Instant, String)> = Vec::new();
    loop {
        while let Ok(f) = prx.try_recv() {
            if !write(conn, sock, &server_frame(0x1, &f)) {
                return;
            }
        }
        let now = Instant::now();
        let mut k = 0;
        while k < later.len() {
            if later[k].0 <= now {
                let (_, f) = later.remove(k);
                if !write(conn, sock, &server_frame(0x1, f.as_bytes())) {
                    return;
                }
            } else {
                k += 1;
            }
        }
        while let WsReadResult::Frame { header, payload } = ws_read_frame(&buf[..len]) {
            let total = header.header_len as usize + header.payload_len as usize;
            if total > len {
                break;
            }
            assert!(header.masked, "client frames MUST be masked");
            ws_unmask_in_place(&mut buf[payload.start..payload.end], header.mask);
            let text = String::from_utf8_lossy(&buf[payload.start..payload.end]).to_string();
            buf.copy_within(total..len, 0);
            len -= total;
            if header.opcode == WsOpcode::Close {
                return;
            }
            if header.opcode != WsOpcode::Text {
                continue;
            }
            let subscribe = text.contains(r#""method":"subscribe""#);
            let snapshot = if subscribe && text.contains(r#""type":"userFills""#) {
                sh.snapshot.lock().unwrap().clone()
            } else {
                None
            };
            let reply = if subscribe {
                Some(format!(
                    r#"{{"channel":"subscriptionResponse","data":{text}}}"#
                ))
            } else if text.contains(r#""method":"ping""#) {
                Some(r#"{"channel":"pong"}"#.to_string())
            } else if text.contains(r#""method":"post""#) {
                let at = text.find(r#""id":"#).expect("post id") + 5;
                let id: u64 = text[at..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .expect("post id digits");
                let seen = Seen {
                    conn: idx,
                    body: text.clone(),
                };
                sh.posts.lock().unwrap().push(seen.clone());
                let answer = (sh.policy.lock().unwrap())(&seen);
                match answer {
                    Answer::Action(p) => Some(post_frame(id, "action", &p)),
                    Answer::Later(d, p) => {
                        later.push((Instant::now() + d, post_frame(id, "action", &p)));
                        None
                    }
                    Answer::Error(m) => Some(post_frame(id, "error", &format!("\"{m}\""))),
                    Answer::Info(p) => Some(post_frame(id, "info", &p)),
                    Answer::Silent => None,
                    Answer::Drop => return,
                }
            } else {
                None
            };
            if let Some(r) = reply {
                if !write(conn, sock, &server_frame(0x1, r.as_bytes())) {
                    return;
                }
            }
            if let Some(f) = snapshot {
                let s = format!(
                    r#"{{"channel":"userFills","data":{{"isSnapshot":true,"user":"0x","fills":{f}}}}}"#
                );
                if !write(conn, sock, &server_frame(0x1, s.as_bytes())) {
                    return;
                }
            }
        }
        match read_some(conn, sock, &mut buf[len..]) {
            Some(0) => return,
            Some(n) => len += n,
            None => {}
        }
    }
}
