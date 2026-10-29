// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # pm15_probe — the PM15 rolling ingress on its own, against the venue
//!
//! `multivenue-engine pm15-probe --assets btc,eth --seconds 300` runs the
//! two PM15 threads exactly as the engine spawns them (`cli::spawn_pm15`'s
//! parts: [`ingress_polymarket::pm15_discovery`] and
//! [`ingress_polymarket::pm15_loop`]) with no engine behind the lane: this
//! thread drains the tick and event rings itself and prints one JSON line
//! every 10 s and a final one — the counters, the bound slots, what came
//! down the lane per channel, the last mark of each family and the last
//! touch of each leg. It touches no account, places nothing and writes no
//! capture (records are counted, not stored), so it can run beside the
//! live engine: the verification of the venue path before a deploy.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use core_ring::Ring;
use core_time::{now_ns, WallAnchor};
use core_types::pm_roll::{pm_rolling_of, PM_OFF_MARK};
use core_types::{ChannelEvent, ChannelId, NullCapture, Tick, EVENT_RING_SIZE};
use ingress_polymarket::gamma15::PmWindowSpec;
use ingress_polymarket::pm15_discovery::Pm15Discovery;
use ingress_polymarket::pm15_loop::{
    run_pm15, Pm15Counters, Pm15Driver, Pm15Out, Pm15Sockets, PM15_COUNTERS, PM15_COUNTER_NAMES,
    PM15_EVENT_MASK, PM15_MARKET_HOST, PM15_MARKET_PATH, PM15_MARKET_TOKEN, PM15_RTDS_TOKEN,
    PM15_SPEC_RING,
};
use ingress_polymarket::rtds::{RTDS_HOST, RTDS_PATH};
use ingress_polymarket::DEFAULT_TICK_RING_CAP;

/// Longest probe, s.
pub const PROBE_SECONDS_MAX: u64 = 3_600;

