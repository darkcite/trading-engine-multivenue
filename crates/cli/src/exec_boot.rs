// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Execution boot: `exec.toml` + `--arm-live` → an [`ExecRoute`].
//!
//! This is where the artifact becomes the router's table, and where the
//! **two-switch interlock** is enforced. `core_config::exec` answers
//! "is this artifact well-formed?"; this module answers "may this
//! binary, on this command line, actually arm what it asks for?" —
//! which only the cli can answer, because only the binary knows which
//! live arms were compiled into it.
//!
//! ## The interlock (plan §3.4)
//!
//! `--arm-live <slots>` must name **exactly** the set of slots the
//! artifact marks live:
//!
//! * artifact live, flag silent  → boot refusal;
//! * flag names it, artifact paper → boot refusal;
//! * both agree → armed.
//!
//! Set equality rather than "the flag is a subset" is deliberate. A
//! subset rule would let an operator arm a slot by editing one file,
//! which is exactly the single point of failure the second switch
//! exists to remove. Both spellings of disagreement print the two sets
//! so the fix is obvious from the log alone.
//!
//! ## Why `mode = "live"` refuses the boot in E1
//!
//! [`LIVE_ARM_VENUES`] is the set of venues this binary has a live
//! execution arm for. **In E1 it is empty** — E1 builds the routing
//! mechanism, E2 builds the Hyperliquid signer and E3 its HTTP arm. So
//! any artifact marking a slot live refuses the boot today, and the
//! refusal names the phase that will supply the arm rather than leaving
//! an operator guessing. That is the plan's own law: *"`mode = "live"`
//! on a slot whose venue has no compiled live arm refuses the boot —
//! never a silent no-op, never a downgrade."*
//!
//! An absent artifact is not an error and never will be: it is every
//! slot paper, which is the engine's behaviour bit for bit.

use std::path::{Path, PathBuf};

use exec_router::{ExecMode, ExecRoute, HaltLimits, SlotCaps, EXEC_SLOTS};

/// **E6 — the mirrored constants, held in agreement.**
///
/// `core_config::exec` refuses a live slot whose `max_open_orders`
/// exceeds its share of `exec_router`'s shared resting table, and it
/// has to restate that table's size: `exec-router` depends on
/// `core-config`, so the dependency cannot run the other way.
///
/// This crate depends on BOTH, which makes it the one place the
/// restatement can be checked. It matters in one direction
/// especially: if `LEDGER_RESTING` were ever REDUCED, `core_config`
/// would go on admitting eight slots of sixty-four into a smaller
/// table and every one of them would ratchet into permanent refusal
/// through the ledger's fail-closed path.
///
/// An earlier draft of the `core_config` comment claimed
/// `exec_router::route::tests` already asserted this. It did not —
/// which is worse than no check, because a false claim of coverage
/// stops the next reader adding one.
const _: () = assert!(
    core_config::exec::LEDGER_RESTING_MIRROR == exec_router::LEDGER_RESTING,
    "core_config::exec::LEDGER_RESTING_MIRROR is out of step with exec_router::LEDGER_RESTING"
);
const _: () = assert!(
    core_config::exec::MAX_OPEN_ORDERS_PER_SLOT
        == exec_router::LEDGER_RESTING / EXEC_SLOTS,
    "the per-slot open-order share no longer divides the resting table"
);
use tracing::info;

/// Venues this binary can actually dispatch to live.
///
/// **E7 (2026-09-19): `VenueId::Hyperliquid`.** E1 shipped this empty
/// with a compile-time assertion that it stayed empty — the whole
/// barrier between a configuration file and a real order. Deleting
/// that assertion is the moment the engine became capable of trading
/// real money, and it was reviewed as such: the four E7 review
/// reports under `Claude outputs/review-e1e6/` are the record. The
/// barriers that remain are the ones E6 built — the two-switch
/// interlock (`--arm-live` must agree with the artifact), the seeding
/// interlock (no live PLACE until the arm has reconciled and agreed),
/// the four clamps, the six halt triggers and `exec.HALT` — plus the
/// network interlock in the boot (`HYPERLIQUID_WS_HOST` and
/// `HYPERLIQUID_EXCHANGE_HOST` on the same network, or no boot), and
/// the `.env` itself: a boot with no `HYPERLIQUID_*` variables refuses
/// before any socket opens.
///
/// **HYPARB L5 (2026-09-24, ruling O-HL1): `VenueId::HyperEvm`**, for
/// slot 0 only ([`HYPEREVM_SLOT`]) and only together with Hyperliquid:
/// its live arm (`cli::hyparb_live`) swaps on HyperEVM mainnet and
/// hedges on Hyperliquid from the slot's own wallet.
pub const LIVE_ARM_VENUES: &[u8] = &[
    core_types::VenueId::Hyperliquid as u8,
    core_types::VenueId::HyperEvm as u8,
];

/// The one slot with a live HyperEVM arm: `hyparb`. It is live on BOTH
/// of its venues or not at all — an AMM leg without its hedge, or a
/// hedge without its AMM leg, is a one-legged arb.
pub const HYPEREVM_SLOT: usize = 0;

/// XMM XH1: slot 6 (`xmm`) has NO live arm of its own until XH4 gives it
/// its own Hyperliquid master account and gateway (rulings O-XH3,
/// O-XH7). Until then a live slot 6 refuses the boot: armed today it
/// would share slot 3's Hyperliquid arm, where a slot-6 halt's
/// venue-wide cancel would pull bin15's quotes.
pub const XMM_SLOT: usize = strategy_set::SLOT_XMM as usize;

