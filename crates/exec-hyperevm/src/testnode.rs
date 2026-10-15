// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! A scripted HyperEVM JSON-RPC node over a real rustls server on
//! `127.0.0.1` — the loopback every write-path test runs against (this
//! crate's own, and the cli's shadow-thread tests). Feature `testnode`
//! only; never in a production build.
//!
//! The node is HONEST where honesty is checkable: it answers
//! `eth_sendRawTransaction` with `keccak256` of the raw bytes it
//! received, so a `Sent` outcome proves the arm's local hash equals the
//! hash of what went over the wire. Everything else — accounts, the
//! answer to each send, when a transaction is mined — is scripted.
//!
//! COPY-DOCTRINE: test fixture; allocates and copies freely.

#![allow(missing_docs, clippy::missing_panics_doc)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::ServerConnection;
use rustls::{ClientConfig, RootCertStore, ServerConfig, Stream};

/// What the node does with the next `eth_sendRawTransaction`.
#[derive(Clone)]
pub enum SendScript {
    /// Accept; the receipt will name `from` / `to` / `contract`.
    Accept {
        from: [u8; 20],
        to: Option<[u8; 20]>,
        contract: Option<[u8; 20]>,
        status: u8,
    },
    /// Accept, but answer a hash that is not the raw bytes'.
    WrongHash,
    /// Refuse with this message.
    Refuse(&'static str),
    /// Keep the transaction but hang up without answering.
    DropAfterRead {
        from: [u8; 20],
        to: Option<[u8; 20]>,
    },
}

/// One transaction the node holds.
pub struct Tx {
    pub from: [u8; 20],
    pub to: Option<[u8; 20]>,
    pub contract: Option<[u8; 20]>,
    pub status: u8,
    /// Whether its receipt is served yet.
    pub mined: bool,
}

/// The node's whole state, shared with the test through a mutex.
#[derive(Default)]
pub struct Node {
    pub chain_id: u64,
    pub base_fee: u128,
    /// address → (latest, pending, balance).
    pub accounts: HashMap<[u8; 20], (u64, u64, u128)>,
    /// Answers for the next sends, in order. An unscripted send panics
    /// the node thread (the test did not expect it).
    pub script: VecDeque<SendScript>,
    /// When no script is queued, accept as `(from, to)` (a thread test
    /// that cannot know how many sends it will make).
    pub default_accept: Option<([u8; 20], [u8; 20])>,
    /// Serve every accepted transaction's receipt at once.
    pub auto_mine: bool,
    /// Answer with `Transfer-Encoding: chunked` in this many chunks
    /// (0 = `Content-Length`); the archive endpoint answers chunked.
    pub chunked: u8,
    /// contract → the address its `owner()` returns (`eth_call`).
    pub owners: HashMap<[u8; 20], [u8; 20]>,
    /// Hang up after this many answers on one connection (0 = never),
    /// as an endpoint's idle or request-count limit does.
    pub close_after: u32,
    /// …after sleeping this long first (the idle close: the client has
    /// already read the answer when the FIN arrives).
    pub close_delay_ms: u64,
    /// …and say so on the last answer (`Connection: close`).
    pub announce_close: bool,
    pub txs: HashMap<[u8; 32], Tx>,
    /// Every raw transaction received, in order (what was signed).
    pub raws: Vec<Vec<u8>>,
    /// Methods seen, in order.
    pub seen: Vec<String>,
    /// Connections accepted.
    pub conns: u32,
    /// Answer the next `n` calls of `method` with the public endpoint's
    /// throttle (`-32005 rate limited`), as it answers bursts.
    pub rate_limit: Option<(&'static str, u32)>,
}

impl Node {
    /// Serve every held transaction's receipt from now on.
    pub fn mine_all(&mut self) {
        for t in self.txs.values_mut() {
            t.mined = true;
        }
    }
}

/// A running node: its port, a client config that trusts it, and its
/// state.
pub struct TestNode {
    pub port: u16,
    pub client_cfg: Arc<ClientConfig>,
    pub state: Arc<Mutex<Node>>,
}

/// Lower-case hex, no prefix.
pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Hex (optional `0x`) to bytes.
pub fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim_start_matches("0x");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn field<'a>(req: &'a str, key: &str) -> &'a str {
    let at = req.find(&format!("\"{key}\":")).unwrap() + key.len() + 3;
    let rest = &req[at..];
    let end = rest.find([',', '}']).unwrap();
    rest[..end].trim_matches('"')
}