/// Run the probe (module docs). `assets` are the families' assets (the
/// slug's spelling), `seconds` its length.
///
/// # Errors
/// A refused argument, a host that does not resolve, or a thread that
/// could not start — the reason as text.
pub fn run(assets: &[String], seconds: u64) -> Result<(), String> {
    if assets.is_empty() || assets.len() > core_types::pm_roll::PM_ROLLING_MAX {
        return Err(format!(
            "1..={} assets",
            core_types::pm_roll::PM_ROLLING_MAX
        ));
    }
    if seconds == 0 || seconds > PROBE_SECONDS_MAX {
        return Err(format!("--seconds 1..={PROBE_SECONDS_MAX}"));
    }
    let tls = core_net::TlsTransport::default_client_config();
    let refs: Vec<&[u8]> = assets.iter().map(|a| a.as_bytes()).collect();
    let mut discovery = Pm15Discovery::new(tls.clone(), &refs)
        .map_err(|e| format!("discovery clients (gamma, clob): {e:?}"))?;
    let seed = now_ns();
    let market = core_net::WsConn::new(
        PM15_MARKET_HOST,
        443,
        PM15_MARKET_PATH,
        tls.clone(),
        crate::paper::PM15_MARKET_WS,
        PM15_MARKET_TOKEN,
        seed,
    )
    .map_err(|e| format!("market socket: {e}"))?;
    let rtds = core_net::WsConn::new(
        RTDS_HOST,
        443,
        RTDS_PATH,
        tls,
        crate::paper::PM15_RTDS_WS,
        PM15_RTDS_TOKEN,
        seed.rotate_left(17),
    )
    .map_err(|e| format!("rtds socket: {e}"))?;
    let mut driver = Pm15Driver::new(&refs, 5_000);
    drop(refs);
    let wall = WallAnchor::now();
    let stop = Arc::new(AtomicBool::new(false));
    let counters = Arc::new(Pm15Counters::new());
    let status = Arc::new(core_metrics::IngressStatus::new());
    let (mut spec_tx, mut spec_rx) = Ring::<PmWindowSpec, PM15_SPEC_RING>::new().split();
    let (mut tick_tx, mut tick_rx) = Ring::<Tick, DEFAULT_TICK_RING_CAP>::new().split();
    let (mut event_tx, mut event_rx) = Ring::<ChannelEvent, EVENT_RING_SIZE>::new().split();

    let (d_stop, d_counters) = (stop.clone(), counters.clone());
    let h_disc = thread::Builder::new()
        .name("pm15-discovery".into())
        .spawn(move || discovery.run(wall, &mut spec_tx, &d_counters, &d_stop))
        .map_err(|e| format!("spawn discovery: {e}"))?;
    let (i_stop, i_counters, i_status) = (stop.clone(), counters.clone(), status.clone());
    let h_ing = thread::Builder::new()
        .name("ingress-pm15".into())
        .spawn(move || {
            // One market socket: the probe's driver keeps the default (PMJ J7).
            let mut socks = Pm15Sockets {
                market: vec![market],
                rtds,
            };
            let mut capture = NullCapture;
            let mut out = Pm15Out {
                tick: &mut tick_tx,
                event: &mut event_tx,
                event_mask: PM15_EVENT_MASK,
                status: &i_status,
                counters: &i_counters,
                capture: &mut capture,
                depth: None,
            };
            if let Err(e) = run_pm15(
                &mut driver,
                &mut socks,
                &mut spec_rx,
                &mut out,
                wall,
                seed,
                &i_stop,
            ) {
                eprintln!("pm15-probe: poll failed: {e}");
            }
        })
        .map_err(|e| format!("spawn ingress: {e}"))?;

    let mut tally = Tally::default();
    let end = now_ns().saturating_add(seconds.saturating_mul(1_000_000_000));
    let mut next_report = now_ns().saturating_add(10_000_000_000);
    while now_ns() < end {
        thread::sleep(Duration::from_millis(100));
        while let Some(t) = tick_rx.try_pop_ref() {
            tally.tick(&t);
        }
        while let Some(e) = event_rx.try_pop_ref() {
            tally.event(&e);
        }
        if now_ns() >= next_report {
            next_report = next_report.saturating_add(10_000_000_000);
            println!("{}", tally.line(&counters, &status, false));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = h_ing.join();
    let _ = h_disc.join();
    println!("{}", tally.line(&counters, &status, true));
    Ok(())
}

/// What came down the lane.
#[derive(Default)]
struct Tally {
    ticks: u64,
    by_channel: [u64; 16],
    /// Last mark per family: (second, px ×1e6).
    marks: [(u64, i64); core_types::pm_roll::PM_ROLLING_MAX],
    /// Last touch per family offset 0..4: (bid, bid size, ask, ask size).
    touch: [[(i64, i64, i64, i64); 4]; core_types::pm_roll::PM_ROLLING_MAX],
    rolls_created: u64,
    rolls_settled: u64,
    last_settled: Option<(u32, i64)>,
}

impl Tally {
    fn tick(&mut self, t: &Tick) {
        self.ticks += 1;
        if let Some((f, off)) = pm_rolling_of(t.sym) {
            if off < PM_OFF_MARK {
                self.touch[f][off as usize] = (
                    t.bid_px.raw(),
                    t.bid_qty.raw(),
                    t.ask_px.raw(),
                    t.ask_qty.raw(),
                );
            }
        }
    }

    fn event(&mut self, e: &ChannelEvent) {
        self.by_channel[(e.channel & 15) as usize] += 1;
        if e.channel == ChannelId::Mark as u8 {
            if let Some((f, _)) = pm_rolling_of(e.sym) {
                self.marks[f] = (e.venue_seq, e.v0);
            }
        } else if e.channel == ChannelId::InstrumentRoll as u8 {
            let (inst, _, _, _) = core_types::unpack_roll_seq(e.venue_seq);
            if core_types::roll_kind(e.venue_seq) == core_types::ROLL_KIND_SETTLED {
                self.rolls_settled += 1;
                self.last_settled = Some((inst, e.v0));
            } else {
                self.rolls_created += 1;
            }
        }
    }

    fn line(&self, c: &Pm15Counters, st: &core_metrics::IngressStatus, last: bool) -> String {
        let mut s = String::with_capacity(2048);
        let _ = write!(
            s,
            "{{\"final\":{last},\"state\":{},\"msgs\":{},\"parse_errors\":{},\"reconnects\":{},\"ring_drops\":{},\"ticks\":{},\"slots_bound\":{}",
            st.state() as u8,
            st.msgs_total(),
            st.parse_errors_total(),
            st.reconnects_total(),
            st.ring_drops_total(),
            self.ticks,
            c.slots_bound()
        );
        let mut i = 0usize;
        while i < PM15_COUNTERS {
            let _ = write!(s, ",\"{}\":{}", PM15_COUNTER_NAMES[i], c.get(i));
            i += 1;
        }
        let _ = write!(
            s,
            ",\"lane\":{{\"roll\":{},\"order_law\":{},\"mark\":{},\"trade\":{}}},\"rolls_created\":{},\"rolls_settled\":{}",
            self.by_channel[ChannelId::InstrumentRoll as usize],
            self.by_channel[ChannelId::OutcomeMeta as usize],
            self.by_channel[ChannelId::Mark as usize],
            self.by_channel[ChannelId::Trade as usize],
            self.rolls_created,
            self.rolls_settled
        );
        if let Some((inst, w)) = self.last_settled {
            let _ = write!(
                s,
                ",\"last_settled\":{{\"instance\":{inst},\"winner\":{w}}}"
            );
        }
        if let Some(e) = c.last_ws_err(0) {
            let _ = write!(s, ",\"last_market_err\":{e}");
        }
        if let Some(e) = c.last_ws_err(1) {
            let _ = write!(s, ",\"last_rtds_err\":{e}");
        }
        if let Some((f, cause)) = c.last_law_refusal() {
            let _ = write!(
                s,
                ",\"last_law_refusal\":{{\"family\":{f},\"cause\":{cause}}}"
            );
        }
        s.push_str(",\"families\":[");
        let mut f = 0usize;
        while f < core_types::pm_roll::PM_ROLLING_MAX {
            if f > 0 {
                s.push(',');
            }
            let (sec, px) = self.marks[f];
            let _ = write!(s, "{{\"mark_s\":{sec},\"mark_1e6\":{px},\"touch\":[");
            let mut k = 0usize;
            while k < 4 {
                if k > 0 {
                    s.push(',');
                }
                let (b, bs, a, as_) = self.touch[f][k];
                let _ = write!(s, "[{b},{bs},{a},{as_}]");
                k += 1;
            }
            s.push_str("]}");
            f += 1;
        }
        s.push_str("]}");
        s
    }
}
