// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! HC11 boot: `hcv.toml` → `strategy_hcv::HcvParams`, and the calendar
//! reader thread (`hcv-events`).
//!
//! COPY-DOCTRINE: boot-only module plus one cold thread. The reader
//! re-reads `scheduled-events.json` when its mtime moves (the news lane
//! rewrites it every cycle, O-HC8), parses it, and hands the engine a
//! fixed table through a [`core_ring::Mailbox`] — the engine thread never
//! opens the file (plan hc9-hc11 §4). Allocates and copies freely.
//!
//! The member takes no `core-config` dependency, so this is where the
//! artifact becomes its configuration:
//!
//! * each traded underlying's hedge (`hyperliquid:<hedge>`, O-HC20)
//!   resolves against the boot universe, and its `szDecimals` come from
//!   the boot's own Hyperliquid discovery (a builder perp's from its
//!   dex's meta, HC10);
//! * the options are the boot's own Hypercall chain (O-HC2's capped
//!   selection): those of a traded underlying are the member's, the rest
//!   (BABA, BOT, anything untraded) are counted in the tell;
//! * the wall anchor is NOT taken here — the set builder takes it right
//!   before the loop, like every other member's.
//!
//! Laws, from the other lanes verbatim:
//!
//! * **Requested-but-absent REFUSES** (the icdp/F19 law) — the caller
//!   turns `Ok(None)` into a refusal when slot 7 was requested.
//! * **Present-and-unreadable refuses too**, and so does a hedge the boot
//!   universe does not carry or whose size decimals the venue did not
//!   state: a member that cannot hedge is a member selling naked options.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use core_config::hcv::HcvFile;
use core_ring::MailboxTx;
use core_types::SymbolId;
use exec_hypercall::json;
use strategy_hcv::{BucketOrder, HcvEvents, HcvOpt, HcvParams, HcvUnd, HCV_MAX_EVENTS};
use tracing::{info, warn};

// The artifact's table is the member's: one statement of each bound.
const _: () = assert!(core_config::hcv::HCV_UNDERLYINGS.len() <= strategy_hcv::HCV_MAX_UND);
const _: () = assert!(ingress_hypercall::HC_MAX_INSTRUMENTS <= strategy_hcv::HCV_MAX_OPTIONS);

/// The feed schema this reader speaks (`claude_worker.news.SCHEMA_VERSION`).
pub const FEED_SCHEMA: u64 = 1;
/// Between two looks at the calendar file.
pub const EVENTS_PASS: Duration = Duration::from_secs(2);
/// A calendar file larger than this is refused (the feed is a few KiB).
pub const EVENTS_FILE_MAX: u64 = 4 << 20;
/// Events older than this at the read are dropped (the member asks about
/// `(now, expiry]` only); the table vouches from here on.
pub const EVENTS_KEEP_PAST_MS: u64 = 60_000;

/// Everything slot 7 needs to be configured.
#[derive(Debug, Clone)]
pub struct HcvBoot {
    /// The member's configuration (the anchor is the set builder's).
    pub params: HcvParams,
    /// sha256 of the artifact BYTES — the boot tell's identity.
    pub hash: [u8; 32],
    /// The artifact path actually read.
    pub path: PathBuf,
    /// The traded underlyings, in calendar-bit order.
    pub underlyings: Vec<&'static str>,
    /// Options of the chain per traded underlying (the tell).
    pub per_und: Vec<usize>,
    /// Chain rows of an untraded underlying (not the member's).
    pub skipped: usize,
    /// The calendar the reader thread follows.
    pub events_path: PathBuf,
    /// Every underlying name a calendar row may carry — the configured
    /// `[hypercall] underlyings` and the traded ones: a row naming anything
    /// else refuses the calendar (a misspelt event would pass the law).
    pub known: Vec<String>,
}

/// Whether the operator asked for the member.
#[must_use]
pub fn hcv_wanted(requested: u8) -> bool {
    requested & strategy_set::BIT_HCV != 0
}

