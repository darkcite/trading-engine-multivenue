// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The poll loop: `/state`, the three capture tails, the clock, the inbox,
//! then the outbox and the store — once every `poll_ms`. Sending happens on
//! its own thread (`telegram::run_sender`). Everything is persisted every
//! poll, so a SIGTERM from launchd loses at most a message that is then
//! sent twice.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use core_io::pmlr::SlotKind;

use crate::capture::{EventRec, FillRec, OrderRec, Slot, Tail};
use crate::clock;
use crate::config::Config;
use crate::envfile;
use crate::inbox::{self, Gate};
use crate::model::{Model, Msg};
use crate::names::Names;
use crate::outbox::Outbox;
use crate::persist::{Cursor, Store};
use crate::state::{self, StateView};
use crate::telegram::{self, DryRun, Transport};

/// Records taken from one capture per poll.
const MAX_RECORDS_PER_POLL: usize = 4096;
/// `/state` request deadline.
const STATE_TIMEOUT: Duration = Duration::from_secs(2);

/// A timestamped line on stderr (the launchd log).
pub fn log(msg: &str) {
    let ns = clock::now_wall_ns() as i64;
    let s = ns / 1_000_000_000;
    eprintln!("{} {} notify: {msg}", clock::date(s), clock::hhmmss(s));
}

struct Tails {
    run_dir: String,
    fills: Tail,
    orders: Tail,
    events: Tail,
}

impl Tails {
    fn new(run_dir: &str, c: Option<&Cursor>) -> Tails {
        let d = Path::new(run_dir);
        Tails {
            run_dir: run_dir.to_string(),
            fills: Tail::new(
                d.join(crate::FILLS_FILE),
                SlotKind::Fill,
                c.map_or(0, |c| c.fills),
            ),
            orders: Tail::new(
                d.join(crate::ORDERS_FILE),
                SlotKind::Order,
                c.map_or(0, |c| c.orders),
            ),
            events: Tail::new(
                d.join(crate::EVENTS_FILE),
                SlotKind::OrderEvent,
                c.map_or(0, |c| c.events),
            ),
        }
    }

    fn cursor(&self) -> Cursor {
        Cursor {
            run_dir: self.run_dir.clone(),
            fills: self.fills.consumed(),
            orders: self.orders.consumed(),
            events: self.events.consumed(),
        }
    }
}

/// The transport `cfg` asks for.
///
/// # Errors
/// The `.env` keys cannot be read (never echoes a value).
pub fn transport_for(cfg: &Config) -> Result<Box<dyn Transport>, String> {
    if cfg.dry_run {
        return Ok(Box::new(DryRun));
    }
    let path = cfg.env_file.as_ref().ok_or("env_file is not set")?;
    let keys = envfile::read(path).map_err(|e| e.to_string())?;
    Ok(Box::new(telegram::Telegram::new(keys)))
}

/// The runner's state.
pub struct Runner {
    cfg: Config,
    store: Store,
    outbox: Outbox,
    model: Model,
    tails: Option<Tails>,
    saved_cursor: Option<Cursor>,
    saved_kv: std::collections::BTreeMap<String, String>,
    gate: Gate,
    last_err: String,
    state_ok: bool,
    polls: u64,
}

impl Runner {
    /// Open the store and outbox and build the model.
    ///
    /// # Errors
    /// The state directory cannot be used.
    pub fn new(cfg: Config) -> Result<Runner, String> {
        let store = Store::open(&cfg.state_dir)
            .map_err(|e| format!("state_dir {}: {e}", cfg.state_dir.display()))?;
        let outbox = Outbox::open(&cfg.state_dir).map_err(|e| format!("outbox: {e}"))?;
        let names = Names::load(cfg.universe.as_deref());
        if cfg.universe.is_some() && names.families.is_empty() {
            log("universe names no [hyperliquid] rolling families: symbols print as raw ids");
        }
        let now_s = (clock::now_wall_ns() / 1_000_000_000) as i64;
        let kv = store.load_kv();
        let model = Model::new(
            cfg.clone(),
            names,
            store.load_positions(),
            store.load_settled(),
            kv.clone(),
            now_s,
        );
        let saved_cursor = store.load_cursor();
        Ok(Runner {
            gate: Gate::new(cfg.inbox_max_per_hour),
            cfg,
            store,
            outbox,
            model,
            tails: None,
            saved_cursor,
            saved_kv: kv,
            last_err: String::new(),
            state_ok: true,
            polls: 0,
        })
    }

