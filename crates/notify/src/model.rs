// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! What happened, turned into messages. The model folds the three capture
//! streams and the `/state` snapshots into: fills and settlements (with
//! the instance's gross P&L from its own fills), order attempts coalesced
//! per instance, safety transitions, forward-test watch tallies and the
//! hourly / daily digests. It never acts on the engine; every output is a
//! [`Msg`].
//!
//! Tiers (R-NT-6): settlements and safety ring; fills, attempts and
//! digests go silently (`disable_notification`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::capture::{
    EventRec, FillRec, OrderRec, OEV_REJECTED, ORDER_VERB_PLACE, REASON_IOC_MISS,
};
use crate::clock;
use crate::config::{Config, WatchCfg, WatchUnit};
use crate::fmt;
use crate::names::{expiry_after, Names};
use crate::persist::{Position, Settled};
use crate::state::StateView;

/// One message for the outbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Msg {
    /// Ring (true) or deliver silently.
    pub loud: bool,
    /// The text, tag included.
    pub text: String,
}

/// A settlement arriving this long after its instance's expiry still
/// matches it.
const SETTLE_SLACK_S: i64 = 120;
/// An attempt group is flushed after this much quiet …
const GROUP_QUIET_S: i64 = 15;
/// … or this long after its instance expired.
const GROUP_AFTER_EXPIRY_S: i64 = 5;
/// Refusal / reconciliation bursts are flushed after this much quiet.
const BURST_QUIET_S: i64 = 20;
/// Orders older than this are forgotten.
const ORDER_TTL_S: i64 = 1800;

/// Counters for one hour or one day.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Period {
    /// Hour or day index.
    pub key: i64,
    /// When counting began, s (later than the period's start after a
    /// first start mid-period).
    pub start_s: i64,
    /// Fill rows.
    pub fills: u64,
    /// Their notional ×1e12.
    pub fill_notional: i128,
    /// Settled won.
    pub settled_w: u64,
    /// Settled lost.
    pub settled_l: u64,
    /// Gross of the settled ×1e12.
    pub gross: i128,
    /// Orders sent.
    pub sent: u64,
    /// IoC misses.
    pub missed: u64,
    /// Other rejections.
    pub rejected: u64,
    /// Router / gateway refusals.
    pub refused: u64,
    /// Holds begun.
    pub holds: u64,
    /// Longest hold, s.
    pub hold_max_s: i64,
    /// Engine boots seen.
    pub restarts: u64,
    /// Seconds `/state` was silent.
    pub dark_s: i64,
    /// AI-inbox messages relayed.
    pub inbox: u64,
}

impl Period {
    fn new(key: i64, start_s: i64) -> Period {
        Period {
            key,
            start_s,
            ..Period::default()
        }
    }

    /// The kv spelling.
    #[must_use]
    pub fn encode(&self) -> String {
        format!(
            "{} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
            self.key,
            self.start_s,
            self.fills,
            self.fill_notional,
            self.settled_w,
            self.settled_l,
            self.gross,
            self.sent,
            self.missed,
            self.rejected,
            self.refused,
            self.holds,
            self.hold_max_s,
            self.restarts,
            self.dark_s,
            self.inbox
        )
    }

    /// Parse [`Period::encode`].
    #[must_use]
    pub fn decode(s: &str) -> Option<Period> {
        let f: Vec<&str> = s.split(' ').collect();
        if f.len() != 16 {
            return None;
        }
        Some(Period {
            key: f[0].parse().ok()?,
            start_s: f[1].parse().ok()?,
            fills: f[2].parse().ok()?,
            fill_notional: f[3].parse().ok()?,
            settled_w: f[4].parse().ok()?,
            settled_l: f[5].parse().ok()?,
            gross: f[6].parse().ok()?,
            sent: f[7].parse().ok()?,
            missed: f[8].parse().ok()?,
            rejected: f[9].parse().ok()?,
            refused: f[10].parse().ok()?,
            holds: f[11].parse().ok()?,
            hold_max_s: f[12].parse().ok()?,
            restarts: f[13].parse().ok()?,
            dark_s: f[14].parse().ok()?,
            inbox: f[15].parse().ok()?,
        })
    }
}

#[derive(Debug, Clone)]
struct OrderInfo {
    sym: u32,
    expiry_s: i64,
    px: i64,
    wall_s: i64,
}

#[derive(Debug, Clone, Default)]
struct Group {
    slot: u8,
    sent: u64,
    filled: BTreeSet<u64>,
    filled_qty: i64,
    missed: BTreeSet<u64>,
    rejected: BTreeMap<&'static str, u64>,
    lim_min: i64,
    lim_max: i64,
    first_tau_ns: Option<i64>,
    last_s: i64,
}

#[derive(Debug, Clone, Default)]
struct Burst {
    detail: BTreeMap<String, u64>,
    first_s: i64,
    last_s: i64,
}

impl Burst {
    fn add(&mut self, key: &str, n: u64, now_s: i64) {
        if n == 0 {
            return;
        }
        if self.detail.is_empty() {
            self.first_s = now_s;
        }
        *self.detail.entry(key.to_string()).or_default() += n;
        self.last_s = now_s;
    }

    fn total(&self) -> u64 {
        self.detail.values().sum()
    }

