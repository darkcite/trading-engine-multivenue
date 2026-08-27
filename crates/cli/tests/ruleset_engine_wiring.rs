// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! Phase 8g item 7 — §11 integration: the ruleset table's full path
//! through the REAL engine loop.
//!
//! `RulesetSidePath::stage` (validate → ring push, copy #1) → the
//! engine's pre-AI-drain pop (`Engine::tick` → `on_ruleset_table` →
//! the set's vm member, copy #2) → in-stream `RulesetCommit` flip →
//! committed rows FIRE on venue ticks through `on_tick` → orders
//! reach the dispatcher. Everything from the ring boundary inward is
//! the production shape: `Engine<StrategySet, PaperDispatcher>` with
//! every lane wired exactly as `run_engine_loop` wires it.
//!
//! Clock note (G3 first-window lesson): the engine stamps callbacks
//! from `core_time::now_ns()` — production wallclock (≥ 1e17 ns), so
//! fresh cooldown stamps arm naturally; no synthetic clock here.
//!
//! No sockets, no captures, no metrics: the listener/UDS half has its
//! own 8f/G1 suites in `ingress-ai`; this file owns the §6 handoff
//! and flip ordering (the §11 rows for item 7).

use clob_dispatcher::{OrderDispatch, PaperDispatcher};
use core_ring::{Producer, Ring};
use core_time::now_ns;
use core_types::{
    AiCmd, AiCmdKind, Price, Qty, RuleTableSlot, Tick, VenueId, AI_RING_SIZE, AI_SIDE_NONE,
    RULE_TABLE_RING_SLOTS, STRATEGY_SLOT_VM, SYMBOL_ID_NONE,
};
use engine::{
    Engine, FILL_RING_SIZE, NUM_FILL_LANES, NUM_TICK_LANES, SIGNAL_RING_SIZE, TICK_RING_SIZE,
};
use ingress_ai::{AiIngressStatus, RulesetSidePath};
use std::path::PathBuf;
use std::sync::Arc;
use strategy_set::{StrategySet, BIT_VM};

// The lane arrays below are written out for the Phase 8a geometry;
// break the build loudly if that drifts.
const _: () = assert!(NUM_TICK_LANES == 5 && NUM_FILL_LANES == 4);

/// Raw Polymarket SymbolId (venue byte 0) — the boot-universe shape
/// `build_ai_universe` produces for `--polymarket-sym-id`.
const PM_SYM: u32 = 11;

/// One-row level_breach ruleset: side `bid` fires when best ask ≤
/// `level` (buy at/below). `level` distinguishes A (0.5) from B (0.4).
fn ruleset_json(name: &str, level: &str) -> String {
    format!(
        r#"{{"rows":[{{"name":"{name}","family":"crypto","trigger":{{"type":"level_breach","level":{level}}},"sym":{PM_SYM},"side":"bid","edge_bps":0,"horizon_ms":60000,"max_risk_usd":50.0}}]}}"#
    )
}

/// Operator install step: write the artifact under its
/// `<hash128-hex>.json` name; return the hash128 the frames carry.
fn install(dir: &PathBuf, bytes: &[u8]) -> [u8; 16] {
    let digest = core_crypto::sha256(bytes);
    let mut h = [0u8; 16];
    h.copy_from_slice(&digest[..16]);
    let mut name = String::with_capacity(37);
    for b in &h {
        name.push_str(&format!("{b:02x}"));
    }
    name.push_str(".json");
    std::fs::write(dir.join(name), bytes).expect("write ruleset artifact");
    h
}

/// Ruleset Stage/Commit frame — hash128 rides the px/qty halves
/// (`AiCmd::ruleset_hash128` pairing), `ttl_ns = 0` (pinned: ruleset
/// frames never expire in-ring), slot 5.
fn ruleset_cmd(kind: AiCmdKind, seq: u32, hash128: [u8; 16]) -> AiCmd {
    let px = i64::from_le_bytes(hash128[..8].try_into().expect("8 bytes"));
    let qty = i64::from_le_bytes(hash128[8..].try_into().expect("8 bytes"));
    AiCmd::new(
        now_ns(),
        seq,
        SYMBOL_ID_NONE,
        px,
        qty,
        0,
        kind,
        VenueId::Ai,
        STRATEGY_SLOT_VM,
        AI_SIDE_NONE,
        0,
        0,
    )
}