    /// The outbox (the sender thread's input).
    #[must_use]
    pub fn outbox(&self) -> &Outbox {
        &self.outbox
    }

    fn note_err(&mut self, e: &str) {
        if e != self.last_err {
            log(e);
            self.last_err = e.to_string();
        }
    }

    fn drain(&mut self, tails: &mut Tails, now_s: i64, quiet: bool, msgs: &mut Vec<Msg>) -> bool {
        let mut buf: Vec<Slot> = Vec::new();
        let mut any = false;
        let mut errs = Vec::new();
        loop {
            buf.clear();
            match tails.orders.poll(MAX_RECORDS_PER_POLL, &mut buf) {
                Ok(_) => {}
                Err(e) => errs.push(format!("{}: {e}", tails.orders.path().display())),
            }
            for s in &buf {
                self.model.on_order(&OrderRec::decode(s), now_s);
            }
            let n_orders = buf.len();
            buf.clear();
            match tails.events.poll(MAX_RECORDS_PER_POLL, &mut buf) {
                Ok(_) => {}
                Err(e) => errs.push(format!("{}: {e}", tails.events.path().display())),
            }
            for s in &buf {
                self.model.on_event(&EventRec::decode(s), now_s);
            }
            let n_events = buf.len();
            buf.clear();
            match tails.fills.poll(MAX_RECORDS_PER_POLL, &mut buf) {
                Ok(_) => {}
                Err(e) => errs.push(format!("{}: {e}", tails.fills.path().display())),
            }
            for s in &buf {
                self.model.on_fill(&FillRec::decode(s), now_s, quiet, msgs);
            }
            let n_fills = buf.len();
            any |= n_orders + n_events + n_fills > 0;
            if n_orders < MAX_RECORDS_PER_POLL
                && n_events < MAX_RECORDS_PER_POLL
                && n_fills < MAX_RECORDS_PER_POLL
            {
                break;
            }
        }
        for e in errs {
            self.note_err(&e);
        }
        any
    }

