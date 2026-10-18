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

use core_types::SlotMask;
use exec_router::{ExecMode, ExecRoute, HaltLimits, PerpLimits, SlotCaps, EXEC_SLOTS};

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
/// XMM XH4f: the gateway slot's floor the parser demands is the one the
/// gateway arm accepts (`ArmErr::Budget` below it).
const _: () = assert!(
    core_config::exec::GATEWAY_BUDGET_FLOOR_MIN as u64 == exec_hyperliquid::arm::MIN_BUDGET_FLOOR,
    "core_config::exec::GATEWAY_BUDGET_FLOOR_MIN is out of step with the gateway arm's floor"
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
///
/// **BX6 (obligation 9): `VenueId::Binance`**, for USDⓈ-M × classic (the
/// products and modes this binary's gateway speaks; every other one is
/// refused naming its phase), for at most ONE live slot (BX-7 as built),
/// in the same commit as the O-BX18 side table and its reader, the
/// `VenueLock` and `MarginRisk` producers and obligations 1–4 and 8.
///
/// **PM15-6c: `VenueId::Polymarket`**, for slot 9 only ([`PM15_SLOT`]) and
/// only alone: its arm (`exec_polymarket`, `cli::pm15_live`) sends FAK
/// BUYs on CLOB V2 from the slot's own account. The legacy Polymarket
/// `--live` dispatcher is not this arm and never arms through it.
pub const LIVE_ARM_VENUES: &[u8] = &[
    core_types::VenueId::Hyperliquid as u8,
    core_types::VenueId::HyperEvm as u8,
    core_types::VenueId::Binance as u8,
    core_types::VenueId::Polymarket as u8,
];

/// **BX6 (obligation 5)** — the venues whose live arm judges the E7
/// session bound (reports an equity anchor and a delta). A slot two arms
/// trade has its bound judged only when BOTH can (`HaltSignal::merged`
/// ANDs `pnl_judged` and SUMS the deltas), so a bounded slot spanning an
/// arm outside this set would never halt on it: refused at boot.
///
/// PM15-6c: Polymarket — slot 9's arm judges its account's pUSD with its
/// unredeemed legs at cost (a settled loss at 0) against the session
/// anchor (`exec_polymarket::arm`).
pub const PNL_JUDGING_VENUES: &[u8] = &[
    core_types::VenueId::Hyperliquid as u8,
    core_types::VenueId::Binance as u8,
    core_types::VenueId::Polymarket as u8,
];

/// PM15-6c: the one slot with a live Polymarket arm, `pm15` — live on
/// `["polymarket"]` alone or not at all.
pub const PM15_SLOT: usize = strategy_set::SLOT_PM15 as usize;

/// The one slot with a live HyperEVM arm: `hyparb`. It is live on BOTH
/// of its venues or not at all — an AMM leg without its hedge, or a
/// hedge without its AMM leg, is a one-legged arb.
pub const HYPEREVM_SLOT: usize = 0;

/// XMM: slot 6 (`xmm`) is live only on its own arm, the Hyperliquid
/// gateway (`transport = "gateway"`, XH4f, deviation 10) — never on the
/// operator's blocking arm, where its requests would spend slot 3's
/// address and its sweeps reach slot 3's orders — and the gateway is
/// slot 6's alone in an engine (XH4h-3: slot 0's hedges run on it in the
/// HYPARB rehearsal on testnet; XH4i-5: slot 3 runs on a gateway arm of its
/// own, never in an engine with slot 6's).
pub const XMM_SLOT: usize = strategy_set::SLOT_XMM as usize;

/// **XMM XH4i-5** — slot 3 (`bin15`) trades HIP-4 outcome legs: on the
/// operator's blocking arm by default, or on a gateway arm of its own in the
/// outcome mode (`transport = "gateway"`) — never with the perp pair (a perp
/// slot's every order must be on a linear row), never beside a live slot of
/// the blocking arm (one master, one arm), never beside slot 6's gateway
/// (A-S7-85).
pub const BIN15_SLOT: usize = strategy_set::SLOT_BIN15 as usize;

/// HC11 (O-HC18): slot 7 (`hcv`) is built DARK — paper only. A live slot
/// 7 refuses the boot until its own live-arming ruling, whatever venues
/// it names: the engine does not arm the Hypercall order arm (HC9), the
/// E6 ledger has no options row, and the member's HL hedges would need
/// their own reconciled account.
pub const HCV_SLOT: usize = strategy_set::SLOT_HCV as usize;

/// **PVM impl-08 T4 (operator ruling 2026-10-16): slot 8 (`pine`) is PAPER
/// ONLY** — third-party Pine scripts are refused in every live arm. Three
/// independent refusals, each fail-closed: `core_config::exec` refuses a
/// live `[exec.slot.8]` at parse; [`resolve`] refuses any arming that names
/// slot 8 — the artifact OR `--arm-live` — before the interlock compares
/// them ([`PAPER_ONLY_MASK`]); and the per-slot table build refuses a Live
/// mode at [`PINE_SLOT`]. Arming it is its own live-arming ruling (O-PVM-2
/// names Binance and Hyperliquid for later).
pub const PINE_SLOT: usize = strategy_set::SLOT_PINE as usize;

/// The slots no live arm may ever name in this binary (PVM ruling
/// 2026-10-16): the Pine member's bit.
pub const PAPER_ONLY_MASK: SlotMask = strategy_set::BIT_PINE;

// The parser's paper-only slot is the member table's pine slot.
const _: () = assert!(PINE_SLOT == core_config::pine::PINE_SLOT);
const _: () = assert!(PAPER_ONLY_MASK == (1 as SlotMask) << PINE_SLOT);

/// Three crates name their own slot count and the dependency graph
/// forbids them importing each other's. Assert all three agree at
/// COMPILE time: a mismatch would silently truncate the per-slot arrays
/// that cross the `OrderDispatch` boundary, and would make
/// `ExecFile::live_mask`'s shift set the wrong bit.
const _: () = assert!(core_config::exec::EXEC_SLOTS == EXEC_SLOTS);
const _: () = assert!(clob_dispatcher::EXEC_COUNTER_SLOTS == EXEC_SLOTS);

/// XMM XH5a — the operator arm's sweep table holds every live leg the feed
/// can bind (two per family): a halt's per-slot request queues them all at
/// once, and one the table could not hold would be re-asked behind the
/// same leading legs, never swept by the halt cycle.
const _: () = assert!(
    2 * ingress_hyperliquid::family::HL_MAX_FAMILIES
        <= exec_hyperliquid::exchange::MAX_PENDING_SWEEPS
);

/// The name of a slot with no built member — `strategy-set`'s own word
/// for slots 10–15 (S16-R5; 8–15 before PVM impl-08 T4 built slot 8 and
/// PM15-5 slot 9), and
/// `/state`'s for slot 7 before HC11.
pub const UNBUILT_SLOT_NAME: &str = "reserved";

/// Slot names, for boot tells and refusal messages. Index = slot.
/// Derived from `strategy_set::MEMBERS` — the member table's own names,
/// so the two cannot drift — and [`UNBUILT_SLOT_NAME`] for every slot no
/// row names (10–15, S16-R5; 8 is pine's since PVM impl-08 T4, 9 is PM15's
/// since PM15-5).
pub const SLOT_NAMES: [&str; EXEC_SLOTS] = {
    let mut names = [UNBUILT_SLOT_NAME; EXEC_SLOTS];
    let mut i = 0;
    while i < strategy_set::MEMBERS.len() {
        names[strategy_set::MEMBERS[i].slot as usize] = strategy_set::MEMBERS[i].name;
        i += 1;
    }
    names
};

/// **Is `slot`'s bit set in `mask`?** The one bounded slot test every
/// mask reader here uses (S16-3): `false` for a slot at or past
/// [`EXEC_SLOTS`], and otherwise a `SlotMask` shift — never a `u8` one,
/// which in a release build (`overflow-checks = false`) would read slot
/// 11 as slot 3, BIN15's.
#[inline]
#[must_use]
pub const fn slot_in(mask: SlotMask, slot: usize) -> bool {
    slot < EXEC_SLOTS && mask & ((1 as SlotMask) << slot) != 0
}

// `slot_in`'s shift is exact for every slot it lets through.
const _: () = assert!(EXEC_SLOTS <= SlotMask::BITS as usize);

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
    pub live_mask: SlotMask,
    /// The slots the artifact named, as parsed.
    ///
    /// The hot [`ExecRoute`] carries the four clamps and the halt
    /// thresholds; what it does not carry — `request_budget_floor`,
    /// the slot's name, the venue list as written — lives here so the
    /// boot tell can publish every number the operator wrote and the
    /// arm can read the budget floor without re-parsing. Cold; a
    /// `Vec` is fine at boot.
    pub slots: Vec<core_config::exec::ExecSlot>,
    /// **BX6** — `[exec.binance]`, when the artifact has one (a live slot
    /// naming binance cannot boot without it).
    pub binance: Option<core_config::exec::ExecBinance>,
    /// PVM impl-08 §3.6 — `[exec.slot.8.pine]`, when the artifact has one:
    /// `pine_boot::build` reads it from here, never from the file again.
    pub pine8: Option<core_config::pine::PineSlotCfg>,
}

impl ExecBoot {
    /// Is any slot actually armed?
    #[must_use]
    pub fn any_live(&self) -> bool {
        self.live_mask != 0
    }

    /// Is `slot` armed live? [`slot_in`] over [`Self::live_mask`].
    #[must_use]
    pub const fn slot_live(&self, slot: usize) -> bool {
        slot_in(self.live_mask, slot)
    }

    /// HYPARB L5: is slot 0 (its own live arm) armed?
    #[must_use]
    pub fn hyparb_live(&self) -> bool {
        slot_in(self.live_mask, HYPEREVM_SLOT)
    }

    /// PM15-6c: is slot 9 (its own Polymarket arm) armed?
    #[must_use]
    pub fn pm15_live(&self) -> bool {
        slot_in(self.live_mask, PM15_SLOT)
    }

    /// XMM XH4f: does slot `slot` name the gateway transport?
    #[must_use]
    pub fn gateway_slot(&self, slot: usize) -> bool {
        self.slots
            .iter()
            .any(|s| s.slot == slot && s.transport == core_config::exec::TRANSPORT_GATEWAY)
    }

    /// XMM XH4f: slot 6, when it is live on its Hyperliquid gateway arm
    /// (slot 3's own gateway arm is [`Self::bin15_gateway_live`], XH4i-5).
    #[must_use]
    pub fn gateway_live(&self) -> Option<usize> {
        (slot_in(self.live_mask, XMM_SLOT) && self.gateway_slot(XMM_SLOT)).then_some(XMM_SLOT)
    }

    /// **XMM XH4i-5** — is slot 3 live on a gateway arm of its own (the
    /// outcome mode) rather than on the operator's blocking arm?
    #[must_use]
    pub fn bin15_gateway_live(&self) -> bool {
        slot_in(self.live_mask, BIN15_SLOT) && self.gateway_slot(BIN15_SLOT)
    }

    /// **XMM XH5b-3 — slot 6's gateway beside the mainnet-only arms.** Slot
    /// 6's network is the market data's (LAW E-4: its boot holds the
    /// exchange host to it, `xmm_live::check_network`), which the artifact
    /// does not state — so the engine asks this right after [`resolve`],
    /// before any arm boots, and slot 6's boot asks it again. Beside slot 0
    /// or a live Binance slot — arms that trade mainnet only (slot 0's
    /// `MainnetAuthority`, BX-16) — slot 6 runs only when `md_mainnet`: the
    /// market data is the venue's own mainnet host (`xmm_live::md_network`;
    /// a testnet or an unknown host refuses): one engine never mixes a
    /// testnet rehearsal with mainnet orders. Alone, or beside the
    /// operator's arm (XH5a), either network. Its session bound is judged by
    /// its account, which only slot 6's boot knows (XH5b-2,
    /// `xmm_live::check_account_rules`).
    ///
    /// # Errors
    /// Slot 6 live on the gateway, not on mainnet market data, beside a
    /// mainnet-only arm — naming that arm's slot.
    pub fn check_gateway_network(&self, md_mainnet: bool) -> Result<(), String> {
        // XMM XH4i-5: slot 3's own gateway arm too (risk review r1).
        let gw = match (self.gateway_live(), self.bin15_gateway_live()) {
            (Some(s), _) => s,
            (None, true) => BIN15_SLOT,
            (None, false) => return Ok(()),
        };
        if md_mainnet {
            return Ok(());
        }
        let mainnet_only = if self.hyparb_live() {
            Some(HYPEREVM_SLOT)
        } else {
            self.binance_slot()
        };
        match mainnet_only {
            Some(m) => Err(format!(
                "exec: slot {gw} is live on the Hyperliquid gateway on TESTNET (the market \
                 data's network, LAW E-4) in an engine where slot {m} ({}) is live on an arm \
                 that trades mainnet only (slot 0's wallet, or Binance): one engine never mixes \
                 a testnet rehearsal with mainnet orders. Run slot {m} paper, slot {gw} in an \
                 engine of its own, or both on mainnet (XMM XH5b-3, XH4i-5)",
                SLOT_NAMES[m]
            )),
            None => Ok(()),
        }
    }

    /// **The operator Hyperliquid arm's address numbers**, `(floor,
    /// topup_weight, topup_day_max)`, from the live slots that arm trades
    /// — the predicate of [`Self::hl_arm_needed`]: every live slot but
    /// slot 0 (whose address is its own) whose route names hyperliquid.
    /// A slot on another venue has no say in this address's budget (BX3
    /// risk review: from BX6 a Binance-only slot is live beside it).
    ///
    /// * the floor is the HIGHEST any of them asks for (O-BX27): the
    ///   address stops placing at the most conservative floor;
    /// * the top-up weight and day ceiling are the LOWEST non-zero of
    ///   each (S7-L1): the least spending any of them allows. Each slot's
    ///   own ceiling holds at least its own weight (the parser refuses
    ///   less), so the two minima never arm a top-up that cannot fire.
    ///
    /// `0` for a number no such slot states. Cold; boot.
    #[must_use]
    pub fn hl_address_numbers(&self) -> (u64, u64, u64) {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let mut floor = 0i64;
        let mut weight = i64::MAX;
        let mut day_max = i64::MAX;
        for s in &self.slots {
            if s.slot == HYPEREVM_SLOT
                || !slot_in(self.live_mask, s.slot)
                || !self.route.venue_allowed(s.slot as u8, hl)
                || self.gateway_slot(s.slot)
            {
                continue;
            }
            floor = floor.max(s.request_budget_floor);
            if s.request_topup_weight > 0 {
                weight = weight.min(s.request_topup_weight);
            }
            if s.request_topup_day_max > 0 {
                day_max = day_max.min(s.request_topup_day_max);
            }
        }
        let unset_is_zero = |v: i64| {
            if v == i64::MAX {
                0
            } else {
                u64::try_from(v).unwrap_or(0)
            }
        };
        (
            u64::try_from(floor).unwrap_or(0),
            unset_is_zero(weight),
            unset_is_zero(day_max),
        )
    }

    /// **BX6** — the one live slot whose route names Binance (BX-7 as
    /// built: `resolve` refuses a second), which owns every bound row.
    #[must_use]
    pub fn binance_slot(&self) -> Option<usize> {
        let bn = core_types::VenueId::Binance as u8;
        (0..EXEC_SLOTS)
            .find(|&s| slot_in(self.live_mask, s) && self.route.venue_allowed(s as u8, bn))
    }