/// Three crates name their own slot count and the dependency graph
/// forbids them importing each other's. Assert all three agree at
/// COMPILE time: a mismatch would silently truncate the per-slot arrays
/// that cross the `OrderDispatch` boundary, and would make
/// `ExecFile::live_mask`'s shift set the wrong bit.
const _: () = assert!(core_config::exec::EXEC_SLOTS == EXEC_SLOTS);
const _: () = assert!(clob_dispatcher::EXEC_COUNTER_SLOTS == EXEC_SLOTS);

/// Slot names, for boot tells and refusal messages. Index = slot;
/// mirrors `strategy-set`'s composition order.
pub const SLOT_NAMES: [&str; EXEC_SLOTS] = [
    "hyparb", "vrp", "xsd", "bin15", "ai-exec", "vm", "xmm", "reserved",
];

/// A resolved execution configuration.
#[derive(Debug, Clone)]
pub struct ExecBoot {
    /// The table the router runs.
    pub route: ExecRoute,
    /// sha256 of the artifact BYTES — the boot tell's identity, exactly
    /// as `bin15.toml` and `vrp.toml`.
    pub hash: [u8; 32],
    /// Where the artifact was read from.
    pub path: PathBuf,
    /// `[exec] enabled`. `false` means every slot is paper regardless
    /// of what the slot sections say.
    pub enabled: bool,
    /// Bit `i` set = slot `i` is live.
    pub live_mask: u8,
    /// The slots the artifact named, as parsed.
    ///
    /// The hot [`ExecRoute`] carries the four clamps and the halt
    /// thresholds; what it does not carry — `request_budget_floor`,
    /// the slot's name, the venue list as written — lives here so the
    /// boot tell can publish every number the operator wrote and the
    /// arm can read the budget floor without re-parsing. Cold; a
    /// `Vec` is fine at boot.
    pub slots: Vec<core_config::exec::ExecSlot>,
}

impl ExecBoot {
    /// Is any slot actually armed?
    #[must_use]
    pub fn any_live(&self) -> bool {
        self.live_mask != 0
    }

    /// HYPARB L5: is slot 0 (its own live arm) armed?
    #[must_use]
    pub fn hyparb_live(&self) -> bool {
        self.live_mask & (1 << HYPEREVM_SLOT) != 0
    }

    /// Does a live slot OTHER than slot 0 trade Hyperliquid — i.e. is
    /// the operator's Hyperliquid arm (`HlExchange` on the
    /// `HYPERLIQUID_*` account) needed? Slot 0 hedges from its own
    /// account and never counts here.
    #[must_use]
    pub fn hl_arm_needed(&self) -> bool {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let mut slot = 0usize;
        while slot < EXEC_SLOTS {
            if slot != HYPEREVM_SLOT
                && self.live_mask & (1u8 << slot) != 0
                && self.route.venue_allowed(slot as u8, hl)
            {
                return true;
            }
            slot += 1;
        }
        false
    }
}

/// Parse `--arm-live 3` / `--arm-live 3,5` into a slot bitmask.
///
/// An empty string is an empty set (mask 0) — which pairs with an
/// artifact that arms nothing, and is how `--arm-live ""` says "I mean
/// nothing" explicitly.
pub fn parse_arm_live(spec: &str) -> Result<u8, String> {
    let mut mask = 0u8;
    for part in spec.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let slot: usize = p
            .parse()
            .map_err(|_| format!("--arm-live: `{p}` is not a slot number"))?;
        if slot >= EXEC_SLOTS {
            return Err(format!(
                "--arm-live: slot {slot} is out of range (0..{EXEC_SLOTS})"
            ));
        }
        let bit = 1u8 << slot;
        if mask & bit != 0 {
            return Err(format!("--arm-live: slot {slot} named twice"));
        }
        mask |= bit;
    }
    Ok(mask)
}

/// **Where `exec.HALT` lives: beside the artifact that armed the
/// slots.**
///
/// Not a fixed path and not the cwd: an operator who runs two engines
/// from two artifacts must get two halt files, and the artifact
/// directory is the only place that is already per-engine. `path` is
/// a file the boot has read, so it has a parent; a bare filename
/// falls back to the cwd, which is where a bare filename was read
/// from.
#[must_use]
pub fn halt_file_path(artifact: &Path) -> PathBuf {
    artifact
        .parent()
        .map_or_else(|| PathBuf::from(HALT_FILE), |d| d.join(HALT_FILE))
}

/// The name of the file a halt writes, beside the exec artifact.
pub const HALT_FILE: &str = "exec.HALT";

/// Parse `--halt-slot 3` / `--halt-slot 3,5` into a slot mask.
///
/// Deliberately NOT gated on `--arm-live`: halting a slot is the safe
/// direction, so it must be the one thing an operator can always ask
/// for, including on a boot that arms nothing.
///
/// # Errors
/// A part that is not a slot number, a slot outside `0..EXEC_SLOTS`,
/// or a slot named twice.
pub fn parse_halt_slots(spec: &str) -> Result<u8, String> {
    let mut mask = 0u8;
    for part in spec.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let slot: usize = p
            .parse()
            .map_err(|_| format!("--halt-slot: `{p}` is not a slot number"))?;
        if slot >= EXEC_SLOTS {
            return Err(format!(
                "--halt-slot: slot {slot} is out of range (0..{EXEC_SLOTS})"
            ));
        }
        let bit = 1u8 << slot;
        if mask & bit != 0 {
            return Err(format!("--halt-slot: slot {slot} named twice"));
        }
        mask |= bit;
    }
    Ok(mask)
}

