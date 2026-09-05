// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Hand-written JSON writer for `GET /state` (plan §6.1). No serde, no
//! allocation, no `format!` — a cursor over the caller's buffer with a
//! sticky overflow flag: the body is either complete or refused
//! ([`JsonOverflow`]), never truncated.
//!
//! ## Number law
//!
//! JavaScript reads JSON numbers as f64 — exact only below 2^53. So:
//! counters, masks, small ids and ×1e6 prices/quantities are emitted
//! as NUMBERS; nanosecond timestamps, 64-bit hashes/words/ids
//! (`name_h`, `client_oid`, `order_id`, regime words) are emitted as
//! STRINGS (decimal for stamps and ids, lower-hex for hashes/words).
//! Ages (`*_age_s`) are derived here against the snapshot's own
//! `mono_ns` and emitted as numbers; `-1` = never.
//!
//! The schema is documented field-by-field in
//! `docs/regime-and-dashboard-plan.md` §6.1 and pinned by the
//! byte-exact test in `tests/encode.rs`.

use core_types::{Fill, Order, RegimeWord, REGIME_PROFILES};
use strategy_core::{RegimeCounters, RegimeRelView, VmRowView};

use crate::snapshot::{
    EngineSnapshot, IngressSnapshot, RecentRing, SLOT_NAMES, SNAPSHOT_SLOTS, SNAPSHOT_VENUES,
    VENUE_NAMES,
};

/// The destination buffer was too small for the body. The caller's
/// buffer is the fixed 256 KiB response scratch — this is a sizing
/// bug, surfaced as HTTP 500, never as a truncated body.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct JsonOverflow;

/// Upper bound of a full-snapshot body (256 rows, 64 + 64 recents,
/// every text field at capacity) — the server's response buffer must
/// hold this plus its header budget; `tests/encode.rs` pins it.
pub const STATE_JSON_MAX: usize = 160 * 1024;

/// Profile names in `RegimeCounters` index order (only
/// [`REGIME_PROFILES`] are emitted).
const PROFILE_NAMES: [&str; 2] = ["fast", "slow"];