    fn take_if_quiet(&mut self, now_s: i64) -> Option<(u64, String, i64)> {
        if self.detail.is_empty() || now_s - self.last_s < BURST_QUIET_S {
            return None;
        }
        let total = self.total();
        let detail = self
            .detail
            .iter()
            .map(|(k, v)| format!("{k} {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let span = self.last_s - self.first_s;
        self.detail.clear();
        Some((total, detail, span))
    }
}

/// The model.
#[derive(Debug)]
pub struct Model {
    cfg: Config,
    names: Names,
    /// Open positions.
    pub positions: BTreeMap<(u32, i64), Position>,
    /// Every settled instance (loaded from, and appended to, the store).
    pub settled: Vec<Settled>,
    /// Settled this poll, for the store to append.
    pub newly_settled: Vec<Settled>,
    /// Keyed values to persist.
    pub kv: BTreeMap<String, String>,
    /// Something in `positions` or `kv` changed.
    pub dirty: bool,
    orders: HashMap<u64, OrderInfo>,
    groups: BTreeMap<(u32, i64), Group>,
    last_state: Option<StateView>,
    dark_since: Option<i64>,
    dark_announced: bool,
    hold_since: [Option<i64>; 16],
    hold_announced: [bool; 16],
    boot_seen_s: i64,
    seed_warned: bool,
    refused: Burst,
    recon: Burst,
    budget_warned: bool,
    assume_live: bool,
    /// The current hour's counters.
    pub hour: Period,
    /// The current day's counters.
    pub day: Period,
    started: bool,
}

fn notional(px: i64, qty: i64) -> i128 {
    i128::from(px) * i128::from(qty)
}

impl Model {
    /// A model over persisted state.
    #[must_use]
    pub fn new(
        cfg: Config,
        names: Names,
        positions: BTreeMap<(u32, i64), Position>,
        settled: Vec<Settled>,
        kv: BTreeMap<String, String>,
        now_s: i64,
    ) -> Model {
        let hour_key = now_s.div_euclid(3600);
        let day_key = (now_s - i64::from(cfg.daily_utc_hour) * 3600).div_euclid(86_400);
        let hour = kv
            .get("hour")
            .and_then(|s| Period::decode(s))
            .filter(|p| p.key == hour_key)
            .unwrap_or_else(|| Period::new(hour_key, now_s));
        let day = kv
            .get("day")
            .and_then(|s| Period::decode(s))
            .filter(|p| p.key == day_key)
            .unwrap_or_else(|| Period::new(day_key, now_s));
        Model {
            cfg,
            names,
            positions,
            settled,
            newly_settled: Vec::new(),
            kv,
            dirty: false,
            orders: HashMap::new(),
            groups: BTreeMap::new(),
            last_state: None,
            dark_since: None,
            dark_announced: false,
            hold_since: [None; 16],
            hold_announced: [false; 16],
            boot_seen_s: now_s,
            seed_warned: false,
            refused: Burst::default(),
            recon: Burst::default(),
            budget_warned: false,
            assume_live: false,
            hour,
            day,
            started: false,
        }
    }

    fn tag(&self) -> &str {
        &self.cfg.tag
    }

    fn msg(&self, loud: bool, body: &str) -> Msg {
        Msg {
            loud,
            text: format!("[{}] {body}", self.cfg.tag),
        }
    }

    /// The last `/state` seen.
    #[must_use]
    pub fn last_state(&self) -> Option<&StateView> {
        self.last_state.as_ref()
    }

    fn wall_s_of(&self, ts_ns: u64, now_s: i64) -> i64 {
        if ts_ns > 1_500_000_000_000_000_000 {
            return (ts_ns / 1_000_000_000) as i64;
        }
        match &self.last_state {
            Some(s) if s.mono_ns > 0 => (s.mono_to_wall_ns(ts_ns) / 1_000_000_000) as i64,
            _ => now_s,
        }
    }

    fn mode_word(&self, slot: u8) -> String {
        match &self.last_state {
            Some(s) => {
                let halted = s
                    .exec
                    .halted
                    .get(usize::from(slot))
                    .map_or("none", String::as_str);
                if halted != "none" {
                    format!("s{slot} HALTED {halted}")
                } else if s.slot_live(slot) {
                    format!("s{slot} LIVE")
                } else {
                    format!("s{slot} paper")
                }
            }
            None => format!("s{slot} ?"),
        }
    }

    fn any_live(&self) -> bool {
        self.assume_live
            || self
                .last_state
                .as_ref()
                .is_some_and(|s| self.cfg.slots.iter().any(|c| s.slot_live(c.slot)))
    }

    /// Treat every configured slot as live (the `replay` verb, which has
    /// no `/state`).
    pub fn assume_live(&mut self) {
        self.assume_live = true;
    }

    fn instance_of(&self, sym: u32, wall_s: i64) -> i64 {
        self.names
            .name(sym)
            .family
            .map_or(0, |f| expiry_after(wall_s, f.period_s))
    }

    fn session_line(&self) -> Option<String> {
        let s = self.last_state.as_ref()?;
        if !self.any_live() {
            return None;
        }
        let worth = s.exec.anchor_1e6 + s.exec.session_pnl_1e6;
        Some(format!(
            "session net {} (worth {} / anchor {})",
            fmt::usd_signed_1e6(s.exec.session_pnl_1e6),
            fmt::plain_1e6(worth),
            fmt::plain_1e6(s.exec.anchor_1e6)
        ))
    }

    // ---- fills ----------------------------------------------------------

    /// Fold a fill. `quiet` (backfill) updates positions and the ledger
    /// without messages or period counts.
    pub fn on_fill(&mut self, f: &FillRec, now_s: i64, quiet: bool, out: &mut Vec<Msg>) {
        let Some(sc) = self.cfg.slot(f.strategy_id).cloned() else {
            return;
        };
        if sc.live_only && f.is_paper() {
            return;
        }
        let wall_s = self.wall_s_of(f.ts_ns, now_s);
        if f.is_settlement() {
            self.on_settlement(f, wall_s, quiet, out);
            return;
        }
        let expiry = self.instance_of(f.sym, wall_s);
        let pos = self
            .positions
            .entry((f.sym, expiry))
            .or_insert_with(|| Position {
                slot: f.strategy_id,
                paper: f.is_paper(),
                first_ts_s: wall_s,
                ..Position::default()
            });
        let n = notional(f.px, f.qty);
        if f.side == 0 {
            pos.qty += f.qty;
            pos.buy_qty += f.qty;
            pos.buy_cost += n;
            pos.cash -= n;
        } else {
            pos.qty -= f.qty;
            pos.cash += n;
        }
        if !f.is_adopted() {
            pos.fills += 1;
        }
        self.dirty = true;
        if quiet || f.is_adopted() {
            return;
        }
        self.hour.fills += 1;
        self.day.fills += 1;
        self.hour.fill_notional += n;
        self.day.fill_notional += n;
        let mut lim = String::new();
        if let Some(o) = self.orders.get(&f.order_id) {
            lim = format!(" · lim {}", fmt::px_1e6(o.px));
            let key = (o.sym, o.expiry_s);
            if let Some(g) = self.groups.get_mut(&key) {
                g.filled.insert(f.order_id);
                g.filled_qty += f.qty;
                g.last_s = now_s;
            }
        }
        let name = self.names.name(f.sym);
        let exp = if expiry > 0 {
            let ts_wall_ns = if f.ts_ns > 1_500_000_000_000_000_000 {
                f.ts_ns
            } else {
                self.last_state
                    .as_ref()
                    .map_or(0, |s| s.mono_to_wall_ns(f.ts_ns))
            };
            let tau = if ts_wall_ns > 1_500_000_000_000_000_000 {
                format!(
                    " τ {}",
                    fmt::secs_1(expiry * 1_000_000_000 - ts_wall_ns.min(i64::MAX as u64) as i64)
                )
            } else {
                String::new()
            };
            format!(" · exp {}{tau}", clock::hhmm(expiry))
        } else {
            String::new()
        };
        let side = if f.side == 0 { "" } else { " SELL" };
        let body = format!(
            "FILL{side} s{} {} · {}{exp} · {} × {} = {}{lim}",
            f.strategy_id,
            if f.is_paper() { "PAPER" } else { "LIVE" },
            name.text,
            fmt::qty_1e6(f.qty),
            fmt::px_1e6(f.px),
            fmt::usd_1e12(n),
        );
        out.push(self.msg(false, &body));
    }

    fn on_settlement(&mut self, f: &FillRec, wall_s: i64, quiet: bool, out: &mut Vec<Msg>) {
        let key = self
            .positions
            .range((f.sym, i64::MIN)..=(f.sym, wall_s + SETTLE_SLACK_S))
            .rev()
            .find(|(_, p)| p.qty > 0)
            .map(|(k, _)| *k);
        let name = self.names.name(f.sym);
        let won = f.px >= 500_000;
        let payout = notional(f.px, f.qty);
        let Some(key) = key else {
            if !quiet {
                let body = format!(
                    "SETTLED {} · {} · {} @ {} → {} (no recorded entry)",
                    if won { "WON" } else { "LOST" },
                    name.text,
                    fmt::qty_1e6(f.qty),
                    fmt::px_1e6(f.px),
                    fmt::usd_1e12(payout)
                );
                out.push(self.msg(true, &body));
            }
            return;
        };
        let Some(pos) = self.positions.get_mut(&key) else {
            return;
        };
        pos.cash += payout;
        pos.qty -= f.qty;
        self.dirty = true;
        if pos.qty > 0 {
            return; // a partial settlement: wait for the rest
        }
        let pos = self.positions.remove(&key).unwrap_or_default();
        let s = Settled {
            settle_ts_s: wall_s,
            expiry_s: key.1,
            sym: f.sym,
            slot: pos.slot,
            paper: pos.paper,
            buy_qty: pos.buy_qty,
            buy_cost: pos.buy_cost,
            proceeds: pos.cash + pos.buy_cost,
            gross: pos.cash,
            won,
            fills: pos.fills,
            first_ts_s: pos.first_ts_s,
        };
        self.settled.push(s.clone());
        self.newly_settled.push(s.clone());
        if quiet {
            return;
        }
        if won {
            self.hour.settled_w += 1;
            self.day.settled_w += 1;
        } else {
            self.hour.settled_l += 1;
            self.day.settled_l += 1;
        }
        self.hour.gross += s.gross;
        self.day.gross += s.gross;
        let avg = if s.buy_qty > 0 {
            (s.buy_cost / i128::from(s.buy_qty)) as i64
        } else {
            0
        };
        let exp = if key.1 > 0 {
            format!(" exp {}", clock::hhmm(key.1))
        } else {
            String::new()
        };
        let mut body = format!(
            "SETTLED {} · {}{exp} · {} @ {} → {} · gross {}",
            if won { "WON" } else { "LOST" },
            name.text,
            fmt::qty_1e6(s.buy_qty),
            fmt::px_1e6(avg),
            fmt::usd_1e12(s.proceeds),
            fmt::usd_signed_1e12(s.gross)
        );
        if s.paper {
            body.push_str(" · PAPER");
        }
        let mut tail = Vec::new();
        if !s.paper {
            if let Some(l) = self.session_line() {
                tail.push(l);
            }
        }
        // The settlement itself goes first, any watch alert after it.
        let mut alerts = Vec::new();
        for w in self.cfg.watches.clone() {
            if w.slot == s.slot && !s.paper {
                let t = self.watch_tally(&w);
                tail.push(t.summary(&w));
                self.watch_alerts(&w, &t, &mut alerts);
            }
        }
        if !tail.is_empty() {
            body.push('\n');
            body.push_str(&tail.join(" · "));
        }
        out.push(self.msg(true, &body));
        out.extend(alerts);
    }

    // ---- orders and events ------------------------------------------------

    /// Fold an order intent.
    pub fn on_order(&mut self, o: &OrderRec, now_s: i64) {
        if o.verb != ORDER_VERB_PLACE {
            return;
        }
        let Some(sc) = self.cfg.slot(o.strategy_id) else {
            return;
        };
        if !sc.attempts {
            return;
        }
        if sc.live_only
            && !self
                .last_state
                .as_ref()
                .is_some_and(|s| s.slot_live(o.strategy_id))
        {
            return;
        }
        let wall_s = self.wall_s_of(o.ts_ns, now_s);
        let expiry = self.instance_of(o.sym, wall_s);
        self.orders.insert(
            o.client_oid,
            OrderInfo {
                sym: o.sym,
                expiry_s: expiry,
                px: o.px,
                wall_s,
            },
        );
        let g = self.groups.entry((o.sym, expiry)).or_insert_with(|| Group {
            slot: o.strategy_id,
            lim_min: i64::MAX,
            lim_max: i64::MIN,
            ..Group::default()
        });
        g.sent += 1;
        g.lim_min = g.lim_min.min(o.px);
        g.lim_max = g.lim_max.max(o.px);
        if g.first_tau_ns.is_none() && expiry > 0 {
            let ts = self
                .last_state
                .as_ref()
                .map_or(o.ts_ns, |s| s.mono_to_wall_ns(o.ts_ns));
            if ts > 1_500_000_000_000_000_000 {
                g.first_tau_ns = Some(expiry * 1_000_000_000 - ts as i64);
            }
        }
        g.last_s = now_s;
        self.hour.sent += 1;
        self.day.sent += 1;
    }

    /// Fold an order event.
    pub fn on_event(&mut self, e: &EventRec, now_s: i64) {
        let Some(o) = self.orders.get(&e.client_oid).cloned() else {
            return;
        };
        let Some(g) = self.groups.get_mut(&(o.sym, o.expiry_s)) else {
            return;
        };
        if e.kind == OEV_REJECTED {
            if e.reason == REASON_IOC_MISS {
                g.missed.insert(e.client_oid);
                self.hour.missed += 1;
                self.day.missed += 1;
            } else {
                *g.rejected
                    .entry(crate::capture::reason_word(e.reason))
                    .or_default() += 1;
                self.hour.rejected += 1;
                self.day.rejected += 1;
            }
        }
        g.last_s = now_s;
    }

    fn flush_groups(&mut self, now_s: i64, out: &mut Vec<Msg>) {
        let due: Vec<(u32, i64)> = self
            .groups
            .iter()
            .filter(|((_, exp), g)| {
                now_s - g.last_s >= GROUP_QUIET_S
                    || (*exp > 0 && now_s >= exp + GROUP_AFTER_EXPIRY_S)
            })
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            let Some(g) = self.groups.remove(&key) else {
                continue;
            };
            let name = self.names.name(key.0);
            let missed = g
                .missed
                .iter()
                .filter(|oid| !g.filled.contains(oid))
                .count();
            let mut body = format!("attempts s{} · {}", g.slot, name.text);
            if key.1 > 0 {
                body.push_str(&format!(" exp {}", clock::hhmm(key.1)));
            }
            body.push_str(&format!(" · sent {}", g.sent));
            if !g.filled.is_empty() {
                body.push_str(&format!(
                    " · filled {} ({})",
                    g.filled.len(),
                    fmt::qty_1e6(g.filled_qty)
                ));
            } else {
                body.push_str(" · filled 0");
            }
            body.push_str(&format!(" · missed {missed}"));
            if !g.rejected.is_empty() {
                let n: u64 = g.rejected.values().sum();
                let why = g
                    .rejected
                    .iter()
                    .map(|(k, v)| format!("{k} {v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                body.push_str(&format!(" · rejected {n} ({why})"));
            }
            if g.lim_min <= g.lim_max {
                if g.lim_min == g.lim_max {
                    body.push_str(&format!(" · lim {}", fmt::px_1e6(g.lim_min)));
                } else {
                    body.push_str(&format!(
                        " · lim {}–{}",
                        fmt::px_1e6(g.lim_min),
                        fmt::px_1e6(g.lim_max)
                    ));
                }
            }
            if let Some(t) = g.first_tau_ns {
                body.push_str(&format!(" · first τ {}", fmt::secs_1(t)));
            }
            out.push(self.msg(false, &body));
        }
        self.orders.retain(|_, o| now_s - o.wall_s < ORDER_TTL_S);
    }

    // ---- /state -------------------------------------------------------------

    /// Fold a `/state` poll: `Some` when it answered, `None` when silent.
    pub fn on_state(&mut self, now_s: i64, st: Option<&StateView>, out: &mut Vec<Msg>) {
        let Some(s) = st else {
            let since = *self.dark_since.get_or_insert(now_s);
            if let Some(after) = self.cfg.dark_after_s {
                if !self.dark_announced && now_s - since >= after as i64 {
                    self.dark_announced = true;
                    let body = format!(
                        "ENGINE DARK: /state silent since {} ({})",
                        clock::hhmmss(since),
                        fmt::dur(now_s - since)
                    );
                    out.push(self.msg(true, &body));
                }
            }
            return;
        };
        if let Some(since) = self.dark_since.take() {
            let gap = now_s - since;
            self.hour.dark_s += gap;
            self.day.dark_s += gap;
            if self.dark_announced {
                self.dark_announced = false;
                let body = format!("ENGINE BACK after {}", fmt::dur(gap));
                out.push(self.msg(true, &body));
            }
        }
        let prev = self.last_state.take();
        let boot_id = s.boot_id();
        let new_boot = self.kv.get("boot").is_none_or(|b| *b != boot_id);
        if !self.started {
            self.started = true;
            self.last_state = Some(s.clone());
            let modes: Vec<String> = self
                .cfg
                .slots
                .iter()
                .map(|c| self.mode_word(c.slot))
                .collect();
            let body = format!(
                "notify started · engine {} pid {} · {}",
                s.git_sha,
                s.pid,
                modes.join(" · ")
            );
            out.push(self.msg(false, &body));
            if new_boot {
                self.kv.insert("boot".into(), boot_id);
                self.kv.remove("session_hit");
                self.dirty = true;
            }
            self.boot_seen_s = now_s;
            return;
        }
        self.last_state = Some(s.clone());
        if new_boot {
            self.kv.insert("boot".into(), boot_id);
            self.kv.remove("session_hit");
            self.dirty = true;
            self.hour.restarts += 1;
            self.day.restarts += 1;
            self.boot_seen_s = now_s;
            self.seed_warned = false;
            self.budget_warned = false;
            self.hold_since = [None; 16];
            self.hold_announced = [false; 16];
            let live = self.any_live();
            let modes: Vec<String> = self
                .cfg
                .slots
                .iter()
                .map(|c| self.mode_word(c.slot))
                .collect();
            let run = s.run_dir.rsplit('/').next().unwrap_or("");
            let body = format!(
                "BOOT {} · pid {} · {} · {run} · seeded {}",
                s.git_sha,
                s.pid,
                modes.join(" · "),
                u8::from(s.exec.seeded)
            );
            out.push(self.msg(live, &body));
            return; // counters restart with the boot: no deltas across it
        }
        let Some(p) = prev else {
            return;
        };
        let live = self.any_live();
        // Seeding.
        if live && !p.exec.seeded && s.exec.seeded {
            let body = format!(
                "SEEDED (+{} after boot)",
                fmt::dur(now_s - self.boot_seen_s)
            );
            out.push(self.msg(false, &body));
        }
        if let Some(after) = self.cfg.seed_warn_s {
            if live
                && !s.exec.seeded
                && !self.seed_warned
                && now_s - self.boot_seen_s >= after as i64
            {
                self.seed_warned = true;
                let body = format!("NOT SEEDED {} after boot: every live order is refused until a reconciliation agrees", fmt::dur(now_s - self.boot_seen_s));
                out.push(self.msg(true, &body));
            }
        }
        // Halts and holds, per configured slot.
        for c in self.cfg.slots.clone() {
            let i = usize::from(c.slot);
            let was = p.exec.halted.get(i).map_or("none", String::as_str);
            let now = s.exec.halted.get(i).map_or("none", String::as_str);
            if was != now {
                let body = if now == "none" {
                    format!("HALT CLEARED s{} (was {was})", c.slot)
                } else {
                    format!("HALT s{} reason={now}", c.slot)
                };
                out.push(self.msg(true, &body));
            }
            let held_now = s
                .exec
                .held
                .get(i)
                .map_or("none", String::as_str)
                .to_string();
            match (self.hold_since[i], held_now == "none") {
                (None, false) => {
                    self.hold_since[i] = Some(now_s);
                    self.hour.holds += 1;
                    self.day.holds += 1;
                }
                (Some(since), true) => {
                    let d = now_s - since;
                    self.hour.hold_max_s = self.hour.hold_max_s.max(d);
                    self.day.hold_max_s = self.day.hold_max_s.max(d);
                    if self.hold_announced[i] {
                        let body = format!("RESUMED s{} after {}", c.slot, fmt::dur(d));
                        out.push(self.msg(true, &body));
                    }
                    self.hold_since[i] = None;
                    self.hold_announced[i] = false;
                }
                _ => {}
            }
            if let (Some(since), Some(after)) = (self.hold_since[i], self.cfg.hold_loud_s) {
                if !self.hold_announced[i] && now_s - since >= after as i64 {
                    self.hold_announced[i] = true;
                    let body = format!(
                        "HOLD s{} {held_now} ≥ {} (since {})",
                        c.slot,
                        fmt::dur(now_s - since),
                        clock::hhmmss(since)
                    );
                    out.push(self.msg(true, &body));
                }
            }
        }
        if !p.exec.halt_file_present && s.exec.halt_file_present {
            out.push(self.msg(true, "exec.HALT file present"));
        }
        // Refusals and reconciliation (exec-wide; reported while configured).
        if s.exec.configured {
            for (k, n) in &s.exec.refused {
                let before = p
                    .exec
                    .refused
                    .iter()
                    .find(|(pk, _)| pk == k)
                    .map_or(0, |(_, v)| *v);
                let d = n.saturating_sub(before);
                if d > 0 {
                    self.refused.add(k, d, now_s);
                    self.hour.refused += d;
                    self.day.refused += d;
                }
            }
            self.recon.add(
                "recon_failed",
                s.exec.recon_failed.saturating_sub(p.exec.recon_failed),
                now_s,
            );
            self.recon.add(
                "drift_legs",
                s.exec
                    .recon_drift_legs
                    .saturating_sub(p.exec.recon_drift_legs),
                now_s,
            );
            self.recon.add(
                "unseen_legs",
                s.exec
                    .recon_unseen_legs
                    .saturating_sub(p.exec.recon_unseen_legs),
                now_s,
            );
            if let Some(w) = self.cfg.budget_warn {
                if s.exec.budget_remaining < w && !self.budget_warned {
                    self.budget_warned = true;
                    let body = format!("BUDGET request budget {} < {w}", s.exec.budget_remaining);
                    out.push(self.msg(true, &body));
                } else if s.exec.budget_remaining >= w {
                    self.budget_warned = false;
                }
            }
        }
        if live && !self.cfg.session_warn_usd.is_empty() {
            let mut hit: BTreeSet<i64> = self
                .kv
                .get("session_hit")
                .map(|v| v.split(',').filter_map(|x| x.parse().ok()).collect())
                .unwrap_or_default();
            for &lvl in &self.cfg.session_warn_usd.clone() {
                if s.exec.session_pnl_1e6 <= lvl * 1_000_000 && hit.insert(lvl) {
                    let body = format!(
                        "SESSION {} crossed {} (worth {} / anchor {})",
                        fmt::usd_signed_1e6(s.exec.session_pnl_1e6),
                        fmt::usd_signed_1e6(lvl * 1_000_000),
                        fmt::plain_1e6(s.exec.anchor_1e6 + s.exec.session_pnl_1e6),
                        fmt::plain_1e6(s.exec.anchor_1e6)
                    );
                    out.push(self.msg(true, &body));
                    self.kv.insert(
                        "session_hit".into(),
                        hit.iter().map(i64::to_string).collect::<Vec<_>>().join(","),
                    );
                    self.dirty = true;
                }
            }
        }
    }

    // ---- watches ------------------------------------------------------------

    fn watch_tally(&self, w: &WatchCfg) -> Tally {
        let mut rows: Vec<(i64, bool, u32, i128)> = self
            .settled
            .iter()
            .filter(|s| s.slot == w.slot && !s.paper && s.first_ts_s >= w.start_s && s.fills > 0)
            .map(|s| (s.first_ts_s, s.won, s.fills, s.gross))
            .collect();
        rows.sort_by_key(|r| r.0);
        let mut t = Tally::default();
        for (_, won, fills, gross) in rows {
            let units = match w.unit {
                WatchUnit::Fill => fills,
                WatchUnit::Instance => 1,
            };
            for _ in 0..units {
                t.n += 1;
                if won {
                    t.won += 1;
                } else if w.first_n.is_some_and(|f| t.n <= u64::from(f)) {
                    t.losses_in_first += 1;
                }
            }
            t.net += gross;
        }
        t
    }

    fn watch_alerts(&mut self, w: &WatchCfg, t: &Tally, out: &mut Vec<Msg>) {
        let mut met = Vec::new();
        if let (Some(f), Some(m)) = (w.first_n, w.max_losses_in_first) {
            if t.losses_in_first >= m {
                met.push((
                    "first",
                    format!(
                        "{} losses within the first {f} {}",
                        t.losses_in_first,
                        w.unit_word()
                    ),
                ));
            }
        }
        if let (Some(after), Some(pct)) = (w.hit_after, w.min_hit_pct) {
            if t.n >= u64::from(after) && t.won * 100 < u64::from(pct) * t.n {
                met.push((
                    "hit",
                    format!(
                        "hit rate {} < {pct} % after {} {}",
                        fmt::pct(t.won, t.n),
                        t.n,
                        w.unit_word()
                    ),
                ));
            }
        }
        if let Some(l) = w.loss_net_usd {
            if t.net <= i128::from(l) * 1_000_000_000_000 {
                met.push((
                    "net",
                    format!(
                        "realised net {} ≤ {}",
                        fmt::usd_signed_1e12(t.net),
                        fmt::usd_signed_1e6(l * 1_000_000)
                    ),
                ));
            }
        }
        for (k, why) in met {
            let key = format!("watch_met:{}:{k}", w.name);
            if self.kv.contains_key(&key) {
                continue;
            }
            self.kv.insert(key, "1".into());
            self.dirty = true;
            let body = format!(
                "WATCH {}: criterion met — {why}. Reporting only: disarming is yours (bin15-flip.sh paper / exec.HALT).",
                w.name
            );
            out.push(self.msg(true, &body));
        }
    }

    /// The watch summaries, for digests.
    #[must_use]
    pub fn watch_summaries(&self) -> Vec<String> {
        self.cfg
            .watches
            .iter()
            .map(|w| self.watch_tally(w).summary(w))
            .collect()
    }

    // ---- the clock ----------------------------------------------------------

    /// Time-driven work: flush coalesced groups and bursts, agent expiry,
    /// the hourly and daily digests.
    pub fn tick(&mut self, now_s: i64, out: &mut Vec<Msg>) {
        self.flush_groups(now_s, out);
        if let Some((n, detail, span)) = self.refused.take_if_quiet(now_s) {
            let slots: Vec<String> = self
                .cfg
                .slots
                .iter()
                .map(|c| format!("s{}", c.slot))
                .collect();
            let body = format!(
                "REFUSED {} · +{n} in {} ({detail})",
                slots.join(","),
                fmt::dur(span)
            );
            out.push(self.msg(true, &body));
        }
        if let Some((_, detail, span)) = self.recon.take_if_quiet(now_s) {
            let body = format!("RECON {detail} (in {})", fmt::dur(span));
            out.push(self.msg(true, &body));
        }
        if let Some(until) = self.cfg.agent_valid_until_s {
            let days_left = (until - now_s).div_euclid(86_400)
                + i64::from((until - now_s).rem_euclid(86_400) > 0);
            for d in self.cfg.agent_warn_days.clone() {
                let key = format!("agent_warned:{d}");
                if days_left <= d && !self.kv.contains_key(&key) {
                    self.kv.insert(key, "1".into());
                    self.dirty = true;
                    let body = format!(
                        "AGENT the venue API wallet expires in {} ({}) — renew it before then",
                        fmt::dur((until - now_s).max(0)),
                        clock::mmdd_hhmm(until)
                    );
                    out.push(self.msg(true, &body));
                }
            }
        }
        let hour_key = now_s.div_euclid(3600);
        if hour_key != self.hour.key {
            let done = std::mem::replace(&mut self.hour, Period::new(hour_key, hour_key * 3600));
            let mark = done.key.to_string();
            if self.cfg.hourly_when_live
                && self.any_live()
                && self.kv.get("hourly_sent") != Some(&mark)
            {
                out.push(self.hourly(&done));
                self.kv.insert("hourly_sent".into(), mark);
            }
            self.dirty = true;
        }
        let off = i64::from(self.cfg.daily_utc_hour) * 3600;
        let day_key = (now_s - off).div_euclid(86_400);
        if day_key != self.day.key {
            let done =
                std::mem::replace(&mut self.day, Period::new(day_key, day_key * 86_400 + off));
            let mark = done.key.to_string();
            if !self.cfg.slots.is_empty() && self.kv.get("daily_sent") != Some(&mark) {
                out.push(self.daily(&done));
                self.kv.insert("daily_sent".into(), mark);
            }
            self.dirty = true;
        }
        self.kv.insert("hour".into(), self.hour.encode());
        self.kv.insert("day".into(), self.day.encode());
    }

    fn period_core(&self, p: &Period) -> String {
        format!(
            "sent {} · missed {} · fills {} ({}) · settled {} W / {} L · gross {}",
            p.sent,
            p.missed,
            p.fills,
            fmt::usd_1e12(p.fill_notional),
            p.settled_w,
            p.settled_l,
            fmt::usd_signed_1e12(p.gross)
        )
    }

    fn hourly(&self, p: &Period) -> Msg {
        let modes: Vec<String> = self
            .cfg
            .slots
            .iter()
            .map(|c| self.mode_word(c.slot))
            .collect();
        let net = self.last_state.as_ref().map_or(String::new(), |s| {
            format!(" · net {}", fmt::usd_signed_1e6(s.exec.session_pnl_1e6))
        });
        let from = if p.start_s > p.key * 3600 {
            format!(" (from {})", clock::hhmm(p.start_s))
        } else {
            String::new()
        };
        let body = format!(
            "{} · {}{net} · 1h{from}: {} · holds {}",
            clock::hhmm((p.key + 1) * 3600),
            modes.join(" · "),
            self.period_core(p),
            p.holds
        );
        self.msg(false, &body)
    }

    fn daily(&self, p: &Period) -> Msg {
        let modes: Vec<String> = self
            .cfg
            .slots
            .iter()
            .map(|c| self.mode_word(c.slot))
            .collect();
        let off = i64::from(self.cfg.daily_utc_hour) * 3600;
        let from = if p.start_s > p.key * 86_400 + off {
            format!(" (from {})", clock::hhmm(p.start_s))
        } else {
            String::new()
        };
        let mut body = format!(
            "DAY {}{from} · {} · {} · rejected {} · refused {} · holds {} (max {}) · restarts {} · dark {}",
            clock::date(p.key * 86_400 + off),
            modes.join(" · "),
            self.period_core(p),
            p.rejected,
            p.refused,
            p.holds,
            fmt::dur(p.hold_max_s),
            p.restarts,
            fmt::dur(p.dark_s)
        );
        let mut tail = Vec::new();
        if let Some(s) = &self.last_state {
            if self.any_live() {
                let mut l = format!(
                    "session net {}",
                    fmt::usd_signed_1e6(s.exec.session_pnl_1e6)
                );
                if let Some(fee) = s.exec.gw_fees_1e8 {
                    l.push_str(&format!(
                        " · fees {}",
                        fmt::usd_1e12(i128::from(fee) * 10_000)
                    ));
                }
                l.push_str(&format!(
                    " · worth {} / anchor {}",
                    fmt::plain_1e6(s.exec.anchor_1e6 + s.exec.session_pnl_1e6),
                    fmt::plain_1e6(s.exec.anchor_1e6)
                ));
                tail.push(l);
            }
        }
        if let Some(until) = self.cfg.agent_valid_until_s {
            tail.push(format!(
                "agent valid {}",
                fmt::dur((until - (p.key + 1) * 86_400 - off).max(0))
            ));
        }
        tail.extend(self.watch_summaries());
        if p.inbox > 0 {
            tail.push(format!("AI messages {}", p.inbox));
        }
        if !tail.is_empty() {
            body.push('\n');
            body.push_str(&tail.join(" · "));
        }
        self.msg(false, &body)
    }

    /// Count a relayed inbox message.
    pub fn count_inbox(&mut self, n: u64) {
        self.hour.inbox += n;
        self.day.inbox += n;
    }

    /// The tag (for the inbox relay).
    #[must_use]
    pub fn tag_str(&self) -> &str {
        self.tag()
    }
}

/// A watch's running tally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tally {
    /// Units settled.
    pub n: u64,
    /// Units won.
    pub won: u64,
    /// Losses within the first `first_n`.
    pub losses_in_first: u32,
    /// Realised gross ×1e12.
    pub net: i128,
}

impl WatchCfg {
    fn unit_word(&self) -> &'static str {
        match self.unit {
            WatchUnit::Fill => "fills",
            WatchUnit::Instance => "instances",
        }
    }
}

impl Tally {
    fn summary(&self, w: &WatchCfg) -> String {
        let mut parts = vec![match w.hit_after {
            Some(a) => format!("{}: {}/{a} {}", w.name, self.n, w.unit_word()),
            None => format!("{}: {} {}", w.name, self.n, w.unit_word()),
        }];
        if let Some(f) = w.first_n {
            parts.push(format!("losses {} in first {f}", self.losses_in_first));
        }
        parts.push(format!("hit {}", fmt::pct(self.won, self.n)));
        parts.push(format!("net {}", fmt::usd_signed_1e12(self.net)));
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ExecView;

    const T0: i64 = 1_792_800_000; // 2026-10-24-ish, s
    const HL_BTC_YES: u32 = (4 << 24) | 4096;

    fn cfg(extra: &str) -> Config {
        Config::parse(&format!(
            "tag = \"SA\"\nstate_url = \"http://127.0.0.1:9192/state\"\nstate_dir = \"/tmp/x\"\ndry_run = true\n{extra}\n[[slot]]\nslot = 3\n"
        ))
        .unwrap()
    }

    fn names() -> Names {
        Names::from_universe("[hyperliquid]\nrolling = [\"out:BTC:15m\"]\n")
    }

    fn model(extra: &str) -> Model {
        Model::new(
            cfg(extra),
            names(),
            BTreeMap::new(),
            Vec::new(),
            BTreeMap::new(),
            T0,
        )
    }

    fn live_state(boot: u64) -> StateView {
        let mut halted = vec!["none".to_string(); 16];
        halted[0] = "none".into();
        StateView {
            mono_ns: 1_000_000_000,
            wall_ns: T0 as u64 * 1_000_000_000,
            pid: 1,
            git_sha: "abc".into(),
            boot_wall_ns: boot,
            run_dir: "/r/run-1".into(),
            paper: false,
            configured_mask: 8,
            exec: ExecView {
                configured: true,
                seeded: true,
                halted,
                held: vec!["none".to_string(); 16],
                arm_slots: vec![3],
                anchor_1e6: 1_000_000_000,
                refused: vec![("gw.refused_local".into(), 0)],
                budget_remaining: 9000,
                ..ExecView::default()
            },
        }
    }

    fn fill(ts_s: i64, side: u8, px: i64, qty: i64, flags: u8, oid: u64) -> FillRec {
        FillRec {
            ts_ns: ts_s as u64 * 1_000_000_000,
            sym: HL_BTC_YES,
            side,
            strategy_id: 3,
            origin: 0,
            flags,
            px,
            qty,
            order_id: oid,
        }
    }

    fn texts(v: &[Msg]) -> Vec<String> {
        v.iter()
            .map(|m| format!("{}{}", if m.loud { "L " } else { "s " }, m.text))
            .collect()
    }

    #[test]
    fn safety_transitions_ring_once_each() {
        let mut m = model(
            "hold_loud_s = 30\ndark_after_s = 60\nsession_warn_usd = [-250]\nbudget_warn = 1000",
        );
        let mut out = Vec::new();
        let mut s = live_state(1);
        m.on_state(T0, Some(&s), &mut out);
        assert!(
            texts(&out)[0].starts_with("s [SA] notify started · engine abc pid 1 · s3 LIVE"),
            "{out:?}"
        );
        out.clear();
        // Halt, then cleared.
        s.exec.halted[3] = "reject-streak".into();
        m.on_state(T0 + 1, Some(&s), &mut out);
        s.exec.halted[3] = "none".into();
        m.on_state(T0 + 2, Some(&s), &mut out);
        assert_eq!(
            texts(&out),
            [
                "L [SA] HALT s3 reason=reject-streak",
                "L [SA] HALT CLEARED s3 (was reject-streak)"
            ]
        );
        out.clear();
        // A short hold is only counted; a long one rings and resumes.
        s.exec.held[3] = "ws-gap".into();
        m.on_state(T0 + 3, Some(&s), &mut out);
        s.exec.held[3] = "none".into();
        m.on_state(T0 + 10, Some(&s), &mut out);
        assert!(out.is_empty());
        s.exec.held[3] = "ws-gap".into();
        m.on_state(T0 + 20, Some(&s), &mut out);
        m.on_state(T0 + 51, Some(&s), &mut out);
        m.on_state(T0 + 52, Some(&s), &mut out);
        s.exec.held[3] = "none".into();
        m.on_state(T0 + 61, Some(&s), &mut out);
        let t = texts(&out);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("L [SA] HOLD s3 ws-gap ≥ 31 s"));
        assert_eq!(t[1], "L [SA] RESUMED s3 after 41 s");
        assert_eq!(m.day.holds, 2);
        assert_eq!(m.day.hold_max_s, 41);
        out.clear();
        // Dark and back.
        m.on_state(T0 + 100, None, &mut out);
        m.on_state(T0 + 159, None, &mut out);
        assert!(out.is_empty());
        m.on_state(T0 + 160, None, &mut out);
        m.on_state(T0 + 200, Some(&s), &mut out);
        assert_eq!(
            texts(&out),
            [
                "L [SA] ENGINE DARK: /state silent since 00:01:40Z (1 min 0 s)",
                "L [SA] ENGINE BACK after 1 min 40 s"
            ]
        );
        out.clear();
        // Refusals burst into one message after 20 s of quiet.
        s.exec.refused[0].1 = 60;
        m.on_state(T0 + 201, Some(&s), &mut out);
        s.exec.refused[0].1 = 90;
        m.on_state(T0 + 210, Some(&s), &mut out);
        m.tick(T0 + 229, &mut out);
        assert!(out.is_empty());
        m.tick(T0 + 230, &mut out);
        assert_eq!(
            texts(&out),
            ["L [SA] REFUSED s3 · +90 in 9 s (gw.refused_local 90)"]
        );
        out.clear();
        // Budget and session levels ring once.
        s.exec.budget_remaining = 999;
        s.exec.session_pnl_1e6 = -251_000_000;
        m.on_state(T0 + 240, Some(&s), &mut out);
        m.on_state(T0 + 241, Some(&s), &mut out);
        let t = texts(&out);
        assert_eq!(t.len(), 2, "{t:?}");
        assert_eq!(t[0], "L [SA] BUDGET request budget 999 < 1000");
        assert!(t[1].starts_with("L [SA] SESSION -$251.00 crossed -$250.00"));
        out.clear();
        // A new boot: one BOOT line, no deltas across it.
        let mut s2 = live_state(2);
        s2.exec.refused[0].1 = 0;
        s2.exec.seeded = false;
        m.on_state(T0 + 300, Some(&s2), &mut out);
        s2.exec.seeded = true;
        m.on_state(T0 + 348, Some(&s2), &mut out);
        assert_eq!(
            texts(&out),
            [
                "L [SA] BOOT abc · pid 1 · s3 LIVE · run-1 · seeded 0",
                "s [SA] SEEDED (+48 s after boot)"
            ]
        );
        assert_eq!(m.day.restarts, 1);
    }