/// Render a slot bitmask as `[3]` / `[0,1,2]` / `[]`.
#[must_use]
pub fn render_slot_mask(mask: u8) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for s in 0..EXEC_SLOTS {
        if mask & (1u8 << s) != 0 {
            if !first {
                out.push(',');
            }
            out.push_str(&s.to_string());
            first = false;
        }
    }
    out.push(']');
    out
}

/// Resolve the artifact into a route table.
///
/// * `artifact` — `--exec <path>`. `None` means "no `--exec`", which is
///   `Ok(None)`: every slot paper, today's behaviour bit for bit. An
///   EXPLICIT path that does not exist is an error, because an operator
///   who named a file meant it (the icdp/F19 requested-but-absent law).
/// * `arm_live` — the `--arm-live` spec, `None` when the flag is absent
///   (an absent flag arms nothing, so it must pair with an artifact
///   that arms nothing).
pub fn resolve(artifact: Option<&Path>, arm_live: Option<&str>) -> Result<Option<ExecBoot>, String> {
    let Some(path) = artifact else {
        // No `--exec`. If `--arm-live` was passed anyway, that is a
        // half-armed command line and must not boot silently.
        if let Some(spec) = arm_live {
            let mask = parse_arm_live(spec)?;
            if mask != 0 {
                return Err(format!(
                    "--arm-live {} was given without --exec: arming needs BOTH \
                     switches, and there is no artifact to agree with",
                    render_slot_mask(mask)
                ));
            }
        }
        return Ok(None);
    };
    let path = path.to_path_buf();
    if !path.exists() {
        return Err(format!("{}: no such file", path.display()));
    }
    let (file, bytes) = core_config::exec::load(&path).map_err(|e| e.to_string())?;
    let hash = core_crypto::sha256(&bytes);

    // --- The interlock -----------------------------------------------
    let artifact_mask = file.live_mask();
    let flag_mask = parse_arm_live(arm_live.unwrap_or(""))?;
    if artifact_mask != flag_mask {
        return Err(format!(
            "exec: the two arming switches DISAGREE — {} marks slots {} live, \
             --arm-live names {}. They must match exactly; neither switch alone \
             arms anything. (Slots: {})",
            path.display(),
            render_slot_mask(artifact_mask),
            render_slot_mask(flag_mask),
            describe_slots(artifact_mask | flag_mask),
        ));
    }

    // --- Build the table ---------------------------------------------
    let mut route = ExecRoute::all_paper();
    let mut slots: Vec<core_config::exec::ExecSlot> = Vec::new();
    for (slot, slot_name) in SLOT_NAMES.iter().enumerate() {
        let s = file.slot(slot);
        let mode = ExecMode::parse(&s.mode)
            .ok_or_else(|| format!("exec: slot {slot}: unknown mode `{}`", s.mode))?;

        // The artifact's own belief about which member this slot holds,
        // against this binary's map. Slot numbers get reassigned; this
        // is what stops a stale artifact arming the wrong member.
        if mode == ExecMode::Live && s.name != *slot_name {
            return Err(format!(
                "exec: slot {slot} is marked live as `{}`, but in THIS binary slot {slot} is \
                 `{slot_name}`. Slot numbers get reassigned between phases — refusing rather \
                 than arming a member the artifact did not mean. Fix the artifact's `name`, \
                 or the slot number, whichever is stale.",
                s.name
            ));
        }

        // XMM XH1: no live arm for slot 6 before XH4 ([`XMM_SLOT`]).
        if mode == ExecMode::Live && slot == XMM_SLOT {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) is marked live, but slot {slot} has no live \
                 arm of its own before XMM XH4 (its own Hyperliquid master account and \
                 gateway, rulings O-XH3/O-XH7) — refusing rather than arming it on slot 3's \
                 account, where its halt would cancel bin15's quotes"
            ));
        }

        // HYPARB L5: HyperEVM is slot 0's alone, and slot 0 is live on
        // both of its venues or not at all.
        if mode == ExecMode::Live {
            let evm = s.venues.contains(&(core_types::VenueId::HyperEvm as u8));
            let hl = s.venues.contains(&(core_types::VenueId::Hyperliquid as u8));
            if slot == HYPEREVM_SLOT && !(evm && hl && s.venues.len() == 2) {
                return Err(format!(
                    "exec: slot {slot} ({slot_name}) is marked live with venues {:?}; it is live \
                     on exactly [\"hyperliquid\", \"hyperevm\"] or not at all — an AMM leg \
                     without its hedge (or the reverse) is a one-legged arb",
                    s.venues
                ));
            }
            if slot != HYPEREVM_SLOT && evm {
                return Err(format!(
                    "exec: slot {slot} ({slot_name}) is marked live for `hyperevm`, whose only \
                     live arm is slot 0's (hyparb) — refusing"
                ));
            }
        }

        // A live slot needs a compiled arm for EVERY venue it names.
        // Refuse loudly and name the phase that supplies it — never
        // boot inert, never downgrade to paper.
        if mode == ExecMode::Live {
            for v in &s.venues {
                if !LIVE_ARM_VENUES.contains(v) {
                    let vname = core_config::exec::venue_name_from_id(*v).unwrap_or("?");
                    return Err(format!(
                        "exec: slot {slot} ({slot_name}) is marked live for venue `{vname}`, \
                         but this binary has NO live execution arm for it (the arms compiled \
                         in are hyperliquid, and hyperevm for slot 0). Slot {slot} can only be \
                         \"paper\" or \"off\" on that venue. Refusing the boot rather than \
                         trading it on paper under a live label."
                    ));
                }
            }
        }

        route
            .set_slot(
                slot,
                mode,
                &s.venues,
                SlotCaps::new(
                    s.max_order_usd_1e6,
                    s.cap_instance_usd_1e6,
                    s.cap_day_usd_1e6,
                    s.max_open_orders,
                ),
                // E6 commit 3: the operator's halt thresholds. These
                // were parsed and carried nowhere until the halt
                // machine existed to read them.
                HaltLimits::new(
                    u32::try_from(s.halt_on_reject_streak).unwrap_or(u32::MAX),
                    s.halt_on_recon_drift_usd_1e6,
                    s.halt_on_ws_gap_ms,
                    u32::try_from(s.halt_on_asset_refusal_streak).unwrap_or(u32::MAX),
                    s.halt_on_recon_stale_ms,
                )
                // E7: the operator's session bound ("run until it
                // either earns +X or loses Y"); 0/0 = off.
                .with_pnl_bound(s.halt_on_gain_usd_1e6, s.halt_on_loss_usd_1e6),
            )
            .map_err(|e| format!("exec: slot {slot}: {e}"))?;
        // Keep every parsed number, not just the EIGHT the hot table
        // holds — the boot tell publishes all of them, and E4's
        // budget governor reads `request_budget_floor` from here.
        slots.push(s);
    }

    Ok(Some(ExecBoot {
        route,
        hash,
        path,
        enabled: file.enabled,
        live_mask: artifact_mask,
        slots,
    }))
}