fn pm_tick(seq: u32, bid_1e6: i64, ask_1e6: i64) -> Tick {
    Tick::new(
        now_ns(),
        VenueId::Polymarket,
        PM_SYM,
        seq,
        Price::from_raw(bid_1e6),
        Qty::from_raw(1_000_000),
        Price::from_raw(ask_1e6),
        Qty::from_raw(1_000_000),
    )
}

struct Harness {
    side: RulesetSidePath,
    status: Arc<AiIngressStatus>,
    eng: Engine<StrategySet, PaperDispatcher>,
    pm_prod: Producer<Tick, TICK_RING_SIZE>,
    ai_prod: Producer<AiCmd, AI_RING_SIZE>,
    dir: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Production wiring in miniature: ONE table ring shared by the side
/// path (producer, ingress thread in real boots) and the engine
/// (consumer, pre-AI-drain pop) — exactly the bin's split; the AI
/// lane producer stands in for the listener (frames enter the ring
/// the same way after HMAC verify).
fn harness(tag: &str) -> Harness {
    let dir = std::env::temp_dir().join(format!("cw-ai-g5-wiring-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create ruleset dir");

    let status = Arc::new(AiIngressStatus::new());
    let (table_prod, table_cons) =
        Ring::<RuleTableSlot, RULE_TABLE_RING_SLOTS>::new().split();
    let universe: Arc<[u32]> = Arc::from(vec![PM_SYM]);
    let side = RulesetSidePath::new(dir.clone(), Arc::clone(&status), universe, table_prod);

    let (pm_prod, t0) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t1p, t1) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t2p, t2) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t3p, t3) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_t4p, t4) = Ring::<Tick, TICK_RING_SIZE>::new().split();
    let (_sp, sc) = Ring::<core_types::Signal, SIGNAL_RING_SIZE>::new().split();
    let (_f0p, f0) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f1p, f1) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f2p, f2) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (_f3p, f3) = Ring::<core_types::Fill, FILL_RING_SIZE>::new().split();
    let (ai_prod, ai_cons) = Ring::<AiCmd, AI_RING_SIZE>::new().split();

    let mut eng = Engine::new(
        StrategySet::new(BIT_VM),
        PaperDispatcher::new(),
        [t0, t1, t2, t3, t4],
        sc,
        [f0, f1, f2, f3],
        ai_cons,
        Arc::clone(&status),
        table_cons,
    );
    eng.start().expect("set on_start");

    Harness {
        side,
        status,
        eng,
        pm_prod,
        ai_prod,
        dir,
    }
}

/// Listener step-8 order for a Stage: the frame enters the AI ring
/// FIRST, then the seam runs (validate → table push) — so in-ring the
/// Stage cmd precedes the table in real time (§4.4/§6).
fn stage(h: &mut Harness, seq: u32, hash128: [u8; 16]) {
    let cmd = ruleset_cmd(AiCmdKind::RulesetStage, seq, hash128);
    h.ai_prod.try_push(cmd).expect("stage cmd push");
    h.side.on_cmd(&cmd);
}

/// §11 happy path, same-batch §6 race note included: install → Stage
/// (ring push) → Commit frame queued BEFORE any engine iteration →
/// ONE `tick()` pops the table AND flips → venue ticks fire the
/// committed row → the order reaches the dispatcher.
#[test]
fn staged_table_commits_and_fires_through_engine_loop() {
    let mut h = harness("happy");
    let hash_a = install(&h.dir, ruleset_json("g5-a", "0.5").as_bytes());

    stage(&mut h, 1, hash_a);
    assert_eq!(h.status.ruleset_staged(), 1, "side path staged");
    assert_eq!(h.side.staged(), Some(hash_a));

    // Same batch: the Commit is already in the AI ring before the
    // engine runs at all. Pop-precedes-AI-drain makes this flip in
    // ONE iteration.
    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 2, hash_a))
        .expect("commit cmd push");
    h.eng.tick(16);

    assert_eq!(h.eng.ai_dispatched, 2, "Stage + Commit dispatched");
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.commits_applied, 1, "same-batch stage+commit flips (§6)");
    assert_eq!(vm.commits_dropped, 0);
    assert_eq!(vm.rows_active(), 1);
    assert_eq!(vm.active_hash128(), hash_a);
    assert_eq!(vm.active_epoch(), 1, "side-path epochs are gapless from 1");
    assert_eq!(vm.staged_hash128(), None, "flip consumes the staged buffer");

    // Committed row fires: bid row, best ask 0.49 ≤ level 0.5.
    h.pm_prod.try_push(pm_tick(1, 480_000, 490_000)).expect("tick push");
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.fires, 1, "committed row fired through the engine loop");
    assert_eq!(vm.orders_emitted, 1);
    assert_eq!(
        h.eng.dispatcher().stats().accepted,
        1,
        "vm order reached the dispatcher"
    );
}

