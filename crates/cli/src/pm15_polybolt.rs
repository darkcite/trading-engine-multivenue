// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # pm15_polybolt — PolyBolt's reference prices, seen live (PM15-7)
//!
//! `multivenue-engine pm15-polybolt-probe --assets btc,eth --seconds 120`
//! opens PolyBolt (`wss://ws-live-v2.polymarket.com/ws`) on the account's
//! CLOB API credentials — `PM15_API_KEY`, `PM15_API_SECRET`,
//! `PM15_API_PASSPHRASE`, sourced by `scripts/pm15-exec.sh polybolt-probe`
//! and never printed — subscribes `price.crypto` and `price.crypto.twap`
//! (60 s) for each asset, and prints JSON lines: the first frames raw (a
//! frame carrying the API key is withheld), one line every 10 s and a
//! final one — per asset the last price and TWAP ×1e6, their stamps and
//! sequences, and what the dormant parser
//! ([`ingress_polymarket::polybolt`]) made of every frame. No order, no
//! engine: it confirms the live shapes before PolyBolt becomes slot 9's
//! mark (R-PM-3).

use std::fmt::Write as _;

use core_net::{TlsTransport, WsCfg, WsConn, WsNext, WsProgress};
use core_time::now_ns;
use ingress_polymarket::polybolt::{
    parse_polybolt, render_auth, render_subscribe, PbFrame, PbReport, PB_ASSET_MAX, POLYBOLT_HOST,
    POLYBOLT_PATH, POLYBOLT_SUBS_MAX, POLYBOLT_TWAP_S,
};

use crate::pm15_live::{env, ENV_API_KEY, ENV_API_PASSPHRASE, ENV_API_SECRET};

/// Longest probe, s.
pub const PROBE_SECONDS_MAX: u64 = 3_600;
/// Frames printed raw.
const RAW_FRAMES: usize = 8;
/// Bytes of a raw frame printed.
const RAW_MAX: usize = 400;
/// Between two report lines, ns.
const REPORT_EVERY_NS: u64 = 10_000_000_000;

/// The socket: frames up to the venue's 64 KB, the server's ping every
/// 25 s (two missed and a margin make it dead).
pub const POLYBOLT_WS: WsCfg = WsCfg {
    rx_cap: 256 * 1024,
    tx_cap: 16 * 1024,
    establish_ns: 10_000_000_000,
    idle_ns: 60_000_000_000,
};

#[derive(Default, Clone, Copy)]
struct Last {
    px_1e6: i64,
    ts_ms: u64,
    seq: u64,
    n: u64,
}

#[derive(Default)]
struct Tally {
    frames: u64,
    other: u64,
    empty: u64,
    malformed: u64,
    unknown_asset: u64,
    raw: usize,
}