/// Load and resolve. `Ok(None)` = the artifact is absent at its DEFAULT
/// path, which leaves the member unconfigured and its bit unset; the
/// caller turns that into a refusal when the bit was requested.
///
/// * `artifact` — `--hcv`, or `None` for `~/multivenue/hcv.toml`.
/// * `events` — `--hcv-events`, or `None` for the news lane's default.
/// * `resolve` — descriptor → `SymbolId` (the AI descriptor table).
/// * `sz_decimals` — Hyperliquid coin → the `szDecimals` the venue
///   stated at this boot's discovery.
/// * `options` — this boot's Hypercall chain.
/// * `hypercall_underlyings` — `[hypercall] underlyings`, the calendar's
///   known names (with the traded ones).
pub fn load_hcv_boot(
    artifact: Option<&Path>,
    events: Option<&Path>,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    sz_decimals: &dyn Fn(&str) -> Option<u8>,
    options: &[crate::paper::DiscoveredOption],
    hypercall_underlyings: &[String],
) -> Result<Option<HcvBoot>, String> {
    let explicit = artifact.is_some();
    let path: PathBuf = match artifact {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::hcv::default_hcv_path().map_err(|e| e.to_string())?),
    };
    if !path.exists() {
        if explicit {
            return Err(format!("{}: no such file", path.display()));
        }
        info!(path = %path.display(), "hcv: no artifact — member unconfigured");
        return Ok(None);
    }
    let (file, bytes) = core_config::hcv::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);
    let (params, underlyings, per_und, skipped) = build_params(&file, resolve, sz_decimals, options)?;
    let events_path = match events {
        Some(p) => p.to_path_buf(),
        None => PathBuf::from(core_config::hcv::default_events_path().map_err(|e| e.to_string())?),
    };
    let mut known: Vec<String> = hypercall_underlyings.to_vec();
    for u in &underlyings {
        if !known.iter().any(|k| k == u) {
            known.push((*u).to_owned());
        }
    }
    Ok(Some(HcvBoot {
        params,
        hash,
        path,
        underlyings,
        per_und,
        skipped,
        events_path,
        known,
    }))
}

/// The parsed artifact → the member's params, every hedge resolved and
/// every option of a traded underlying bound. The member's own
/// [`HcvParams::validate`] runs last, as the second opinion.
#[allow(clippy::type_complexity)]
pub fn build_params(
    file: &HcvFile,
    resolve: &dyn Fn(&str) -> Option<SymbolId>,
    sz_decimals: &dyn Fn(&str) -> Option<u8>,
    options: &[crate::paper::DiscoveredOption],
) -> Result<(HcvParams, Vec<&'static str>, Vec<usize>, usize), String> {
    let mut und = Vec::new();
    let mut names: Vec<&'static str> = Vec::new();
    for u in file.traded() {
        let desc = format!("hyperliquid:{}", u.hedge);
        let sym = resolve(&desc).ok_or_else(|| {
            format!(
                "hcv.toml: `{} = 1` but its hedge `{desc}` is not in the boot universe \
                 (universe.toml [hyperliquid] coins — O-HC20 appends the xyz coins at go-live)",
                u.key
            )
        })?;
        let sz = sz_decimals(u.hedge).ok_or_else(|| {
            format!(
                "hcv.toml: `{} = 1` but the venue stated no szDecimals for `{}` at this boot \
                 (a builder dex's meta did not load) — the hedge cannot be priced",
                u.key, u.hedge
            )
        })?;
        und.push(HcvUnd::new(u.hc.as_bytes(), sym, sz));
        names.push(u.hc);
    }
    let mut per_und = vec![0usize; names.len()];
    let mut opts = Vec::new();
    let mut skipped = 0usize;
    for (name, sym, strike_1e9, exp_ms, right) in options {
        // `<U>-<YYYYMMDD>-<strike>-<C|P>`: the underlying is the prefix.
        let prefix = name.split('-').next().unwrap_or("");
        match names.iter().position(|n| *n == prefix) {
            Some(k) if *exp_ms > 0 && *strike_1e9 > 0 => {
                opts.push(HcvOpt {
                    sym: *sym,
                    und: k as u8,
                    call: *right == opt_registry::RIGHT_CALL,
                    strike_1e6: strike_1e9 / 1_000,
                    exp_ms: *exp_ms as u64,
                });
                per_und[k] += 1;
            }
            _ => skipped += 1,
        }
    }
    if opts.is_empty() {
        return Err("hcv.toml: no Hypercall option of a traded underlying is in the boot chain \
             (universe.toml [hypercall] underlyings)"
            .to_owned());
    }
    let p = HcvParams {
        und,
        options: opts,
        theta_vol_1e6: file.theta_vol_1e6,
        atm_band_bps: file.atm_band_bps,
        tenor_min_d: file.tenor_min_d,
        tenor_max_d: file.tenor_max_d,
        clip_usd_1e6: file.clip_usd_1e6,
        vega_cap_usd_1e6: file.vega_cap_usd_1e6,
        premium_cap_usd_1e6: file.premium_cap_usd_1e6,
        day_loss_usd_1e6: file.day_loss_usd_1e6,
        tail_loss_usd_1e6: file.tail_loss_usd_1e6,
        opt_size_step_1e6: file.opt_size_step_1e6,
        hedge_band_1e6: file.hedge_band_1e6,
        hedge_min_usd_1e6: file.hedge_min_usd_1e6,
        hedge_slip_bps: file.hedge_slip_bps,
        quote_stale_ms: file.quote_stale_ms,
        oracle_stale_ms: file.oracle_stale_ms,
        unwind_min: file.unwind_min,
        settle_delay_ms: file.settle_delay_ms,
        settle_order: if file.settle_order == 1 {
            BucketOrder::Time
        } else {
            BucketOrder::Sorted
        },
        event_law: file.event_law,
        events_stale_ms: file.events_stale_ms,
        kill: file.kill,
        timer_ms: file.timer_ms,
        anchor: core_time::WallAnchor::new(0, 0),
    };
    p.validate().map_err(|e| format!("hcv.toml: {e}"))?;
    Ok((p, names, per_und, skipped))
}