    fn fold_fills_quietly(&mut self, t: &mut Tail, now_s: i64) {
        let mut buf = Vec::new();
        let mut sink = Vec::new();
        loop {
            buf.clear();
            match t.poll(MAX_RECORDS_PER_POLL, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    for s in &buf {
                        self.model
                            .on_fill(&FillRec::decode(s), now_s, true, &mut sink);
                    }
                }
            }
        }
    }

    /// Fold every run directory beside `run_dir` that may hold fills of a
    /// watch window, then `run_dir` itself — silently (first start only).
    fn backfill(&mut self, run_dir: &str, now_s: i64) {
        let mut runs: Vec<PathBuf> = Vec::new();
        let earliest = self.cfg.watches.iter().map(|w| w.start_s).min();
        if let (Some(start), true) = (earliest, self.model.settled.is_empty()) {
            if let Some(parent) = Path::new(run_dir).parent() {
                if let Ok(rd) = std::fs::read_dir(parent) {
                    let mut found: Vec<(i64, PathBuf)> = rd
                        .filter_map(Result::ok)
                        .filter_map(|e| {
                            let n = e.file_name().into_string().ok()?;
                            let ns: i64 = n.strip_prefix("run-")?.parse().ok()?;
                            (e.path() != Path::new(run_dir)
                                && ns / 1_000_000_000 >= start - 2 * 86_400)
                                .then(|| (ns, e.path()))
                        })
                        .collect();
                    found.sort();
                    runs.extend(found.into_iter().map(|(_, p)| p));
                }
            }
        }
        // Only FILLS are history worth folding: orders and events of a run
        // already over would come back as stale "attempts" messages.
        for r in &runs {
            let mut t = Tail::new(r.join(crate::FILLS_FILE), SlotKind::Fill, 0);
            self.fold_fills_quietly(&mut t, now_s);
        }
        let mut cur = Tails::new(run_dir, None);
        let _ = cur.orders.skip_to_end();
        let _ = cur.events.skip_to_end();
        self.fold_fills_quietly(&mut cur.fills, now_s);
        log(&format!(
            "first start: folded {} earlier run(s) and {} silently ({} open position(s), {} settled on record)",
            runs.len(),
            run_dir,
            self.model.positions.len(),
            self.model.settled.len()
        ));
        self.tails = Some(cur);
    }

    /// One poll.
    pub fn poll(&mut self) {
        self.polls += 1;
        let now_s = (clock::now_wall_ns() / 1_000_000_000) as i64;
        let mut msgs: Vec<Msg> = Vec::new();
        let watching = !self.cfg.slots.is_empty();
        let mut st: Option<StateView> = None;
        if watching {
            if let Some(url) = &self.cfg.state_url {
                match state::fetch(url, STATE_TIMEOUT) {
                    Ok(s) => {
                        if !self.state_ok {
                            log("/state answers again");
                        }
                        self.state_ok = true;
                        st = Some(s);
                    }
                    Err(e) => {
                        if self.state_ok {
                            log(&e.to_string());
                        }
                        self.state_ok = false;
                    }
                }
            }
            self.model.on_state(now_s, st.as_ref(), &mut msgs);
            let target = st
                .as_ref()
                .map(|s| s.run_dir.clone())
                .filter(|d| !d.is_empty());
            match self.tails.take() {
                None => {
                    if let Some(c) = self.saved_cursor.clone() {
                        let mut t = Tails::new(&c.run_dir, Some(&c));
                        match &target {
                            Some(d) if *d != c.run_dir => {
                                self.drain(&mut t, now_s, false, &mut msgs);
                                log(&format!(
                                    "run directory changed while stopped: {} → {d}",
                                    c.run_dir
                                ));
                                self.tails = Some(Tails::new(d, None));
                            }
                            _ => self.tails = Some(t),
                        }
                    } else if let Some(d) = &target {
                        self.backfill(d, now_s);
                    }
                }
                Some(mut t) => {
                    if let Some(d) = target.as_ref().filter(|d| **d != t.run_dir) {
                        self.drain(&mut t, now_s, false, &mut msgs);
                        log(&format!("run directory changed: {} → {d}", t.run_dir));
                        self.tails = Some(Tails::new(d, None));
                    } else {
                        self.tails = Some(t);
                    }
                }
            }
            if let Some(mut t) = self.tails.take() {
                self.drain(&mut t, now_s, false, &mut msgs);
                self.tails = Some(t);
            }
        }
        self.model.tick(now_s, &mut msgs);
        self.relay_inbox(now_s, &mut msgs);
        for m in &msgs {
            if let Err(e) = self.outbox.push(m) {
                self.note_err(&format!("outbox write failed: {e}"));
            }
        }
        self.persist();
        if self.polls % 60 == 0 {
            if let Ok(n) = self.outbox.enforce_cap() {
                if n > 0 {
                    log(&format!(
                        "outbox over its cap: dropped {n} oldest silent message(s)"
                    ));
                }
            }
        }
    }

    fn relay_inbox(&mut self, now_s: i64, msgs: &mut Vec<Msg>) {
        if self.cfg.inbox.is_empty() {
            return;
        }
        for (path, m) in inbox::collect(&self.cfg.inbox) {
            if let Some(m) = m {
                let (ok, ended) = self.gate.admit(now_s);
                if let Some(n) = ended {
                    msgs.push(Msg {
                        loud: false,
                        text: format!(
                            "[AI] {n} message(s) suppressed last hour (cap {}/h)",
                            self.cfg.inbox_max_per_hour
                        ),
                    });
                }
                if ok {
                    let mm = Msg {
                        loud: m.loud,
                        text: crate::clip(&inbox::relay_text(&m), crate::MAX_TEXT_BYTES),
                    };
                    if self.outbox.push(&mm).is_err() {
                        continue; // keep the file: try again next poll
                    }
                    self.model.count_inbox(1);
                }
            } else {
                log(&format!(
                    "inbox: removed an unreadable, oversized or empty entry {}",
                    path.display()
                ));
            }
            let _ = std::fs::remove_file(&path);
        }
    }

    fn persist(&mut self) {
        for s in std::mem::take(&mut self.model.newly_settled) {
            if let Err(e) = self.store.append_settled(&s) {
                self.note_err(&format!("settled.tsv append failed: {e}"));
            }
        }
        if self.model.dirty {
            if let Err(e) = self.store.save_positions(&self.model.positions) {
                self.note_err(&format!("positions.tsv write failed: {e}"));
            }
        }
        if self.model.dirty || self.model.kv != self.saved_kv {
            match self.store.save_kv(&self.model.kv) {
                Ok(()) => self.saved_kv = self.model.kv.clone(),
                Err(e) => self.note_err(&format!("kv.tsv write failed: {e}")),
            }
        }
        self.model.dirty = false;
        if let Some(t) = &self.tails {
            let c = t.cursor();
            if self.saved_cursor.as_ref() != Some(&c) {
                match self.store.save_cursor(&c) {
                    Ok(()) => self.saved_cursor = Some(c),
                    Err(e) => self.note_err(&format!("cursor.tsv write failed: {e}")),
                }
            }
        }
    }
}