fn params(req: &str) -> Vec<String> {
    let at = req.find("\"params\":[").unwrap() + 10;
    let rest = &req[at..];
    let end = rest.rfind(']').unwrap();
    rest[..end]
        .split(',')
        .map(|p| p.trim().trim_matches('"').to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

enum Reply {
    Body(String),
    Hangup,
}

fn handle(node: &mut Node, req: &str) -> Reply {
    let id = field(req, "id").to_string();
    let method = field(req, "method").to_string();
    node.seen.push(method.clone());
    if let Some((m, n)) = node.rate_limit {
        if m == method && n > 0 {
            node.rate_limit = Some((m, n - 1));
            return Reply::Body(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32005,"message":"rate limited"}}}}"#
            ));
        }
    }
    let p = params(req);
    let result = |r: String| Reply::Body(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{r}}}"#));
    let error = |m: &str| {
        Reply::Body(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32000,"message":"{m}"}}}}"#
        ))
    };
    match method.as_str() {
        "eth_chainId" => result(format!("\"0x{:x}\"", node.chain_id)),
        "eth_getTransactionCount" | "eth_getBalance" => {
            let a: [u8; 20] = unhex(&p[0]).try_into().unwrap();
            let (latest, pending, bal) = *node.accounts.get(&a).unwrap_or(&(0, 0, 0));
            let v = match (method.as_str(), p[1].as_str()) {
                ("eth_getBalance", _) => bal,
                (_, "latest") => latest as u128,
                _ => pending as u128,
            };
            result(format!("\"0x{v:x}\""))
        }
        "eth_feeHistory" => result(format!(
            r#"{{"baseFeePerGas":["0x1","0x{:x}"],"gasUsedRatio":[0.1],"oldestBlock":"0x10"}}"#,
            node.base_fee
        )),
        "eth_sendRawTransaction" => {
            let raw = unhex(&p[0]);
            let h = signer_eip712::keccak256(&raw);
            node.raws.push(raw);
            let next = match (node.script.pop_front(), node.default_accept) {
                (Some(s), _) => s,
                (None, Some((from, to))) => SendScript::Accept {
                    from,
                    to: Some(to),
                    contract: None,
                    status: 1,
                },
                (None, None) => panic!("a send the test did not script"),
            };
            let auto = node.auto_mine;
            match next {
                SendScript::Accept {
                    from,
                    to,
                    contract,
                    status,
                } => {
                    node.txs.insert(
                        h,
                        Tx {
                            from,
                            to,
                            contract,
                            status,
                            mined: auto,
                        },
                    );
                    result(format!("\"0x{}\"", hex(&h)))
                }
                SendScript::WrongHash => {
                    let mut g = h;
                    g[0] ^= 1;
                    result(format!("\"0x{}\"", hex(&g)))
                }
                SendScript::Refuse(m) => error(m),
                SendScript::DropAfterRead { from, to } => {
                    node.txs.insert(
                        h,
                        Tx {
                            from,
                            to,
                            contract: None,
                            status: 1,
                            mined: false,
                        },
                    );
                    Reply::Hangup
                }
            }
        }
        "eth_call" => {
            let to: [u8; 20] = unhex(field(req, "to")).try_into().unwrap();
            assert_eq!(field(req, "data"), "0x8da5cb5b", "only owner() is scripted");
            match node.owners.get(&to) {
                Some(o) => result(format!("\"0x{}{}\"", "00".repeat(12), hex(o))),
                // No code at the address: a call returns empty.
                None => result("\"0x\"".to_string()),
            }
        }
        "eth_getTransactionReceipt" => {
            let h: [u8; 32] = unhex(&p[0]).try_into().unwrap();
            match node.txs.get(&h) {
                Some(t) if t.mined => {
                    let to =
                        t.to.map_or("null".to_string(), |a| format!("\"0x{}\"", hex(&a)));
                    let c = t
                        .contract
                        .map_or("null".to_string(), |a| format!("\"0x{}\"", hex(&a)));
                    // Logs repeat blockNumber / transactionHash one level
                    // down, with DIFFERENT values: the arm must read the
                    // top level only.
                    result(format!(
                        r#"{{"type":"0x2","status":"0x{:x}","logs":[{{"blockNumber":"0x1","transactionHash":"0x{}","transactionIndex":"0x9"}}],"transactionHash":"0x{}","transactionIndex":"0x2","blockNumber":"0x3e8","gasUsed":"0x1d4c0","effectiveGasPrice":"0x5f5e100","from":"0x{}","to":{to},"contractAddress":{c}}}"#,
                        t.status,
                        "00".repeat(32),
                        hex(&h),
                        hex(&t.from),
                    ))
                }
                _ => result("null".to_string()),
            }
        }
        m => panic!("unexpected method {m}"),
    }
}