    /// Does a live slot OTHER than slot 0 trade Hyperliquid on the
    /// blocking transport — i.e. is the operator's Hyperliquid arm
    /// (`HlExchange` on the `HYPERLIQUID_*` account) needed? Slot 0 hedges
    /// from its own account and a gateway slot (XMM XH4f) from its own arm:
    /// neither counts here.
    #[must_use]
    pub fn hl_arm_needed(&self) -> bool {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let mut slot = 0usize;
        while slot < EXEC_SLOTS {
            if slot != HYPEREVM_SLOT
                && slot_in(self.live_mask, slot)
                && self.route.venue_allowed(slot as u8, hl)
                && !self.gateway_slot(slot)
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
pub fn parse_arm_live(spec: &str) -> Result<SlotMask, String> {
    // A `SlotMask` (S16-3): a `u8` shift by 11 sets bit 3 in release.
    let mut mask: SlotMask = 0;
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
        let bit = (1 as SlotMask) << slot;
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
pub fn parse_halt_slots(spec: &str) -> Result<SlotMask, String> {
    // A `SlotMask`, as `parse_arm_live`'s.
    let mut mask: SlotMask = 0;
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
        let bit = (1 as SlotMask) << slot;
        if mask & bit != 0 {
            return Err(format!("--halt-slot: slot {slot} named twice"));
        }
        mask |= bit;
    }
    Ok(mask)
}

/// **Latch `--halt-slot`'s operator halt** on every slot of `halt_mask`
/// the router holds LIVE. Returns `(halted, not_live)`: the slots that
/// latched, and the ones named that did not (not live, or already halted
/// by the halt file — nothing to halt; the caller says so). Bit `s` is slot `s` alone, via [`slot_in`]: a halt
/// of slot 11 or 15 never lands on slot 3 or 7.
pub fn apply_halt_mask<P, L>(
    d: &mut exec_router::RoutedDispatcher<P, L>,
    halt_mask: SlotMask,
) -> (SlotMask, SlotMask)
where
    P: clob_dispatcher::OrderDispatch,
    L: clob_dispatcher::OrderDispatch,
{
    let mut halted: SlotMask = 0;
    let mut not_live: SlotMask = 0;
    for slot in 0..EXEC_SLOTS {
        if !slot_in(halt_mask, slot) {
            continue;
        }
        let bit = (1 as SlotMask) << slot;
        if d.halt_slot(slot, exec_router::HaltReason::Operator) {
            halted |= bit;
        } else {
            not_live |= bit;
        }
    }
    (halted, not_live)
}

/// Render a slot bitmask as `[3]` / `[0,1,2]` / `[]`.
#[must_use]
pub fn render_slot_mask(mask: SlotMask) -> String {
    let mut out = String::from("[");
    let mut first = true;
    for s in 0..EXEC_SLOTS {
        if slot_in(mask, s) {
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
    // PVM ruling 2026-10-16: slot 8 (pine) is PAPER ONLY. Refused FIRST —
    // from the artifact or the flag, whatever the other switch says — so
    // `--arm-live 8` can never reach the interlock's comparison. (The
    // parser already refuses a live `[exec.slot.8]`; this is the flag's
    // half, and the artifact's again.)
    let paper_only = (artifact_mask | flag_mask) & PAPER_ONLY_MASK;
    if paper_only != 0 {
        return Err(format!(
            "exec: slot {} is PAPER ONLY by ruling (2026-10-16): the Pine member \
             (third-party scripts) is refused in every live arm. ({} marks {} live, \
             --arm-live names {}.) Arming it is its own live-arming ruling.",
            render_slot_mask(paper_only),
            path.display(),
            render_slot_mask(artifact_mask),
            render_slot_mask(flag_mask),
        ));
    }
    // S16-R5: a slot with no member in THIS binary is never armed — not
    // from the artifact, not from the flag. Nothing could trade there,
    // and an order from such a slot is the one an older binary reads as
    // Foreign (the rollback door S16-R5 keeps shut until a member, and
    // its own ruling, arrive). A BUILT slot the `STRATEGY` set leaves
    // disabled is still armable, as before S16: its member is compiled,
    // and enabling it is the strategy set's call, not this artifact's.
    let unbuilt = (artifact_mask | flag_mask) & !strategy_set::BUILT_MASK;
    if unbuilt != 0 {
        return Err(format!(
            "exec: slots {} have no member in THIS binary (S16-R5: slots 9–15 are \
             reserved) — refusing to arm a slot nothing trades from. ({} marks {} live, \
             --arm-live names {}.) A member there is its own lane and its own ruling.",
            render_slot_mask(unbuilt),
            path.display(),
            render_slot_mask(artifact_mask),
            render_slot_mask(flag_mask),
        ));
    }
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

        // XMM XH4f: slot 6 is live only on its own gateway arm, and the
        // gateway is slot 6's alone ([`XMM_SLOT`]).
        let gateway = s.transport == core_config::exec::TRANSPORT_GATEWAY;
        if mode == ExecMode::Live && slot == XMM_SLOT && !gateway {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) is marked live on the blocking transport; slot \
                 {slot} trades only through its own arm, the Hyperliquid gateway — write \
                 `transport = \"gateway\"` (XMM XH4f), never slot 3's account, where its halt \
                 would cancel bin15's quotes"
            ));
        }
        // XMM XH4h-3: slot 0's hedges run on the gateway in the HYPARB
        // rehearsal (testnet) only — the engine's slot 0 is mainnet-only and
        // the gateway testnet-only until XH5. XMM XH4i-5: slot 3 runs on a
        // gateway arm of its own, the outcome mode.
        if gateway && slot != XMM_SLOT && slot != BIN15_SLOT {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) names `transport = \"gateway\"`, which is slot \
                 {XMM_SLOT}'s and slot {BIN15_SLOT}'s alone in an engine: slot 0's hedges run on \
                 the gateway only in the HYPARB rehearsal on testnet (`evm-live arm-smoke \
                 --gateway`, XMM XH4h-3)"
            ));
        }
        // XMM XH4i-5: the perp pair is a gateway slot's role's (the parser
        // takes either): slot 6 quotes perps — live, it states the pair;
        // slot 3 trades HIP-4 legs — never, for a perp slot's every order
        // must be on a linear row (`PerpLimits`).
        if mode == ExecMode::Live && gateway && slot == XMM_SLOT && s.cap_perp_usd_1e6 == 0 {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) is live on the gateway without its perp pair — \
                 write a non-zero `cap_perp_usd_1e6` and `max_resting_usd_1e6` (`0` means UNSET \
                 here, never \"unlimited\"; XMM XH4f)"
            ));
        }
        if gateway && slot == BIN15_SLOT && s.cap_perp_usd_1e6 != 0 {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) on the gateway trades HIP-4 outcome legs: \
                 `cap_perp_usd_1e6` and `max_resting_usd_1e6` would make it a perp slot, whose \
                 every order must be on a linear row — drop them (XMM XH4i-5)"
            ));
        }

        // PVM ruling 2026-10-16: slot 8 is paper only ([`PINE_SLOT`]) —
        // unreachable behind the parser and the mask refusal above, kept so
        // the table build fails closed on its own.
        if mode == ExecMode::Live && slot == PINE_SLOT {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) is marked live, but slot {slot} is PAPER ONLY \
                 by ruling (2026-10-16). Refusing the boot."
            ));
        }

        // HC11 (O-HC18): slot 7 is paper only until its own ruling
        // ([`HCV_SLOT`]).
        if mode == ExecMode::Live && slot == HCV_SLOT {
            return Err(format!(
                "exec: slot {slot} ({slot_name}) is marked live, but slot {slot} is DARK — paper \
                 only by ruling O-HC18. Arming it is its own live-arming ruling, which must \
                 settle the E6 ledger's options row, its HL hedges' reconciled account and the \
                 two-venue composition. Refusing the boot."
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
            // PM15-6c: Polymarket is slot 9's alone, and slot 9 is live on
            // it alone.
            let pm = s.venues.contains(&(core_types::VenueId::Polymarket as u8));
            if slot == PM15_SLOT && !(pm && s.venues.len() == 1) {
                return Err(format!(
                    "exec: slot {slot} ({slot_name}) is marked live with venues {:?}; it is live \
                     on exactly [\"polymarket\"] or not at all (PM15-6c)",
                    s.venues
                ));
            }
            if slot != PM15_SLOT && pm {
                return Err(format!(
                    "exec: slot {slot} ({slot_name}) is marked live for `polymarket`, whose only \
                     live arm is slot 9's (pm15) — refusing (the legacy `--live` dispatcher is \
                     not an exec arm)"
                ));
            }
        }

        // A live slot needs a compiled arm for EVERY venue it names.
        // Refuse loudly and name the phase that supplies it — never
        // boot inert, never downgrade to paper.
        if mode == ExecMode::Live {
            for v in &s.venues {
                // HC9 (O-HC19): the Hypercall arm is BUILT — the
                // `hypercall-live` verbs drive it on mainnet — but the
                // engine does not arm it before slot 7's live-arming
                // ruling (plan hc9-hc11 §1). Named on its own so the
                // refusal says what that ruling has to settle.
                if *v == core_types::VenueId::Hypercall as u8 {
                    return Err(format!(
                        "exec: slot {slot} ({slot_name}) is marked live for venue `hypercall`. \
                         The Hypercall arm (HC9) is built, but the engine does not arm it \
                         before slot 7's live-arming ruling, which must settle three things: \
                         the E6 ledger has no options row (a short option would pass \
                         cap_instance unseen), slot 7's HL hedges need an account whose perps \
                         are reconciled, and the two-venue composition lands with it. The \
                         arm's mainnet proof is scripts/hypercall-live.sh. Refusing the boot."
                    ));
                }
                if !LIVE_ARM_VENUES.contains(v) {
                    let vname = core_config::exec::venue_name_from_id(*v).unwrap_or("?");
                    return Err(format!(
                        "exec: slot {slot} ({slot_name}) is marked live for venue `{vname}`, \
                         but this binary has NO live execution arm for it (the arms compiled \
                         in are hyperliquid, binance (USDⓈ-M, classic), hyperevm for slot 0 \
                         and polymarket for slot 9). Slot {slot} can only be \"paper\" or \
                         \"off\" on that venue. \
                         Refusing the boot rather than trading it on paper under a live label."
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
                .with_pnl_bound(s.halt_on_gain_usd_1e6, s.halt_on_loss_usd_1e6)
                // XMM XH4f (XH-7): the gateway slot's ALO halt, percent
                // → permille (the parser bounds it at 100); 0 = off.
                .with_alo_reject_permille(
                    u16::try_from(s.halt_on_alo_reject_pct * 10).unwrap_or(u16::MAX),
                ),
            )
            .map_err(|e| format!("exec: slot {slot}: {e}"))?;
        // XMM XH4f (deviation 19): the perp slot's pair — none elsewhere.
        route
            .set_perp_limits(
                slot,
                PerpLimits::new(s.cap_perp_usd_1e6, s.max_resting_usd_1e6),
            )
            .map_err(|e| format!("exec: slot {slot}: {e}"))?;
        // Keep every parsed number, not just the EIGHT the hot table
        // holds — the boot tell publishes all of them, and E4's
        // budget governor reads `request_budget_floor` from here.
        slots.push(s);
    }
    check_binance(&slots, artifact_mask, file.binance.as_ref())?;
    check_pnl_judged(&slots, artifact_mask)?;
    check_bin15_gateway(&slots, artifact_mask)?;

    let boot = ExecBoot {
        route,
        hash,
        path,
        enabled: file.enabled,
        live_mask: artifact_mask,
        slots,
        binance: file.binance,
        pine8: file.pine8,
    };
    Ok(Some(boot))
}

/// **XMM XH4i-5 — slot 3 on a gateway arm of its own** trades the
/// operator's account (`HYPERLIQUID_*`), the one the blocking arm trades:
/// one master, one arm. Refused beside a live slot the blocking arm would
/// trade (its sweeps and its reconciliation reach every order and fill on
/// the master: slot 3's quotes cancelled, its fills booked twice) and beside
/// slot 6 live on its gateway (A-S7-85: one gateway arm an engine until the
/// arm's report is per slot and the two gateways split the rate shares).
fn check_bin15_gateway(
    slots: &[core_config::exec::ExecSlot],
    live_mask: SlotMask,
) -> Result<(), String> {
    let live_gw = |slot: usize| {
        slot_in(live_mask, slot)
            && slots
                .iter()
                .any(|s| s.slot == slot && s.transport == core_config::exec::TRANSPORT_GATEWAY)
    };
    if !live_gw(BIN15_SLOT) {
        return Ok(());
    }
    if live_gw(XMM_SLOT) {
        return Err(format!(
            "exec: slot {BIN15_SLOT} (bin15) and slot {XMM_SLOT} (xmm) are both live on the \
             Hyperliquid gateway — one gateway arm an engine until the arm's report is per slot \
             and the two gateways split the rate shares (XMM XH4i-5, A-S7-85): run one of them \
             paper, or slot 3 on the blocking transport"
        ));
    }
    let hl = core_types::VenueId::Hyperliquid as u8;
    for s in slots {
        if s.slot != HYPEREVM_SLOT
            && s.slot != BIN15_SLOT
            && slot_in(live_mask, s.slot)
            && s.venues.contains(&hl)
            && s.transport != core_config::exec::TRANSPORT_GATEWAY
        {
            return Err(format!(
                "exec: slot {BIN15_SLOT} (bin15) is live on a gateway arm of its own, on the \
                 operator's account, and slot {} ({}) on the operator's blocking arm — the same \
                 master: the blocking arm's sweeps would cancel slot 3's quotes and its \
                 reconciliation book slot 3's fills twice. One master, one arm: run slot {} \
                 paper, or slot 3 on the blocking transport (XMM XH4i-5)",
                s.slot, SLOT_NAMES[s.slot], s.slot
            ));
        }
    }
    Ok(())
}

/// **BX6** — what this binary's Binance arm can serve: at most one live
/// slot (BX-7 as built — the one owner of every bound row), and only the
/// products and account modes its gateway speaks (BX7: USDⓈ-M and COIN-M ×
/// classic, Portfolio Margin and PM Pro — `exec_binance::mode::built`);
/// anything else is refused naming the phase that builds it (BX8: spot
/// classic; BX9: options classic). A session bound on a classic arm needs
/// it to trade USDⓈ-M and/or options alone (their USD equities, summed:
/// BX9, Q-4).
fn check_binance(
    slots: &[core_config::exec::ExecSlot],
    live_mask: SlotMask,
    section: Option<&core_config::exec::ExecBinance>,
) -> Result<(), String> {
    let bn = core_types::VenueId::Binance as u8;
    let live: Vec<usize> = slots
        .iter()
        .filter(|s| slot_in(live_mask, s.slot) && s.venues.contains(&bn))
        .map(|s| s.slot)
        .collect();
    if live.is_empty() {
        return Ok(());
    }
    if live.len() > 1 {
        return Err(format!(
            "exec: slots {live:?} are all marked live on binance; this binary's Binance arm has \
             ONE owner slot (BX-7 as built: the owner books every venue-originated fill and \
             holds every bound row). Arm one of them."
        ));
    }
    let Some(b) = section else {
        return Err(String::from("exec: a live binance slot needs an [exec.binance] section"));
    };
    if b.account_scope != "dedicated" {
        return Err(format!(
            "exec: [exec.binance] account_scope `{}` is not armable in this binary: on a shared \
             account a foreign fill on an owned instrument is only counted (O-BX2a wants a halt) \
             and the sweep's REST fallback cancels every order on a symbol. Use a dedicated \
             account.",
            b.account_scope
        ));
    }
    // One failed reconciliation must not read as a stale one.
    let owner = slots.iter().find(|s| s.slot == live[0]);
    let stale_ms = owner.map_or(0, |s| s.halt_on_recon_stale_ms);
    if stale_ms != 0 && b.recon_every_ms.saturating_mul(2) > stale_ms {
        return Err(format!(
            "exec: [exec.binance] recon_every_ms = {} is more than half of slot {}'s \
             halt_on_recon_stale_ms = {stale_ms} — one late cycle would halt the slot as stale",
            b.recon_every_ms, live[0]
        ));
    }
    // The REST weight (§3.9): the worst steady load — the dead-man on every
    // symbol, each futures lane's reconciliation and margin re-reads at
    // their fastest — is judged here against the arm's share of the IP's
    // budget; at run time the gateway's governor (BX7) defers its
    // discretionary reads past 80 % of that share. A futures-less arm is
    // judged as one futures lane, as before BX9 (spot's: a lane it does not
    // build — carried) — but an arm of options alone weighs nothing here:
    // eapi's pool is its own, judged below (BX9).
    let max_symbols = owner.map_or(0, |s| s.max_symbols);
    let lanes = b.products.iter().filter(|p| *p == "usdm" || *p == "coinm").count();
    let options_alone = lanes == 0 && b.products.iter().all(|p| p == "options");
    let weight = exec_binance::gov::arm_weight_per_min(
        u64::try_from(max_symbols).unwrap_or(0),
        u64::try_from(b.heartbeat_ms).unwrap_or(0),
        u64::try_from(b.recon_every_ms).unwrap_or(0),
        u64::try_from(lanes.max(1)).unwrap_or(1),
    );
    if !options_alone && weight > exec_binance::gov::ARM_WEIGHT_MAX_PER_MIN {
        return Err(format!(
            "exec: [exec.binance] heartbeat_ms = {} with slot {}'s max_symbols = {max_symbols} and \
             recon_every_ms = {} weighs up to {weight} per minute — over the arm's {} of the IP's \
             {} (each symbol's dead-man costs {} a heartbeat). Raise heartbeat_ms or lower \
             max_symbols.",
            b.heartbeat_ms,
            live[0],
            b.recon_every_ms,
            exec_binance::gov::ARM_WEIGHT_MAX_PER_MIN,
            exec_binance::gov::IP_WEIGHT_PER_MIN,
            exec_binance::gov::W_COUNTDOWN
        ));
    }
    // BX9: eapi's REQUEST_WEIGHT is its own pool (400 a minute per IP): the
    // options lane's worst steady load — a cycle every recon_every_ms with
    // its exercise read, the margin re-reads at their fastest, the clock
    // when options are the only lane — and a whole cycle kept in hand must
    // leave the day read (paced at run time, Q-5) some of the arm's share:
    // else the boot could never end (R6).
    let opt_armed = b.products.iter().any(|p| p == "options");
    if opt_armed {
        let only = b.products.iter().all(|p| p == "options");
        let re = u64::try_from(b.recon_every_ms).unwrap_or(0);
        if exec_binance::gov::opt_day_row_ns(re, only).is_none() {
            return Err(format!(
                "exec: [exec.binance] recon_every_ms = {} weighs the options lane up to {} per minute \
                 on eapi — with a cycle ({}) kept in hand that leaves the boot's day read nothing of \
                 the arm's {} (the IP's {}). Raise recon_every_ms.",
                b.recon_every_ms,
                exec_binance::gov::opt_weight_per_min(re, only),
                exec_binance::gov::W_OPT_RECON,
                exec_binance::gov::OPT_ARM_WEIGHT_MAX_PER_MIN,
                exec_binance::gov::OPT_IP_WEIGHT_PER_MIN
            ));
        }
    }
    let mode = exec_binance::mode::AccountMode::from_word(&b.account_mode)
        .ok_or_else(|| format!("exec: [exec.binance] account_mode `{}` is unknown", b.account_mode))?;
    for word in &b.products {
        let product = exec_binance::inst::PRODUCT_WORDS
            .iter()
            .position(|w| w == word)
            .and_then(|i| u8::try_from(i).ok())
            .ok_or_else(|| format!("exec: [exec.binance] product `{word}` is unknown"))?;
        if let Err(phase) = exec_binance::mode::built(product, mode) {
            return Err(format!(
                "exec: [exec.binance] arms `{word}` in account mode `{}`, which this binary's \
                 Binance gateway does not speak yet — it arrives with {phase}. Refusing the boot \
                 rather than trading a product the arm cannot sweep or reconcile.",
                b.account_mode
            ));
        }
        // BX10 (ruling S-3): Binance Stocks is built and battery-tested,
        // never armed by the engine — no member exists to trade it (O-BX16)
        // until R17's ruling.
        if product == exec_binance::inst::PRODUCT_EQUITY {
            return Err(String::from(
                "exec: [exec.binance] arms `equity` — Binance Stocks is the operator's battery only \
                 (ruling S-3: `exec-smoke --binance equity` carries the D5 keys); no member trades it \
                 (O-BX16) until R17's ruling. Refusing the boot.",
            ));
        }
    }
    // BX7: the E7 session bound is judged on ONE USD equity — USDⓈ-M's
    // margin balance on a classic account, the unified account's in PM and
    // PM Pro. COIN-M margins in each coin and has none: on a classic arm
    // trading it the bound would judge USDⓈ-M's equity alone — or nothing —
    // while COIN-M's losses went unseen (the risk review's N1). Refused.
    // BX8: nor does the spot wallet — spot is classic-only, and a bounded
    // arm trading it would judge USDⓈ-M's equity while spot's losses went
    // unseen. Refused alike. BX9 (Q-4): the options account HAS a USD
    // equity (`marginAccount`'s): the session equity is USDⓈ-M's margin
    // balance plus it — a bounded arm may trade USDⓈ-M, options or both.
    let bounded = owner.is_some_and(|s| s.halt_on_gain_usd_1e6 != 0 || s.halt_on_loss_usd_1e6 != 0);
    let armed = |w: &str| b.products.iter().any(|p| p == w);
    // BX8: spot's user data exists only on a WS API session, whose logon
    // takes Ed25519 keys only — refused here by name, as the gateway would
    // at its boot (`BootErr::SpotRoute`).
    if armed("spot") && (b.order_transport == "rest" || b.signing == "hmac") {
        return Err(format!(
            "exec: [exec.binance] arms spot with order_transport = \"{}\" and signing = \"{}\" — \
             spot's fills arrive only on a WS API session (no listenKey since 2026-02-20), whose \
             logon takes an Ed25519 key. Arm spot on the WS API with an Ed25519 key.",
            b.order_transport, b.signing
        ));
    }
    // BX9 (the risk review's R2-5): options are proven alone and beside
    // USDⓈ-M (the lane order, the two signers, the summed equity); beside
    // COIN-M or spot nothing has run them — refused until a test does.
    if armed("options") && (armed("coinm") || armed("spot")) {
        return Err(String::from(
            "exec: [exec.binance] arms options beside COIN-M or spot — BX9 proves options alone \
             and beside USDⓈ-M only. Arm options alone, or with usdm.",
        ));
    }
    if bounded && mode == exec_binance::mode::AccountMode::Classic && (armed("coinm") || armed("spot") || !(armed("usdm") || armed("options"))) {
        let what = if armed("coinm") {
            "trades COIN-M"
        } else if armed("spot") {
            "trades spot"
        } else {
            "trades neither USDⓈ-M nor options"
        };
        return Err(format!(
            "exec: slot {} has a session bound (halt_on_gain_usd_1e6 / halt_on_loss_usd_1e6) but \
             its classic Binance arm {what} — COIN-M margins in each coin and the spot wallet \
             holds its own assets: no USD equity the bound reads for either, so the bound would \
             judge USDⓈ-M's alone (or nothing) while their losses went unseen. Run the futures \
             as pm or pm_pro (one unified USD equity), arm usdm and/or options alone, or drop the \
             bound.",
            live[0]
        ));
    }
    Ok(())
}


/// **BX6 (obligation 5)** — a live slot with a session bound, traded by
/// two arms, needs both to judge it ([`PNL_JUDGING_VENUES`]); otherwise
/// the merged signal never reports it judged and the bound never fires.
fn check_pnl_judged(
    slots: &[core_config::exec::ExecSlot],
    live_mask: SlotMask,
) -> Result<(), String> {
    let bn = core_types::VenueId::Binance as u8;
    for s in slots {
        let bounded = s.halt_on_gain_usd_1e6 != 0 || s.halt_on_loss_usd_1e6 != 0;
        let two_arms = s.venues.contains(&bn) && s.venues.iter().any(|v| *v != bn);
        if !slot_in(live_mask, s.slot) || !bounded || !two_arms {
            continue;
        }
        if let Some(v) = s.venues.iter().find(|v| !PNL_JUDGING_VENUES.contains(v)) {
            return Err(format!(
                "exec: slot {} has a session bound and is traded by two arms, but venue `{}`'s \
                 arm never judges one — the bound could never fire. Drop the bound or the venue.",
                s.slot,
                core_config::exec::venue_name_from_id(*v).unwrap_or("?")
            ));
        }
    }
    Ok(())
}

/// **BX3 — the boot-fixed route aliases** (plan §3.2 item 4, §13.2 F10).
///
/// Members stamp `Order::venue` from the symbol's venue byte, and
/// Binance spot\[0\] carries the M1 legacy FLAT id
/// (`core_config::universe::LEGACY_BN_ANCHOR_SYM`, 7 — venue byte 0,
/// Polymarket) or whatever `--binance-sym-id` gave it. Unaliased, its
/// orders route as that byte, and a live slot whose mask names Binance
/// refuses them `NoLiveRoute`. The alias is the one correction, by full
/// `SymbolId`, read by the router's `venue_allowed` and by
/// `exec_router::VenueSplit` only — the order bytes, `ExecRoute`, the
/// ledger, captures and the backtest model never see it.
///
/// `bn_anchor` is the allocated spot\[0\] id; `None` (no Binance spot in
/// this universe) and a namespaced Binance id (its byte already routes
/// it) both give the empty table.
///
/// # Errors
/// The table refused the entry (`SYMBOL_ID_NONE` as the anchor). Fatal:
/// a boot without its alias would route the anchor wrongly, silently.
pub fn route_aliases(
    bn_anchor: Option<core_types::SymbolId>,
) -> Result<clob_dispatcher::RouteAliases, String> {
    let bn = core_types::VenueId::Binance as u8;
    match bn_anchor {
        Some(sym) if core_types::symbol_venue_byte(sym) != bn => {
            clob_dispatcher::RouteAliases::NONE
                .with(sym, bn)
                .map_err(|e| format!("exec: the Binance anchor {sym} cannot be aliased: {e:?}"))
        }
        _ => Ok(clob_dispatcher::RouteAliases::NONE),
    }
}

/// **BX6 (obligation 7)** — the route alias sends every order on its id
/// to Binance, so no instrument of the universe but the anchor itself
/// (Binance spot\[0\]) may carry that id: another venue's market under
/// it would be traded on Binance. `SYMBOL_ID_NONE` (no alias) passes.
///
/// # Errors
/// The id is carried by another instrument; the message names it.
pub fn check_alias_unique(
    alias: core_types::SymbolId,
    u: &core_config::universe::AllocatedUniverse,
) -> Result<(), String> {
    if alias == core_types::SYMBOL_ID_NONE {
        return Ok(());
    }
    let refuse = |what: &str| {
        Err(format!(
            "exec: the Binance route alias {alias} is also the id of {what} — its orders would \
             be sent to Binance (obligation 7). Give the Binance anchor its own id."
        ))
    };
    if let Some(t) = u.pm_tokens.iter().find(|t| t.sym == alias) {
        return refuse(&format!("polymarket token {}", t.token_id));
    }
    let lists: [&[core_config::universe::Instrument]; 14] = [
        &u.bn_usdm,
        &u.bn_dated,
        &u.bn_coinm,
        &u.bn_coinm_dated,
        &u.okx,
        &u.deribit,
        &u.deribit_combos,
        &u.hl,
        &u.bybit_spot,
        &u.bybit_linear,
        &u.mexc_spot,
        &u.mexc_perp,
        &u.hyperevm,
        u.bn_spot.get(1..).unwrap_or(&[]),
    ];
    for list in lists {
        if let Some(i) = list.iter().find(|i| i.sym == alias) {
            return refuse(&i.descriptor);
        }
    }
    Ok(())
}

/// `3 (bin15)` / `3 (bin15), 5 (vm)` — for the interlock's message.
fn describe_slots(mask: SlotMask) -> String {
    let mut out = String::new();
    for (s, name) in SLOT_NAMES.iter().enumerate() {
        if slot_in(mask, s) {
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
    let mut paper: SlotMask = 0;
    for s in 0..EXEC_SLOTS {
        if boot.route.mode_at(s) == Some(ExecMode::Paper) {
            paper |= (1 as SlotMask) << s;
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
        // XMM XH4f: a gateway slot's transport, perp pair and ALO halt —
        // only on its lines, so every other slot's lines are unchanged.
        let gw = boot.gateway_slot(slot);
        let perp = if gw {
            format!(
                " transport=gateway perp=${} resting=${}",
                s.map_or(0, |s| s.cap_perp_usd_1e6) / 1_000_000,
                s.map_or(0, |s| s.max_resting_usd_1e6) / 1_000_000,
            )
        } else {
            String::new()
        };
        let alo = if gw {
            format!(
                " alo_reject_pct={}",
                s.map_or(0, |s| s.halt_on_alo_reject_pct)
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "exec: slot {slot} LIVE name={slot_name} venue={venues} \
             caps-ENFORCED order=${} open={} day=${day} instance=${inst}{perp} \
             (the risk gate clamps every live submit and modify; \
             worst case with every quote working = instance + open x order)",
            boot.route.max_order_usd_1e6_at(slot).unwrap_or(0) / 1_000_000,
            boot.route.max_open_orders_at(slot).unwrap_or(0),
        ));
        let gain = s.map_or(0, |s| s.halt_on_gain_usd_1e6) / 1_000_000;
        let loss = s.map_or(0, |s| s.halt_on_loss_usd_1e6) / 1_000_000;
        // BX3: each venue's own numbers, only where the slot names the
        // venue — the Hyperliquid address budget where hyperliquid is
        // named (the arm reads it only from those slots), the margin halt
        // where binance is (the key is refused anywhere else). A
        // Hyperliquid slot's line is therefore unchanged.
        let hl_budget = if vmask & (1u16 << core_types::VenueId::Hyperliquid as u8) != 0 {
            format!(
                " budget_floor={} topup={}/{}",
                s.map_or(0, |s| s.request_budget_floor),
                s.map_or(0, |s| s.request_topup_weight),
                s.map_or(0, |s| s.request_topup_day_max),
            )
        } else {
            String::new()
        };
        let margin = if vmask & (1u16 << core_types::VenueId::Binance as u8) != 0 {
            format!(
                " margin_ratio_1e6={}",
                s.map_or(0, |s| s.halt_on_margin_ratio_1e6)
            )
        } else {
            String::new()
        };
        lines.push(format!(
            "exec: slot {slot} HALTS reject_streak={} asset_refusals={} recon_drift=${drift} \
             recon_stale_ms={} ws_gap_ms={}{hl_budget}{alo} \
             session_bound=+${gain}/-${loss}{margin} halt_file={}",
            s.map_or(0, |s| s.halt_on_reject_streak),
            s.map_or(0, |s| s.halt_on_asset_refusal_streak),
            s.map_or(0, |s| s.halt_on_recon_stale_ms),
            s.map_or(0, |s| s.halt_on_ws_gap_ms),
            halt_file_path(&boot.path).display(),
        ));
    }
    // BX6: the Binance arm's own knobs, on the one slot that owns it.
    if let (Some(slot), Some(b)) = (boot.binance_slot(), boot.binance.as_ref()) {
        let s = boot.slots.iter().find(|s| s.slot == slot);
        lines.push(format!(
            "exec: binance arm owner=slot {slot} network={} account={}/{} products=[{}] \
             stp={} recv_window_ms={} countdown_ms={} heartbeat_ms={} recon_every_ms={} \
             orders_frac_1e6={} qtr_frac_1e6={} max_symbols={} min_maker_ttl_ms={} spin={}",
            b.network,
            b.account_mode,
            b.account_scope,
            b.products.join(","),
            b.stp_mode,
            b.recv_window_ms,
            b.countdown_ms,
            b.heartbeat_ms,
            b.recon_every_ms,
            b.orders_frac_1e6,
            b.qtr_frac_1e6,
            s.map_or(0, |s| s.max_symbols),
            s.map_or(0, |s| s.min_maker_ttl_ms),
            u8::from(b.spin),
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

// ---- B'4 — the Hyperliquid perp boot sequence (impl-Bp §4.13) -----------
//
// Boot only (allocates). NOTHING ARMS here: these build and judge the
// perp half of a boot that a live slot routing a `hyperliquid:<COIN>` perp
// would run; wiring it into the binary, and arming, are the operator's.

/// One perp coin a live slot routes (impl-08's route supplies these).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlPerpRoute<'a> {
    /// Engine `SymbolId`.
    pub sym: u32,
    /// The venue's coin spelling, e.g. `"BTC"`.
    pub coin: &'a str,
    /// The slot that owns the coin (the only one that may trade it).
    pub slot: u8,
}

/// The probe artifact (`hl-perp-limits.tsv`, §4.13) as `(coin,
/// min_notional_1e6)` rows.
///
/// pin:bp4-limits-tsv — the spec names the file, not its layout: `#`
/// comments and blank lines skipped; the first other line is a TAB header
/// that must name `coin` and `min_notional_1e6` (other columns ignored,
/// for T27 to add); each row's minimum is a positive integer, USD ×1e6; a
/// coin listed twice is refused. T27's probe writes this layout.
///
/// # Errors
/// A missing header or column, a malformed or non-positive minimum, a
/// duplicate coin.
pub fn parse_hl_perp_limits(src: &str) -> Result<Vec<(String, i64)>, String> {
    let mut cols: Option<(usize, usize, usize)> = None;
    let mut out: Vec<(String, i64)> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let l = raw.trim_end_matches('\r');
        if l.trim().is_empty() || l.trim_start().starts_with('#') {
            continue;
        }
        let f: Vec<&str> = l.split('\t').collect();
        let Some((c_coin, c_min, width)) = cols else {
            let coin = f.iter().position(|h| h.trim() == "coin");
            let min = f.iter().position(|h| h.trim() == "min_notional_1e6");
            match (coin, min) {
                (Some(c), Some(m)) => cols = Some((c, m, f.len())),
                _ => {
                    return Err(format!(
                        "hl-perp-limits line {ln}: the header must name `coin` and \
                         `min_notional_1e6` (TAB-separated)"
                    ))
                }
            }
            continue;
        };
        if f.len() != width {
            return Err(format!(
                "hl-perp-limits line {ln}: {} columns, the header has {width}",
                f.len()
            ));
        }
        let coin = f[c_coin].trim();
        if coin.is_empty() {
            return Err(format!("hl-perp-limits line {ln}: empty coin"));
        }
        let min: i64 = f[c_min].trim().parse().map_err(|_| {
            format!(
                "hl-perp-limits line {ln}: `{}` is not an integer USD ×1e6",
                f[c_min].trim()
            )
        })?;
        if min <= 0 {
            return Err(format!(
                "hl-perp-limits line {ln}: minimum {min} must be positive"
            ));
        }
        if out.iter().any(|(c, _)| c == coin) {
            return Err(format!(
                "hl-perp-limits line {ln}: coin {coin} listed twice"
            ));
        }
        out.push((coin.to_owned(), min));
    }
    if cols.is_none() {
        return Err("hl-perp-limits: no header (the probe file is empty)".to_owned());
    }
    Ok(out)
}

/// §4.13 step 1 and the probe gate: the wants a perp boot binds. Empty
/// `routes` → no wants and no requirement. Refused: `[exec.hl_perp]`
/// absent while a live slot routes a perp, `isolated ≠ 1`, a leverage of
/// 0 or above `u16`, a coin absent from the probe file (it cannot arm).
/// Duplicates, a delisted or absent coin and a leverage above
/// `maxLeverage` are refused by the binding itself (step 2).
///
/// # Errors
/// The refusal, as the boot prints it.
pub fn hl_perp_wants<'a>(
    cfg: Option<&core_config::exec::ExecHlPerp>,
    routes: &[HlPerpRoute<'a>],
    limits: &[(String, i64)],
) -> Result<Vec<exec_hyperliquid::perp::PerpWant<'a>>, String> {
    if routes.is_empty() {
        return Ok(Vec::new());
    }
    let Some(cfg) = cfg else {
        return Err(
            "exec.toml: a live slot routes a Hyperliquid perp — `[exec.hl_perp]` \
                    (isolated, leverage, limits) is required"
                .to_owned(),
        );
    };
    if !cfg.isolated {
        return Err(format!(
            "[exec.hl_perp] line {}: isolated must be 1 (isolated margin per coin)",
            cfg.line
        ));
    }
    if cfg.leverage == 0 {
        return Err(format!(
            "[exec.hl_perp] line {}: leverage 0 cannot arm",
            cfg.line
        ));
    }
    let mut out = Vec::with_capacity(routes.len());
    for r in routes {
        let Some((_, min)) = limits.iter().find(|(c, _)| c == r.coin) else {
            return Err(format!(
                "perp {}: absent from the probe file ({}) — a coin the probe has not \
                 measured cannot arm",
                r.coin, cfg.limits
            ));
        };
        out.push(exec_hyperliquid::perp::PerpWant {
            sym: r.sym,
            coin: r.coin.as_bytes(),
            leverage: cfg.leverage,
            min_notional_1e6: *min,
            owner_slot: r.slot,
        });
    }
    Ok(out)
}

/// A perp boot refusal as the boot prints it.
#[must_use]
pub fn render_perp_boot_error(e: exec_hyperliquid::perp::PerpBootError) -> String {
    use exec_hyperliquid::perp::{PerpBootError, PerpError};
    match e {
        PerpBootError::Bind(b) => {
            let why = match b {
                PerpError::NotInMeta => "a coin is absent from this boot's `meta` answer",
                PerpError::Delisted => "a coin is delisted in this boot's `meta` answer",
                PerpError::LeverageAboveMax => {
                    "[exec.hl_perp] leverage is above a coin's maxLeverage"
                }
                PerpError::LeverageZero => "[exec.hl_perp] leverage 0",
                PerpError::Duplicate => "a perp coin or symbol is routed twice",
                PerpError::TableFull => "more perp coins than the table holds",
                PerpError::MalformedMeta => "the `meta` answer is malformed",
            };
            format!("hyperliquid perp boot refused: {why}")
        }
        PerpBootError::Leverage { asset } => format!(
            "hyperliquid perp boot refused: updateLeverage (isolated) for asset {asset} was not \
             confirmed"
        ),
    }
}

/// impl-Bp §4.13 steps 1–4, the venue I/O injected: the wants (step 1 and
/// the probe gate), then `bind_and_lever` — in the binary
/// `HlExchange::boot_perps` over this boot's `{"type":"meta"}` answer —
/// which binds (step 2), tells once per coin (step 3, pushed onto the
/// returned lines) and sets the isolated leverage once per coin (step 4).
///
/// # Errors
/// The first refusal, as the boot prints it.
pub fn boot_hl_perps<B>(
    cfg: Option<&core_config::exec::ExecHlPerp>,
    routes: &[HlPerpRoute<'_>],
    limits_src: Option<&str>,
    bind_and_lever: B,
) -> Result<Vec<String>, String>
where
    B: FnOnce(
        &[exec_hyperliquid::perp::PerpWant<'_>],
        &mut Vec<String>,
    ) -> Result<(), exec_hyperliquid::perp::PerpBootError>,
{
    if routes.is_empty() {
        return Ok(Vec::new());
    }
    let limits = match limits_src {
        Some(s) => parse_hl_perp_limits(s)?,
        None => {
            return Err(format!(
                "hyperliquid perp boot refused: the probe file{} could not be read — no coin \
                 can arm without it",
                cfg.map(|c| format!(" {}", c.limits)).unwrap_or_default()
            ))
        }
    };
    let wants = hl_perp_wants(cfg, routes, &limits)?;
    let mut tells: Vec<String> = Vec::with_capacity(wants.len());
    bind_and_lever(&wants, &mut tells).map_err(render_perp_boot_error)?;
    Ok(tells)
}

#[cfg(test)]
mod hl_perp_boot_tests {
    use super::*;
    use exec_hyperliquid::perp::{boot_bind_and_lever, boot_tell, PerpTable, PerpWant};

    const META: &[u8] = br#"{"universe":[{"szDecimals":5,"name":"BTC","maxLeverage":40},{"szDecimals":4,"name":"ETH","maxLeverage":25},{"szDecimals":1,"name":"MATIC","maxLeverage":20,"isDelisted":true}]}"#;
    const LIMITS: &str = "# hl-perp-limits.tsv (probe)\ncoin\tmin_notional_1e6\tprobed_at\nBTC\t10000000\t2026-10-17\nETH\t12000000\t2026-10-17\nMATIC\t10000000\t2026-10-17\n";

    fn cfg(lev: u16) -> core_config::exec::ExecHlPerp {
        core_config::exec::ExecHlPerp {
            isolated: true,
            leverage: lev,
            limits: "~/multivenue/hl-perp-limits.tsv".to_owned(),
            line: 9,
        }
    }

    /// The sequence the binary runs, the venue's leverage answer scripted.
    fn run(
        cfg: Option<&core_config::exec::ExecHlPerp>,
        routes: &[HlPerpRoute<'_>],
        limits: Option<&str>,
        calls: &mut Vec<(u32, u16)>,
    ) -> Result<Vec<String>, String> {
        boot_hl_perps(cfg, routes, limits, |wants: &[PerpWant<'_>], tells| {
            let mut t = PerpTable::new();
            boot_bind_and_lever(
                &mut t,
                META,
                wants,
                |s| tells.push(boot_tell(s)),
                |asset, lev| {
                    calls.push((asset, lev));
                    true
                },
            )
        })
    }

    const BTC8: HlPerpRoute<'static> = HlPerpRoute {
        sym: 900,
        coin: "BTC",
        slot: 8,
    };
    const ETH8: HlPerpRoute<'static> = HlPerpRoute {
        sym: 901,
        coin: "ETH",
        slot: 8,
    };

    #[test]
    fn hl_perp_boot_binds_tells_then_levers_once_per_coin() {
        let mut calls = Vec::new();
        let tells = run(Some(&cfg(3)), &[BTC8, ETH8], Some(LIMITS), &mut calls).expect("boot");
        assert_eq!(
            tells,
            vec![
                "perp coin=BTC asset=0 szDecimals=5 maxLev=40 lev=3 min=$10".to_owned(),
                "perp coin=ETH asset=1 szDecimals=4 maxLev=25 lev=3 min=$12".to_owned(),
            ]
        );
        assert_eq!(calls, vec![(0, 3), (1, 3)]);
    }

    #[test]
    fn no_perp_route_needs_nothing() {
        let mut calls = Vec::new();
        assert_eq!(run(None, &[], None, &mut calls), Ok(Vec::new()));
        assert!(calls.is_empty());
    }

    #[test]
    fn a_perp_route_without_the_section_refuses_boot() {
        let mut calls = Vec::new();
        let e = run(None, &[BTC8], Some(LIMITS), &mut calls).expect_err("refused");
        assert!(e.contains("`[exec.hl_perp]`"), "{e}");
        assert!(calls.is_empty());
    }

    #[test]
    fn a_coin_absent_from_the_probe_file_cannot_arm() {
        let mut calls = Vec::new();
        let only_btc = "coin\tmin_notional_1e6\nBTC\t10000000\n";
        let e = run(Some(&cfg(3)), &[BTC8, ETH8], Some(only_btc), &mut calls).expect_err("refused");
        assert!(e.contains("perp ETH: absent from the probe file"), "{e}");
        let e = run(Some(&cfg(3)), &[BTC8], None, &mut calls).expect_err("no file");
        assert!(e.contains("could not be read"), "{e}");
        assert!(
            calls.is_empty(),
            "nothing is levered before every coin can arm"
        );
    }

    #[test]
    fn delisted_absent_duplicate_and_over_max_refuse_boot() {
        let mut calls = Vec::new();
        let matic = HlPerpRoute {
            sym: 902,
            coin: "MATIC",
            slot: 8,
        };
        let e = run(Some(&cfg(3)), &[matic], Some(LIMITS), &mut calls).expect_err("delisted");
        assert!(e.contains("delisted"), "{e}");
        let doge_limits = format!("{LIMITS}DOGE\t10000000\t2026-10-17\n");
        let doge = HlPerpRoute {
            sym: 903,
            coin: "DOGE",
            slot: 8,
        };
        let e = run(Some(&cfg(3)), &[doge], Some(&doge_limits), &mut calls).expect_err("absent");
        assert!(e.contains("absent from this boot's `meta`"), "{e}");
        let e = run(Some(&cfg(3)), &[BTC8, BTC8], Some(LIMITS), &mut calls).expect_err("dup");
        assert!(e.contains("routed twice"), "{e}");
        let e = run(Some(&cfg(41)), &[BTC8], Some(LIMITS), &mut calls).expect_err("over max");
        assert!(e.contains("above a coin's maxLeverage"), "{e}");
        let e = run(Some(&cfg(0)), &[BTC8], Some(LIMITS), &mut calls).expect_err("zero");
        assert!(e.contains("leverage 0"), "{e}");
        assert!(calls.is_empty());
    }

    #[test]
    fn the_probe_file_layout_is_refused_when_malformed() {
        assert!(parse_hl_perp_limits("").is_err());
        assert!(parse_hl_perp_limits("coin\tmin\nBTC\t1\n").is_err());
        assert!(parse_hl_perp_limits("coin\tmin_notional_1e6\nBTC\t-1\n").is_err());
        assert!(parse_hl_perp_limits("coin\tmin_notional_1e6\nBTC\t1.5\n").is_err());
        assert!(parse_hl_perp_limits("coin\tmin_notional_1e6\nBTC\t1\nBTC\t2\n").is_err());
        assert!(parse_hl_perp_limits("coin\tmin_notional_1e6\nBTC\n").is_err());
        assert_eq!(
            parse_hl_perp_limits(LIMITS).expect("ok")[1],
            ("ETH".to_owned(), 12_000_000)
        );
    }
}

/// XMM XH4f / XH5e: `exec.toml.example` with its slot-6 recipe live — the
/// section's keys uncommented, its paper line dropped (tests: the template
/// resolves on the gateway; the probe's section is this recipe).
#[cfg(test)]
pub(crate) fn template_slot6_recipe() -> String {
    let src = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../exec.toml.example"),
    )
    .expect("exec.toml.example must ship with the repo");
    let mut six = false;
    let mut out = String::with_capacity(src.len());
    for l in src.lines() {
        if l.starts_with("[exec.slot.") || l.starts_with("# ---- [exec.") {
            six = l == "[exec.slot.6]";
        }
        if six && l == "mode = \"paper\"" {
            continue;
        }
        out.push_str(if six {
            l.strip_prefix("# ").unwrap_or(l)
        } else {
            l
        });
        out.push('\n');
    }
    out
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
        // BX3: the margin halt ships COMMENTED OUT — it is refused on a
        // slot that does not name binance — and the template states the
        // recipe for a Binance slot: name the venue, uncomment the key.
        assert_eq!(s3.halt_on_margin_ratio_1e6, 0);
        // BX6: the recipe is the key plus the `[exec.binance]` block, both
        // uncommented — and the result must also RESOLVE (a gateway this
        // binary has, the one owner slot).
        let bn = binance_recipe(&live);
        let b = core_config::exec::parse(&bn).expect("the template's Binance recipe parses");
        assert_eq!(b.slot(3).halt_on_margin_ratio_1e6, 600_000);
        assert_eq!(b.binance.as_ref().map(|b| b.products.clone()), Some(vec![String::from("usdm")]));
        let d = tmp();
        let p = write(&d, "exec.toml", &bn);
        let eb = resolve(Some(&p), Some("3")).expect("the Binance recipe boots").unwrap();
        assert_eq!(eb.binance_slot(), Some(3));
        std::fs::remove_dir_all(&d).ok();
        let e = core_config::exec::parse(&live.replacen(
            "venues = [\"hyperliquid\"]",
            "venues = [\"binance\"]",
            1,
        ))
        .expect_err("a live Binance slot without the key");
        assert!(e.to_string().contains("halt_on_margin_ratio_1e6"), "{e}");
    }

    /// The template's Binance recipe (BX6): slot 3 names binance, with the
    /// margin key and the `[exec.binance]` block uncommented.
    fn binance_recipe(live: &str) -> String {
        let mut in_block = false;
        let mut bn = String::new();
        for l in live
            .replacen("venues = [\"hyperliquid\"]", "venues = [\"binance\"]", 1)
            .replacen(
                "# halt_on_margin_ratio_1e6 = 600000",
                "halt_on_margin_ratio_1e6 = 600000",
                1,
            )
            .lines()
        {
            in_block |= l == "# [exec.binance]";
            bn.push_str(if in_block { l.strip_prefix("# ").unwrap_or(l) } else { l });
            bn.push('\n');
        }
        bn
    }

    /// BX11: the template's Binance recipe boots every product and account
    /// mode this binary builds, each by the edits its block's comments name
    /// — and refuses the two the comments forbid (PM, and an HMAC key, off
    /// the REST route). BX8: spot classic boots, alone and beside USD-M;
    /// spot beside PM Pro refuses (it waits on a ruling).
    #[test]
    fn the_templates_binance_recipe_boots_every_built_product_and_mode() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../exec.toml.example"),
        )
        .expect("exec.toml.example must ship with the repo");
        let base = binance_recipe(&src.replacen("mode = \"paper\"", "mode = \"live\"", 1));
        const PRODUCTS: &str = "products = [\"usdm\"]";
        const CLASSIC: &str = "account_mode = \"classic\"";
        const WS: &str = "order_transport = \"ws\"";
        const REST: &str = "order_transport = \"rest\"";
        const ED25519: &str = "signing = \"ed25519\"";
        let cases: [(&str, &[(&str, &str)], bool); 18] = [
            ("USD-M classic on the WS API", &[], true),
            ("COIN-M classic", &[(PRODUCTS, "products = [\"coinm\"]")], true),
            ("both products, classic", &[(PRODUCTS, "products = [\"usdm\", \"coinm\"]")], true),
            ("USD-M classic on REST", &[(WS, REST)], true),
            ("an HMAC key on REST", &[(ED25519, "signing = \"hmac\""), (WS, REST)], true),
            ("PM on REST, both products", &[(CLASSIC, "account_mode = \"pm\""), (WS, REST), (PRODUCTS, "products = [\"usdm\", \"coinm\"]")], true),
            ("PM Pro", &[(CLASSIC, "account_mode = \"pm_pro\"")], true),
            ("PM on the WS API", &[(CLASSIC, "account_mode = \"pm\"")], false),
            ("spot classic", &[(PRODUCTS, "products = [\"spot\"]")], true),
            ("USD-M and spot", &[(PRODUCTS, "products = [\"usdm\", \"spot\"]")], true),
            ("spot beside PM Pro", &[(CLASSIC, "account_mode = \"pm_pro\""), (PRODUCTS, "products = [\"usdm\", \"spot\"]")], false),
            ("spot on REST", &[(WS, REST), (PRODUCTS, "products = [\"usdm\", \"spot\"]")], false),
            ("spot with an HMAC key on REST", &[(ED25519, "signing = \"hmac\""), (WS, REST), (PRODUCTS, "products = [\"spot\"]")], false),
            // BX9 (the risk review, round 3): the options variants it lists.
            ("options classic", &[(PRODUCTS, "products = [\"options\"]")], true),
            ("USD-M and options", &[(PRODUCTS, "products = [\"usdm\", \"options\"]")], true),
            ("options written", &[(PRODUCTS, "products = [\"options\"]"), ("options_write = 0", "options_write = 1")], true),
            ("options beside COIN-M", &[(PRODUCTS, "products = [\"coinm\", \"options\"]")], false),
            ("options beside PM Pro", &[(CLASSIC, "account_mode = \"pm_pro\""), (PRODUCTS, "products = [\"usdm\", \"options\"]")], false),
        ];
        for (what, edits, boots) in cases {
            let mut src = base.clone();
            for (from, to) in edits {
                assert!(src.contains(from), "{what}: the template has no `{from}`");
                src = src.replacen(from, to, 1);
            }
            let d = tmp();
            let p = write(&d, "exec.toml", &src);
            let got = resolve(Some(&p), Some("3"));
            std::fs::remove_dir_all(&d).ok();
            if boots {
                let eb = got.unwrap_or_else(|e| panic!("{what}: {e}")).expect("a table");
                assert_eq!(eb.binance_slot(), Some(3), "{what}");
            } else {
                assert!(got.is_err(), "{what} must refuse");
            }
        }
        let hmac_ws = base.replacen(ED25519, "signing = \"hmac\"", 1);
        let e = core_config::exec::parse(&hmac_ws).map(|_| ()).expect_err("an HMAC key on the WS API");
        assert!(e.to_string().contains("rest"), "{e}");
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
        // S16-3: 16 is the first slot past the table (9 was, at eight).
        assert!(parse_halt_slots("16").unwrap_err().contains("out of range"));
        assert!(parse_halt_slots("255").unwrap_err().contains("out of range"));
        // Slot 11 is bit 11 alone — never bit 3 (BIN15's), its `u8` alias.
        assert_eq!(parse_halt_slots("11").unwrap(), 1 << 11);
        assert_eq!(parse_halt_slots("15").unwrap(), 1 << 15);
        assert_eq!(parse_halt_slots("3,11,15").unwrap(), (1 << 3) | (1 << 11) | (1 << 15));
        assert!(parse_halt_slots("11,11").unwrap_err().contains("twice"));
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

    /// The smallest live gateway slot 6 (XMM XH4f).
    fn gw_six() -> String {
        MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.6]")
            .replace("name = \"bin15\"", "name = \"xmm\"")
            + "transport = \"gateway\"\ncap_perp_usd_1e6 = 150000000\n\
               max_resting_usd_1e6 = 300000000\nhalt_on_alo_reject_pct = 20\n"
    }

    /// XMM XH4f (deviation 10): slot 6 is live only on its own arm, the
    /// Hyperliquid gateway — never on the operator's blocking arm — and
    /// the gateway is slot 6's alone. Its clamps reach the route (the perp
    /// pair, the ALO permille); the operator's arm neither boots for it
    /// nor counts its floor; both switches still arm it.
    #[test]
    fn a_live_slot_six_resolves_only_on_its_gateway_arm() {
        let d = tmp();
        let blocking = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.6]")
            .replace("name = \"bin15\"", "name = \"xmm\"");
        let p = write(&d, "exec.toml", &blocking);
        let e = resolve(Some(&p), Some("6")).expect_err("never on the blocking arm");
        assert!(
            e.contains("slot 6 (xmm)") && e.contains("transport = \"gateway\""),
            "{e}"
        );
        let p = write(&d, "exec-gw.toml", &gw_six());
        let boot = resolve(Some(&p), Some("6"))
            .expect("the gateway resolves")
            .expect("present");
        assert_eq!(boot.route.mode_at(XMM_SLOT), Some(ExecMode::Live));
        assert_eq!(
            boot.route.perp_at(XMM_SLOT),
            Some(PerpLimits::new(150_000_000, 300_000_000))
        );
        assert_eq!(
            boot.route.halts_at(XMM_SLOT).map(|h| h.alo_reject_permille),
            Some(200)
        );
        assert_eq!(boot.gateway_live(), Some(XMM_SLOT));
        assert!(!boot.hl_arm_needed(), "the operator's arm is not slot 6's");
        assert_eq!(boot.hl_address_numbers(), (0, 0, 0));
        assert!(
            resolve(Some(&p), None).is_err(),
            "the artifact alone arms nothing"
        );
        // XMM XH4i-5: slot 3 runs on a gateway arm of its own — never with
        // the perp pair (its legs are no linear rows); slot 6 states it.
        let three = gw_six()
            .replace("[exec.slot.6]", "[exec.slot.3]")
            .replace("name = \"xmm\"", "name = \"bin15\"");
        let p = write(&d, "exec-gw3.toml", &three);
        let e = resolve(Some(&p), Some("3")).expect_err("no perp pair on slot 3");
        assert!(e.contains("slot 3 (bin15)") && e.contains("XH4i-5"), "{e}");
        let no_pair = gw_six().replace(
            "cap_perp_usd_1e6 = 150000000\nmax_resting_usd_1e6 = 300000000\n",
            "",
        );
        let p = write(&d, "exec-gw6-bare.toml", &no_pair);
        let e = resolve(Some(&p), Some("6")).expect_err("slot 6 states its pair");
        assert!(e.contains("slot 6 (xmm)") && e.contains("perp pair"), "{e}");
        let paper = blocking.replacen("mode = \"live\"", "mode = \"paper\"", 1);
        let p = write(&d, "exec-paper.toml", &paper);
        let boot = resolve(Some(&p), None)
            .expect("a paper slot 6 is fine")
            .expect("present");
        assert_eq!(boot.route.mode_at(XMM_SLOT), Some(ExecMode::Paper));
        assert_eq!(boot.gateway_live(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// XMM XH4i-5: slot 3 live on a gateway arm of its own (the outcome
    /// mode) — no perp pair; the operator's blocking arm neither boots for it
    /// nor counts its floor — and never beside slot 6's gateway (A-S7-85) nor
    /// beside a live slot of the blocking arm (one master, one arm).
    #[test]
    fn slot_three_runs_on_a_gateway_arm_of_its_own_alone() {
        let d = tmp();
        let three = format!("{MINIMAL_LIVE}transport = \"gateway\"\nhalt_on_alo_reject_pct = 20\n");
        let p = write(&d, "exec.toml", &three);
        let boot = resolve(Some(&p), Some("3"))
            .expect("slot 3 on its gateway")
            .expect("present");
        assert!(boot.bin15_gateway_live());
        assert_eq!(boot.gateway_live(), None, "slot 6's gateway is not armed");
        assert!(!boot.hl_arm_needed(), "the operator's arm does not boot");
        assert_eq!(boot.hl_address_numbers(), (0, 0, 0));
        assert_eq!(
            boot.route
                .halts_at(BIN15_SLOT)
                .map(|h| h.alo_reject_permille),
            Some(200)
        );
        let six = gw_six().replacen("[exec]\n", "", 1);
        let p = write(&d, "exec-36.toml", &format!("{three}{six}"));
        let e = resolve(Some(&p), Some("3,6")).expect_err("one gateway arm an engine");
        assert!(e.contains("A-S7-85"), "{e}");
        let four = MINIMAL_LIVE
            .replacen("[exec]\n", "", 1)
            .replace("[exec.slot.3]", "[exec.slot.4]")
            .replace("name = \"bin15\"", "name = \"ai-exec\"");
        let p = write(&d, "exec-34.toml", &format!("{three}{four}"));
        let e = resolve(Some(&p), Some("3,4")).expect_err("one master, one arm");
        assert!(
            e.contains("slot 4 (ai-exec)") && e.contains("one arm"),
            "{e}"
        );
        // Beside slot 0 (mainnet only): on mainnet market data alone.
        let hyparb = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.0]")
            .replace("name = \"bin15\"", "name = \"hyparb\"")
            .replace("[\"hyperliquid\"]", "[\"hyperliquid\", \"hyperevm\"]");
        let p = write(
            &d,
            "exec-03.toml",
            &format!("{hyparb}{}", three.replacen("[exec]\n", "", 1)),
        );
        let b = resolve(Some(&p), Some("0,3"))
            .expect("slot 3's gateway beside slot 0 resolves")
            .expect("present");
        let e = b
            .check_gateway_network(false)
            .expect_err("testnet beside slot 0");
        assert!(
            e.contains("slot 3 is live") && e.contains("slot 0 (hyparb)"),
            "{e}"
        );
        assert_eq!(
            b.check_gateway_network(true),
            Ok(()),
            "mainnet beside slot 0"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// XMM XH5a (R-XH4-1, D2 ruled): a live slot 3 on the operator's arm
    /// beside the gateway slot resolves — a halt cancels only the halted
    /// slot's orders. The operator's arm boots for slot 3 alone, and its
    /// address numbers are slot 3's, never slot 6's.
    #[test]
    fn the_gateway_slot_runs_beside_the_operator_s_arm() {
        let d = tmp();
        let six = gw_six().replacen("[exec]\n", "", 1).replace(
            "request_budget_floor = 2000\n",
            "request_budget_floor = 9000\n",
        );
        let both = format!("{MINIMAL_LIVE}{six}");
        let p = write(&d, "exec.toml", &both);
        let boot = resolve(Some(&p), Some("3,6"))
            .expect("slot 6 beside slot 3")
            .expect("present");
        assert_eq!(boot.gateway_live(), Some(XMM_SLOT));
        assert_eq!(boot.check_gateway_network(false), Ok(()), "testnet");
        assert_eq!(boot.check_gateway_network(true), Ok(()), "mainnet");
        assert!(boot.hl_arm_needed(), "slot 3 trades on the operator's arm");
        assert_eq!(
            boot.hl_address_numbers(),
            (2000, 0, 0),
            "slot 3's floor, never slot 6's"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// XMM XH5b-3: the gateway slot shares an engine with an arm that trades
    /// mainnet only — a live Binance slot, or slot 0 — on mainnet alone: the
    /// artifact resolves (it states no network) and the boot's network check
    /// refuses testnet market data, naming the arm. Alone, either network.
    /// XH5b-2: its session bound resolves here — slot 6's boot judges it by
    /// its account (`xmm_live::check_account_rules`).
    #[test]
    fn the_gateway_slot_beside_mainnet_arms_needs_mainnet_market_data() {
        let d = tmp();
        let six = gw_six().replacen("[exec]\n", "", 1);
        let with_bn = format!("{}{six}{BN_SECTION}", bn_live());
        let p = write(&d, "exec-bn.toml", &with_bn);
        let b = resolve(Some(&p), Some("3,6"))
            .expect("the artifact states no network")
            .expect("present");
        let e = b
            .check_gateway_network(false)
            .expect_err("testnet beside Binance");
        assert!(e.contains("slot 3 (bin15)") && e.contains("XH5b-3"), "{e}");
        assert_eq!(
            b.check_gateway_network(true),
            Ok(()),
            "mainnet beside Binance"
        );
        let hyparb = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.0]")
            .replace("name = \"bin15\"", "name = \"hyparb\"")
            .replace("[\"hyperliquid\"]", "[\"hyperliquid\", \"hyperevm\"]");
        let p = write(&d, "exec-0.toml", &format!("{hyparb}{six}"));
        let b = resolve(Some(&p), Some("0,6"))
            .expect("the artifact states no network")
            .expect("present");
        let e = b
            .check_gateway_network(false)
            .expect_err("testnet beside slot 0");
        assert!(
            e.contains("slot 0 (hyparb)") && e.contains("mainnet only"),
            "{e}"
        );
        assert_eq!(
            b.check_gateway_network(true),
            Ok(()),
            "mainnet beside slot 0"
        );
        let bounded = format!("{}halt_on_loss_usd_1e6 = 5000000\n", gw_six());
        let p = write(&d, "exec-pnl.toml", &bounded);
        let b = resolve(Some(&p), Some("6"))
            .expect("a bound resolves: slot 6's boot judges it by its account")
            .expect("present");
        assert!(b
            .slots
            .iter()
            .any(|s| s.slot == XMM_SLOT && s.halt_on_loss_usd_1e6 == 5_000_000));
        assert_eq!(b.check_gateway_network(false), Ok(()), "alone, testnet");
        assert_eq!(b.check_gateway_network(true), Ok(()), "alone, mainnet");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// XMM XH4f: the template's slot-6 section is the recipe for a live
    /// gateway slot — uncomment its keys, drop the paper line — and it
    /// resolves under `--arm-live 6`; the tell names the gateway's clamps
    /// on slot 6's lines alone.
    #[test]
    fn the_template_s_slot_six_recipe_resolves_on_the_gateway() {
        let out = super::template_slot6_recipe();
        let d = tmp();
        let p = write(&d, "exec.toml", &out);
        let boot = resolve(Some(&p), Some("6"))
            .expect("the slot-6 recipe boots")
            .expect("present");
        assert_eq!(boot.gateway_live(), Some(XMM_SLOT));
        let tell = render_boot_tell(&boot);
        assert!(
            tell.iter().any(|l| l.starts_with("exec: slot 6 LIVE")
                && l.contains("transport=gateway perp=$150 resting=$300")),
            "{tell:?}"
        );
        assert!(
            tell.iter()
                .any(|l| l.starts_with("exec: slot 6 HALTS") && l.contains("alo_reject_pct=20")),
            "{tell:?}"
        );
        // XMM XH5b-2: the recipe is an own account's — its loss bound
        // stated, its stale floor four 30 s periods; on the shared account
        // its bound refuses.
        let s6 = boot
            .slots
            .iter()
            .find(|s| s.slot == XMM_SLOT)
            .expect("slot 6");
        assert_eq!(crate::xmm_live::check_account_rules(s6, true), Ok(()));
        assert!(crate::xmm_live::check_account_rules(s6, false).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// HC11 (O-HC18): a live slot 7 refuses the boot — correctly named,
    /// agreed by `--arm-live`, on any venue — while a PAPER slot 7 is an
    /// ordinary artifact.
    #[test]
    fn a_live_slot_seven_refuses_it_is_dark() {
        let d = tmp();
        let seven = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.7]")
            .replace("name = \"bin15\"", "name = \"hcv\"");
        let p = write(&d, "exec.toml", &seven);
        let e = resolve(Some(&p), Some("7")).expect_err("slot 7 is paper only");
        assert!(e.contains("slot 7 (hcv)") && e.contains("O-HC18"), "{e}");
        let paper = seven.replacen("mode = \"live\"", "mode = \"paper\"", 1);
        let p = write(&d, "exec-paper.toml", &paper);
        let boot = resolve(Some(&p), None).expect("a paper slot 7 is fine").expect("present");
        assert_eq!(boot.route.mode_at(HCV_SLOT), Some(ExecMode::Paper));
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
                core_types::VenueId::HyperEvm as u8,
                core_types::VenueId::Binance as u8,
                core_types::VenueId::Polymarket as u8,
            ]
        );
        assert!(b.hl_arm_needed() && !b.hyparb_live() && !b.pm15_live());
        std::fs::remove_dir_all(&d).ok();
    }

    /// **BX3 risk review, O-BX27** — the HL arm's address numbers come
    /// only from the live slots it trades: the highest floor, the lowest
    /// top-up weight and ceiling. Slot 0 (its own address), a paper slot
    /// and a live slot on another venue have no say — each holds the
    /// number that would win. Break-and-watch: dropping any one of the
    /// three filters moves the result, and so does the pre-BX3 `min` for
    /// the floor.
    #[test]
    fn the_hl_address_numbers_come_from_the_slots_the_arm_trades() {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let bn = core_types::VenueId::Binance as u8;
        let boot = boot_of(&[
            (0, hl, 9_000, 1, 1, true),
            (2, bn, 9_500, 1_000, 10_000, true),
            (3, hl, 2_000, 5_000, 30_000, true),
            (4, hl, 3_000, 6_000, 20_000, true),
            (5, hl, 8_000, 1, 1, false),
        ]);
        assert_eq!(boot.hl_address_numbers(), (3_000, 5_000, 20_000));
    }

    /// An [`ExecBoot`] over hand-built rows `(slot, venue, budget floor,
    /// top-up weight, top-up day max, live)` — no artifact, so a test can
    /// arm a slot `resolve` refuses (8 paper only; 9–15, S16-R5).
    fn boot_of(rows: &[(usize, u8, i64, i64, i64, bool)]) -> ExecBoot {
        let caps = SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64);
        let mut route = ExecRoute::all_paper();
        let mut slots = Vec::new();
        let mut live_mask: SlotMask = 0;
        for &(slot, venue, floor, weight, day_max, live) in rows {
            let mode = if live {
                ExecMode::Live
            } else {
                ExecMode::Paper
            };
            route
                .set_slot(slot, mode, &[venue], caps, HaltLimits::none())
                .unwrap();
            let mut s = core_config::exec::ExecSlot::paper_default(slot);
            s.venues = vec![venue];
            s.request_budget_floor = floor;
            s.request_topup_weight = weight;
            s.request_topup_day_max = day_max;
            if live {
                s.mode = String::from("live");
                live_mask |= (1 as SlotMask) << slot;
            }
            slots.push(s);
        }
        ExecBoot {
            route,
            hash: [0u8; 32],
            path: PathBuf::from("/tmp/exec.toml"),
            enabled: true,
            live_mask,
            slots,
            binance: None,
            pine8: None,
        }
    }

    /// The smallest valid `[exec.binance]` (core-config's own fixture).
    const BN_SECTION: &str = "[exec.binance]\nnetwork = \"mainnet\"\naccount_mode = \"classic\"\n\
         account_scope = \"dedicated\"\nproducts = [\"usdm\"]\nstp_mode = \"EXPIRE_MAKER\"\n";

    /// Slot 3 live on binance, with the margin key its artifact law wants.
    fn bn_live() -> String {
        MINIMAL_LIVE.replace("[\"hyperliquid\"]", "[\"binance\"]").replace(
            "request_budget_floor = 2000\n",
            "halt_on_margin_ratio_1e6 = 600000\n",
        )
    }

    /// **BX6 (obligation 9)** — Binance has a live arm now: a live slot
    /// naming it resolves with its `[exec.binance]` section and becomes the
    /// arm's one owner; without the section, with a product or a mode the
    /// gateway does not speak, or with a second live Binance slot, the
    /// boot refuses and names why. Paper never needs an arm.
    #[test]
    fn a_live_binance_slot_resolves_with_its_section_and_nothing_else_does() {
        let d = tmp();
        let p = write(&d, "exec.toml", &format!("{}{BN_SECTION}", bn_live()));
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        assert_eq!(b.binance_slot(), Some(3));
        assert_eq!(b.binance.as_ref().map(|b| b.products.clone()), Some(vec![String::from("usdm")]));
        assert!(!b.hl_arm_needed(), "a Binance-only slot needs no Hyperliquid arm");
        assert!(LIVE_ARM_VENUES.contains(&(core_types::VenueId::Binance as u8)));
        // No section: the artifact's own law refuses first.
        let p = write(&d, "bare.toml", &bn_live());
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("[exec.binance]"), "{e}");
        // BX10 (S-3): Binance Stocks is the battery's alone — the engine
        // refuses it by its rulings.
        let equity = BN_SECTION.replace("products = [\"usdm\"]", "products = [\"equity\"]\nequity_session = \"RTH\"\nequity_tokenize = 0\nequity_quote = \"USDC\"");
        let p = write(&d, "unbuilt.toml", &format!("{}{equity}", bn_live()));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("S-3") && e.contains("O-BX16") && e.contains("R17"), "equity: {e}");
        // BX9: options classic are built — alone, or beside the futures;
        // beside Portfolio Margin they wait on a ruling; eapi's weight is
        // judged on its own pool.
        for to in ["products = [\"options\"]", "products = [\"usdm\", \"options\"]"] {
            let src = format!("{}{}", bn_live(), BN_SECTION.replace("products = [\"usdm\"]", to));
            let p = write(&d, "options.toml", &src);
            let b = resolve(Some(&p), Some("3")).unwrap_or_else(|e| panic!("{to}: {e}")).unwrap();
            assert!(b.binance.as_ref().is_some_and(|b| b.products.iter().any(|p| p == "options")), "{to}");
        }
        let opt_pro = BN_SECTION
            .replace("products = [\"usdm\"]", "products = [\"usdm\", \"options\"]")
            .replace("account_mode = \"classic\"", "account_mode = \"pm_pro\"");
        let p = write(&d, "options-pro.toml", &format!("{}{opt_pro}", bn_live()));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("a ruling"), "options beside PM Pro: {e}");
        let fast = BN_SECTION.replace("products = [\"usdm\"]", "products = [\"options\"]\nrecon_every_ms = 10000");
        let p = write(&d, "options-fast.toml", &format!("{}{fast}", bn_live()));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("on eapi"), "a 10 s options cycle: {e}");
        // BX8: spot classic is built — alone, or beside the futures; beside
        // Portfolio Margin it waits on a ruling.
        for to in ["products = [\"spot\"]", "products = [\"usdm\", \"spot\"]"] {
            let src = format!("{}{}", bn_live(), BN_SECTION.replace("products = [\"usdm\"]", to));
            let p = write(&d, "spot.toml", &src);
            let b = resolve(Some(&p), Some("3")).unwrap_or_else(|e| panic!("{to}: {e}")).unwrap();
            assert!(b.binance.as_ref().is_some_and(|b| b.products.iter().any(|p| p == "spot")), "{to}");
        }
        let spot_pro = BN_SECTION
            .replace("products = [\"usdm\"]", "products = [\"usdm\", \"spot\"]")
            .replace("account_mode = \"classic\"", "account_mode = \"pm_pro\"");
        let p = write(&d, "spot-pro.toml", &format!("{}{spot_pro}", bn_live()));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("a ruling"), "spot beside PM Pro: {e}");
        // BX7 (C3): PM Pro is built — the classic routes.
        let pm_pro = BN_SECTION.replace("account_mode = \"classic\"", "account_mode = \"pm_pro\"");
        let p = write(&d, "pm-pro.toml", &format!("{}{pm_pro}", bn_live()));
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        assert_eq!(b.binance.as_ref().map(|b| b.account_mode.as_str()), Some("pm_pro"));
        // BX7 (C2): Portfolio Margin is built — papi's REST route only.
        let pm = BN_SECTION.replace("account_mode = \"classic\"", "account_mode = \"pm\"");
        let p = write(&d, "pm.toml", &format!("{}{pm}order_transport = \"rest\"\n", bn_live()));
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        assert_eq!(b.binance.as_ref().map(|b| b.account_mode.as_str()), Some("pm"));
        let p = write(&d, "pm-ws.toml", &format!("{}{pm}", bn_live()));
        assert!(resolve(Some(&p), Some("3")).unwrap_err().contains("needs `order_transport = \"rest\"`"));
        // BX7 (C1): COIN-M classic is built — beside USDⓈ-M, or alone.
        for to in ["products = [\"usdm\", \"coinm\"]", "products = [\"coinm\"]"] {
            let src = format!("{}{}", bn_live(), BN_SECTION.replace("products = [\"usdm\"]", to));
            let p = write(&d, "coinm.toml", &src);
            let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
            assert!(b.binance.as_ref().is_some_and(|b| b.products.iter().any(|w| w == "coinm")), "{to}");
        }
        // A shared account is not armable in this binary (its foreign-fill
        // halt is not built).
        let shared = BN_SECTION.replace("\"dedicated\"", "\"shared\"") + "owned_usdm = [\"BTCUSDT\"]\n";
        let p = write(&d, "shared.toml", &format!("{}{shared}", bn_live()));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("account_scope `shared`"), "{e}");
        // A reconciliation cadence one late cycle from a stale halt.
        let slow = format!("{}{BN_SECTION}recon_every_ms = 200000\n", bn_live());
        let p = write(&d, "slow.toml", &slow);
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("more than half"), "{e}");
        // A dead-man heavier than the arm's share of the IP's weight (S5).
        let heavy = format!("{}{BN_SECTION}heartbeat_ms = 1000\n", bn_live());
        let p = write(&d, "heavy.toml", &heavy);
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("weighs up to"), "{e}");
        // BX9 (the risk review, round 3): spot alone keeps its one futures
        // lane in that judgment (`lanes.max(1)`); options alone are judged
        // on eapi's pool instead, never on fapi's.
        for (to, judged) in [("products = [\"spot\"]", true), ("products = [\"options\"]", false)] {
            let one = format!("{}{}heartbeat_ms = 1000\n", bn_live(), BN_SECTION.replace("products = [\"usdm\"]", to));
            let p = write(&d, "heavy-one.toml", &one);
            let got = resolve(Some(&p), Some("3"));
            assert_eq!(got.as_ref().err().is_some_and(|e| e.contains("weighs up to")), judged, "{to}: {:?}", got.err());
        }
        // A second live Binance slot.
        let two = format!(
            "{}{}{BN_SECTION}",
            bn_live(),
            bn_live()
                .replace("[exec]\n", "")
                .replace("[exec.slot.3]", "[exec.slot.4]")
                .replace("\"bin15\"", "\"ai-exec\"")
        );
        let p = write(&d, "two.toml", &two);
        let e = resolve(Some(&p), Some("3,4")).unwrap_err();
        assert!(e.contains("ONE owner slot"), "{e}");
        // Paper never needs an arm.
        let p = write(&d, "paper.toml", &bn_live().replace("mode = \"live\"", "mode = \"paper\""));
        let b = resolve(Some(&p), None).unwrap().unwrap();
        assert!(!b.any_live() && b.binance_slot().is_none());
        assert_eq!(b.slots[3].halt_on_margin_ratio_1e6, 600_000);
        std::fs::remove_dir_all(&d).ok();
    }

    /// **BX7 (the risk review's R3, N1)** — a session bound is judged on
    /// one USD equity: a classic arm trading COIN-M has none for it (COIN-M
    /// margins in each coin), alone or beside USDⓈ-M (whose equity would
    /// judge it while COIN-M's losses went unseen) — refused, naming why.
    /// USDⓈ-M alone judges its own; PM and PM Pro the unified account's.
    /// Break-and-watch: without N1's check the bounded USDⓈ-M + COIN-M
    /// classic slot boots.
    #[test]
    fn a_session_bound_needs_a_usd_equity() {
        let d = tmp();
        let bounded = bn_live().replace(
            "halt_on_margin_ratio_1e6 = 600000\n",
            "halt_on_margin_ratio_1e6 = 600000\nhalt_on_loss_usd_1e6 = 5000000\n",
        );
        let cm = BN_SECTION.replace("products = [\"usdm\"]", "products = [\"coinm\"]");
        let p = write(&d, "cm-bound.toml", &format!("{bounded}{cm}"));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("no USD"), "{e}");
        let p = write(&d, "cm.toml", &format!("{}{cm}", bn_live()));
        assert!(resolve(Some(&p), Some("3")).is_ok(), "COIN-M alone, no bound");
        let both = BN_SECTION.replace("products = [\"usdm\"]", "products = [\"usdm\", \"coinm\"]");
        let p = write(&d, "both.toml", &format!("{bounded}{both}"));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("no USD") && e.contains("trades COIN-M"), "USDⓈ-M beside COIN-M: {e}");
        let p = write(&d, "um.toml", &format!("{bounded}{BN_SECTION}"));
        assert!(resolve(Some(&p), Some("3")).is_ok(), "USDⓈ-M alone judges its own equity");
        let pm_pro = cm.replace("account_mode = \"classic\"", "account_mode = \"pm_pro\"");
        let p = write(&d, "pm-pro.toml", &format!("{bounded}{pm_pro}"));
        assert!(resolve(Some(&p), Some("3")).is_ok(), "PM Pro: the unified account's equity");
        // BX8: the spot wallet has no USD equity the bound reads — refused
        // beside USDⓈ-M as alone; unbounded, it boots.
        let spot = BN_SECTION.replace("products = [\"usdm\"]", "products = [\"usdm\", \"spot\"]");
        let p = write(&d, "spot-bound.toml", &format!("{bounded}{spot}"));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("no USD") && e.contains("trades spot"), "spot beside USDⓈ-M: {e}");
        let p = write(&d, "spot.toml", &format!("{}{spot}", bn_live()));
        assert!(resolve(Some(&p), Some("3")).is_ok(), "spot unbounded");
        // BX9 (Q-4): the options account has a USD equity — bounded beside
        // USDⓈ-M or alone; beside COIN-M still refused.
        for to in ["products = [\"usdm\", \"options\"]", "products = [\"options\"]"] {
            let opt = BN_SECTION.replace("products = [\"usdm\"]", to);
            let p = write(&d, "opt-bound.toml", &format!("{bounded}{opt}"));
            assert!(resolve(Some(&p), Some("3")).is_ok(), "{to}, bounded");
        }
        // Beside COIN-M or spot: refused, bounded or not (R2-5: unproven).
        for to in ["products = [\"coinm\", \"options\"]", "products = [\"spot\", \"options\"]"] {
            let opt = BN_SECTION.replace("products = [\"usdm\"]", to);
            for head in [bounded.as_str(), bn_live().as_str()] {
                let p = write(&d, "opt-beside.toml", &format!("{head}{opt}"));
                assert!(resolve(Some(&p), Some("3")).unwrap_err().contains("alone, or with usdm"), "{to}");
            }
        }
        std::fs::remove_dir_all(&d).ok();
    }

    /// **BX6 (obligation 5)** — a bounded slot two arms trade needs both to
    /// judge the bound. Every venue with a live arm today does, so the check
    /// is proved on a venue that has no judging arm. Break-and-watch:
    /// without the check the okx slot passes.
    #[test]
    fn a_bounded_two_arm_slot_needs_both_arms_to_judge_it() {
        let bn = core_types::VenueId::Binance as u8;
        let hl = core_types::VenueId::Hyperliquid as u8;
        let okx = core_types::VenueId::Okx as u8;
        let mut s = core_config::exec::ExecSlot::paper_default(3);
        s.mode = String::from("live");
        s.halt_on_loss_usd_1e6 = 5_000_000;
        s.venues = vec![hl, bn];
        assert!(check_pnl_judged(&[s.clone()], 1 << 3).is_ok());
        s.venues = vec![okx, bn];
        let e = check_pnl_judged(&[s.clone()], 1 << 3).unwrap_err();
        assert!(e.contains("okx"), "{e}");
        s.halt_on_loss_usd_1e6 = 0;
        assert!(check_pnl_judged(&[s.clone()], 1 << 3).is_ok(), "no bound, nothing to judge");
        s.halt_on_loss_usd_1e6 = 5_000_000;
        assert!(check_pnl_judged(&[s], 0).is_ok(), "a paper slot is never judged");
        for v in LIVE_ARM_VENUES {
            assert!(
                PNL_JUDGING_VENUES.contains(v) || *v == core_types::VenueId::HyperEvm as u8,
                "venue {v} has a live arm that never judges the bound"
            );
        }
    }

    /// **BX6** — the Binance arm's knobs reach the boot tell, on its owner.
    #[test]
    fn the_boot_tell_names_the_binance_arms_knobs() {
        let d = tmp();
        let p = write(&d, "exec.toml", &format!("{}{BN_SECTION}", bn_live()));
        let b = resolve(Some(&p), Some("3")).unwrap().unwrap();
        let tell = render_boot_tell(&b);
        let line = tell.iter().find(|l| l.starts_with("exec: binance arm")).expect("a Binance line");
        assert!(line.contains("owner=slot 3 network=mainnet account=classic/dedicated products=[usdm]"), "{line}");
        assert!(line.contains("stp=EXPIRE_MAKER recv_window_ms=1000 countdown_ms=30000 heartbeat_ms=10000"), "{line}");
        assert!(line.contains("max_symbols=20 min_maker_ttl_ms=5000"), "{line}");
        let hl = write(&d, "hl.toml", MINIMAL_LIVE);
        let b = resolve(Some(&hl), Some("3")).unwrap().unwrap();
        assert!(render_boot_tell(&b).iter().all(|l| !l.contains("binance arm")));
        std::fs::remove_dir_all(&d).ok();
    }

    /// **BX6 (obligation 7)** — the alias id is the anchor's alone: another
    /// venue's instrument under it refuses; the anchor itself, and no
    /// alias at all, pass. Break-and-watch: skipping the Binance spot tail
    /// passes a second spot instrument under the anchor's id.
    #[test]
    fn the_alias_id_is_the_anchors_alone() {
        use core_config::universe::{AllocatedUniverse, Instrument};
        let i = |sym, d: &str| Instrument { sym, name: String::from("x"), descriptor: String::from(d) };
        let mut u = AllocatedUniverse {
            bn_spot: vec![i(7, "binance:btcusdt")],
            hl: vec![i(0x0400_0001, "hyperliquid:BTC")],
            ..AllocatedUniverse::default()
        };
        assert!(check_alias_unique(7, &u).is_ok());
        assert!(check_alias_unique(core_types::SYMBOL_ID_NONE, &u).is_ok());
        u.okx = vec![i(7, "okx:BTC-USDT")];
        let e = check_alias_unique(7, &u).unwrap_err();
        assert!(e.contains("okx:BTC-USDT") && e.contains("obligation 7"), "{e}");
        u.okx.clear();
        u.bn_spot.push(i(7, "binance:ethusdt"));
        assert!(check_alias_unique(7, &u).unwrap_err().contains("binance:ethusdt"));
    }

    /// **BX3** — the alias exists only for a flat anchor: the legacy 7
    /// (and a flat `--binance-sym-id`) routes to Binance; a namespaced
    /// Binance anchor, or no Binance spot at all, needs none.
    #[test]
    fn the_route_alias_covers_only_a_flat_binance_anchor() {
        let bn = core_types::VenueId::Binance as u8;
        let a = route_aliases(Some(core_config::universe::LEGACY_BN_ANCHOR_SYM)).unwrap();
        assert_eq!(a.len(), 1);
        assert_eq!(
            a.route_venue(7, 0),
            bn,
            "the legacy anchor routes to Binance"
        );
        assert_eq!(a.route_venue(8, 0), 0, "and nothing else does");
        let flat = route_aliases(Some(1234)).unwrap();
        assert_eq!(flat.route_venue(1234, 0), bn);
        let native = core_types::make_symbol_id(core_types::VenueId::Binance, 1);
        assert!(route_aliases(Some(native)).unwrap().is_empty());
        assert!(route_aliases(None).unwrap().is_empty());
        let e = route_aliases(Some(core_types::SYMBOL_ID_NONE)).unwrap_err();
        assert!(e.contains("NoneSym"), "{e}");
    }

    /// HC9: a live `hypercall` venue refuses the boot with its own
    /// reason — the arm exists, the engine does not arm it before slot
    /// 7's ruling — and the refusal names what that ruling settles.
    #[test]
    fn a_live_hypercall_venue_refuses_and_names_the_ruling() {
        let d = tmp();
        let p = write(&d, "exec.toml", &MINIMAL_LIVE.replace("[\"hyperliquid\"]", "[\"hypercall\"]"));
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(e.contains("`hypercall`"), "{e}");
        assert!(e.contains("slot 7's live-arming ruling"), "{e}");
        assert!(e.contains("options row") && e.contains("reconciled"), "{e}");
        assert!(e.contains("Refusing the boot"), "{e}");
        assert!(!LIVE_ARM_VENUES.contains(&(core_types::VenueId::Hypercall as u8)));
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
        // Schema 3 (S16-5): `/state` publishes all sixteen slots — the
        // binding fails to compile if the two tables' lengths part — and
        // names the unbuilt ones (8–15, S16-R5) with this module's word.
        let published: &[&str; EXEC_SLOTS] = &engine_snapshot::SLOT_NAMES;
        assert_eq!(engine_snapshot::RESERVED_SLOT_NAME, UNBUILT_SLOT_NAME);
        for (i, n) in published.iter().enumerate() {
            if !slot_in(strategy_set::BUILT_MASK, i) {
                assert_eq!(*n, UNBUILT_SLOT_NAME, "unbuilt slot {i}");
            }
        }
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
            R::VenueLock,
            R::MarginRisk,
            R::AloRejectRate,
            R::GatewayHalt,
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

    /// **BX3 (F9):** `/state` spells the retirement `why` from its own
    /// copy of the codes, for the same reason. Pinned here: the words
    /// cover exactly the defined codes, in code order, and the unknown
    /// bucket is the router's.
    #[test]
    fn the_retirement_words_match_the_codes_state_publishes() {
        let codes = [
            (clob_dispatcher::RETIRED_REJECTED, "rejected"),
            (clob_dispatcher::RETIRED_EXPIRED, "expired"),
            (clob_dispatcher::RETIRED_CANCELED_VENUE, "canceled_venue"),
            (clob_dispatcher::RETIRED_CANCELED_TTL, "canceled_ttl"),
            (clob_dispatcher::RETIRED_CANCELED_MEMBER, "canceled_member"),
            (clob_dispatcher::RETIRED_FILLED, "filled"),
        ];
        assert_eq!(codes.len(), engine_snapshot::RETIRED_WHY_WORDS.len());
        for (code, word) in codes {
            assert_eq!(engine_snapshot::RETIRED_WHY_WORDS[code as usize], word);
        }
        assert_eq!(
            engine_snapshot::RETIRED_WHY_UNKNOWN,
            clob_dispatcher::RETIRED_WHY_UNKNOWN
        );
        assert_eq!(
            core::mem::size_of_val(&engine_snapshot::ExecSnapshot::default().retired),
            core::mem::size_of_val(&clob_dispatcher::ExecCounters::default().retired)
        );
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

    /// **PM15-6c** — slot 9 is live on exactly `["polymarket"]` or not at
    /// all, and Polymarket is slot 9's alone. Armed alone it needs no HL
    /// arm; beside a live slot 3 both arms boot; a second venue on slot 9,
    /// another venue in its place, or polymarket on any other slot each
    /// refuse the boot by name.
    #[test]
    fn slot_9_is_live_on_polymarket_alone_and_polymarket_is_slot_9_s_alone() {
        let d = tmp();
        let pm15 = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.9]")
            .replace("name = \"bin15\"", "name = \"pm15\"")
            .replace("[\"hyperliquid\"]", "[\"polymarket\"]");
        let p = write(&d, "exec.toml", &pm15);
        let b = resolve(Some(&p), Some("9")).unwrap().unwrap();
        assert!(b.pm15_live() && !b.hl_arm_needed() && !b.hyparb_live());
        assert!(b.route.venue_live(core_types::VenueId::Polymarket as u8));
        assert!(!b.route.venue_live(core_types::VenueId::Hyperliquid as u8));
        // A second venue beside polymarket: refused.
        let two = pm15.replace("[\"polymarket\"]", "[\"polymarket\", \"hyperliquid\"]");
        let p = write(&d, "exec-two.toml", &two);
        let e = resolve(Some(&p), Some("9")).unwrap_err();
        assert!(
            e.contains("slot 9 (pm15)") && e.contains("exactly [\"polymarket\"]"),
            "{e}"
        );
        // Another venue in its place: refused the same way.
        let hl = pm15.replace("[\"polymarket\"]", "[\"hyperliquid\"]");
        let p = write(&d, "exec-hl.toml", &hl);
        let e = resolve(Some(&p), Some("9")).unwrap_err();
        assert!(e.contains("exactly [\"polymarket\"]"), "{e}");
        // Polymarket on slot 3: refused, naming slot 9's arm.
        let bin15_pm = MINIMAL_LIVE.replace("[\"hyperliquid\"]", "[\"polymarket\"]");
        let p = write(&d, "exec-3pm.toml", &bin15_pm);
        let e = resolve(Some(&p), Some("3")).unwrap_err();
        assert!(
            e.contains("slot 3 (bin15)") && e.contains("slot 9's (pm15)"),
            "{e}"
        );
        // With slot 3 live too: both arms.
        let both = format!("{pm15}{}", MINIMAL_LIVE.replace("[exec]\n", ""));
        let p = write(&d, "exec-both.toml", &both);
        let b = resolve(Some(&p), Some("3,9")).unwrap().unwrap();
        assert!(b.pm15_live() && b.slot_live(3) && b.hl_arm_needed());
        // The two switches still have to agree.
        assert!(
            resolve(Some(&p), Some("3")).is_err(),
            "slot 9 live without --arm-live 9"
        );
        // Paper arms nothing.
        let paper = pm15.replace("mode = \"live\"", "mode = \"paper\"");
        let p = write(&d, "exec-paper.toml", &paper);
        let b = resolve(Some(&p), None).unwrap().unwrap();
        assert!(!b.any_live() && !b.pm15_live());
        std::fs::remove_dir_all(&d).ok();
    }

    /// **PM15-6e** — the section `scripts/pm15-flip.sh live` writes
    /// resolves through the interlock at the frame R-PM-8 names: $250 an
    /// order and a market, the −$500 bound, the day cap out of reach.
    #[test]
    fn the_pm15_flip_writes_a_slot_9_section_the_interlock_resolves() {
        const FLIP: &str = include_str!("../../../scripts/pm15-flip.sh");
        let start = FLIP.find("[exec.slot.9]\n# pm15").expect("the section");
        let end = start + FLIP[start..].find("\nEOF").expect("its end");
        let section = FLIP[start..end]
            .replace("$1", "live")
            .replace("$TS", "test");
        let d = tmp();
        let p = write(&d, "exec.toml", &format!("[exec]\n{section}\n"));
        let b = resolve(Some(&p), Some("9")).unwrap().unwrap();
        assert!(b.pm15_live() && !b.hl_arm_needed());
        let s = b.slots.iter().find(|s| s.slot == PM15_SLOT).unwrap();
        assert_eq!(
            (
                s.max_order_usd_1e6,
                s.cap_instance_usd_1e6,
                s.halt_on_loss_usd_1e6
            ),
            (250_000_000, 250_000_000, 500_000_000)
        );
        assert_eq!(
            s.cap_day_usd_1e6, 100_000_000_000,
            "no day cap: out of reach"
        );
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
            binance: None,
            pine8: None,
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

        // BX3: a slot naming binance also states its margin halt — and
        // only such a slot, so the Hyperliquid line above is unchanged.
        let bn = core_types::VenueId::Binance as u8;
        let mut boot = boot;
        boot.route
            .set_slot(
                3,
                ExecMode::Live,
                &[bn],
                SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64),
                HaltLimits::none(),
            )
            .unwrap();
        boot.slots[0].venues = vec![bn];
        boot.slots[0].halt_on_margin_ratio_1e6 = 600_000;
        let lines = render_boot_tell(&boot);
        assert_eq!(
            lines[2],
            "exec: slot 3 HALTS reject_streak=5 asset_refusals=3 recon_drift=$5 \
             recon_stale_ms=300000 ws_gap_ms=30000 \
             session_bound=+$0/-$0 margin_ratio_1e6=600000 halt_file=/tmp/exec.HALT",
            "no Hyperliquid budget on a Binance-only slot"
        );
    }

    /// **S16-3** — the slot map comes from the member table: every
    /// member's name at its slot, [`UNBUILT_SLOT_NAME`] everywhere else.
    #[test]
    fn slot_names_are_the_member_table_s_and_reserved_past_it() {
        for m in strategy_set::MEMBERS {
            assert_eq!(SLOT_NAMES[m.slot as usize], m.name, "slot {}", m.slot);
        }
        assert_eq!(
            SLOT_NAMES[..8],
            ["hyparb", "vrp", "xsd", "bin15", "ai-exec", "vm", "xmm", "hcv"]
        );
        for (s, n) in SLOT_NAMES.iter().enumerate() {
            let built = strategy_set::BUILT_MASK & (1 << s) != 0;
            assert_eq!(*n == UNBUILT_SLOT_NAME, !built, "slot {s}");
        }
    }

    /// **S16-3** — the masks render and describe slots 11 and 15 as
    /// themselves; `slot_in` is false past the table.
    #[test]
    fn slots_11_and_15_render_and_describe_as_themselves() {
        assert_eq!(render_slot_mask((1 << 11) | (1 << 15)), "[11,15]");
        assert_eq!(render_slot_mask((1 << 3) | (1 << 11)), "[3,11]");
        assert_eq!(render_slot_mask(SlotMask::MAX), "[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]");
        assert_eq!(describe_slots((1 << 3) | (1 << 11)), "3 (bin15), 11 (reserved)");
        assert_eq!(describe_slots(1 << 15), "15 (reserved)");
        assert!(slot_in(1 << 11, 11) && !slot_in(1 << 11, 3));
        assert!(!slot_in(SlotMask::MAX, 16) && !slot_in(SlotMask::MAX, 255));
    }

    /// **PVM ruling 2026-10-16** — slot 8 (pine) is refused in every live
    /// arm: `--arm-live 8` beside a paper artifact, beside a live slot 3,
    /// without `--exec`, and a live `[exec.slot.8]` at parse. A paper or
    /// off slot 8 resolves and arms nothing.
    #[test]
    fn pine_slot_8_is_refused_in_every_live_arm() {
        let d = tmp();
        let paper = write(
            &d,
            "paper.toml",
            "[exec]\n[exec.slot.8]\nmode = \"paper\"\n",
        );
        let e = resolve(Some(&paper), Some("8")).unwrap_err();
        assert!(e.contains("PAPER ONLY") && e.contains("[8]"), "{e}");
        assert!(!e.contains("  "), "{e}");
        let p3 = write(&d, "exec3.toml", MINIMAL_LIVE);
        let e = resolve(Some(&p3), Some("3,8")).unwrap_err();
        assert!(e.contains("PAPER ONLY") && e.contains("[8]"), "{e}");
        assert!(resolve(None, Some("8")).is_err(), "no --exec: refused too");
        let live8 = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.8]")
            .replace("name = \"bin15\"", "name = \"pine\"");
        let p8 = write(&d, "exec8.toml", &live8);
        for flag in [Some("8"), None, Some("")] {
            let e = resolve(Some(&p8), flag).unwrap_err();
            assert!(e.contains("PAPER ONLY"), "{flag:?}: {e}");
        }
        for mode in ["paper", "off"] {
            let src = format!("[exec]\n[exec.slot.8]\nmode = \"{mode}\"\n[exec.slot.8.pine]\n");
            let b = resolve(Some(&write(&d, "ok8.toml", &src)), None)
                .unwrap()
                .unwrap();
            assert_eq!(b.live_mask, 0);
            assert!(!b.slot_live(PINE_SLOT));
        }
        assert_eq!(SLOT_NAMES[PINE_SLOT], "pine");
    }

    /// **S16-R5** — a slot with no built member is never armed, from the
    /// artifact or the flag; a paper section for it still resolves.
    #[test]
    fn an_unbuilt_slot_is_never_armed() {
        let d = tmp();
        let live11 = MINIMAL_LIVE
            .replace("[exec.slot.3]", "[exec.slot.11]")
            .replace("name = \"bin15\"", "name = \"reserved\"");
        let p = write(&d, "exec.toml", &live11);
        let e = resolve(Some(&p), Some("11")).unwrap_err();
        assert!(e.contains("no member") && e.contains("[11]"), "{e}");
        // The message renders on single spaces (its `\` continuations).
        assert!(e.contains("are reserved) ") && e.contains("live, --arm-live"), "{e}");
        assert!(!e.contains("  "), "{e}");
        // The flag alone, beside an artifact arming slot 3: refused as
        // unbuilt, never read as slot 3.
        let p3 = write(&d, "exec3.toml", MINIMAL_LIVE);
        let e = resolve(Some(&p3), Some("3,15")).unwrap_err();
        assert!(e.contains("no member") && e.contains("[15]"), "{e}");
        let e = resolve(Some(&p3), Some("11")).unwrap_err();
        assert!(e.contains("[11]"), "{e}");
        assert!(resolve(Some(&p3), Some("3")).unwrap().unwrap().slot_live(3));
        // A paper section for a reserved slot arms nothing and resolves.
        let paper11 = write(&d, "exec11.toml", "[exec]\n[exec.slot.11]\nmode = \"paper\"\n");
        let b = resolve(Some(&paper11), None).unwrap().unwrap();
        assert_eq!(b.live_mask, 0);
        assert_eq!(b.route.mode_at(11), Some(ExecMode::Paper));
    }

    /// **S16-3** — `ExecBoot`'s live questions read bit `s` as slot `s`
    /// alone: a live 11 or 15 is never slot 3 or 7.
    #[test]
    fn a_live_mask_of_11_and_15_never_reads_as_3_or_7() {
        let mut s3 = core_config::exec::ExecSlot::paper_default(BIN15_SLOT);
        s3.transport = String::from(core_config::exec::TRANSPORT_GATEWAY);
        s3.venues = vec![core_types::VenueId::Hyperliquid as u8];
        let boot = ExecBoot {
            route: ExecRoute::all_paper(),
            hash: [0u8; 32],
            path: PathBuf::from("/tmp/exec.toml"),
            enabled: true,
            live_mask: (1 << 11) | (1 << 15),
            slots: vec![s3],
            binance: None,
            pine8: None,
        };
        assert!(boot.any_live());
        assert!(boot.slot_live(11) && boot.slot_live(15));
        assert!(!boot.slot_live(3) && !boot.slot_live(7) && !boot.slot_live(16));
        assert!(!boot.bin15_gateway_live(), "slot 3 is not live");
        assert!(!boot.hyparb_live());
        assert_eq!(boot.gateway_live(), None);
    }

    /// **S16-3** — the gateway and session-bound checks read a live 11 as
    /// slot 11 alone, in both directions.
    #[test]
    fn the_mask_checks_read_slot_11_as_slot_11() {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let bn = core_types::VenueId::Binance as u8;
        let okx = core_types::VenueId::Okx as u8;
        let mut gw3 = core_config::exec::ExecSlot::paper_default(BIN15_SLOT);
        gw3.mode = String::from("live");
        gw3.transport = String::from(core_config::exec::TRANSPORT_GATEWAY);
        gw3.venues = vec![hl];
        let mut blk11 = core_config::exec::ExecSlot::paper_default(11);
        blk11.mode = String::from("live");
        blk11.venues = vec![hl];
        let slots = [gw3, blk11];
        // Slot 3 on its gateway beside a live slot 11 on the blocking arm:
        // refused, naming 11.
        let e = check_bin15_gateway(&slots, (1 << 3) | (1 << 11)).unwrap_err();
        assert!(e.contains("slot 11 (reserved)"), "{e}");
        // A live 11 alone is not slot 3 on its gateway.
        assert!(check_bin15_gateway(&slots, 1 << 11).is_ok());
        // Slot 3 alone: slot 11's section is paper by the mask.
        assert!(check_bin15_gateway(&slots, 1 << 3).is_ok());

        let mut b11 = core_config::exec::ExecSlot::paper_default(11);
        b11.mode = String::from("live");
        b11.halt_on_loss_usd_1e6 = 5_000_000;
        b11.venues = vec![okx, bn];
        let e = check_pnl_judged(&[b11.clone()], 1 << 11).unwrap_err();
        assert!(e.contains("slot 11"), "{e}");
        assert!(check_pnl_judged(&[b11], 1 << 3).is_ok(), "bit 3 is not slot 11");
    }

    /// **S16-3** — the operator Hyperliquid arm's predicates and the
    /// Binance owner read a live 11 or 15 as that slot alone — never slot 3
    /// (BIN15's) or 7, their `u8` aliases — and a live 3 as before. Every
    /// row is live in the route; the mask alone decides.
    #[test]
    fn the_venue_arm_predicates_read_slots_11_and_15_as_themselves() {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let bn = core_types::VenueId::Binance as u8;
        // Hyperliquid: each row's numbers are its own, so the result names
        // the rows read.
        let mut b = boot_of(&[
            (3, hl, 2_000, 5_000, 30_000, true),
            (7, hl, 9_000, 1, 1, true),
            (11, hl, 3_000, 6_000, 20_000, true),
            (15, hl, 8_000, 2, 2, true),
        ]);
        for (mask, needed, numbers) in [
            (1 << 11, true, (3_000, 6_000, 20_000)),
            (1 << 15, true, (8_000, 2, 2)),
            ((1 << 11) | (1 << 15), true, (8_000, 2, 2)),
            (1 << 3, true, (2_000, 5_000, 30_000)),
            (0, false, (0, 0, 0)),
        ] {
            b.live_mask = mask;
            assert_eq!(b.hl_arm_needed(), needed, "{mask:#06x}");
            assert_eq!(b.hl_address_numbers(), numbers, "{mask:#06x}");
            assert_eq!(b.binance_slot(), None, "{mask:#06x}");
        }
        // Binance: the one owner, and `check_binance`'s reading of it.
        let mut b = boot_of(&[
            (3, bn, 0, 0, 0, true),
            (7, bn, 0, 0, 0, true),
            (11, hl, 0, 0, 0, true),
            (15, bn, 0, 0, 0, true),
        ]);
        for (mask, owner) in [
            (1 << 15, Some(15)),
            ((1 << 11) | (1 << 15), Some(15)),
            (1 << 11, None),
            (1 << 3, Some(3)),
        ] {
            b.live_mask = mask;
            assert_eq!(b.binance_slot(), owner, "{mask:#06x}");
        }
        let d = tmp();
        let p = write(&d, "exec.toml", &format!("{}{BN_SECTION}", bn_live()));
        let section = resolve(Some(&p), Some("3")).unwrap().unwrap().binance;
        let s15 = b.slots.iter_mut().find(|s| s.slot == 15).unwrap();
        s15.halt_on_recon_stale_ms = 1;
        // Slot 15 owns the arm: its own stale bound is judged, naming it.
        let e = check_binance(&b.slots, (1 << 11) | (1 << 15), section.as_ref()).unwrap_err();
        assert!(e.contains("slot 15's halt_on_recon_stale_ms"), "{e}");
        let e = check_binance(&b.slots, (1 << 3) | (1 << 15), section.as_ref()).unwrap_err();
        assert!(e.contains("slots [3, 15]"), "{e}");
        // A live 11 (Hyperliquid) is not slot 3 on Binance; a live 3 owns
        // the arm as before.
        assert!(check_binance(&b.slots, 1 << 11, None).is_ok());
        assert!(check_binance(&b.slots, 1 << 3, section.as_ref()).is_ok());
        let e = check_binance(&b.slots, 1 << 3, None).unwrap_err();
        assert!(e.contains("[exec.binance] section"), "{e}");
        std::fs::remove_dir_all(&d).ok();
    }

    /// **S16-3** — `--halt-slot`'s mask halts slot 11 alone and reports
    /// slot 15 (not live) as not halted; BIN15's slot 3 and slot 7 are
    /// untouched.
    #[test]
    fn a_halt_mask_of_11_and_15_halts_11_alone() {
        let hl = core_types::VenueId::Hyperliquid as u8;
        let caps = SlotCaps::new(100_000_000, 1_000_000_000, 30_000_000_000, 64);
        let mut route = ExecRoute::all_paper();
        for s in [3usize, 7, 11] {
            route.set_slot(s, ExecMode::Live, &[hl], caps, HaltLimits::none()).unwrap();
        }
        let mut d = exec_router::RoutedDispatcher::new(
            route,
            clob_dispatcher::PaperDispatcher::new(),
            exec_router::NullLiveDispatcher::new(),
            core_time::WallAnchor::new(1, 1),
        );
        let (halted, not_live) = apply_halt_mask(&mut d, (1 << 11) | (1 << 15));
        assert_eq!(halted, 1 << 11);
        assert_eq!(not_live, 1 << 15);
        assert!(d.halt().is_halted(11));
        assert!(!d.halt().is_halted(3), "a halt of 11 never lands on BIN15's 3");
        assert!(!d.halt().is_halted(7) && !d.halt().is_halted(15));
        let (halted, not_live) = apply_halt_mask(&mut d, (1 << 3) | (1 << 11));
        assert_eq!(halted, 1 << 3);
        assert_eq!(not_live, 1 << 11, "already halted: nothing to latch");
        assert_eq!(apply_halt_mask(&mut d, 0), (0, 0));
    }

    #[test]
    fn arm_live_parsing() {
        assert_eq!(parse_arm_live("").unwrap(), 0);
        assert_eq!(parse_arm_live("3").unwrap(), 0b0000_1000);
        assert_eq!(parse_arm_live("3,5").unwrap(), 0b0010_1000);
        assert_eq!(parse_arm_live(" 3 , 5 ").unwrap(), 0b0010_1000);
        // S16-3: 16 is the first slot past the table (8 was, at eight).
        assert!(parse_arm_live("16").unwrap_err().contains("out of range"));
        assert!(parse_arm_live("255").unwrap_err().contains("out of range"));
        // Slot 11 arms bit 11 alone — never bit 3, BIN15's (its `u8` alias).
        assert_eq!(parse_arm_live("11").unwrap(), 1 << 11);
        assert_eq!(parse_arm_live("11").unwrap() & (1 << 3), 0);
        assert_eq!(parse_arm_live("15").unwrap(), 1 << 15);
        assert_eq!(parse_arm_live("3,11").unwrap(), (1 << 3) | (1 << 11));
        assert!(parse_arm_live("3,11,3").unwrap_err().contains("named twice"));
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
        // S16-3: sixteen slots (the boot tell's `slots=16`).
        assert!(l.contains("slots=16"), "{l}");
        assert!(l.contains("enabled=1"), "{l}");
        assert!(l.contains("live=[]"), "{l}");
        assert!(l.contains("off=[6]"), "{l}");
        assert!(l.contains("paper=[0,1,2,3,4,5,7,8,9,10,11,12,13,14,15]"), "{l}");
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
