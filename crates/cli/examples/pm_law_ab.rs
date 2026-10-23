// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! PMJ J12 proof 4 — an A/B of the REAL slot 9 / slot 11 members on
//! captured runs: A = the first strike law (`mean60`), B = law (iii)
//! (`trap62` with its residual), both booted through the cli's own
//! `load_pm15_boot` / `load_pmj_boot` from two artifacts.
//!
//! Cold research driver: offline, allocates freely, never loaded by the
//! engine. For each run it
//!
//! 1. reads the run's `instrument-manifest.tsv` (the resolve table and the
//!    `[polymarket] rolling` families, in ingress order);
//! 2. boots one member per artifact with the run's wall clock
//!    (`backtest::clock::RunClock`: the venue law from the Hyperliquid
//!    ticks' venue stamps, else the anchor law);
//! 3. feeds both members the same merged timeline in capture order — the
//!    period's events (`pm15-` / `pm5-events`: rolls, order laws, the
//!    Chainlink reports) and leg ticks, plus every family's perp ticks —
//!    with a 1 s timer, as the engine's strategy set would;
//! 4. records every order each member submits (no fill is fed back: the
//!    judge prices the orders on the captured depth afterwards) and each
//!    member's counters at the run's end.
//!
//! Usage:
//!
//! ```text
//! cargo run --release -p cli --example pm_law_ab -- \
//!     <pm15|pmj> <A.toml> <B.toml> <runs.tsv> <orders.tsv> <counters.tsv>
//! ```
//!
//! `runs.tsv`: one run per line, `<run_dir>[\t<perp_dir>]`; the perp tick
//! files (`hl-ticks.pmlr`, `bn-ticks.pmlr`) are read from `perp_dir` when
//! given, else from the run dir. A perp file that is absent leaves its
//! families without a nowcast (they hold, `hold_nowcast`), which the
//! counters show.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use cli::backtest::clock::{RunClock, VenueOffsetFit};
use core_io::PmlrReader;
use core_time::WallAnchor;
use core_types::{ChannelEvent, NsTs, Order, SymbolId, Tick};
use strategy_core::{Ctx, Pm15Counters, Strategy, SubmitErr};

/// The member's timer period, ns (`timer_period_ns` of both members).
const TIMER_NS: u64 = 1_000_000_000;
/// Laws, as the output names them.
const LAWS: [&str; 2] = ["mean60", "trap62"];

/// A `Ctx` that records every submit and answers the record's clock.
struct RecCtx {
    now: NsTs,
    orders: Vec<Order>,
}

impl Ctx for RecCtx {
    fn submit(&mut self, order: Order) -> Result<(), SubmitErr> {
        self.orders.push(order);
        Ok(())
    }
    fn now_ns(&self) -> NsTs {
        self.now
    }
}

/// The two members this driver knows.
trait Member: Strategy {
    fn counters_of(&self) -> Pm15Counters;
}

impl Member for strategy_pm15::Pm15Strategy {
    fn counters_of(&self) -> Pm15Counters {
        self.counters()
    }
}

impl Member for strategy_pmj::PmjStrategy {
    fn counters_of(&self) -> Pm15Counters {
        self.counters()
    }
}

/// `(raw sym, descriptor)` of a run's manifest.
fn manifest(dir: &Path) -> Result<Vec<(SymbolId, String)>, String> {
    let p = dir.join("instrument-manifest.tsv");
    let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut out = Vec::new();
    for line in text.lines() {
        let mut it = line.splitn(2, '\t');
        let (Some(a), Some(b)) = (it.next(), it.next()) else {
            continue;
        };
        if let Ok(sym) = a.trim().parse::<u64>() {
            if let Ok(sym) = SymbolId::try_from(sym) {
                out.push((sym, b.trim().to_string()));
            }
        }
    }
    Ok(out)
}