/// `3 (bin15)` / `3 (bin15), 5 (vm)` — for the interlock's message.
fn describe_slots(mask: u8) -> String {
    let mut out = String::new();
    for (s, name) in SLOT_NAMES.iter().enumerate() {
        if mask & (1u8 << s) != 0 {
            if !out.is_empty() {
                out.push_str(", ");
            }
            out.push_str(&format!("{s} ({name})"));
        }
    }
    if out.is_empty() {
        out.push_str("none");
    }
    out
}

/// The `exec:` boot tell — plan §3.5.
///
/// Line 1 names the artifact and the whole partition, so one line tells
/// an operator every slot's disposition.
///
/// A LIVE slot then gets two more lines: the clamps, labelled
/// `caps-ENFORCED` because since E6 every live submit and modify
/// passes `RoutedDispatcher::risk_check` against them, and the halt
/// thresholds, every one of them. E1 printed the same numbers under
/// `caps-DECLARED-NOT-ENFORCED` with a WARNING line saying no clamp
/// existed — and that text was still what a live boot printed after
/// E6 had made it false, pinned by a test that asserted the false
/// text (E7 review, 2026-09-19). The last line an operator reads
/// before real money must say what the code does.
#[must_use]
pub fn render_boot_tell(boot: &ExecBoot) -> Vec<String> {
    let mut hex = String::with_capacity(64);
    for b in &boot.hash {
        hex.push_str(&format!("{b:02x}"));
    }
    let mut paper = 0u8;
    for s in 0..EXEC_SLOTS {
        if boot.route.mode_at(s) == Some(ExecMode::Paper) {
            paper |= 1u8 << s;
        }
    }
    let mut lines = vec![format!(
        "exec: artifact configured hash={hex} slots={EXEC_SLOTS} enabled={} live={} paper={} off={}",
        u8::from(boot.enabled),
        render_slot_mask(boot.live_mask),
        render_slot_mask(paper),
        render_slot_mask(boot.route.off_mask()),
    )];
    for (slot, slot_name) in SLOT_NAMES.iter().enumerate() {
        if boot.route.mode_at(slot) != Some(ExecMode::Live) {
            continue;
        }
        let vmask = boot.route.venue_mask_at(slot).unwrap_or(0);
        let mut venues = String::new();
        for v in 0..core_types::VENUE_COUNT as u8 {
            if vmask & (1u16 << v) != 0 {
                if !venues.is_empty() {
                    venues.push(',');
                }
                venues.push_str(core_config::exec::venue_name_from_id(v).unwrap_or("?"));
            }
        }
        let s = boot.slots.iter().find(|s| s.slot == slot);
        let day = s.map_or(0, |s| s.cap_day_usd_1e6) / 1_000_000;
        let inst = s.map_or(0, |s| s.cap_instance_usd_1e6) / 1_000_000;
        let drift = s.map_or(0, |s| s.halt_on_recon_drift_usd_1e6) / 1_000_000;
        lines.push(format!(
            "exec: slot {slot} LIVE name={slot_name} venue={venues} \
             caps-ENFORCED order=${} open={} day=${day} instance=${inst} \
             (the risk gate clamps every live submit and modify; \
             worst case with every quote working = instance + open x order)",
            boot.route.max_order_usd_1e6_at(slot).unwrap_or(0) / 1_000_000,
            boot.route.max_open_orders_at(slot).unwrap_or(0),
        ));
        let gain = s.map_or(0, |s| s.halt_on_gain_usd_1e6) / 1_000_000;
        let loss = s.map_or(0, |s| s.halt_on_loss_usd_1e6) / 1_000_000;
        lines.push(format!(
            "exec: slot {slot} HALTS reject_streak={} asset_refusals={} recon_drift=${drift} \
             recon_stale_ms={} ws_gap_ms={} budget_floor={} topup={}/{} \
             session_bound=+${gain}/-${loss} halt_file={}",
            s.map_or(0, |s| s.halt_on_reject_streak),
            s.map_or(0, |s| s.halt_on_asset_refusal_streak),
            s.map_or(0, |s| s.halt_on_recon_stale_ms),
            s.map_or(0, |s| s.halt_on_ws_gap_ms),
            s.map_or(0, |s| s.request_budget_floor),
            s.map_or(0, |s| s.request_topup_weight),
            s.map_or(0, |s| s.request_topup_day_max),
            halt_file_path(&boot.path).display(),
        ));
    }
    lines
}