/// The one-line boot tell: the artifact's identity, what is traded on
/// what, and every knob that decides what the member does.
#[must_use]
pub fn render_boot_tell(boot: &HcvBoot) -> String {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let mut unds = String::new();
    let mut i = 0usize;
    while i < boot.underlyings.len() {
        if i > 0 {
            unds.push(',');
        }
        unds.push_str(&format!("{}:{}", boot.underlyings[i], boot.per_und.get(i).copied().unwrap_or(0)));
        i += 1;
    }
    let p = &boot.params;
    format!(
        "hcv: artifact configured hash={hex} path={} underlyings={unds} options={} skipped={} \
         events={} theta_vol_1e6={} atm_band_bps={} tenors={}..{}d clip_usd_1e6={} \
         vega_cap_usd_1e6={} premium_cap_usd_1e6={} day_loss_usd_1e6={} tail_loss_usd_1e6={} \
         hedge_band_1e6={} hedge_min_usd_1e6={} unwind_min={} settle_order={:?} event_law={} \
         kill={} phase=HC11(paper, DARK)",
        boot.path.display(),
        p.options.len(),
        boot.skipped,
        boot.events_path.display(),
        p.theta_vol_1e6,
        p.atm_band_bps,
        p.tenor_min_d,
        p.tenor_max_d,
        p.clip_usd_1e6,
        p.vega_cap_usd_1e6,
        p.premium_cap_usd_1e6,
        p.day_loss_usd_1e6,
        p.tail_loss_usd_1e6,
        p.hedge_band_1e6,
        p.hedge_min_usd_1e6,
        p.unwind_min,
        p.settle_order,
        u8::from(p.event_law),
        u8::from(p.kill),
    )
}

// ---------------------------------------------------------------
// The calendar reader
// ---------------------------------------------------------------