/// The run's `[polymarket] rolling` keys of one period, in ingress
/// (family) order: `updown:<asset>:<period>` from the `[mark]` slots.
fn rolling(man: &[(SymbolId, String)], pm15: bool) -> Vec<String> {
    let (base, stride, period) = if pm15 { (4096u32, 5u32, "15m") } else { (4160u32, 7u32, "5m") };
    let mut fams: Vec<(u32, String)> = Vec::new();
    for (sym, d) in man {
        let Some(rest) = d.strip_prefix("polymarket-rolling:") else {
            continue;
        };
        let Some(key) = rest.strip_suffix("[mark]") else {
            continue;
        };
        if !key.ends_with(&format!(":{period}")) || *sym < base {
            continue;
        }
        let off = *sym - base;
        if off % stride == stride - 1 {
            fams.push((off / stride, key.to_string()));
        }
    }
    fams.sort();
    fams.into_iter().map(|(_, k)| k).collect()
}

/// Boot one member from `artifact`; `None` with the reason when it refuses.
fn boot(
    pm15: bool,
    artifact: &Path,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    keys: &[String],
    anchor: WallAnchor,
) -> Result<Box<dyn std::any::Any>, String> {
    if pm15 {
        let b = cli::pm15_boot::load_pm15_boot(Some(artifact), resolve, keys)?
            .ok_or_else(|| String::from("pm15: no artifact"))?;
        let mut m = strategy_pm15::Pm15Strategy::new();
        m.configure(b.params, b.lut, anchor).map_err(|e| format!("pm15 configure: {e:?}"))?;
        Ok(Box::new(m))
    } else {
        let b = cli::pmj_boot::load_pmj_boot(Some(artifact), resolve, keys)?
            .ok_or_else(|| String::from("pmj: no artifact"))?;
        let mut m = strategy_pmj::PmjStrategy::new();
        m.configure(b.params, b.lut, anchor).map_err(|e| format!("pmj configure: {e:?}"))?;
        Ok(Box::new(m))
    }
}

/// One record of the merged timeline.
enum Rec<'a> {
    Ev(&'a ChannelEvent),
    Tk(&'a Tick),
}

/// Drive both members over the merged streams; returns their orders.
fn drive<M: Member>(
    a: &mut M,
    b: &mut M,
    events: &[ChannelEvent],
    legs: &[Tick],
    perps: &[&[Tick]],
    perp_syms: &[SymbolId],
) -> [RecCtx; 2] {
    let mut ca = RecCtx { now: 0, orders: Vec::new() };
    let mut cb = RecCtx { now: 0, orders: Vec::new() };
    let _ = a.on_start(&mut ca);
    let _ = b.on_start(&mut cb);
    let mut ie = 0usize;
    let mut il = 0usize;
    let mut ip = vec![0usize; perps.len()];
    let mut next_timer: u64 = 0;
    loop {
        // the next perp tick of a wanted sym, per stream
        let mut best: Option<(u64, usize)> = None; // (ts, stream: 0 events, 1 legs, 2+ perps)
        if ie < events.len() {
            best = Some((events[ie].ts_ns, 0));
        }
        if il < legs.len() && best.is_none_or(|(t, _)| legs[il].ts_ns < t) {
            best = Some((legs[il].ts_ns, 1));
        }
        for (k, s) in perps.iter().enumerate() {
            while ip[k] < s.len() && !perp_syms.contains(&s[ip[k]].sym) {
                ip[k] += 1;
            }
            if ip[k] < s.len() && best.is_none_or(|(t, _)| s[ip[k]].ts_ns < t) {
                best = Some((s[ip[k]].ts_ns, 2 + k));
            }
        }
        let Some((ts, stream)) = best else {
            break;
        };
        let rec = match stream {
            0 => {
                ie += 1;
                Rec::Ev(&events[ie - 1])
            }
            1 => {
                il += 1;
                Rec::Tk(&legs[il - 1])
            }
            k => {
                ip[k - 2] += 1;
                Rec::Tk(&perps[k - 2][ip[k - 2] - 1])
            }
        };
        ca.now = ts;
        cb.now = ts;
        if next_timer == 0 {
            next_timer = ts + TIMER_NS;
        } else if ts >= next_timer {
            a.on_timer(ts, &mut ca);
            b.on_timer(ts, &mut cb);
            next_timer = ts + TIMER_NS;
        }
        match rec {
            Rec::Ev(e) => {
                a.on_venue_event(e, &mut ca);
                b.on_venue_event(e, &mut cb);
            }
            Rec::Tk(t) => {
                a.on_tick(t, &mut ca);
                b.on_tick(t, &mut cb);
            }
        }
    }
    [ca, cb]
}

fn run_dir_epoch(dir: &Path) -> u64 {
    dir.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("run-"))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0)
}