/// Emit the boot tell through `tracing`.
pub fn log_boot_tell(boot: &ExecBoot) {
    for line in render_boot_tell(boot) {
        info!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, src: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        p
    }

    /// **The shipped template must be a bootable live artifact.**
    ///
    /// `exec.toml.example` tells the operator to change `mode` to
    /// `"live"`, so every key a live slot requires has to be in it.
    /// E6 added two and the template was missed — the only sign was a
    /// boot refusal naming a key the operator had never heard of.
    /// This test is the thing that would have said so.
    #[test]
    fn the_shipped_template_arms_a_live_slot_without_further_edits() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../exec.toml.example"),
        )
        .expect("exec.toml.example must ship with the repo");
        // The one edit the file's own comments ask for.
        let live = src.replacen("mode = \"paper\"", "mode = \"live\"", 1);
        let f = core_config::exec::parse(&live).expect("the template must parse as live");
        let s3 = f.slot(3);
        assert!(s3.is_live(), "slot 3 is the one the template arms");
        for (key, v) in [
            ("halt_on_reject_streak", s3.halt_on_reject_streak),
            ("halt_on_recon_drift_usd_1e6", s3.halt_on_recon_drift_usd_1e6),
            ("halt_on_ws_gap_ms", s3.halt_on_ws_gap_ms),
            ("halt_on_asset_refusal_streak", s3.halt_on_asset_refusal_streak),
            ("halt_on_recon_stale_ms", s3.halt_on_recon_stale_ms),
            ("request_budget_floor", s3.request_budget_floor),
        ] {
            assert!(v > 0, "template leaves `{key}` unset — the boot refuses");
        }
        // E7: the session bound ships COMMENTED OUT — it is a ruling
        // the operator makes per session, not a default — and the
        // template's own text is the recipe for turning it on.
        assert_eq!((s3.halt_on_gain_usd_1e6, s3.halt_on_loss_usd_1e6), (0, 0));
        let bounded = live
            .replacen("# halt_on_gain_usd_1e6 = 15000000", "halt_on_gain_usd_1e6 = 15000000", 1)
            .replacen("# halt_on_loss_usd_1e6 = 5000000", "halt_on_loss_usd_1e6 = 5000000", 1);
        let b = core_config::exec::parse(&bounded).expect("the template's recipe parses");
        assert_eq!(
            (b.slot(3).halt_on_gain_usd_1e6, b.slot(3).halt_on_loss_usd_1e6),
            (15_000_000, 5_000_000)
        );
    }

    #[test]
    fn the_halt_file_lands_beside_the_artifact_that_armed_the_slots() {
        assert_eq!(
            halt_file_path(Path::new("/srv/bin15/exec.toml")),
            PathBuf::from("/srv/bin15/exec.HALT")
        );
        // Two engines from two artifacts get two halt files.
        assert_ne!(
            halt_file_path(Path::new("/srv/a/exec.toml")),
            halt_file_path(Path::new("/srv/b/exec.toml"))
        );
        // A bare filename was read from the cwd; the halt goes there.
        assert_eq!(
            halt_file_path(Path::new("exec.toml")),
            PathBuf::from("exec.HALT")
        );
    }

    #[test]
    fn halt_slots_parse_the_same_shapes_arm_live_does() {
        assert_eq!(parse_halt_slots("3").unwrap(), 0b0000_1000);
        assert_eq!(parse_halt_slots("3,5").unwrap(), 0b0010_1000);
        assert_eq!(parse_halt_slots("").unwrap(), 0);
        assert!(parse_halt_slots("x").unwrap_err().contains("not a slot"));
        assert!(parse_halt_slots("9").unwrap_err().contains("out of range"));
        assert!(parse_halt_slots("3,3").unwrap_err().contains("twice"));
    }

    /// The smallest artifact that legally arms slot 3.
    const MINIMAL_LIVE: &str = "[exec]\n[exec.slot.3]\nmode = \"live\"\nname = \"bin15\"\n\
         venues = [\"hyperliquid\"]\nmax_order_usd_1e6 = 100000000\n\
         cap_instance_usd_1e6 = 1000000000\ncap_day_usd_1e6 = 30000000000\n\
         max_open_orders = 64\nrequest_budget_floor = 2000\n\
         halt_on_reject_streak = 5\nhalt_on_recon_drift_usd_1e6 = 5000000\n\
         halt_on_ws_gap_ms = 30000\nhalt_on_asset_refusal_streak = 3\n\
         halt_on_recon_stale_ms = 300000\n";

    /// A directory of this test's own.
    ///
    /// **Counted, not clocked.** Keying this on the wall clock made it
    /// a flaky gate: `cargo test` runs these in parallel, two threads
    /// landing in the same clock tick got the SAME directory, and the
    /// first to finish removed the other's artifact out from under it
    /// — surfacing as `resolve()` reporting "no such file" in whatever
    /// test lost the race, roughly one run in six. A counter cannot
    /// tie.
    fn tmp() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("exec-boot-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// XMM XH1: a live slot 6 refuses the boot until XH4 gives xmm its
    /// own arm — even correctly named and agreed by `--arm-live` — while
    /// a PAPER slot 6 is an ordinary artifact.
    #[test]
    fn a_live_slot_six_refuses_until_its_own_arm_exists() {
        let d = tmp();
        let six = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.6]")
            .replace("name = \"bin15\"", "name = \"xmm\"");
        let p = write(&d, "exec.toml", &six);
        let e = resolve(Some(&p), Some("6")).expect_err("no live arm for slot 6 before XH4");
        assert!(e.contains("slot 6 (xmm)") && e.contains("XH4"), "{e}");
        let paper = six.replacen("mode = \"live\"", "mode = \"paper\"", 1);
        let p = write(&d, "exec-paper.toml", &paper);
        let boot = resolve(Some(&p), None).expect("a paper slot 6 is fine").expect("present");
        assert_eq!(boot.route.mode_at(XMM_SLOT), Some(ExecMode::Paper));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn no_exec_flag_is_ok_none_and_arms_nothing() {
        assert!(resolve(None, None).unwrap().is_none());
        assert!(resolve(None, Some("")).unwrap().is_none());
    }

    #[test]
    fn arm_live_without_exec_is_refused() {
        let e = resolve(None, Some("3")).unwrap_err();
        assert!(e.contains("without --exec"), "{e}");
        assert!(e.contains("BOTH"), "{e}");
    }

    #[test]
    fn an_explicit_missing_artifact_is_refused() {
        let e = resolve(Some(Path::new("/nonexistent/exec.toml")), None).unwrap_err();
        assert!(e.contains("no such file"), "{e}");
    }

    #[test]
    fn an_all_paper_artifact_resolves_to_an_all_paper_table() {
        let d = tmp();
        let p = write(&d, "exec.toml", "[exec]\nenabled = 1\n[exec.slot.1]\nmode = \"paper\"\n");
        let b = resolve(Some(&p), None).unwrap().unwrap();
        assert!(!b.any_live());
        assert_eq!(b.live_mask, 0);
        for s in 0..EXEC_SLOTS {
            assert_eq!(b.route.mode_at(s), Some(ExecMode::Paper), "slot {s}");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// The interlock, both directions.
    #[test]
    fn the_two_switches_must_agree_exactly() {
        let d = tmp();
        let p = write(&d, "exec.toml", MINIMAL_LIVE);

        // Artifact says live, flag silent.
        let e = resolve(Some(&p), None).unwrap_err();
        assert!(e.contains("DISAGREE"), "{e}");
        assert!(e.contains("marks slots [3] live"), "{e}");
        assert!(e.contains("--arm-live names []"), "{e}");
        assert!(e.contains("3 (bin15)"), "{e}");

        // Flag names a slot the artifact calls paper.
        let p2 = write(&d, "paper.toml", "[exec]\n[exec.slot.3]\nmode = \"paper\"\n");
        let e = resolve(Some(&p2), Some("3")).unwrap_err();
        assert!(e.contains("DISAGREE"), "{e}");
        assert!(e.contains("marks slots [] live"), "{e}");
        assert!(e.contains("--arm-live names [3]"), "{e}");

        // Flag names a DIFFERENT slot than the artifact.
        let e = resolve(Some(&p), Some("5")).unwrap_err();
        assert!(e.contains("DISAGREE"), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// A live slot on a venue with no compiled arm must refuse loudly
    /// and never boot inert. Since E7 the Hyperliquid arm exists, so
    /// the refusal is asserted on a venue that has none (okx) and the
    /// hyperliquid slot is asserted to RESOLVE — with the two switches
    /// agreeing, which is the whole interlock.
    #[test]
    fn a_live_slot_refuses_the_boot_on_a_venue_with_no_arm_and_resolves_on_hyperliquid() {
        let d = tmp();
        let p = write(&d, "exec.toml", &MINIMAL_LIVE.replace("hyperliquid", "okx"));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("NO live execution arm"), "{e}");
        assert!(e.contains("bin15"), "must name the slot: {e}");
        assert!(e.contains("okx"), "must name the venue: {e}");
        assert!(
            e.contains("Refusing the boot"),
            "must not downgrade silently: {e}"
        );
        let p = write(&d, "exec-hl.toml", MINIMAL_LIVE);
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        assert!(b.any_live());
        assert!(b.route.venue_live(core_types::VenueId::Hyperliquid as u8));
        assert!(!b.route.venue_live(core_types::VenueId::Okx as u8));
        assert_eq!(
            LIVE_ARM_VENUES,
            &[
                core_types::VenueId::Hyperliquid as u8,
                core_types::VenueId::HyperEvm as u8
            ]
        );
        assert!(b.hl_arm_needed() && !b.hyparb_live());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn enabled_zero_disarms_and_therefore_needs_no_arm_live() {
        let d = tmp();
        let p = write(
            &d,
            "exec.toml",
            &MINIMAL_LIVE.replace("[exec]", "[exec]\nenabled = 0"),
        );
        // `enabled = 0` zeroes the artifact's live mask, so the empty
        // flag agrees with it and the whole thing resolves to paper.
        let b = resolve(Some(&p), None).unwrap().unwrap();
        assert!(!b.enabled);
        assert!(!b.any_live());
        assert_eq!(b.route.mode_at(3), Some(ExecMode::Paper));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn off_slots_survive_into_the_table() {
        let d = tmp();
        let p = write(&d, "exec.toml", "[exec]\n[exec.slot.6]\nmode = \"off\"\n");
        let b = resolve(Some(&p), None).unwrap().unwrap();
        assert_eq!(b.route.mode_at(6), Some(ExecMode::Off));
        assert_eq!(b.route.off_mask(), 0b0100_0000);
        assert!(!b.any_live());
        std::fs::remove_dir_all(&d).ok();
    }

    /// This module keeps its own copy of the slot map (the dependency
    /// graph puts `engine-snapshot` out of reach of a const assert), so
    /// pin it against the one `/state` publishes. A silent divergence
    /// would make the live-slot name check compare against the wrong
    /// map — the very failure that check exists to prevent.
    #[test]
    fn the_slot_map_matches_the_one_state_publishes() {
        assert_eq!(
            SLOT_NAMES.len(),
            engine_snapshot::SLOT_NAMES.len(),
            "slot map length"
        );
        for (i, (mine, theirs)) in SLOT_NAMES
            .iter()
            .zip(engine_snapshot::SLOT_NAMES.iter())
            .enumerate()
        {
            assert_eq!(mine, theirs, "slot {i} name disagrees with engine-snapshot");
        }
    }

    /// **E6 c4:** `/state` spells the halt reason from its own copy of
    /// the word table, because the dependency runs the wrong way for
    /// it to import `HaltReason`. A silent divergence would make
    /// `/state` name the wrong reason for a halt — the one field an
    /// operator reads it for. This module sees both, so it pins them.
    #[test]
    fn the_halt_reason_words_match_the_ones_state_publishes() {
        use exec_router::HaltReason as R;
        let all = [
            R::None,
            R::RejectStreak,
            R::BudgetFloor,
            R::ReconDrift,
            R::WsGap,
            R::AssetRefusals,
            R::Operator,
            R::ReconStale,
            R::PnlGain,
            R::PnlLoss,
        ];
        assert_eq!(
            all.len(),
            engine_snapshot::HALT_REASON_WORDS.len(),
            "a reason was added without a word"
        );
        for why in all {
            assert_eq!(
                engine_snapshot::halt_reason_word(why as u8),
                why.as_str(),
                "`/state` and the router disagree about {why:?}"
            );
        }
        // A byte from a newer binary is named, not panicked on and not
        // silently rendered as `none`.
        assert_eq!(engine_snapshot::halt_reason_word(200), "unknown");
    }

    /// Slot numbers get reassigned; a stale artifact must not arm the
    /// member that inherited the number.
    #[test]
    fn a_live_slot_naming_the_wrong_member_refuses_the_boot() {
        let d = tmp();
        // Slot 3 is `bin15` in this binary; the artifact says
        // `rule-tree`, which is what slot 3 held before 2026-09-12.
        let stale = MINIMAL_LIVE.replace("name = \"bin15\"", "name = \"rule-tree\"");
        let p = write(&d, "exec.toml", &stale);
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("marked live as `rule-tree`"), "{e}");
        assert!(e.contains("slot 3 is `bin15`"), "{e}");
        assert!(e.contains("reassigned"), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// HYPARB L5 (O-HL1): slot 0 arms on exactly its two venues —
    /// never one of them — and HyperEVM on no other slot.
    #[test]
    fn slot_0_is_live_on_both_of_its_venues_or_not_at_all() {
        let d = tmp();
        let hyparb = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.0]")
            .replace("name = \"bin15\"", "name = \"hyparb\"");
        // Hyperliquid alone: the hedge without the AMM leg.
        let p = write(&d, "exec.toml", &hyparb);
        let e = resolve(Some(&p), Some("0")).unwrap_err();
        assert!(e.contains("slot 0 (hyparb)") && e.contains("one-legged"), "{e}");
        // HyperEVM alone: the AMM leg without the hedge.
        let evm_only = hyparb.replace("[\"hyperliquid\"]", "[\"hyperevm\"]");
        let p = write(&d, "exec-evm.toml", &evm_only);
        assert!(resolve(Some(&p), Some("0")).unwrap_err().contains("one-legged"));
        // Both: armed, and it needs no operator HL arm of its own.
        let both = hyparb.replace("[\"hyperliquid\"]", "[\"hyperliquid\", \"hyperevm\"]");
        let p = write(&d, "exec-both.toml", &both);
        let b = resolve(Some(&p), Some("0")).unwrap().unwrap();
        assert!(b.hyparb_live() && !b.hl_arm_needed());
        assert!(b.route.venue_allowed(0, core_types::VenueId::HyperEvm as u8));
        // With slot 3 live too: both arms.
        let two = format!("{both}{}", MINIMAL_LIVE.replace("[exec]\n", ""));
        let p = write(&d, "exec-two.toml", &two);
        let b = resolve(Some(&p), Some("0,3")).unwrap().unwrap();
        assert!(b.hyparb_live() && b.hl_arm_needed());
        // HyperEVM on any other slot is refused.
        let bin15_evm = MINIMAL_LIVE.replace("[\"hyperliquid\"]", "[\"hyperliquid\", \"hyperevm\"]");
        let p = write(&d, "exec-3evm.toml", &bin15_evm);
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("only") && e.contains("slot 0"), "{e}");
        // Paper is fine.
        let paper = hyparb.replace("mode = \"live\"", "mode = \"paper\"");
        let p = write(&d, "exec-paper.toml", &paper);
        assert!(!resolve(Some(&p), None).unwrap().unwrap().any_live());
        // `hyparb-flip.sh paper` leaves slot 0's whole live section with
        // `mode = "paper"` beside a live slot 3: that boots with slot 3
        // armed alone, and `live` is the one word back.
        let flipped = format!(
            "{}{}",
            both.replace("mode = \"live\"", "mode = \"paper\""),
            MINIMAL_LIVE.replace("[exec]\n", "")
        );
        let p = write(&d, "exec-flipped.toml", &flipped);
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        assert!(!b.hyparb_live() && b.hl_arm_needed());
        assert!(resolve(Some(&p), Some("0,3")).is_err(), "--arm-live 0 on a paper slot 0");
        std::fs::remove_dir_all(&d).ok();
    }

    /// The caps line is the last thing an operator reads before real
    /// money, so pin it exactly — including the two tokens that say
    /// nothing is enforced yet.
    #[test]
    fn the_live_boot_tell_says_plainly_that_nothing_is_enforced() {
        // `resolve()` cannot produce a live ExecBoot in E1 (no arm), so
        // build one directly — otherwise this line would never once
        // have executed before the day it mattered.
        let mut route = ExecRoute::all_paper();
        route
            .set_slot(
                3,
                ExecMode::Live,
                &[4],
                SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
                HaltLimits::none(),
            )
            .unwrap();
        let mut slot = core_config::exec::ExecSlot::paper_default(3);
        slot.mode = String::from("live");
        slot.name = String::from("bin15");
        slot.venues = vec![4];
        slot.max_order_usd_1e6 = 100_000_000;
        slot.max_open_orders = 64;
        slot.cap_day_usd_1e6 = 30_000_000_000;
        slot.cap_instance_usd_1e6 = 1_000_000_000;
        slot.request_budget_floor = 2_000;
        slot.halt_on_reject_streak = 5;
        slot.halt_on_recon_drift_usd_1e6 = 5_000_000;
        slot.halt_on_ws_gap_ms = 30_000;
        slot.halt_on_asset_refusal_streak = 3;
        slot.halt_on_recon_stale_ms = 300_000;
        let boot = ExecBoot {
            route,
            hash: [0u8; 32],
            path: PathBuf::from("/tmp/exec.toml"),
            enabled: true,
            live_mask: 0b0000_1000,
            slots: vec![slot],
        };
        let lines = render_boot_tell(&boot);
        assert_eq!(lines.len(), 3, "header + caps + halts");
        assert_eq!(
            lines[1],
            "exec: slot 3 LIVE name=bin15 venue=hyperliquid \
             caps-ENFORCED order=$100 open=64 day=$30000 instance=$1000 \
             (the risk gate clamps every live submit and modify; \
             worst case with every quote working = instance + open x order)"
        );
        assert_eq!(
            lines[2],
            "exec: slot 3 HALTS reject_streak=5 asset_refusals=3 recon_drift=$5 \
             recon_stale_ms=300000 ws_gap_ms=30000 budget_floor=2000 topup=0/0 \
             session_bound=+$0/-$0 halt_file=/tmp/exec.HALT"
        );
        // The stale E1 wording must never come back: it described a
        // clamp that did not exist, and after E6 it described one
        // that did as if it did not.
        for line in &lines {
            assert!(!line.contains("DECLARED-NOT-ENFORCED"), "{line}");
            assert!(!line.contains("no risk gate"), "{line}");
        }
        for n in ["$100", "64", "$30000", "$1000"] {
            assert!(lines[1].contains(n), "missing {n} in: {}", lines[1]);
        }
        for n in ["reject_streak=5", "asset_refusals=3", "$5", "300000", "30000", "2000"] {
            assert!(lines[2].contains(n), "missing {n} in: {}", lines[2]);
        }
    }

    #[test]
    fn arm_live_parsing() {
        assert_eq!(parse_arm_live("").unwrap(), 0);
        assert_eq!(parse_arm_live("3").unwrap(), 0b0000_1000);
        assert_eq!(parse_arm_live("3,5").unwrap(), 0b0010_1000);
        assert_eq!(parse_arm_live(" 3 , 5 ").unwrap(), 0b0010_1000);
        assert!(parse_arm_live("8").unwrap_err().contains("out of range"));
        assert!(parse_arm_live("x").unwrap_err().contains("not a slot number"));
        assert!(parse_arm_live("3,3").unwrap_err().contains("named twice"));
    }

    #[test]
    fn slot_masks_render_readably() {
        assert_eq!(render_slot_mask(0), "[]");
        assert_eq!(render_slot_mask(0b0000_1000), "[3]");
        assert_eq!(render_slot_mask(0b0000_0111), "[0,1,2]");
    }

    #[test]
    fn the_boot_tell_names_the_artifact_and_the_whole_partition() {
        let d = tmp();
        let p = write(&d, "exec.toml", "[exec]\n[exec.slot.6]\nmode = \"off\"\n");
        let b = resolve(Some(&p), None).unwrap().unwrap();
        let lines = render_boot_tell(&b);
        assert_eq!(lines.len(), 1, "no live slot, so no second line");
        let l = &lines[0];
        assert!(l.starts_with("exec: artifact configured hash="), "{l}");
        assert!(l.contains("slots=8"), "{l}");
        assert!(l.contains("enabled=1"), "{l}");
        assert!(l.contains("live=[]"), "{l}");
        assert!(l.contains("off=[6]"), "{l}");
        assert!(l.contains("paper=[0,1,2,3,4,5,7]"), "{l}");
        // The hash is the artifact's, not a constant.
        let hex: String = b.hash.iter().map(|x| format!("{x:02x}")).collect();
        assert!(l.contains(&hex), "{l}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_committed_example_resolves_and_arms_nothing() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../exec.toml.example");
        let b = resolve(Some(&example), None)
            .expect("the committed example must resolve with no --arm-live")
            .expect("and must produce a table");
        assert!(!b.any_live(), "exec.toml.example must ship disarmed");
        assert_eq!(b.route.mode_at(3), Some(ExecMode::Paper));
        // It still carries slot 3's clamps, ready for E6.
        assert_eq!(b.route.max_order_usd_1e6_at(3), Some(100_000_000));
        assert_eq!(b.route.max_open_orders_at(3), Some(64));
    }
}