/// Encode `s` as the `/state` JSON body into `dst`; `Ok(len)` or
/// [`JsonOverflow`]. Zero-alloc; ≈ 100 KB for a full snapshot.
pub fn encode_state_json(s: &EngineSnapshot, dst: &mut [u8]) -> Result<usize, JsonOverflow> {
    let mut c = Cursor::new(dst);
    c.put(b"{\"v\":");
    c.u64(u64::from(s.schema));
    c.key("seq");
    c.u64(s.seq);

    // --- now ---
    c.key("now");
    c.put(b"{\"mono_ns\":");
    c.u64_str(s.mono_ns);
    c.key("wall_ns");
    c.u64_str(s.wall_ns);
    c.key("uptime_s");
    c.u64(s.mono_ns.saturating_sub(s.boot.boot_mono_ns) / 1_000_000_000);
    c.put(b"}");

    // --- boot ---
    let b = &s.boot;
    c.key("boot");
    c.put(b"{\"pid\":");
    c.u64(u64::from(b.pid));
    c.key("git_sha");
    c.text(b.git_sha());
    c.key("binary_mtime_ns");
    c.u64_str(b.binary_mtime_ns);
    c.key("boot_wall_ns");
    c.u64_str(b.boot_wall_ns);
    c.key("run_epoch_ns");
    c.u64_str(b.run_epoch_ns);
    c.key("run_dir");
    c.text(b.run_dir());
    c.key("strategy");
    c.text(b.strategy_name());
    c.key("strategy_kind");
    c.text(s.strategy_kind());
    c.key("paper");
    c.u64(u64::from(b.paper));
    c.key("requested_mask");
    c.u64(u64::from(b.requested_mask));
    c.key("configured_mask");
    c.u64(u64::from(b.configured_mask));
    c.key("enabled_mask");
    c.u64(u64::from(s.enabled_mask));
    c.key("halted");
    c.u64(u64::from(s.halted));
    c.key("ruleset_hash");
    c.hex(&s.vm.active_hash);
    c.key("ruleset_staged_hash");
    c.hex(&s.vm.staged_hash);
    c.key("icdp_hash");
    c.hex(&s.icdp.hash);
    c.key("regime_hash");
    c.hex(&b.regime_hash);
    c.key("regime_configured");
    c.u64(u64::from(b.regime_configured));
    c.put(b"}");

    // --- counters ---
    let k = &s.counters;
    c.key("counters");
    c.put(b"{\"iterations\":");
    c.u64(k.iterations);
    c.key("ticks");
    c.u64(k.ticks);
    c.key("signals");
    c.u64(k.signals);
    c.key("fills");
    c.u64(k.fills);
    c.key("events");
    c.u64(k.events);
    c.key("depths");
    c.u64(k.depths);
    c.key("opts");
    c.u64(k.opts);
    c.key("orders_emitted");
    c.u64(k.orders_emitted);
    c.key("orders_dropped");
    c.u64(k.orders_dropped);
    c.key("ai_dispatched");
    c.u64(k.ai_dispatched);
    c.key("ai_drain_malformed");
    c.u64(k.ai_drain_malformed);
    c.put(b"}");

    // --- latency ---
    c.key("latency");
    c.put(b"{");
    let stages: [&[u8]; 3] = [b"ingest", b"decide", b"ack"];
    let mut i = 0usize;
    while i < 3 {
        if i > 0 {
            c.put(b",");
        }
        c.quoted(stages[i]);
        c.put(b":{\"p50_ns\":");
        c.u64(s.latency.p50_ns[i]);
        c.key("p99_ns");
        c.u64(s.latency.p99_ns[i]);
        c.put(b"}");
        i += 1;
    }
    c.put(b"}");

    // --- regime ---
    c.key("regime");
    regime(&mut c, &s.regime, &s.regime_rel, s.mono_ns);

    // --- slots ---
    c.key("slots");
    c.put(b"[");
    let mut slot = 0usize;
    while slot < SNAPSHOT_SLOTS {
        if slot > 0 {
            c.put(b",");
        }
        let sc = &s.slots[slot];
        c.put(b"{\"slot\":");
        c.u64(slot as u64);
        c.key("name");
        c.text(SLOT_NAMES[slot].as_bytes());
        c.key("configured");
        c.u64(u64::from((s.boot.configured_mask >> slot) & 1));
        c.key("enabled");
        c.u64(u64::from((s.enabled_mask >> slot) & 1));
        c.key("gate");
        c.u64(u64::from(s.regime.gates[slot]));
        c.key("label_terms");
        c.u64(u64::from(sc.label_terms));
        c.key("label_off");
        c.u64(u64::from(sc.label_off));
        c.key("orders_emitted");
        c.u64(sc.orders_emitted);
        c.key("orders_dropped");
        c.u64(sc.orders_dropped);
        c.put(b"}");
        slot += 1;
    }
    c.put(b"]");

    // --- vm ---
    let v = &s.vm;
    c.key("vm");
    c.put(b"{\"active_hash\":");
    c.hex(&v.active_hash);
    c.key("staged_hash");
    c.hex(&v.staged_hash);
    c.key("rows_active");
    c.u64(u64::from(v.rows_active));
    c.key("epoch");
    c.u64(u64::from(v.epoch));
    c.key("fires");
    c.u64(v.fires);
    c.key("orders_emitted");
    c.u64(v.orders_emitted);
    c.key("orders_dropped");
    c.u64(v.orders_dropped);
    c.key("commit_dropped");
    c.u64(v.commit_dropped);
    c.key("regime_blocked");
    c.u64(v.regime_blocked);
    c.key("regime_hard_exits");
    c.u64(v.regime_hard_exits);
    c.key("rows");
    c.put(b"[");
    let n = (v.rows_active as usize).min(v.rows.len());
    let mut i = 0usize;
    while i < n {
        if i > 0 {
            c.put(b",");
        }
        vm_row(&mut c, i, &v.rows[i], s.mono_ns);
        i += 1;
    }
    c.put(b"]}");

    // --- icdp ---
    let ic = &s.icdp;
    c.key("icdp");
    c.put(b"{\"configured\":");
    c.u64(u64::from(ic.instruments > 0));
    c.key("hash");
    c.hex(&ic.hash);
    c.key("instruments");
    c.u64(u64::from(ic.instruments));
    let cnt = &ic.counters;
    c.key("decisions");
    c.u64(cnt.decisions);
    c.key("signals");
    c.u64(cnt.signals);
    c.key("intents");
    c.u64(cnt.intents);
    c.key("exits");
    c.u64(cnt.exits);
    c.key("exit_on_stale");
    c.u64(cnt.exit_on_stale);
    c.key("skipped_spread");
    c.u64(cnt.skipped_spread);
    c.key("skipped_stale_open");
    c.u64(cnt.skipped_stale_open);
    c.key("skipped_stale_dec");
    c.u64(cnt.skipped_stale_dec);
    c.key("skipped_prev");
    c.u64(cnt.skipped_prev);
    c.key("late_bars");
    c.u64(cnt.late_bars);
    c.key("caps_rejected");
    c.u64(cnt.caps_rejected);
    c.key("rolls");
    c.u64(cnt.rolls);
    c.key("regime_blocked");
    c.u64(cnt.regime_blocked);
    c.key("regime_exits");
    c.u64(cnt.regime_exits);
    c.put(b"}");

    // --- ai ---
    let a = &s.ai;
    c.key("ai");
    c.put(b"{\"cmds\":");
    c.u64(a.cmds);
    c.key("hmac_fail");
    c.u64(a.hmac_fail);
    c.key("protocol_err");
    c.u64(a.protocol_err);
    c.key("malformed");
    c.u64(a.malformed);
    c.key("seq_gap");
    c.u64(a.seq_gap);
    c.key("seq_regress");
    c.u64(a.seq_regress);
    c.key("ring_drops");
    c.u64(a.ring_drops);
    c.key("expired");
    c.u64(a.expired);
    c.key("rejected_conns");
    c.u64(a.rejected_conns);
    c.key("drain_malformed");
    c.u64(a.drain_malformed);
    c.key("enable_refused");
    c.u64(a.enable_refused);
    c.key("ruleset_staged");
    c.u64(a.ruleset_staged);
    c.key("ruleset_committed");
    c.u64(a.ruleset_committed);
    c.key("ruleset_rejected");
    c.u64(a.ruleset_rejected);
    c.key("table_push_fail");
    c.u64(a.table_push_fail);
    c.key("heartbeat_age_s");
    c.age_s(s.mono_ns, a.last_heartbeat_ns);
    c.put(b"}");

    // --- ingress ---
    c.key("ingress");
    c.put(b"[");
    let mut i = 0usize;
    while i < SNAPSHOT_VENUES {
        if i > 0 {
            c.put(b",");
        }
        ingress(&mut c, VENUE_NAMES[i].as_bytes(), &s.ingress[i], s.mono_ns);
        i += 1;
    }
    c.put(b"]");

    // --- capture ---
    let cp = &s.capture;
    c.key("capture");
    c.put(b"{\"fills_records\":");
    c.u64(cp.fills_records);
    c.key("fills_io_errors");
    c.u64(cp.fills_io_errors);
    c.key("orders_records");
    c.u64(cp.orders_records);
    c.key("orders_io_errors");
    c.u64(cp.orders_io_errors);
    c.put(b"}");

    // --- recent ---
    c.key("recent");
    c.put(b"{\"orders_total\":");
    c.u64(s.recent_orders.total);
    c.key("orders");
    recent_orders(&mut c, &s.recent_orders, s.mono_ns);
    c.key("fills_total");
    c.u64(s.recent_fills.total);
    c.key("fills");
    recent_fills(&mut c, &s.recent_fills, s.mono_ns);
    c.put(b"}}");

    c.finish()
}