/// Failure leg: a Commit with NOTHING staged engine-side drops in the
/// vm member (counted), activates nothing, and later ticks fire
/// nothing.
#[test]
fn commit_without_staged_table_drops_through_engine_loop() {
    let mut h = harness("nostage");
    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 1, [0x5A; 16]))
        .expect("commit cmd push");
    h.eng.tick(16);

    let vm = h.eng.strategy().vm();
    assert_eq!(vm.commits_dropped, 1, "no staged table ⇒ Commit dropped");
    assert_eq!(vm.commits_applied, 0);
    assert_eq!(vm.rows_active(), 0, "still inert (§7.3)");

    h.pm_prod.try_push(pm_tick(1, 480_000, 490_000)).expect("tick push");
    h.eng.tick(16);
    assert_eq!(h.eng.strategy().vm().fires, 0);
    assert_eq!(h.eng.dispatcher().stats().accepted, 0);
}

/// Failure leg: a mismatched Commit drops the COMMIT, not the staged
/// table — the staged table survives for a later correct Commit
/// (G3 interpretation, now pinned through the engine loop).
#[test]
fn mismatched_commit_drops_and_staged_survives_through_engine_loop() {
    let mut h = harness("mismatch");
    let hash_a = install(&h.dir, ruleset_json("g5-a", "0.5").as_bytes());

    stage(&mut h, 1, hash_a);
    h.eng.tick(16);
    assert_eq!(h.eng.strategy().vm().staged_hash128(), Some(hash_a), "pop staged");

    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 2, [0x5A; 16]))
        .expect("commit cmd push");
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.commits_dropped, 1, "mismatch drops the Commit");
    assert_eq!(vm.commits_applied, 0);
    assert_eq!(vm.staged_hash128(), Some(hash_a), "staged survives");

    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 3, hash_a))
        .expect("commit cmd push");
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.commits_applied, 1, "correct Commit still lands");
    assert_eq!(vm.rows_active(), 1);
}

/// Restage supersedes through the whole pipe: two Stages queue two
/// ring slots; one engine iteration drains to the NEWEST; the old
/// hash no longer commits, the new one does, and the fire threshold
/// proves table B (level 0.4) is what runs — a book that only A
/// (level 0.5) would fire on stays quiet.
#[test]
fn restage_supersedes_and_newest_table_runs_through_engine_loop() {
    let mut h = harness("restage");
    let hash_a = install(&h.dir, ruleset_json("g5-a", "0.5").as_bytes());
    let hash_b = install(&h.dir, ruleset_json("g5-b", "0.4").as_bytes());

    stage(&mut h, 1, hash_a);
    stage(&mut h, 2, hash_b);
    assert_eq!(h.status.ruleset_staged(), 2);
    assert_eq!(h.side.staged(), Some(hash_b), "side path: restage supersedes");

    // ONE iteration drains both slots in order; vm keeps the newest.
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.staged_hash128(), Some(hash_b), "engine-side supersede mirror");

    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 3, hash_a))
        .expect("commit cmd push");
    h.ai_prod
        .try_push(ruleset_cmd(AiCmdKind::RulesetCommit, 4, hash_b))
        .expect("commit cmd push");
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.commits_dropped, 1, "superseded hash must not commit");
    assert_eq!(vm.commits_applied, 1, "newest hash commits");
    assert_eq!(vm.active_hash128(), hash_b);
    assert_eq!(vm.active_epoch(), 2, "epoch 2 = second successful Stage");

    // Ask 0.45: at/below A's level (0.5) but ABOVE B's (0.4) — if A
    // were live this would fire; B stays quiet.
    h.pm_prod.try_push(pm_tick(1, 440_000, 450_000)).expect("tick push");
    h.eng.tick(16);
    assert_eq!(h.eng.strategy().vm().fires, 0, "A's threshold must be gone");

    // Ask 0.39 ≤ 0.4: B fires.
    h.pm_prod.try_push(pm_tick(2, 380_000, 390_000)).expect("tick push");
    h.eng.tick(16);
    let vm = h.eng.strategy().vm();
    assert_eq!(vm.fires, 1, "B's committed row fires");
    assert_eq!(h.eng.dispatcher().stats().accepted, 1);
}
