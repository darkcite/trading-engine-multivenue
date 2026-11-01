// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `--live`: public market data (TERM plan §4.7, ruling O-T16). No keys.
//!
//! - Hyperliquid: one socket to `wss://api.hyperliquid.xyz/ws`, `l2Book` +
//!   `trades` per coin (2 subscriptions per coin; the budget is ≤ 40).
//! - Binance USDⓈ-M: one combined-stream connection (`@depth@100ms` +
//!   `@aggTrade`), with the REST depth snapshot and the documented sync:
//!   drop events with `u` < `lastUpdateId`; the first applied event has
//!   `U ≤ lastUpdateId ≤ u`; after that every `pu` equals the previous `u`,
//!   else resync. The REST call runs on a side thread.
//! - Budgets (T-L6): ≥ 10 s between connection or REST attempts (backoff to
//!   60 s); a venue's feed stops for the session on its first 429/418.
//! - T-L5: none of this ever reaches the engine.

pub mod scan;

use std::collections::BTreeMap;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_net::{TlsTransport, WsCfg, WsConn, WsNext, WsProgress};
use mio::{Events, Poll, Token};
use rustls_client::ClientConfig;
use term_proto::ctl::{InstrDef, Venue};
use term_proto::exec::{EngineStatus, SlotState};
use term_proto::md::*;
use term_proto::{Ascii, ChunkHdr, Side};

use crate::source::Source;
use scan::{Bn, Hl};

mod rustls_client {
    pub use rustls::client::ClientConfig;
}

const DEPTH: usize = 50;
const HEAT_ROWS: usize = 256;
const BAR_TF_S: u32 = 60;
const MAX_BARS: usize = 1000;
const RETRY_MIN: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(60);
const HL_HOST: &str = "api.hyperliquid.xyz";
const BN_HOST: &str = "fstream.binance.com";
const BN_REST: &str = "fapi.binance.com";
const UA: &[u8] = b"multivenue-terminal/0.1";

pub struct LiveCfg {
    /// Hyperliquid perp coins, e.g. `BTC`.
    pub hl: Vec<String>,
    /// Binance USDⓈ-M symbols, e.g. `BTCUSDT`.
    pub bn: Vec<String>,
    /// Extra trust anchors (PEM) on top of the Mozilla roots. Only for a
    /// host whose egress re-signs TLS (a sandbox gateway); empty on the Mac.
    pub extra_ca: Vec<std::path::PathBuf>,
}