/// The feed (`claude_worker.news.scheduled`) → `out`: the events after
/// `now_ms − EVENTS_KEEP_PAST_MS` that move a traded underlying (bit `k`
/// = `names[k]`), time-ordered, at most [`HCV_MAX_EVENTS`] — past that the
/// table vouches only up to the first event it could not hold. Returns
/// the events kept.
///
/// A row naming an underlying outside `known` refuses the whole feed: a
/// misspelt name ("SPX" for "SP500") would otherwise drop its event and
/// let the event law pass a trade straight through it (fail closed).
///
/// # Errors
///
/// What is wrong with the file; `out` is untouched.
pub fn parse_feed(
    buf: &[u8],
    names: &[&str],
    known: &[String],
    now_ms: u64,
    out: &mut HcvEvents,
) -> Result<usize, String> {
    let root = json::root(buf).ok_or("not one JSON object")?;
    let num = |key: &[u8]| json::field_in(buf, &root, key).and_then(|v| v.as_u64(buf));
    if num(b"v") != Some(FEED_SCHEMA) {
        return Err("not a schema-1 feed (`v`)".to_owned());
    }
    let (Some(generated_s), Some(from_s), Some(until_s)) = (num(b"generated_ts"), num(b"from_ts"), num(b"until_ts")) else {
        return Err("no `generated_ts` / `from_ts` / `until_ts`".to_owned());
    };
    let events = json::field_in(buf, &root, b"events").ok_or("no `events`")?;
    if events.kind != json::Kind::Arr {
        return Err("`events` is not an array".to_owned());
    }
    let keep_after = now_ms.saturating_sub(EVENTS_KEEP_PAST_MS);
    let mut rows: Vec<(u64, u16)> = Vec::new();
    let mut it = json::items(buf, &events);
    while let Some(item) = it.next_item() {
        let item = item.map_err(|_| "a malformed `events` array")?;
        let at_s = json::field_in(buf, &item, b"at_ts")
            .and_then(|v| v.as_u64(buf))
            .ok_or("an event without `at_ts`")?;
        let unds = json::field_in(buf, &item, b"underlyings").ok_or("an event without `underlyings`")?;
        let mut mask = 0u16;
        let mut ui = json::items(buf, &unds);
        while let Some(u) = ui.next_item() {
            let u = u.map_err(|_| "a malformed `underlyings` array")?;
            if u.kind == json::Kind::Str {
                let name = u.bytes(buf);
                if !known.iter().any(|n| n.as_bytes() == name) {
                    return Err(format!(
                        "an event names `{}`, which is not a Hypercall underlying here — \
                         fix news.toml [events] (the event law cannot apply it)",
                        String::from_utf8_lossy(name)
                    ));
                }
                let mut k = 0usize;
                while k < names.len() && k < 16 {
                    if name == names[k].as_bytes() {
                        mask |= 1u16 << k;
                    }
                    k += 1;
                }
            }
        }
        let at_ms = at_s.saturating_mul(1_000);
        if mask != 0 && at_ms > keep_after {
            rows.push((at_ms, mask));
        }
    }
    rows.sort_unstable();
    let mut cal = HcvEvents::new();
    cal.generated_ms = generated_s.saturating_mul(1_000);
    cal.from_ms = from_s.saturating_mul(1_000).max(keep_after);
    cal.until_ms = until_s.saturating_mul(1_000);
    if rows.len() > HCV_MAX_EVENTS {
        cal.until_ms = cal.until_ms.min(rows[HCV_MAX_EVENTS].0.saturating_sub(1));
        rows.truncate(HCV_MAX_EVENTS);
    }
    let mut i = 0usize;
    while i < rows.len() {
        cal.push(rows[i].0, rows[i].1);
        i += 1;
    }
    *out = cal;
    Ok(rows.len())
}

/// The running calendar reader (module doc). Dropping it stops and joins
/// it like [`HcvEventsReader::shutdown`].
pub struct HcvEventsReader {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HcvEventsReader {
    /// Spawn the reader (`hcv-events`) over `path`, handing each parsed
    /// calendar to slot 7 through `tx`; `names` are the member's
    /// underlyings in bit order.
    ///
    /// # Errors
    ///
    /// The thread could not be spawned — the member then never gets a
    /// calendar and, under the event law, never trades (fail closed).
    pub fn spawn(
        path: PathBuf,
        names: Vec<&'static str>,
        known: Vec<String>,
        tx: MailboxTx<HcvEvents>,
    ) -> std::io::Result<Self> {
        Self::spawn_with(path, names, known, tx, EVENTS_PASS)
    }