fn regime(c: &mut Cursor<'_>, r: &RegimeCounters, rel: &RegimeRelView, mono_ns: u64) {
    c.put(b"{\"configured\":");
    c.u64(u64::from(r.configured));
    c.key("minutes_judged");
    c.u64(r.minutes_judged);
    c.key("seed_rows");
    c.u64(r.seed_rows);
    c.key("declared_total");
    c.u64(r.declared_total);
    c.key("gate_changes");
    c.u64(r.gate_changes);
    c.key("gates");
    c.put(b"[");
    let mut slot = 0usize;
    while slot < SNAPSHOT_SLOTS {
        if slot > 0 {
            c.put(b",");
        }
        c.u64(u64::from(r.gates[slot]));
        slot += 1;
    }
    c.put(b"]");
    c.key("profiles");
    c.put(b"[");
    let mut p = 0usize;
    while p < REGIME_PROFILES {
        if p > 0 {
            c.put(b",");
        }
        c.put(b"{\"name\":");
        c.text(PROFILE_NAMES[p].as_bytes());
        c.key("measured");
        word(c, r.measured[p]);
        c.key("declared");
        word(c, r.declared[p]);
        c.key("effective");
        word(c, r.effective[p]);
        c.key("declared_age_s");
        c.age_s(mono_ns, r.declared_ts_ns[p]);
        c.key("declared_ttl_s");
        c.u64(r.declared_ttl_ns[p] / 1_000_000_000);
        c.key("disagree");
        c.u64(r.disagree[p]);
        c.key("flips");
        c.put(b"[");
        let mut d = 0usize;
        while d < 8 {
            if d > 0 {
                c.put(b",");
            }
            c.u64(r.flips[p][d]);
            d += 1;
        }
        c.put(b"]");
        c.key("raw");
        c.put(b"{\"present\":");
        c.u64(u64::from(r.raw_present[p]));
        c.key("ret_bps_1e9");
        c.i64(r.raw[p][0]);
        c.key("er_1e9");
        c.i64(r.raw[p][1]);
        c.key("rv_bps_1e9");
        c.i64(r.raw[p][2]);
        c.key("stretch_1e9");
        c.i64(r.raw[p][3]);
        c.put(b"}}");
        p += 1;
    }
    c.put(b"]");
    c.key("rel");
    c.put(b"{\"syms\":[");
    let n = (rel.n as usize).min(rel.syms.len());
    let mut i = 0usize;
    while i < n {
        if i > 0 {
            c.put(b",");
        }
        c.u64(u64::from(rel.syms[i]));
        i += 1;
    }
    c.put(b"]");
    let mut p = 0usize;
    while p < REGIME_PROFILES {
        c.put(b",");
        c.quoted(PROFILE_NAMES[p].as_bytes());
        c.put(b":[");
        let mut i = 0usize;
        while i < n {
            if i > 0 {
                c.put(b",");
            }
            c.u64(u64::from(rel.rel[p][i]));
            i += 1;
        }
        c.put(b"]");
        p += 1;
    }
    c.put(b"}}");
}