/// `multivenue-notify run`: poll until killed.
///
/// # Errors
/// A fatal setup failure (state directory, `.env` keys).
pub fn run(cfg: Config) -> Result<(), String> {
    let transport = transport_for(&cfg)?;
    let mut r = Runner::new(cfg.clone())?;
    let stop = Arc::new(AtomicBool::new(false));
    let ob = r.outbox().clone();
    let s2 = stop.clone();
    std::thread::Builder::new()
        .name("notify-sender".into())
        .spawn(move || telegram::run_sender(ob, transport, s2))
        .map_err(|e| format!("cannot start the sender thread: {e}"))?;
    log(&format!(
        "running: tag {} · {} slot(s) · {} watch(es) · {} inbox dir(s) · {}",
        cfg.tag,
        cfg.slots.len(),
        cfg.watches.len(),
        cfg.inbox.len(),
        if cfg.dry_run {
            "DRY RUN (nothing is sent)"
        } else {
            "sending to Telegram"
        }
    ));
    let period = Duration::from_millis(cfg.poll_ms);
    loop {
        let t0 = Instant::now();
        r.poll();
        if let Some(rest) = period.checked_sub(t0.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::testkit::{append, bytes_of, write_header};
    use core_types::{Fill, Order, OrderEvent, Price, Qty, Side, VenueId};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notify-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_inbox_only_runner_relays_and_removes() {
        let d = dir("inbox");
        let inb = d.join("in");
        let cfg = Config::parse(&format!(
            "tag = \"MAIN\"\nstate_dir = \"{}\"\ndry_run = true\ninbox = [\"{}\"]\ninbox_max_per_hour = 2\n",
            d.join("st").display(),
            inb.display()
        ))
        .unwrap();
        let mut r = Runner::new(cfg).unwrap();
        inbox::write_message(&inb, true, Some("cowork"), "hello").unwrap();
        inbox::write_message(&inb, false, None, "two").unwrap();
        inbox::write_message(&inb, false, None, "three").unwrap();
        r.poll();
        let p = r.outbox().pending().unwrap();
        let texts: Vec<_> = p.iter().map(|m| (m.loud, m.text.as_str())).collect();
        assert_eq!(
            texts,
            [(true, "[AI:cowork] hello"), (false, "[AI] two")],
            "the third is over the cap"
        );
        assert!(inbox::collect(&[inb]).is_empty(), "every file removed");
    }

    /// First start: earlier runs' fills are folded, their orders and events
    /// are not replayed as attempts, and nothing of the history is sent.
    #[test]
    fn first_start_folds_earlier_fills_silently() {
        let d = dir("backfill");
        let runs = d.join("runs");
        let old = runs.join("run-1792800000000000000");
        let cur = runs.join("run-1792900000000000000");
        let hl = core_types::make_symbol_id(VenueId::Hyperliquid, 4096);
        for r in [&old, &cur] {
            std::fs::create_dir_all(r).unwrap();
            write_header(&r.join(crate::FILLS_FILE), SlotKind::Fill);
            write_header(&r.join(crate::ORDERS_FILE), SlotKind::Order);
            write_header(&r.join(crate::EVENTS_FILE), SlotKind::OrderEvent);
        }
        let t = clock::parse_rfc3339_z("2026-10-22T07:29:41Z").unwrap() as u64 * 1_000_000_000;
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            hl,
            Side::Bid,
            0,
            Price(700_000),
            Qty(18_000_000),
            501,
        );
        o.strategy_id = 3;
        append(&old.join(crate::ORDERS_FILE), &bytes_of(&o));
        let miss = OrderEvent::new(
            2,
            VenueId::Hyperliquid,
            hl,
            501,
            3,
            core_types::ORDER_EVENT_REJECTED,
            12,
            0,
        );
        append(&old.join(crate::EVENTS_FILE), &bytes_of(&miss));
        let buy = Fill::new(t, hl, Side::Bid, Price(700_000), Qty(18_000_000), 501)
            .with_attribution(3, 0);
        append(&old.join(crate::FILLS_FILE), &bytes_of(&buy));
        let settle = Fill::new(
            t + 30_000_000_000,
            hl,
            Side::Ask,
            Price(1_000_000),
            Qty(18_000_000),
            9,
        )
        .with_attribution(3, 0)
        .with_flags(core_types::FILL_FLAG_SETTLEMENT);
        append(&old.join(crate::FILLS_FILE), &bytes_of(&settle));
        append(&cur.join(crate::ORDERS_FILE), &bytes_of(&o));
        let text = format!(
            "tag = 'SA'\nstate_url = 'http://127.0.0.1:1/state'\nstate_dir = '{}'\ndry_run = true\n[[slot]]\nslot = 3\n[[watch]]\nname = 'P2'\nslot = 3\nstart = '2026-10-01T00:00:00Z'\n",
            d.join("st").display()
        );
        let mut r = Runner::new(Config::parse(&text).unwrap()).unwrap();
        let now_s = (clock::now_wall_ns() / 1_000_000_000) as i64;
        r.backfill(&cur.to_string_lossy(), now_s);
        let mut out = Vec::new();
        r.model.tick(now_s + 3600, &mut out);
        assert!(out.iter().all(|m| !m.text.contains("attempts")), "{out:?}");
        assert_eq!(r.model.settled.len(), 1);
        assert_eq!(r.model.settled[0].gross, 5_400_000_000_000);
        assert_eq!(
            r.tails.as_ref().unwrap().orders.consumed(),
            1,
            "the current run's orders skip to their end"
        );
        assert!(r.outbox().pending().unwrap().is_empty());
    }

    /// A whole BIN15 live instance end to end through the captures: the
    /// three venue fills of 10-22 / 10-23 as the known answers.
    #[test]
    fn captures_to_messages_end_to_end() {
        let d = dir("e2e");
        let run = d.join("run-1");
        std::fs::create_dir_all(&run).unwrap();
        let fills = run.join(crate::FILLS_FILE);
        let orders = run.join(crate::ORDERS_FILE);
        let events = run.join(crate::EVENTS_FILE);
        write_header(&fills, SlotKind::Fill);
        write_header(&orders, SlotKind::Order);
        write_header(&events, SlotKind::OrderEvent);
        let uni = d.join("universe.toml");
        std::fs::write(&uni, "[hyperliquid]\nrolling = [\"out:BTC:15m\", \"out:ETH:15m\", \"out:SOL:15m\", \"out:HYPE:15m\"]\n").unwrap();
        let cfg = Config::parse(&format!(
            "tag = \"SA\"\nstate_url = \"http://127.0.0.1:1/state\"\nuniverse = \"{}\"\nstate_dir = \"{}\"\ndry_run = true\n[[slot]]\nslot = 3\nlive_only = false\n[[watch]]\nname = \"P2\"\nslot = 3\nstart = \"2026-10-01T00:00:00Z\"\nfirst_n = 20\nmax_losses_in_first = 2\nhit_after = 30\nmin_hit_pct = 93\n",
            uni.display(),
            d.join("st").display()
        ))
        .unwrap();
        let mut r = Runner::new(cfg).unwrap();
        // Point the runner at the run directly (no engine on port 1).
        r.saved_cursor = Some(Cursor {
            run_dir: run.to_string_lossy().into(),
            fills: 0,
            orders: 0,
            events: 0,
        });
        let hl = |ord: u32| core_types::make_symbol_id(VenueId::Hyperliquid, ord);
        let eth_no = hl(4096 + 3);
        // 10-22 07:29:41Z: ETH No 18 × 0.70 (instance 07:30), settles at 1.
        let t_fill = clock::parse_rfc3339_z("2026-10-22T07:29:41Z").unwrap() as u64 * 1_000_000_000;
        let t_settle =
            clock::parse_rfc3339_z("2026-10-22T07:30:05Z").unwrap() as u64 * 1_000_000_000;
        let mut o = Order::new(
            1,
            VenueId::Hyperliquid,
            eth_no,
            Side::Bid,
            0,
            Price(700_000),
            Qty(18_000_000),
            501,
        );
        o.strategy_id = 3;
        append(&orders, &bytes_of(&o));
        append(
            &events,
            &bytes_of(&OrderEvent::new(
                2,
                VenueId::Hyperliquid,
                eth_no,
                501,
                3,
                core_types::ORDER_EVENT_FILLED,
                0,
                0,
            )),
        );
        append(
            &fills,
            &bytes_of(
                &Fill::new(
                    t_fill,
                    eth_no,
                    Side::Bid,
                    Price(700_000),
                    Qty(18_000_000),
                    501,
                )
                .with_attribution(3, 0),
            ),
        );
        append(
            &fills,
            &bytes_of(
                &Fill::new(
                    t_settle,
                    eth_no,
                    Side::Ask,
                    Price(1_000_000),
                    Qty(18_000_000),
                    9_999,
                )
                .with_attribution(3, 0)
                .with_flags(core_types::FILL_FLAG_SETTLEMENT),
            ),
        );
        r.poll();
        let p = r.outbox().pending().unwrap();
        let texts: Vec<&str> = p.iter().map(|m| m.text.as_str()).collect();
        let fill = texts
            .iter()
            .find(|t| t.contains("FILL"))
            .expect("a FILL message");
        assert!(fill.starts_with("[SA] FILL s3 LIVE · ETH 15m No · exp 07:30Z τ 19.0s · 18 × 0.700 = $12.60 · lim 0.700"), "{fill}");
        let settled = p
            .iter()
            .find(|m| m.text.contains("SETTLED"))
            .expect("a SETTLED message");
        assert!(settled.loud);
        assert!(
            settled.text.starts_with(
                "[SA] SETTLED WON · ETH 15m No exp 07:30Z · 18 @ 0.700 → $18.00 · gross +$5.40"
            ),
            "{}",
            settled.text
        );
        assert!(
            settled
                .text
                .contains("P2: 1/30 fills · losses 0 in first 20 · hit 100.0 % · net +$5.40"),
            "{}",
            settled.text
        );
        // The ledger and the cursor persisted.
        let store = Store::open(&d.join("st")).unwrap();
        assert_eq!(store.load_settled().len(), 1);
        assert_eq!(store.load_cursor().unwrap().fills, 2);
        assert!(store.load_positions().is_empty());
    }
}