/// Read one request (head + Content-Length body); `None` at EOF.
fn read_request<T: Read>(s: &mut T) -> Option<String> {
    let mut buf = Vec::new();
    let mut b = [0u8; 4096];
    loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let clen: usize = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                .and_then(|l| l.split(':').nth(1))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < i + 4 + clen {
                let n = s.read(&mut b).ok()?;
                if n == 0 {
                    return None;
                }
                buf.extend_from_slice(&b[..n]);
            }
            return Some(String::from_utf8_lossy(&buf[i + 4..i + 4 + clen]).to_string());
        }
        let n = s.read(&mut b).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&b[..n]);
    }
}

/// A self-signed `localhost` identity — share one between nodes so a
/// single client config trusts them all (a read node and a write node).
pub struct Certs {
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl Certs {
    /// The certificate, DER — what a client in ANOTHER process needs to
    /// trust this identity (bench gate 72 runs the node in a child).
    pub fn cert_der(&self) -> &[u8] {
        self.cert.as_ref()
    }
}

/// A fresh identity.
pub fn certs() -> Certs {
    let c = generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen");
    Certs {
        cert: c.cert.der().clone(),
        key: PrivateKeyDer::try_from(c.key_pair.serialize_der()).expect("key"),
    }
}

/// Start a node on an ephemeral port, with its own identity.
pub fn boot(node: Node) -> TestNode {
    boot_with(node, &certs())
}

/// Start a node on an ephemeral port under `certs`.
pub fn boot_with(node: Node, certs: &Certs) -> TestNode {
    let server_cfg = Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certs.cert.clone()], certs.key.clone_key())
            .expect("server cfg"),
    );
    let mut roots = RootCertStore::empty();
    roots.add(certs.cert.clone()).expect("anchor");
    let client_cfg = Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let node = Arc::new(Mutex::new(node));
    let shared = node.clone();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { return };
            shared.lock().unwrap().conns += 1;
            let mut tls = ServerConnection::new(server_cfg.clone()).expect("conn");
            let mut answered = 0u32;
            loop {
                let Some(req) = read_request(&mut Stream::new(&mut tls, &mut sock)) else {
                    break;
                };
                let (reply, chunks, close_after, delay_ms, announce) = {
                    let mut n = shared.lock().unwrap();
                    (
                        handle(&mut n, &req),
                        n.chunked,
                        n.close_after,
                        n.close_delay_ms,
                        n.announce_close,
                    )
                };
                answered += 1;
                let last = close_after != 0 && answered >= close_after;
                let conn_hdr = if last && announce {
                    "close"
                } else {
                    "keep-alive"
                };
                match reply {
                    Reply::Body(body) => {
                        let wire = if chunks > 0 {
                            let mut w = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: {conn_hdr}\r\n\r\n"
                            );
                            let step = body.len().div_ceil(chunks as usize).max(1);
                            let mut at = 0usize;
                            while at < body.len() {
                                let end = (at + step).min(body.len());
                                w.push_str(&format!("{:x}\r\n{}\r\n", end - at, &body[at..end]));
                                at = end;
                            }
                            w.push_str("0\r\n\r\n");
                            w
                        } else {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {conn_hdr}\r\n\r\n{body}",
                                body.len()
                            )
                        };
                        let mut stream = Stream::new(&mut tls, &mut sock);
                        if stream.write_all(wire.as_bytes()).is_err() {
                            break;
                        }
                        let _ = stream.flush();
                    }
                    Reply::Hangup => break,
                }
                if last {
                    thread::sleep(std::time::Duration::from_millis(delay_ms));
                    tls.send_close_notify();
                    let _ = tls.write_tls(&mut sock);
                    let _ = sock.shutdown(std::net::Shutdown::Both);
                    break;
                }
            }
        }
    });
    TestNode {
        port,
        client_cfg,
        state: node,
    }
}