/// A regime word: its raw hex plus the seven dimension bytes (the
/// page decodes names with the `core_types::regime` byte map).
fn word(c: &mut Cursor<'_>, w: RegimeWord) {
    c.put(b"{\"hex\":");
    c.hex(&w.0.to_be_bytes());
    c.key("dims");
    c.put(b"[");
    let mut d = 0u8;
    while d < 7 {
        if d > 0 {
            c.put(b",");
        }
        c.u64(u64::from(w.dim(d)));
        d += 1;
    }
    c.put(b"]}");
}

fn vm_row(c: &mut Cursor<'_>, idx: usize, r: &VmRowView, mono_ns: u64) {
    c.put(b"{\"i\":");
    c.u64(idx as u64);
    c.key("name_h");
    c.hex(&r.name_h.to_be_bytes());
    c.key("sym");
    c.u64(u64::from(r.sym));
    c.key("ref_sym");
    c.u64(u64::from(r.ref_sym));
    c.key("flags");
    c.u64(u64::from(r.flags));
    c.key("family");
    c.u64(u64::from(r.family));
    c.key("gate");
    c.u64(u64::from(r.gate));
    c.key("regime_off");
    c.u64(u64::from(r.regime_off));
    c.key("state");
    c.u64(u64::from(r.state));
    c.key("side");
    c.u64(u64::from(r.side));
    c.key("entry_sign");
    c.i64(i64::from(r.entry_sign));
    c.key("entry_px_1e6");
    c.i64(r.entry_px_1e6);
    c.key("qty_sym_1e6");
    c.i64(r.qty_sym_1e6);
    c.key("entry_ts_ns");
    c.u64_str(r.entry_ts_ns);
    c.key("age_s");
    c.age_s(mono_ns, r.entry_ts_ns);
    c.put(b"}");
}

fn ingress(c: &mut Cursor<'_>, name: &[u8], g: &IngressSnapshot, mono_ns: u64) {
    c.put(b"{\"venue\":");
    c.text(name);
    c.key("state");
    c.u64(u64::from(g.state));
    c.key("last_tick_age_s");
    c.age_s(mono_ns, g.last_tick_ns);
    c.key("ticks");
    c.u64(g.ticks);
    c.key("msgs");
    c.u64(g.msgs);
    c.key("reconnects");
    c.u64(g.reconnects);
    c.key("ring_drops");
    c.u64(g.ring_drops);
    c.key("stale_ticks");
    c.u64(g.stale_ticks);
    c.key("parse_errors");
    c.u64(g.parse_errors);
    c.key("gaps");
    c.u64(g.gaps);
    c.key("sub_drops");
    c.u64(g.sub_drops);
    c.key("feed_delay_ema_ms");
    c.u64(u64::from(g.feed_delay_ema_ms));
    c.put(b"}");
}