/// The Mozilla roots plus any explicitly configured extra anchors.
pub fn tls_config(extra: &[std::path::PathBuf]) -> Result<Arc<ClientConfig>, String> {
    if extra.is_empty() {
        return Ok(TlsTransport::default_client_config());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for p in extra {
        let pem = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
        let mut n = 0;
        for block in pem.split("-----BEGIN CERTIFICATE-----").skip(1) {
            let b64: String = block.split("-----END CERTIFICATE-----").next().unwrap_or("").chars().filter(|c| !c.is_whitespace()).collect();
            let mut der = vec![0u8; core_crypto::base64_decoded_max_len(b64.len())];
            let len = core_crypto::base64_decode(b64.as_bytes(), &mut der).map_err(|_| format!("{}: bad base64", p.display()))?;
            der.truncate(len);
            roots.add(rustls_pki_types::CertificateDer::from(der)).map_err(|e| format!("{}: {e}", p.display()))?;
            n += 1;
        }
        if n == 0 {
            return Err(format!("{}: no certificate", p.display()));
        }
        eprintln!("live: {} extra trust anchor(s) from {}", n, p.display());
    }
    Ok(Arc::new(ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()))
}

enum Sync {
    /// Waiting for the REST snapshot; events are buffered (parsed).
    Waiting(Vec<BnEv>),
    Synced(u64),
}

struct BnEv {
    first: u64,
    last: u64,
    prev: u64,
    levels: Vec<(Side, i64, i64)>,
}

struct Snap {
    instr: usize,
    res: Result<(u64, Vec<(Side, i64, i64)>), String>,
}

struct Inst {
    def: InstrDef,
    sym: String,
    heat_scale: i64,
    heat_cap: f64,
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
    sent_b: BTreeMap<i64, i64>,
    sent_a: BTreeMap<i64, i64>,
    snapped: bool,
    book_dirty: bool,
    bars: Vec<Bar>,
    bar: Option<Bar>,
    bar_dirty: bool,
    cvd: i64,
    prev_l1: (i64, i64, i64, i64),
    trades: Vec<Trade>,
    col: u32,
    sync: Sync,
    last_rest: Option<Instant>,
    /// HL ticks come from the first book (5 significant figures).
    tick_known: bool,
    /// Re-announce the `InstrDef` (its tick changed).
    redef: bool,
}

/// Hyperliquid perps quote 5 significant figures: the tick at `px_1e6`.
pub fn hl_tick_1e6(px_1e6: i64) -> i64 {
    let mut t: i64 = 1;
    let mut p = px_1e6.max(1);
    while p >= 100_000 {
        p /= 10;
        t *= 10;
    }
    t.max(1)
}

struct Feed {
    conn: Option<WsConn>,
    token: Token,
    failures: u32,
    next_try: Instant,
    stopped: bool,
    opened: bool,
    last_ping: Instant,
}

impl Feed {
    fn new(token: Token) -> Feed {
        Feed { conn: None, token, failures: 0, next_try: Instant::now(), stopped: false, opened: false, last_ping: Instant::now() }
    }
    fn backoff(&mut self, why: &str, name: &str) {
        self.conn = None;
        self.opened = false;
        self.failures += 1;
        let d = (RETRY_MIN * 2u32.saturating_pow(self.failures.saturating_sub(1).min(3))).min(RETRY_MAX);
        self.next_try = Instant::now() + d;
        eprintln!("live: {name} down ({why}); retry in {d:?}");
    }
}

pub struct Live {
    poll: Poll,
    events: Events,
    tls: Arc<ClientConfig>,
    hl: Feed,
    bn: Feed,
    ins: Vec<Inst>,
    snap_tx: mpsc::Sender<Snap>,
    snap_rx: mpsc::Receiver<Snap>,
    scratch: Vec<u8>,
    seq: u32,
    t_ns: u64,
    pub stats: LiveStats,
}

#[derive(Debug, Default, Clone)]
pub struct LiveStats {
    pub hl_msgs: u64,
    pub bn_msgs: u64,
    pub bad_msgs: u64,
    pub resyncs: u64,
    pub crossed: u64,
}

fn ns() -> u64 {
    core_time_now()
}

fn core_time_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

impl Live {
    pub fn new(cfg: &LiveCfg) -> std::io::Result<Live> {
        let mut ins = Vec::new();
        for c in &cfg.hl {
            ins.push((Venue::Hyperliquid, c.clone(), 1_000_000i64, 10i64, 1i64, 40.0f64));
        }
        for s in &cfg.bn {
            ins.push((Venue::Binance, s.clone(), 100_000, 1_000, 5, 40.0));
        }
        let ins = ins
            .into_iter()
            .enumerate()
            .map(|(k, (venue, sym, tick, lot, heat_scale, heat_cap))| {
                let tag = if venue == Venue::Hyperliquid { "HL" } else { "BN-UM" };
                let label = format!("{sym} {tag}");
                let label = Ascii::new(&label.as_bytes()[..label.len().min(24)]).unwrap_or(Ascii::new(b"?").unwrap_or_else(|_| unreachable!()));
                Inst {
                    def: InstrDef { instr: k as u16, venue, tick_1e6: tick, lot_1e6: lot, flags: 0, label },
                    sym,
                    heat_scale,
                    heat_cap,
                    bids: BTreeMap::new(),
                    asks: BTreeMap::new(),
                    sent_b: BTreeMap::new(),
                    sent_a: BTreeMap::new(),
                    snapped: false,
                    book_dirty: false,
                    bars: Vec::with_capacity(MAX_BARS),
                    bar: None,
                    bar_dirty: false,
                    cvd: 0,
                    prev_l1: (0, 0, 0, 0),
                    trades: Vec::with_capacity(256),
                    col: 0,
                    sync: Sync::Waiting(Vec::new()),
                    last_rest: None,
                    tick_known: venue != Venue::Hyperliquid,
                    redef: false,
                }
            })
            .collect();
        let (snap_tx, snap_rx) = mpsc::channel();
        let mut hl = Feed::new(Token(1));
        let mut bn = Feed::new(Token(2));
        hl.stopped = cfg.hl.is_empty();
        bn.stopped = cfg.bn.is_empty();
        let tls = tls_config(&cfg.extra_ca).map_err(std::io::Error::other)?;
        Ok(Live { poll: Poll::new()?, events: Events::with_capacity(64), tls, hl, bn, ins, snap_tx, snap_rx, scratch: vec![0u8; term_proto::MAX_FRAME], seq: 0, t_ns: 0, stats: LiveStats::default() })
    }

    fn seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn dial(&mut self, which: u8) {
        let (feed, host, path, name) = if which == 0 {
            (&mut self.hl, HL_HOST, "/ws".to_owned(), "hyperliquid")
        } else {
            let streams: Vec<String> = self.ins.iter().filter(|i| i.def.venue == Venue::Binance).flat_map(|i| {
                let s = i.sym.to_ascii_lowercase();
                [format!("{s}@depth@100ms"), format!("{s}@aggTrade")]
            }).collect();
            (&mut self.bn, BN_HOST, format!("/stream?streams={}", streams.join("/")), "binance-um")
        };
        if feed.stopped || feed.conn.is_some() || Instant::now() < feed.next_try {
            return;
        }
        let cfg = WsCfg { rx_cap: 4 << 20, tx_cap: 64 << 10, establish_ns: 10_000_000_000, idle_ns: 60_000_000_000 };
        match WsConn::new(host, 443, &path, self.tls.clone(), cfg, feed.token, ns()) {
            Ok(mut c) => {
                // Prefer IPv4: a host without an IPv6 route fails the dial.
                use std::net::ToSocketAddrs;
                if let Some(v4) = (host, 443u16).to_socket_addrs().ok().and_then(|mut a| a.find(|x| x.is_ipv4())) {
                    c.set_addr(v4);
                }
                match c.connect(self.poll.registry(), ns()) {
                    Ok(()) => feed.conn = Some(c),
                    Err(e) => feed.backoff(&e.to_string(), name),
                }
            }
            Err(e) => feed.backoff(&e.to_string(), name),
        }
        if which == 1 {
            for i in &mut self.ins {
                if i.def.venue == Venue::Binance {
                    i.sync = Sync::Waiting(Vec::new());
                }
            }
        }
    }

    fn on_open(&mut self, which: u8) {
        if which == 0 {
            let coins: Vec<String> = self.ins.iter().filter(|i| i.def.venue == Venue::Hyperliquid).map(|i| i.sym.clone()).collect();
            if let Some(c) = self.hl.conn.as_mut() {
                for coin in coins {
                    for ty in ["l2Book", "trades"] {
                        let m = format!(r#"{{"method":"subscribe","subscription":{{"type":"{ty}","coin":"{coin}"}}}}"#);
                        let _ = c.queue_text(&[m.as_bytes()]);
                    }
                }
                let _ = c.flush(self.poll.registry());
            }
            self.hl.opened = true;
            self.hl.failures = 0;
            eprintln!("live: hyperliquid open");
        } else {
            self.bn.opened = true;
            self.bn.failures = 0;
            eprintln!("live: binance-um open");
            for k in 0..self.ins.len() {
                if self.ins[k].def.venue == Venue::Binance {
                    self.request_snapshot(k);
                }
            }
        }
    }

    fn request_snapshot(&mut self, k: usize) {
        if self.bn.stopped || self.ins[k].last_rest.is_some_and(|t| t.elapsed() < RETRY_MIN) {
            return;
        }
        self.ins[k].last_rest = Some(Instant::now());
        let tls = self.tls.clone();
        let tx = self.snap_tx.clone();
        let path = format!("/fapi/v1/depth?symbol={}&limit=1000", self.ins[k].sym);
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(1 << 20);
            let res = match core_net::boot_http::https_get(&tls, BN_REST, 443, &path, UA, &mut out, 4 << 20, Duration::from_secs(10)) {
                Ok(r) => {
                    let body = &out[r];
                    let mut lv = Vec::with_capacity(2000);
                    let ok = (|| {
                        let id = scan::int(scan::get(body, b"lastUpdateId")?)?;
                        scan::bn_pairs(scan::get(body, b"bids")?, |p, q| lv.push((Side::Buy, p, q)))?;
                        scan::bn_pairs(scan::get(body, b"asks")?, |p, q| lv.push((Side::Sell, p, q)))?;
                        Some(id)
                    })();
                    ok.map(|id| (id, lv)).ok_or_else(|| "snapshot unparseable".to_owned())
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(Snap { instr: k, res });
        });
    }

    fn pump(&mut self) {
        self.dial(0);
        self.dial(1);
        if self.poll.poll(&mut self.events, Some(Duration::ZERO)).is_err() {
            return;
        }
        let now = ns();
        let mut opened = [false; 2];
        let mut failed: [Option<String>; 2] = [None, None];
        for ev in self.events.iter() {
            let which = if ev.token() == self.hl.token { 0 } else { 1 };
            let feed = if which == 0 { &mut self.hl } else { &mut self.bn };
            if let Some(c) = feed.conn.as_mut() {
                match c.on_event(ev, self.poll.registry(), now) {
                    WsProgress::Opened => opened[which] = true,
                    WsProgress::Failed(e) => failed[which] = Some(e.to_string()),
                    _ => {}
                }
            }
        }
        for which in 0..2u8 {
            let feed = if which == 0 { &mut self.hl } else { &mut self.bn };
            if let Some(c) = feed.conn.as_mut() {
                if let WsProgress::Failed(e) = c.on_tick(now) {
                    failed[which as usize] = Some(e.to_string());
                }
            }
            if opened[which as usize] {
                self.on_open(which);
            }
            self.drain(which);
            let (feed, name) = if which == 0 { (&mut self.hl, "hyperliquid") } else { (&mut self.bn, "binance-um") };
            if let Some(why) = failed[which as usize].take() {
                if let Some(mut c) = feed.conn.take() {
                    c.close();
                }
                feed.backoff(&why, name);
            }
        }
        // HL closes sockets idle for 60 s; ping every 30 s.
        if self.hl.opened && self.hl.last_ping.elapsed() > Duration::from_secs(30) {
            self.hl.last_ping = Instant::now();
            if let Some(c) = self.hl.conn.as_mut() {
                let _ = c.queue_text(&[br#"{"method":"ping"}"#]);
                let _ = c.flush(self.poll.registry());
            }
        }
        while let Ok(s) = self.snap_rx.try_recv() {
            self.on_snapshot(s);
        }
    }

    fn drain(&mut self, which: u8) {
        loop {
            let feed = if which == 0 { &mut self.hl } else { &mut self.bn };
            let Some(c) = feed.conn.as_mut() else { return };
            match c.next_frame() {
                WsNext::Idle => return,
                WsNext::Text(span) | WsNext::Binary(span) => {
                    let msg = c.payload(span).to_vec();
                    if which == 0 {
                        self.stats.hl_msgs += 1;
                        self.on_hl(&msg);
                    } else {
                        self.stats.bn_msgs += 1;
                        self.on_bn(&msg);
                    }
                }
                WsNext::Failed(e) => {
                    let name = if which == 0 { "hyperliquid" } else { "binance-um" };
                    feed.backoff(&e.to_string(), name);
                    return;
                }
            }
        }
    }

    fn find(&self, venue: Venue, sym: &[u8]) -> Option<usize> {
        self.ins.iter().position(|i| i.def.venue == venue && i.sym.as_bytes() == sym)
    }

    fn on_trade(&mut self, k: usize, side: Side, px: i64, qty: i64, t_ms: u64) {
        let i = &mut self.ins[k];
        i.trades.push(Trade { ts_ns: t_ms * 1_000_000, px, qty, aggressor: side });
        i.cvd += if side == Side::Buy { qty } else { -qty };
        let bar_ix = (t_ms / 1000 / BAR_TF_S as u64) as i64;
        let d = if side == Side::Buy { qty } else { -qty };
        let open = Bar { instr: k as u16, tf_s: BAR_TF_S, bar: bar_ix, o: px, h: px, l: px, c: px, vol: qty, delta: d };
        match i.bar.as_mut() {
            Some(b) if b.bar == bar_ix => {
                b.h = b.h.max(px);
                b.l = b.l.min(px);
                b.c = px;
                b.vol += qty;
                b.delta += d;
            }
            Some(b) if b.bar > bar_ix => {} // a late print for a closed bar
            Some(_) => {
                if let Some(closed) = i.bar.replace(open) {
                    if i.bars.len() == MAX_BARS {
                        i.bars.remove(0);
                    }
                    i.bars.push(closed);
                }
            }
            None => i.bar = Some(open),
        }
        i.bar_dirty = true;
    }

    fn on_hl(&mut self, msg: &[u8]) {
        match scan::hl(msg) {
            Some(Hl::L2 { coin, levels, .. }) => {
                let Some(k) = self.find(Venue::Hyperliquid, coin) else { return };
                let mut lv = Vec::with_capacity(64);
                if scan::hl_levels(levels, |s, p, q| lv.push((s, p, q))).is_none() {
                    self.stats.bad_msgs += 1;
                    return;
                }
                let i = &mut self.ins[k];
                if !i.tick_known {
                    if let Some(&(_, p, _)) = lv.first() {
                        i.def.tick_1e6 = hl_tick_1e6(p);
                        i.tick_known = true;
                        i.redef = true;
                    }
                }
                i.bids.clear();
                i.asks.clear();
                for (s, p, q) in lv {
                    let m = if s == Side::Buy { &mut i.bids } else { &mut i.asks };
                    if q > 0 {
                        m.insert(p, q);
                    }
                }
                i.book_dirty = true;
            }
            Some(Hl::Trades { data }) => {
                let mut ts = Vec::new();
                for t in scan::items(data) {
                    match scan::hl_trade(t) {
                        Some((coin, s, p, q, tm)) => {
                            if let Some(k) = self.find(Venue::Hyperliquid, coin) {
                                ts.push((k, s, p, q, tm));
                            }
                        }
                        None => self.stats.bad_msgs += 1,
                    }
                }
                for (k, s, p, q, tm) in ts {
                    self.on_trade(k, s, p, q, tm);
                }
            }
            Some(_) => {}
            None => self.stats.bad_msgs += 1,
        }
    }

    fn apply(i: &mut Inst, levels: &[(Side, i64, i64)]) {
        for &(s, p, q) in levels {
            let m = if s == Side::Buy { &mut i.bids } else { &mut i.asks };
            if q == 0 {
                m.remove(&p);
            } else {
                m.insert(p, q);
            }
        }
        i.book_dirty = true;
    }

    fn on_bn(&mut self, msg: &[u8]) {
        match scan::bn(msg) {
            Some(Bn::Depth { sym, first, last, prev, bids, asks }) => {
                let Some(k) = self.find(Venue::Binance, sym) else { return };
                let mut levels = Vec::with_capacity(64);
                let ok = scan::bn_pairs(bids, |p, q| levels.push((Side::Buy, p, q))).and_then(|_| scan::bn_pairs(asks, |p, q| levels.push((Side::Sell, p, q))));
                if ok.is_none() {
                    self.stats.bad_msgs += 1;
                    return;
                }
                let i = &mut self.ins[k];
                let resync = match &mut i.sync {
                    Sync::Waiting(buf) => {
                        if buf.len() >= 2000 {
                            buf.remove(0);
                        }
                        buf.push(BnEv { first, last, prev, levels });
                        false
                    }
                    Sync::Synced(last_u) => {
                        if prev != *last_u {
                            true
                        } else {
                            *last_u = last;
                            Self::apply(i, &levels);
                            false
                        }
                    }
                };
                if resync {
                    self.stats.resyncs += 1;
                    self.ins[k].sync = Sync::Waiting(Vec::new());
                    self.request_snapshot(k);
                }
            }
            Some(Bn::Agg { sym, px, qty, buyer_maker, t_ms }) => {
                if let Some(k) = self.find(Venue::Binance, sym) {
                    self.on_trade(k, if buyer_maker { Side::Sell } else { Side::Buy }, px, qty, t_ms);
                }
            }
            Some(Bn::Other) => {}
            None => self.stats.bad_msgs += 1,
        }
    }

    fn on_snapshot(&mut self, s: Snap) {
        let k = s.instr;
        let (id, lv) = match s.res {
            Ok(x) => x,
            Err(e) => {
                eprintln!("live: binance depth snapshot failed: {e}");
                // 429/418 are rate refusals and 451 a legal block: all stop
                // the venue's feed for the session (T-L6).
                if e.contains("429") || e.contains("418") || e.contains("451") {
                    eprintln!("live: binance-um feed stopped for this session (T-L6)");
                    self.bn.stopped = true;
                    if let Some(mut c) = self.bn.conn.take() {
                        c.close();
                    }
                }
                return;
            }
        };
        let i = &mut self.ins[k];
        let Sync::Waiting(buf) = std::mem::replace(&mut i.sync, Sync::Synced(id)) else { return };
        i.bids.clear();
        i.asks.clear();
        Self::apply(i, &lv);
        let mut last = id;
        let mut first_applied = true;
        let mut broken = false;
        for ev in buf {
            if ev.last < id {
                continue;
            }
            if first_applied {
                if !(ev.first <= id && id <= ev.last) {
                    broken = true;
                    break;
                }
                first_applied = false;
            } else if ev.prev != last {
                broken = true;
                break;
            }
            Self::apply(i, &ev.levels);
            last = ev.last;
        }
        if broken {
            self.stats.resyncs += 1;
            i.sync = Sync::Waiting(Vec::new());
            self.request_snapshot(k);
        } else {
            i.sync = Sync::Synced(last);
        }
    }

    fn top(m: &BTreeMap<i64, i64>, bids: bool) -> Vec<(i64, i64)> {
        if bids {
            m.iter().rev().take(DEPTH).map(|(p, q)| (*p, *q)).collect()
        } else {
            m.iter().take(DEPTH).map(|(p, q)| (*p, *q)).collect()
        }
    }

    fn emit(&mut self, k: usize, out: &mut Vec<u8>) {
        let t = self.t_ns;
        if self.ins[k].redef {
            self.ins[k].redef = false;
            let d = self.ins[k].def;
            let seq = self.seq();
            if let Ok(n) = d.encode(seq, t, &mut self.scratch) {
                out.extend_from_slice(&self.scratch[..n]);
            }
        }
        let synced = !matches!(self.ins[k].sync, Sync::Waiting(_)) || self.ins[k].def.venue != Venue::Binance;
        if self.ins[k].book_dirty && synced && !self.ins[k].bids.is_empty() && !self.ins[k].asks.is_empty() {
            self.ins[k].book_dirty = false;
            let (tb, ta) = (Self::top(&self.ins[k].bids, true), Self::top(&self.ins[k].asks, false));
            if tb[0].0 >= ta[0].0 {
                self.stats.crossed += 1;
            }
            let mut deltas = Vec::new();
            for (side, top, sent) in [(Side::Buy, &tb, &self.ins[k].sent_b), (Side::Sell, &ta, &self.ins[k].sent_a)] {
                let now: BTreeMap<i64, i64> = top.iter().copied().collect();
                for (p, _) in sent.iter() {
                    if !now.contains_key(p) {
                        deltas.push(Delta { side, px: *p, qty: 0 });
                    }
                }
                for (p, q) in &now {
                    if sent.get(p) != Some(q) {
                        deltas.push(Delta { side, px: *p, qty: *q });
                    }
                }
            }
            if !self.ins[k].snapped || deltas.len() > MAX_DELTAS {
                self.snap_book(k, out);
            } else if !deltas.is_empty() {
                let seq = self.seq();
                if let Ok(n) = BookDelta::encode(k as u16, &deltas, seq, t, &mut self.scratch) {
                    out.extend_from_slice(&self.scratch[..n]);
                }
            }
            self.ins[k].sent_b = tb.iter().copied().collect();
            self.ins[k].sent_a = ta.iter().copied().collect();
            self.flow_and_heat(k, &tb, &ta, out);
        }
        if !self.ins[k].trades.is_empty() {
            let tr = std::mem::take(&mut self.ins[k].trades);
            for chunk in tr.chunks(MAX_TRADES) {
                let seq = self.seq();
                if let Ok(n) = TradeBatch::encode(k as u16, chunk, seq, t, &mut self.scratch) {
                    out.extend_from_slice(&self.scratch[..n]);
                }
            }
            self.ins[k].trades = tr;
            self.ins[k].trades.clear();
        }
        if self.ins[k].bar_dirty {
            self.ins[k].bar_dirty = false;
            if let Some(b) = self.ins[k].bar {
                let seq = self.seq();
                if let Ok(n) = b.encode(seq, t, &mut self.scratch) {
                    out.extend_from_slice(&self.scratch[..n]);
                }
            }
        }
    }

    fn snap_book(&mut self, k: usize, out: &mut Vec<u8>) {
        let (tb, ta) = (Self::top(&self.ins[k].bids, true), Self::top(&self.ins[k].asks, false));
        for (side, top) in [(Side::Buy, tb), (Side::Sell, ta)] {
            let lv: Vec<Level> = top.iter().map(|&(px, qty)| Level { px, qty }).collect();
            let n = lv.len() as u32;
            let ch = ChunkHdr { total: n, offset: 0, count: n as u16, last: true };
            let seq = self.seq();
            if let Ok(m) = BookSnap::encode(k as u16, side, ch, &lv, seq, self.t_ns, &mut self.scratch) {
                out.extend_from_slice(&self.scratch[..m]);
            }
        }
        self.ins[k].snapped = true;
    }

    fn flow_and_heat(&mut self, k: usize, tb: &[(i64, i64)], ta: &[(i64, i64)], out: &mut Vec<u8>) {
        let t = self.t_ns;
        let ((bp, bq), (ap, aq)) = (tb[0], ta[0]);
        let i = &mut self.ins[k];
        let (pbp, pbq, pap, paq) = i.prev_l1;
        let e_b = if bp > pbp { bq } else if bp == pbp { bq - pbq } else { -pbq };
        let e_a = if ap < pap { aq } else if ap == pap { aq - paq } else { -paq };
        i.prev_l1 = (bp, bq, ap, aq);
        let sb: i64 = tb.iter().take(5).map(|x| x.1).sum();
        let sa: i64 = ta.iter().take(5).map(|x| x.1).sum();
        let imb = if sb + sa > 0 { ((sb - sa) as i128 * 1_000_000 / (sb + sa) as i128) as i64 } else { 0 };
        let micro = if bq + aq > 0 { ((ap as i128 * bq as i128 + bp as i128 * aq as i128) / (bq + aq) as i128) as i64 } else { (bp + ap) / 2 };
        let fp = FlowPoint { instr: k as u16, cvd: i.cvd, ofi_l1: e_b - e_a, ofi_k: e_b - e_a, imb_1e6: imb, microprice: micro };
        // Heat: rows of `heat_scale` ticks around the mid, ln-quantized.
        let tick = i.def.tick_1e6;
        let mid_tick = (bp + ap) / 2 / tick;
        let base = mid_tick - (HEAT_ROWS as i64 / 2) * i.heat_scale;
        let mut acc = [0i64; HEAT_ROWS];
        for &(p, q) in tb.iter().chain(ta.iter()) {
            let r = (p / tick - base).div_euclid(i.heat_scale);
            if (0..HEAT_ROWS as i64).contains(&r) {
                acc[r as usize] += q;
            }
        }
        let mut cells = [0u8; HEAT_ROWS];
        let cap = (1.0 + i.heat_cap).ln();
        for (c, a) in cells.iter_mut().zip(acc) {
            *c = (((1.0 + a as f64 / 1e6).ln() / cap) * 255.0).clamp(0.0, 255.0) as u8;
        }
        let col = i.col;
        i.col = col.wrapping_add(1);
        let scale = i.heat_scale as u16;
        let seq = self.seq();
        if let Ok(n) = fp.encode(seq, t, &mut self.scratch) {
            out.extend_from_slice(&self.scratch[..n]);
        }
        let hc = HeatCol { instr: k as u16, col, base_tick: base as i32, tick_scale: scale, cells: &cells };
        let seq = self.seq();
        if let Ok(n) = hc.encode(seq, t, &mut self.scratch) {
            out.extend_from_slice(&self.scratch[..n]);
        }
    }
}

impl Source for Live {
    fn hello(&mut self, out: &mut Vec<u8>) {
        for k in 0..self.ins.len() {
            let d = self.ins[k].def;
            let seq = self.seq();
            if let Ok(n) = d.encode(seq, self.t_ns, &mut self.scratch) {
                out.extend_from_slice(&self.scratch[..n]);
            }
        }
        let st = EngineStatus { boot_epoch: 0, git_sha: Ascii::new(b"0").unwrap_or_else(|_| unreachable!()), engine_up: false, n: 0, slots: [SlotState::default(); 16] };
        let seq = self.seq();
        if let Ok(n) = st.encode(seq, self.t_ns, &mut self.scratch) {
            out.extend_from_slice(&self.scratch[..n]);
        }
    }

    fn snapshot(&mut self, instr: u16, out: &mut Vec<u8>) {
        let k = instr as usize;
        if k >= self.ins.len() {
            return;
        }
        if !self.ins[k].bids.is_empty() && !self.ins[k].asks.is_empty() {
            self.snap_book(k, out);
        }
        let bars: Vec<Bar> = self.ins[k].bars.iter().copied().chain(self.ins[k].bar).collect();
        for b in bars {
            let seq = self.seq();
            if let Ok(n) = b.encode(seq, self.t_ns, &mut self.scratch) {
                out.extend_from_slice(&self.scratch[..n]);
            }
        }
    }

    fn tick(&mut self, t_ns: u64, out: &mut Vec<u8>) {
        self.t_ns = t_ns;
        self.pump();
        for k in 0..self.ins.len() {
            self.emit(k, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_bn() -> Live {
        let mut l = Live::new(&LiveCfg { hl: vec![], bn: vec!["BTCUSDT".into()], extra_ca: vec![] }).unwrap();
        l.bn.stopped = true; // no network in tests: request_snapshot is a no-op
        l
    }

    fn depth(first: u64, last: u64, prev: u64, bid: (&str, &str)) -> Vec<u8> {
        format!(r#"{{"stream":"btcusdt@depth@100ms","data":{{"e":"depthUpdate","E":1,"T":1,"s":"BTCUSDT","U":{first},"u":{last},"pu":{prev},"b":[["{}","{}"]],"a":[]}}}}"#, bid.0, bid.1).into_bytes()
    }

    #[test]
    fn hl_ticks() {
        assert_eq!(hl_tick_1e6(77_998_000_000), 1_000_000);
        assert_eq!(hl_tick_1e6(2_269_100_000), 100_000);
        assert_eq!(hl_tick_1e6(152_340_000), 10_000);
        assert_eq!(hl_tick_1e6(1), 1);
    }

    #[test]
    fn binance_sync_follows_the_documented_rule() {
        let mut l = live_bn();
        l.on_bn(&depth(90, 95, 89, ("100.0", "1")));   // u < lastUpdateId: dropped
        l.on_bn(&depth(98, 103, 95, ("101.0", "2")));  // straddles 100: first applied
        l.on_bn(&depth(104, 110, 103, ("102.0", "3"))); // pu = previous u
        let snap = vec![(Side::Buy, 99_000_000, 5_000_000), (Side::Sell, 200_000_000, 1_000_000)];
        l.on_snapshot(Snap { instr: 0, res: Ok((100, snap)) });
        assert!(matches!(l.ins[0].sync, Sync::Synced(110)));
        assert_eq!(l.ins[0].bids.get(&100_000_000), None, "dropped event not applied");
        assert_eq!(l.ins[0].bids.get(&101_000_000), Some(&2_000_000));
        assert_eq!(l.ins[0].bids.get(&102_000_000), Some(&3_000_000));
        // Live: a pu gap resyncs.
        l.on_bn(&depth(111, 115, 110, ("103.0", "1")));
        assert!(matches!(l.ins[0].sync, Sync::Synced(115)));
        l.on_bn(&depth(120, 125, 119, ("104.0", "1")));
        assert!(matches!(l.ins[0].sync, Sync::Waiting(_)));
        assert_eq!(l.stats.resyncs, 1);
    }

    #[test]
    fn binance_sync_refuses_a_snapshot_the_events_do_not_straddle() {
        let mut l = live_bn();
        l.on_bn(&depth(105, 110, 104, ("101.0", "2"))); // starts after lastUpdateId 100
        l.on_snapshot(Snap { instr: 0, res: Ok((100, vec![(Side::Buy, 99_000_000, 1)])) });
        assert!(matches!(l.ins[0].sync, Sync::Waiting(_)));
        assert_eq!(l.stats.resyncs, 1);
    }

    #[test]
    fn a_legal_block_stops_the_feed() {
        let mut l = live_bn();
        l.bn.stopped = false;
        l.on_snapshot(Snap { instr: 0, res: Err("non-200 status: 451".into()) });
        assert!(l.bn.stopped);
    }
}