/// Run the probe (module docs).
///
/// # Errors
/// A refused argument, a missing credential, or the socket failing — the
/// reason as text.
pub fn run(assets: &[String], seconds: u64) -> Result<(), String> {
    if assets.is_empty() || 2 * assets.len() > POLYBOLT_SUBS_MAX {
        return Err(format!("1..={} assets", POLYBOLT_SUBS_MAX / 2));
    }
    if seconds == 0 || seconds > PROBE_SECONDS_MAX {
        return Err(format!("--seconds 1..={PROBE_SECONDS_MAX}"));
    }
    let refs: Vec<&[u8]> = assets.iter().map(|a| a.as_bytes()).collect();
    if refs
        .iter()
        .any(|a| a.len() > PB_ASSET_MAX || !a.iter().all(u8::is_ascii_lowercase))
    {
        return Err(String::from("assets are lowercase, e.g. btc,eth"));
    }
    let key = env(ENV_API_KEY)?;
    let secret = env(ENV_API_SECRET)?;
    let pass = env(ENV_API_PASSPHRASE)?;
    let mut auth = [0u8; 512];
    let na = render_auth(
        &mut auth,
        key.trim().as_bytes(),
        secret.trim().as_bytes(),
        pass.trim().as_bytes(),
    )
    .ok_or_else(|| String::from("the API credentials are not plain tokens"))?;
    let mut sub = [0u8; 8 * 1024];
    let ns = render_subscribe(&mut sub, &refs, POLYBOLT_TWAP_S)
        .ok_or_else(|| String::from("the subscribe frame does not render"))?;
    let tls = TlsTransport::default_client_config();
    let mut ws = WsConn::new(
        POLYBOLT_HOST,
        443,
        POLYBOLT_PATH,
        tls,
        POLYBOLT_WS,
        mio::Token(0),
        now_ns(),
    )
    .map_err(|e| format!("the PolyBolt socket: {e}"))?;
    let mut poll = mio::Poll::new().map_err(|e| format!("poll: {e}"))?;
    let mut events = mio::Events::with_capacity(16);
    ws.connect(poll.registry(), now_ns())
        .map_err(|e| format!("connect: {e:?}"))?;
    let mut price = vec![Last::default(); refs.len()];
    let mut twap = vec![Last::default(); refs.len()];
    let mut tally = Tally::default();
    let end = now_ns().saturating_add(seconds.saturating_mul(1_000_000_000));
    let mut next_report = now_ns().saturating_add(REPORT_EVERY_NS);
    let mut closed: Option<String> = None;
    while now_ns() < end && closed.is_none() {
        poll.poll(&mut events, Some(std::time::Duration::from_millis(100)))
            .map_err(|e| format!("poll: {e}"))?;
        for ev in events.iter() {
            match ws.on_event(ev, poll.registry(), now_ns()) {
                WsProgress::Opened => {
                    println!(r#"{{"event":"open","host":"{POLYBOLT_HOST}"}}"#);
                    if ws.queue_text(&[&auth[..na]]).is_err()
                        || ws.queue_text(&[&sub[..ns]]).is_err()
                    {
                        closed = Some(String::from("the auth or subscribe frame did not queue"));
                    }
                }
                WsProgress::Readable => loop {
                    match ws.next_frame() {
                        WsNext::Text(span) => {
                            let f = ws.payload(span);
                            take(
                                f,
                                key.trim().as_bytes(),
                                &refs,
                                &mut price,
                                &mut twap,
                                &mut tally,
                            );
                        }
                        WsNext::Binary(_) => tally.other += 1,
                        WsNext::Idle => break,
                        WsNext::Failed(e) => {
                            closed = Some(format!("{e:?}"));
                            break;
                        }
                    }
                },
                WsProgress::Failed(e) => closed = Some(format!("{e:?}")),
                WsProgress::Waiting => {}
            }
        }
        if let WsProgress::Failed(e) = ws.on_tick(now_ns()) {
            closed = Some(format!("{e:?}"));
        }
        if ws.wants_flush() && ws.flush(poll.registry()).is_err() {
            closed = Some(String::from("flush failed"));
        }
        if now_ns() >= next_report {
            next_report = next_report.saturating_add(REPORT_EVERY_NS);
            println!("{}", line(&refs, &price, &twap, &tally, false, None));
        }
    }
    ws.close();
    println!(
        "{}",
        line(&refs, &price, &twap, &tally, true, closed.as_deref())
    );
    Ok(())
}

fn take(
    f: &[u8],
    key: &[u8],
    refs: &[&[u8]],
    price: &mut [Last],
    twap: &mut [Last],
    t: &mut Tally,
) {
    t.frames += 1;
    if t.raw < RAW_FRAMES {
        t.raw += 1;
        if !key.is_empty() && f.windows(key.len()).any(|w| w == key) {
            println!(
                r#"{{"raw":{},"bytes":{},"text":"<withheld: it names the API key>"}}"#,
                t.raw,
                f.len()
            );
        } else {
            println!(
                r#"{{"raw":{},"bytes":{},"text":"{}"}}"#,
                t.raw,
                f.len(),
                json_escape(&f[..f.len().min(RAW_MAX)])
            );
        }
    }
    let mut r = PbReport::default();
    let kind = parse_polybolt(f, &mut r);
    let table = match kind {
        PbFrame::Price => price,
        PbFrame::Twap => twap,
        PbFrame::Empty => {
            t.empty += 1;
            return;
        }
        PbFrame::Other => {
            t.other += 1;
            return;
        }
        PbFrame::Malformed => {
            t.malformed += 1;
            return;
        }
    };
    let asset = r.px.asset(f);
    let Some(i) = refs.iter().position(|a| *a == asset) else {
        t.unknown_asset += 1;
        return;
    };
    table[i] = Last {
        px_1e6: r.px.px_1e6,
        ts_ms: r.px.ts_ms,
        seq: r.seq,
        n: table[i].n + 1,
    };
}

fn line(
    refs: &[&[u8]],
    price: &[Last],
    twap: &[Last],
    t: &Tally,
    last: bool,
    closed: Option<&str>,
) -> String {
    let mut s = String::with_capacity(512);
    let _ = write!(
        s,
        r#"{{"final":{last},"frames":{},"other":{},"empty":{},"malformed":{},"unknown_asset":{},"assets":{{"#,
        t.frames, t.other, t.empty, t.malformed, t.unknown_asset
    );
    for (i, a) in refs.iter().enumerate() {
        let p = price[i];
        let w = twap[i];
        let _ = write!(
            s,
            r#"{}"{}":{{"price_1e6":{},"price_ts_ms":{},"price_seq":{},"prices":{},"twap_1e6":{},"twap_ts_ms":{},"twap_seq":{},"twaps":{}}}"#,
            if i > 0 { "," } else { "" },
            String::from_utf8_lossy(a),
            p.px_1e6,
            p.ts_ms,
            p.seq,
            p.n,
            w.px_1e6,
            w.ts_ms,
            w.seq,
            w.n
        );
    }
    s.push('}');
    if let Some(c) = closed {
        let _ = write!(s, r#","closed":"{}""#, json_escape(c.as_bytes()));
    }
    s.push('}');
    s
}

fn json_escape(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() + 8);
    for &c in b {
        match c {
            b'"' => s.push_str("\\\""),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(c as char),
            _ => {
                let _ = write!(s, "\\u{c:04x}");
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_tallied_by_asset_and_channel_and_the_key_is_withheld() {
        let refs: Vec<&[u8]> = vec![b"btc", b"eth"];
        let mut price = vec![Last::default(); 2];
        let mut twap = vec![Last::default(); 2];
        let mut t = Tally::default();
        let f = br#"{"v":1,"channel":"price.crypto","seq":3,"payload":{"symbol":"ethusd","value":"2500.5","timestamp":1}}"#;
        take(f, b"key-1", &refs, &mut price, &mut twap, &mut t);
        assert_eq!(
            (price[1].px_1e6, price[1].seq, price[1].n),
            (2_500_500_000, 3, 1)
        );
        take(br#"{"channel":"price.crypto","payload":{"symbol":"solusd","value":"1","timestamp":1}}"#, b"key-1", &refs, &mut price, &mut twap, &mut t);
        assert_eq!(t.unknown_asset, 1);
        take(
            br#"{"op":"auth","apiKey":"key-1"}"#,
            b"key-1",
            &refs,
            &mut price,
            &mut twap,
            &mut t,
        );
        assert_eq!((t.frames, t.other, t.raw), (3, 1, 3));
        let l = line(&refs, &price, &twap, &t, true, Some("closed \"4001\""));
        assert!(l.contains(r#""eth":{"price_1e6":2500500000"#), "{l}");
        assert!(l.ends_with(r#""closed":"closed \"4001\""}"#), "{l}");
        assert_eq!(json_escape(b"a\"\\\n"), "a\\\"\\\\\\u000a");
    }
}