    fn spawn_with(
        path: PathBuf,
        names: Vec<&'static str>,
        known: Vec<String>,
        tx: MailboxTx<HcvEvents>,
        pass: Duration,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("hcv-events".into())
            .spawn(move || run(&path, &names, &known, tx, &flag, pass))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the thread and join it.
    pub fn shutdown(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        let Some(t) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Release);
        t.thread().unpark();
        if t.join().is_err() {
            tracing::error!("hcv: the calendar reader thread panicked");
        }
    }
}

impl Drop for HcvEventsReader {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The reader thread: look at the file's mtime each pass; on a change,
/// read and parse it; hand the newest parsed calendar over whenever the
/// mailbox is FREE (the member takes it at its next timer). A file that
/// fails to parse is logged once per change and never handed — the
/// member keeps the last good calendar until it goes stale.
fn run(
    path: &Path,
    names: &[&'static str],
    known: &[String],
    mut tx: MailboxTx<HcvEvents>,
    stop: &AtomicBool,
    pass: Duration,
) {
    let mut last_mtime: Option<SystemTime> = None;
    let mut pending: Option<Box<HcvEvents>> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut absent_warned = false;
    while !stop.load(Ordering::Acquire) {
        match std::fs::metadata(path) {
            Ok(meta) => {
                absent_warned = false;
                let mtime = meta.modified().ok();
                if mtime != last_mtime || last_mtime.is_none() {
                    last_mtime = mtime;
                    if meta.len() > EVENTS_FILE_MAX {
                        warn!(path = %path.display(), bytes = meta.len(), "hcv: calendar file too large — ignored");
                    } else {
                        buf.clear();
                        match std::fs::read(path) {
                            Ok(b) => buf = b,
                            Err(e) => warn!(path = %path.display(), error = %e, "hcv: calendar unreadable"),
                        }
                        if !buf.is_empty() {
                            let mut cal = Box::new(HcvEvents::new());
                            match parse_feed(&buf, names, known, wall_ms(), &mut cal) {
                                Ok(n) => {
                                    info!(path = %path.display(), events = n, generated_ms = cal.generated_ms, "hcv: calendar read");
                                    pending = Some(cal);
                                }
                                Err(reason) => warn!(path = %path.display(), %reason, "hcv: calendar refused — the last good one stays"),
                            }
                        }
                    }
                }
            }
            Err(_) => {
                if !absent_warned {
                    warn!(path = %path.display(), "hcv: no calendar file — under the event law slot 7 trades nothing");
                    absent_warned = true;
                }
            }
        }
        if let Some(cal) = pending.as_deref() {
            if let Some(mut slot) = tx.try_fill() {
                // COPY: the ≤ 1.1 KiB calendar into the mailbox slot, once
                // per file change, on this cold thread — the slot is the
                // engine's to read in place; the parse buffer is not.
                *slot = *cal;
                slot.commit();
                pending = None;
            }
        }
        std::thread::park_timeout(pass);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::{make_symbol_id, VenueId};

    const EXAMPLE: &str = include_str!("../../../hcv.toml.example");
    const T_MS: u64 = 1_790_424_000_000;

    fn known() -> Vec<String> {
        ["SP500", "MU", "BTC", "BABA"].iter().map(|s| (*s).to_owned()).collect()
    }

    fn feed(events: &str) -> String {
        format!(
            "{{\"events\":[{events}],\"from_ts\":{},\"generated_ts\":{},\"until_ts\":{},\"v\":1}}\n",
            T_MS / 1_000 - 14 * 86_400,
            T_MS / 1_000 - 30,
            T_MS / 1_000 + 60 * 86_400
        )
    }

    fn ev(at_s: u64, unds: &str) -> String {
        format!(
            "{{\"at_ts\":{at_s},\"confirmed\":1,\"detail\":\"a \\\"quoted\\\" detail\",\"kind\":\"earnings\",\"source\":\"news.toml [events]\",\"underlyings\":[{unds}]}}"
        )
    }

    #[test]
    fn the_feed_parses_to_the_members_bits_in_time_order() {
        let t = T_MS / 1_000;
        let body = feed(&format!(
            "{},{},{},{}",
            ev(t + 3 * 86_400, "\"MU\""),
            ev(t + 86_400, "\"SP500\",\"BTC\""),
            ev(t + 2 * 86_400, "\"BABA\""),
            ev(t - 3_600, "\"SP500\"")
        ));
        let mut cal = HcvEvents::new();
        let n = parse_feed(body.as_bytes(), &["SP500", "MU", "BTC"], &known(), T_MS, &mut cal).expect("parses");
        assert_eq!(n, 2, "BABA is not ours; the hour-old event is past");
        assert_eq!((cal.at_ms[0], cal.mask[0]), ((t + 86_400) * 1_000, 0b101));
        assert_eq!((cal.at_ms[1], cal.mask[1]), ((t + 3 * 86_400) * 1_000, 0b010));
        assert_eq!(cal.generated_ms, (t - 30) * 1_000);
        assert_eq!(cal.from_ms, T_MS - EVENTS_KEEP_PAST_MS, "the table vouches from the read");
        assert_eq!(cal.until_ms, (t + 60 * 86_400) * 1_000);
        assert_eq!(cal.any_in(0, T_MS, T_MS + 2 * 86_400_000), Some(true));
        assert_eq!(cal.any_in(1, T_MS, T_MS + 2 * 86_400_000), Some(false));
    }

    #[test]
    fn a_full_table_vouches_only_up_to_what_it_could_not_hold() {
        let t = T_MS / 1_000;
        let mut rows = String::new();
        let mut i = 0u64;
        while i < 70 {
            if i > 0 {
                rows.push(',');
            }
            rows.push_str(&ev(t + (i + 1) * 3_600, "\"MU\""));
            i += 1;
        }
        let mut cal = HcvEvents::new();
        let n = parse_feed(feed(&rows).as_bytes(), &["MU"], &known(), T_MS, &mut cal).unwrap();
        assert_eq!(n, HCV_MAX_EVENTS);
        assert_eq!(cal.until_ms, (t + 65 * 3_600) * 1_000 - 1, "the 65th event is not vouched for");
        assert_eq!(cal.any_in(0, T_MS, T_MS + 66 * 3_600_000), None);
    }

    #[test]
    fn a_bad_feed_refuses_and_leaves_the_table() {
        let mut cal = HcvEvents::new();
        cal.generated_ms = 7;
        let t = T_MS / 1_000;
        for bad in [
            String::from("not json"),
            feed("").replace("\"v\":1", "\"v\":2"),
            feed("").replace("\"generated_ts\"", "\"made_ts\""),
            feed("{\"underlyings\":[\"MU\"]}"),
            feed(&ev(t, "\"MU\"")).replace("\"events\":[", "\"events\":{").replace("}],\"from", "}},\"from"),
        ] {
            assert!(parse_feed(bad.as_bytes(), &["MU"], &known(), T_MS, &mut cal).is_err(), "{bad}");
            assert_eq!(cal.generated_ms, 7, "untouched");
        }
        // A name Hypercall does not list refuses the whole feed.
        let e = parse_feed(feed(&ev(t + 86_400, "\"SPX\"")).as_bytes(), &["SP500"], &known(), T_MS, &mut cal)
            .unwrap_err();
        assert!(e.contains("`SPX`"), "{e}");
        assert_eq!(cal.generated_ms, 7);
    }

    #[test]
    fn the_reader_hands_each_new_calendar_over_the_mailbox() {
        let dir = std::env::temp_dir().join(format!("hcv-events-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scheduled-events.json");
        let now_s = wall_ms() / 1_000;
        let write = |at_s: u64| {
            let body = format!(
                "{{\"events\":[{}],\"from_ts\":{},\"generated_ts\":{},\"until_ts\":{},\"v\":1}}\n",
                ev(at_s, "\"MU\""),
                now_s - 86_400,
                now_s,
                now_s + 60 * 86_400
            );
            std::fs::write(&path, body).unwrap();
        };
        write(now_s + 86_400);
        let (tx, mut rx) = core_ring::Mailbox::new(Box::new(HcvEvents::new())).split();
        let reader =
            HcvEventsReader::spawn_with(path.clone(), vec!["MU"], known(), tx, Duration::from_millis(10)).unwrap();
        let take = |rx: &mut core_ring::MailboxRx<HcvEvents>| {
            let mut tries = 0;
            loop {
                if let Some(c) = rx.try_take() {
                    return *c;
                }
                tries += 1;
                assert!(tries < 500, "no calendar handed");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        let first = take(&mut rx);
        assert_eq!((first.n, first.at_ms[0]), (1, (now_s + 86_400) * 1_000));
        // A rewrite (a new mtime) is handed again.
        std::thread::sleep(Duration::from_millis(1_100));
        write(now_s + 2 * 86_400);
        let second = take(&mut rx);
        assert_eq!(second.at_ms[0], (now_s + 2 * 86_400) * 1_000);
        reader.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_example_builds_against_a_chain_and_refuses_what_it_cannot_hedge() {
        let file = core_config::hcv::parse(EXAMPLE).unwrap();
        let resolve = |d: &str| match d {
            "hyperliquid:xyz:SP500" => Some(make_symbol_id(VenueId::Hyperliquid, 40)),
            "hyperliquid:BTC" => Some(make_symbol_id(VenueId::Hyperliquid, 1)),
            _ => Some(make_symbol_id(VenueId::Hyperliquid, 99)),
        };
        let sz = |c: &str| if c == "BTC" { Some(5) } else { Some(2) };
        let chain: Vec<crate::paper::DiscoveredOption> = vec![
            ("SP500-20261002-6600-C".into(), make_symbol_id(VenueId::Hypercall, 513), 6_600_000_000_000, 1_791_000_000_000, opt_registry::RIGHT_CALL),
            ("BTC-20261002-110000-P".into(), make_symbol_id(VenueId::Hypercall, 514), 110_000_000_000_000, 1_791_000_000_000, opt_registry::RIGHT_PUT),
            ("BABA-20261002-150-C".into(), make_symbol_id(VenueId::Hypercall, 515), 150_000_000_000, 1_791_000_000_000, opt_registry::RIGHT_CALL),
        ];
        let (p, names, per_und, skipped) = build_params(&file, &resolve, &sz, &chain).expect("builds");
        assert_eq!(names.len(), 10);
        assert_eq!((p.options.len(), skipped), (2, 1), "BABA is not traded");
        assert_eq!(p.options[0].strike_1e6, 6_600_000_000);
        assert!(p.options[0].call && !p.options[1].call);
        assert_eq!(names[p.options[1].und as usize], "BTC");
        assert_eq!(per_und[0], 1);
        assert_eq!(p.und[8].hedge_sz_decimals, 5);
        // A hedge the universe does not carry, or without stated decimals.
        let none = |d: &str| if d == "hyperliquid:xyz:MU" { None } else { resolve(d) };
        assert!(build_params(&file, &none, &sz, &chain).unwrap_err().contains("xyz:MU"));
        let no_sz = |c: &str| if c == "xyz:NVDA" { None } else { sz(c) };
        assert!(build_params(&file, &resolve, &no_sz, &chain).unwrap_err().contains("szDecimals"));
        // No option of a traded underlying at all.
        assert!(build_params(&file, &resolve, &sz, &chain[2..]).is_err());
    }

    #[test]
    fn an_absent_default_is_none_an_absent_explicit_path_refuses() {
        let resolve = |_: &str| None;
        let sz = |_: &str| None;
        let e = load_hcv_boot(Some(Path::new("/nonexistent/hcv.toml")), None, &resolve, &sz, &[], &[]).unwrap_err();
        assert!(e.contains("no such file"), "{e}");
        assert!(hcv_wanted(strategy_set::BIT_HCV));
        assert!(hcv_wanted(strategy_set::BUILT_MASK));
        assert!(!hcv_wanted(strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM));
    }
}