    #[test]
    fn thresholds_left_out_stay_silent() {
        let mut m = model("");
        let mut out = Vec::new();
        let mut s = live_state(1);
        m.on_state(T0, Some(&s), &mut out);
        out.clear();
        s.exec.held[3] = "ws-gap".into();
        m.on_state(T0 + 1, Some(&s), &mut out);
        m.on_state(T0 + 500, Some(&s), &mut out);
        s.exec.session_pnl_1e6 = -499_000_000;
        s.exec.budget_remaining = 1;
        m.on_state(T0 + 501, None, &mut out);
        m.on_state(T0 + 5000, None, &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn a_sweep_settles_once_and_a_loss_is_counted() {
        let mut m = model("");
        let mut out = Vec::new();
        let exp = (T0 / 900 + 1) * 900;
        m.on_fill(
            &fill(exp - 20, 0, 940_000, 20_000_000, 0, 1),
            T0,
            false,
            &mut out,
        );
        m.on_fill(
            &fill(exp - 20, 0, 950_000, 10_000_000, 0, 1),
            T0,
            false,
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert_eq!(m.positions.len(), 1);
        out.clear();
        // Partial settlement waits; the rest completes it.
        m.on_fill(
            &fill(
                exp + 3,
                1,
                0,
                10_000_000,
                crate::capture::FILL_FLAG_SETTLEMENT,
                9,
            ),
            T0,
            false,
            &mut out,
        );
        assert!(out.is_empty());
        m.on_fill(
            &fill(
                exp + 3,
                1,
                0,
                20_000_000,
                crate::capture::FILL_FLAG_SETTLEMENT,
                9,
            ),
            T0,
            false,
            &mut out,
        );
        let t = texts(&out);
        assert_eq!(t.len(), 1);
        assert!(
            t[0].starts_with("L [SA] SETTLED LOST · BTC 15m Yes exp "),
            "{t:?}"
        );
        assert!(
            t[0].contains("30 @ 0.9433 → $0.00 · gross -$28.30"),
            "{t:?}"
        );
        assert!(m.positions.is_empty());
        assert_eq!((m.day.settled_w, m.day.settled_l), (0, 1));
        assert_eq!(m.newly_settled.len(), 1);
        assert_eq!(m.newly_settled[0].fills, 2);
        // A settlement with no entry on record still reports.
        out.clear();
        m.on_fill(
            &fill(
                exp + 5,
                1,
                1_000_000,
                5_000_000,
                crate::capture::FILL_FLAG_SETTLEMENT,
                9,
            ),
            T0,
            false,
            &mut out,
        );
        assert!(texts(&out)[0].contains("(no recorded entry)"));
    }

    #[test]
    fn paper_fills_are_ignored_on_a_live_only_slot_and_quiet_folds_silently() {
        let mut m = model("");
        let mut out = Vec::new();
        let mut f = fill(T0, 0, 900_000, 1_000_000, 0, 1);
        f.origin = crate::capture::FILL_ORIGIN_PAPER;
        m.on_fill(&f, T0, false, &mut out);
        assert!(out.is_empty() && m.positions.is_empty());
        let mut f = fill(T0, 0, 900_000, 1_000_000, 0, 1);
        f.strategy_id = 6; // not configured
        m.on_fill(&f, T0, false, &mut out);
        assert!(m.positions.is_empty());
        m.on_fill(&fill(T0, 0, 900_000, 1_000_000, 0, 1), T0, true, &mut out);
        assert!(out.is_empty());
        assert_eq!(m.positions.len(), 1);
        assert_eq!(m.day.fills, 0);
    }

    #[test]
    fn attempts_coalesce_per_instance() {
        let mut m = model("");
        let mut out = Vec::new();
        m.on_state(T0, Some(&live_state(1)), &mut out);
        out.clear();
        let exp = (T0 / 900 + 1) * 900;
        let mono_at = |wall_s: i64| (wall_s - T0) as u64 * 1_000_000_000 + 1_000_000_000;
        for (k, px) in [(1u64, 946_000), (2, 948_000), (3, 949_000)] {
            m.on_order(
                &OrderRec {
                    ts_ns: mono_at(exp - 21),
                    sym: HL_BTC_YES,
                    side: 0,
                    px,
                    qty: 20_000_000,
                    client_oid: k,
                    strategy_id: 3,
                    verb: 0,
                },
                T0,
            );
        }
        m.on_event(
            &EventRec {
                ts_ns: 0,
                client_oid: 1,
                sym: HL_BTC_YES,
                strategy_id: 3,
                kind: OEV_REJECTED,
                reason: REASON_IOC_MISS,
            },
            T0,
        );
        m.on_event(
            &EventRec {
                ts_ns: 0,
                client_oid: 2,
                sym: HL_BTC_YES,
                strategy_id: 3,
                kind: OEV_REJECTED,
                reason: 14,
            },
            T0,
        );
        m.on_fill(
            &fill(exp - 20, 0, 949_000, 18_000_000, 0, 3),
            T0,
            false,
            &mut out,
        );
        assert!(texts(&out)[0].ends_with("· lim 0.949"), "{out:?}");
        out.clear();
        m.tick(T0 + 14, &mut out);
        assert!(out.is_empty());
        m.tick(T0 + 15, &mut out);
        let t = texts(&out);
        assert_eq!(t.len(), 1, "{t:?}");
        assert!(
            t[0].starts_with("s [SA] attempts s3 · BTC 15m Yes exp ")
                && t[0].contains("· sent 3 · filled 1 (18) · missed 1 · rejected 1 (rate-limit 1) · lim 0.946–0.949 · first τ 21.0s"),
            "{t:?}"
        );
        assert_eq!((m.day.sent, m.day.missed, m.day.rejected), (3, 1, 1));
    }

    #[test]
    fn a_watch_rings_once_per_criterion_and_digests_carry_it() {
        let mut m = model("[[watch]]\nname = \"P2\"\nslot = 3\nstart = \"2026-10-01T00:00:00Z\"\nfirst_n = 20\nmax_losses_in_first = 2\nhit_after = 3\nmin_hit_pct = 93\nloss_net_usd = -10");
        let mut out = Vec::new();
        let mut exp = (T0 / 900 + 1) * 900;
        for won in [false, false, true] {
            m.on_fill(
                &fill(exp - 20, 0, 900_000, 10_000_000, 0, 1),
                T0,
                false,
                &mut out,
            );
            m.on_fill(
                &fill(
                    exp + 2,
                    1,
                    if won { 1_000_000 } else { 0 },
                    10_000_000,
                    crate::capture::FILL_FLAG_SETTLEMENT,
                    9,
                ),
                T0,
                false,
                &mut out,
            );
            exp += 900;
        }
        let watch: Vec<String> = texts(&out)
            .into_iter()
            .filter(|t| t.contains("WATCH"))
            .collect();
        assert_eq!(watch.len(), 3, "{watch:?}");
        assert!(watch
            .iter()
            .any(|w| w.contains("2 losses within the first 20 fills")));
        assert!(watch
            .iter()
            .any(|w| w.contains("hit rate 33.3 % < 93 % after 3 fills")));
        assert!(watch
            .iter()
            .any(|w| w.contains("realised net -$18.00 ≤ -$10.00")));
        // The settlement precedes its alerts.
        let first_watch = out.iter().position(|m| m.text.contains("WATCH")).unwrap();
        assert!(out[first_watch - 1].text.contains("SETTLED"));
        assert_eq!(
            m.watch_summaries(),
            ["P2: 3/3 fills · losses 2 in first 20 · hit 33.3 % · net -$17.00"]
        );
        // The daily digest at the day roll carries the watch and the counts.
        out.clear();
        m.on_state(T0, Some(&live_state(1)), &mut out);
        out.clear();
        let next_day = (T0 / 86_400 + 1) * 86_400;
        m.tick(next_day, &mut out);
        let day = texts(&out)
            .into_iter()
            .find(|t| t.contains("DAY "))
            .expect("a daily digest");
        assert!(
            day.contains("fills 3 ($27.00) · settled 1 W / 2 L · gross -$17.00"),
            "{day}"
        );
        assert!(day.contains("P2: 3/3 fills"), "{day}");
        // …and is sent once even if the clock is re-ticked.
        out.clear();
        m.tick(next_day + 1, &mut out);
        assert!(texts(&out).iter().all(|t| !t.contains("DAY ")));
    }

    #[test]
    fn hourly_only_while_live() {
        let mut m = model("");
        let mut out = Vec::new();
        let h1 = (T0 / 3600 + 1) * 3600;
        m.tick(h1, &mut out);
        assert!(
            out.iter().all(|m| !m.text.contains("· 1h")),
            "not live: no hourly"
        );
        m.on_state(h1 + 1, Some(&live_state(1)), &mut out);
        out.clear();
        m.tick(h1 + 3600, &mut out);
        let t = texts(&out);
        assert_eq!(t.len(), 1, "{t:?}");
        assert!(
            t[0].starts_with("s [SA] ") && t[0].contains(" · s3 LIVE · net $0.00 · 1h: sent 0"),
            "{t:?}"
        );
    }

    #[test]
    fn agent_expiry_rings_at_each_configured_day() {
        let mut m =
            model("agent_valid_until = \"2026-11-01T00:00:00Z\"\nagent_warn_days = [14, 7]");
        let mut out = Vec::new();
        let until = clock::parse_rfc3339_z("2026-11-01T00:00:00Z").unwrap();
        m.tick(until - 20 * 86_400, &mut out);
        assert!(out.iter().all(|m| !m.text.contains("AGENT")));
        m.tick(until - 13 * 86_400, &mut out);
        m.tick(until - 13 * 86_400 + 60, &mut out);
        assert_eq!(out.iter().filter(|m| m.text.contains("AGENT")).count(), 1);
        m.tick(until - 6 * 86_400, &mut out);
        assert_eq!(out.iter().filter(|m| m.text.contains("AGENT")).count(), 2);
    }

    #[test]
    fn periods_round_trip_through_kv() {
        let p = Period {
            key: 5,
            start_s: 6,
            fills: 1,
            fill_notional: -7,
            settled_w: 2,
            gross: 9,
            hold_max_s: 3,
            ..Period::default()
        };
        assert_eq!(Period::decode(&p.encode()), Some(p));
        assert_eq!(Period::decode("1 2 3"), None);
    }
}