fn open_ticks(p: &Path) -> Option<PmlrReader<Tick>> {
    if !p.exists() {
        return None;
    }
    PmlrReader::<Tick>::open(p).ok()
}

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 7 {
        return Err(String::from(
            "usage: pm_law_ab <pm15|pmj> <A.toml> <B.toml> <runs.tsv> <orders.tsv> <counters.tsv>",
        ));
    }
    let pm15 = match args[1].as_str() {
        "pm15" => true,
        "pmj" => false,
        other => return Err(format!("slot `{other}`: pm15 or pmj")),
    };
    let arts = [PathBuf::from(&args[2]), PathBuf::from(&args[3])];
    let runs_text = std::fs::read_to_string(&args[4]).map_err(|e| format!("{}: {e}", args[4]))?;
    let mut out = BufWriter::new(File::create(&args[5]).map_err(|e| e.to_string())?);
    let mut cnt = BufWriter::new(File::create(&args[6]).map_err(|e| e.to_string())?);
    writeln!(out, "law\tslot\trun\twall_ns\tts_ns\tinstance\tfamily\tleg\tsym\tpx_1e6\tqty_1e6\tclient_oid\tvenue")
        .map_err(|e| e.to_string())?;
    writeln!(cnt, "law\tslot\trun\tclock\tperp_files\tcounters").map_err(|e| e.to_string())?;
    let slot = if pm15 { 9 } else { 11 };
    let prefix = if pm15 { "pm15" } else { "pm5" };
    for line in runs_text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split('\t');
        let dir = PathBuf::from(it.next().unwrap_or(""));
        let perp_dir = it.next().map_or_else(|| dir.clone(), PathBuf::from);
        let run = dir.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
        let man = match manifest(&dir) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("{run}: skipped ({e})");
                continue;
            }
        };
        let keys = rolling(&man, pm15);
        let table: HashMap<String, SymbolId> = man.iter().map(|(s, d)| (d.clone(), *s)).collect();
        let resolve = |d: &str| table.get(d).copied();
        let ev = match PmlrReader::<ChannelEvent>::open(dir.join(format!("{prefix}-events.pmlr"))) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{run}: skipped (events: {e})");
                continue;
            }
        };
        let legs = match PmlrReader::<Tick>::open(dir.join(format!("{prefix}-ticks.pmlr"))) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{run}: skipped (leg ticks: {e})");
                continue;
            }
        };
        let hl = open_ticks(&perp_dir.join("hl-ticks.pmlr"));
        let bn = open_ticks(&perp_dir.join("bn-ticks.pmlr"));
        let perp_files = format!(
            "hl={} bn={}",
            if hl.is_some() { "yes" } else { "no" },
            if bn.is_some() { "yes" } else { "no" }
        );
        // the clock: the venue law from the HL ticks, else the anchor law
        let mut fit = VenueOffsetFit::new();
        if let Some(h) = &hl {
            for t in h.records() {
                if fit.full() {
                    break;
                }
                fit.observe(t);
            }
        }
        let mut ts_first = u64::MAX;
        if let Some(e) = ev.records().first() {
            ts_first = ts_first.min(e.ts_ns);
        }
        if let Some(t) = legs.records().first() {
            ts_first = ts_first.min(t.ts_ns);
        }
        if ts_first == u64::MAX {
            eprintln!("{run}: skipped (empty)");
            continue;
        }
        let (clock, tell) = RunClock::choose(run_dir_epoch(&dir), ts_first, fit.fit());
        let anchor = WallAnchor::new(ts_first, clock.wall_of(ts_first));
        let mut members = Vec::new();
        let mut refused = false;
        for art in &arts {
            match boot(pm15, art, &resolve, &keys, anchor) {
                Ok(m) => members.push(m),
                Err(e) => {
                    eprintln!("{run}: {} refused: {e}", art.display());
                    refused = true;
                }
            }
        }
        if refused {
            continue;
        }
        // the perp syms the artifacts name (both artifacts share `perp`)
        let perp_syms: Vec<SymbolId> = {
            let text = std::fs::read_to_string(&arts[0]).map_err(|e| e.to_string())?;
            let mut v = Vec::new();
            for l in text.lines() {
                if let Some(rest) = l.trim().strip_prefix("perp") {
                    for d in rest.split('"').skip(1).step_by(2) {
                        if let Some(s) = table.get(d) {
                            v.push(*s);
                        }
                    }
                }
            }
            v
        };
        let mut perps: Vec<&[Tick]> = Vec::new();
        if let Some(h) = &hl {
            perps.push(h.records());
        }
        if let Some(b) = &bn {
            perps.push(b.records());
        }
        let mut it_m = members.into_iter();
        let (Some(ma), Some(mb)) = (it_m.next(), it_m.next()) else {
            continue;
        };
        let (ctxs, counters) = if pm15 {
            let (Ok(mut a), Ok(mut b)) = (
                ma.downcast::<strategy_pm15::Pm15Strategy>(),
                mb.downcast::<strategy_pm15::Pm15Strategy>(),
            ) else {
                return Err(String::from("pm15 member type"));
            };
            let c = drive(a.as_mut(), b.as_mut(), ev.records(), legs.records(), &perps, &perp_syms);
            (c, [a.counters_of(), b.counters_of()])
        } else {
            let (Ok(mut a), Ok(mut b)) = (
                ma.downcast::<strategy_pmj::PmjStrategy>(),
                mb.downcast::<strategy_pmj::PmjStrategy>(),
            ) else {
                return Err(String::from("pmj member type"));
            };
            let c = drive(a.as_mut(), b.as_mut(), ev.records(), legs.records(), &perps, &perp_syms);
            (c, [a.counters_of(), b.counters_of()])
        };
        for (k, c) in ctxs.iter().enumerate() {
            for o in &c.orders {
                let oid = o.client_oid;
                writeln!(
                    out,
                    "{}\t{slot}\t{run}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{oid}\t{}",
                    LAWS[k],
                    clock.wall_of(o.ts_ns),
                    o.ts_ns,
                    oid & 0xFFFF_FFFF,
                    (oid >> 32) & 0xFFFF,
                    (oid >> 48) & 1,
                    o.sym,
                    o.px.raw(),
                    o.qty.raw(),
                    o.venue
                )
                .map_err(|e| e.to_string())?;
            }
            writeln!(cnt, "{}\t{slot}\t{run}\t{tell}\t{perp_files}\t{:?}", LAWS[k], counters[k])
                .map_err(|e| e.to_string())?;
        }
        eprintln!(
            "{run}: {} families, clock {tell}, {perp_files}: orders A {} B {}",
            keys.len(),
            ctxs[0].orders.len(),
            ctxs[1].orders.len()
        );
    }
    out.flush().map_err(|e| e.to_string())?;
    cnt.flush().map_err(|e| e.to_string())?;
    Ok(())
}