fn recent_orders<const N: usize>(c: &mut Cursor<'_>, r: &RecentRing<Order, N>, mono_ns: u64) {
    c.put(b"[");
    let n = r.len();
    let mut k = 0usize;
    while k < n {
        if k > 0 {
            c.put(b",");
        }
        // `oldest_first(k)` is `Some` for every `k < len()`.
        if let Some(o) = r.oldest_first(k) {
            c.put(b"{\"ts_ns\":");
            c.u64_str(o.ts_ns);
            c.key("age_s");
            c.age_s(mono_ns, o.ts_ns);
            c.key("slot");
            c.u64(u64::from(o.strategy_id));
            c.key("venue");
            c.u64(u64::from(o.venue));
            c.key("sym");
            c.u64(u64::from(o.sym));
            c.key("side");
            c.u64(o.side as u64);
            c.key("kind");
            c.u64(u64::from(o.kind));
            c.key("px_1e6");
            c.i64(o.px.raw());
            c.key("qty_1e6");
            c.i64(o.qty.raw());
            c.key("oid");
            c.u64_str(o.client_oid);
            c.key("ttl_ns");
            c.u64_str(o.ttl_ns);
            c.put(b"}");
        }
        k += 1;
    }
    c.put(b"]");
}

fn recent_fills<const N: usize>(c: &mut Cursor<'_>, r: &RecentRing<Fill, N>, mono_ns: u64) {
    c.put(b"[");
    let n = r.len();
    let mut k = 0usize;
    while k < n {
        if k > 0 {
            c.put(b",");
        }
        if let Some(f) = r.oldest_first(k) {
            c.put(b"{\"ts_ns\":");
            c.u64_str(f.ts_ns);
            c.key("age_s");
            c.age_s(mono_ns, f.ts_ns);
            c.key("sym");
            c.u64(u64::from(f.sym));
            c.key("side");
            c.u64(f.side as u64);
            c.key("px_1e6");
            c.i64(f.px.raw());
            c.key("qty_1e6");
            c.i64(f.qty.raw());
            c.key("oid");
            c.u64_str(f.order_id);
            c.put(b"}");
        }
        k += 1;
    }
    c.put(b"]");
}

// -----------------------------------------------------------------
// Cursor — sticky-overflow byte writer
// -----------------------------------------------------------------

struct Cursor<'a> {
    buf: &'a mut [u8],
    pos: usize,
    overflow: bool,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn new(buf: &'a mut [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            overflow: false,
        }
    }

    #[inline]
    fn put(&mut self, src: &[u8]) {
        let end = self.pos.saturating_add(src.len());
        if end > self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.pos..end].copy_from_slice(src);
        self.pos = end;
    }

    #[inline]
    fn byte(&mut self, b: u8) {
        if self.pos >= self.buf.len() {
            self.overflow = true;
            return;
        }
        self.buf[self.pos] = b;
        self.pos += 1;
    }

    /// `,"key":` — every key but the first of an object.
    #[inline]
    fn key(&mut self, k: &str) {
        self.put(b",\"");
        self.put(k.as_bytes());
        self.put(b"\":");
    }

    /// `"bytes"` — a key or a name that needs no escaping (ASCII
    /// identifiers only).
    #[inline]
    fn quoted(&mut self, s: &[u8]) {
        self.byte(b'"');
        self.put(s);
        self.byte(b'"');
    }

    /// A JSON string with `"`, `\` and control bytes escaped (paths
    /// and free text; ASCII-safe — non-ASCII bytes pass through, which
    /// is valid when the source was UTF-8, as every text field is).
    fn text(&mut self, s: &[u8]) {
        self.byte(b'"');
        let mut i = 0usize;
        while i < s.len() {
            let b = s[i];
            match b {
                b'"' => self.put(b"\\\""),
                b'\\' => self.put(b"\\\\"),
                b'\n' => self.put(b"\\n"),
                b'\r' => self.put(b"\\r"),
                b'\t' => self.put(b"\\t"),
                0..=0x1F => {
                    self.put(b"\\u00");
                    self.byte(HEX[(b >> 4) as usize]);
                    self.byte(HEX[(b & 0xF) as usize]);
                }
                _ => self.byte(b),
            }
            i += 1;
        }
        self.byte(b'"');
    }

    /// Decimal u64, max 20 chars.
    fn u64(&mut self, mut v: u64) {
        if v == 0 {
            self.byte(b'0');
            return;
        }
        let mut tmp = [0u8; 20];
        let mut i = tmp.len();
        while v > 0 {
            i -= 1;
            tmp[i] = b'0' + (v % 10) as u8;
            v /= 10;
        }
        self.put(&tmp[i..]);
    }

    /// Decimal i64.
    fn i64(&mut self, v: i64) {
        if v < 0 {
            self.byte(b'-');
        }
        self.u64(v.unsigned_abs());
    }

    /// Decimal u64 as a JSON string (the > 2^53 law).
    #[inline]
    fn u64_str(&mut self, v: u64) {
        self.byte(b'"');
        self.u64(v);
        self.byte(b'"');
    }

    /// Lower-hex string of `bytes`.
    fn hex(&mut self, bytes: &[u8]) {
        self.byte(b'"');
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            self.byte(HEX[(b >> 4) as usize]);
            self.byte(HEX[(b & 0xF) as usize]);
            i += 1;
        }
        self.byte(b'"');
    }

    /// Whole seconds from `stamp` to `now` as a number; `-1` when
    /// `stamp == 0` (never).
    #[inline]
    fn age_s(&mut self, now: u64, stamp: u64) {
        if stamp == 0 {
            self.put(b"-1");
        } else {
            self.u64(now.saturating_sub(stamp) / 1_000_000_000);
        }
    }

    #[inline]
    fn finish(self) -> Result<usize, JsonOverflow> {
        if self.overflow {
            Err(JsonOverflow)
        } else {
            Ok(self.pos)
        }
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_primitives_render_exactly() {
        let mut buf = [0u8; 256];
        let mut c = Cursor::new(&mut buf);
        c.u64(0);
        c.byte(b' ');
        c.u64(u64::MAX);
        c.byte(b' ');
        c.i64(-42);
        c.byte(b' ');
        c.u64_str(7);
        c.byte(b' ');
        c.hex(&[0x00, 0xAB, 0xFF]);
        c.byte(b' ');
        c.text(b"a\"b\\c\nd\x01");
        c.byte(b' ');
        c.age_s(5_000_000_000, 0);
        c.byte(b' ');
        c.age_s(5_000_000_000, 2_000_000_000);
        let n = c.finish().unwrap();
        assert_eq!(
            core::str::from_utf8(&buf[..n]).unwrap(),
            "0 18446744073709551615 -42 \"7\" \"00abff\" \"a\\\"b\\\\c\\nd\\u0001\" -1 3"
        );
    }

    #[test]
    fn cursor_overflow_is_sticky_and_refuses() {
        let mut buf = [0u8; 4];
        let mut c = Cursor::new(&mut buf);
        c.put(b"abc");
        c.put(b"de"); // does not fit
        c.put(b"f"); // would fit alone — still refused: the body is not truncated
        assert_eq!(c.finish(), Err(JsonOverflow));
    }

    #[test]
    fn empty_snapshot_encodes_and_starts_with_schema() {
        let s = EngineSnapshot::empty();
        let mut buf = vec![0u8; STATE_JSON_MAX];
        let n = encode_state_json(&s, &mut buf).unwrap();
        let body = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(body.starts_with("{\"v\":1,\"seq\":0,"), "{body}");
        assert!(body.ends_with("\"fills\":[]}}"), "{body}");
        assert!(body.contains("\"slots\":[{\"slot\":0,\"name\":\"latency-arb\""));
        assert!(body.contains("\"venue\":\"rpc\""));
        assert!(body.contains("\"heartbeat_age_s\":-1"));
    }

    #[test]
    fn too_small_buffer_is_refused_not_truncated() {
        let s = EngineSnapshot::empty();
        let mut buf = [0u8; 512];
        assert_eq!(encode_state_json(&s, &mut buf), Err(JsonOverflow));
    }
}
