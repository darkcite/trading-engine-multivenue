// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! B'3 T13: `backtest --strategy pine` end to end on our hand-written
//! fixture scripts (`tests/fixtures/pine-bp/`) over a tiny local cut root
//! built here (impl-Bp §7.4 names; pin:Bp-b3-tests-in-crate).

use std::path::{Path, PathBuf};

use core_io::{PmlrWriter, SlotKind};
use core_types::{make_symbol_id, ChannelEvent, ChannelId, Price, Qty, Tick, VenueId};

use super::{run_pine_main, PineArgv};

const EA: u64 = 1_767_225_600_000_000_000;
const SEC: u64 = 1_000_000_000;
const STEP: u64 = 500_000_000;
const SYM: u32 = make_symbol_id(VenueId::Hyperliquid, 0);

fn script_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pine-bp")
}

/// One cut root: an HL quote and a print every 500 ms over `[from, to)`
/// (offsets from the run epoch), price rising by 0.01 per step.
fn write_cut(dir: &Path, from: u64, to: u64) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("instrument-manifest.tsv"),
        format!("{SYM}\thyperliquid:BTC\n"),
    )
    .unwrap();
    let mut wt = PmlrWriter::open(dir.join("hl-ticks.pmlr"), SlotKind::Tick, EA).unwrap();
    let mut we = PmlrWriter::open(dir.join("hl-events.pmlr"), SlotKind::Event, EA).unwrap();
    let mut ts = from;
    let mut k = (from / STEP) as i64;
    while ts < to {
        let px = 100_000_000 + k * 10_000;
        let mut t = Tick::new(
            ts + 1,
            VenueId::Hyperliquid,
            SYM,
            k as u32 + 1,
            Price::from_raw(px - 10_000),
            Qty::from_raw(50_000_000),
            Price::from_raw(px + 10_000),
            Qty::from_raw(50_000_000),
        );
        t.venue_time_ms = (EA + ts + 1) / 1_000_000;
        wt.append(&t).unwrap();
        we.append(&ChannelEvent::new(
            ts,
            VenueId::Hyperliquid,
            ChannelId::Trade,
            SYM,
            0,
            (EA + ts) / 1_000_000,
            px,
            1_000_000,
        ))
        .unwrap();
        ts += STEP;
        k += 1;
    }
    wt.flush().unwrap();
    we.flush().unwrap();
}

struct Fx {
    root: PathBuf,
}

impl Drop for Fx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Run A cut at `bounds` (seconds), the manifest for `file`, the universe.
fn fixture(tag: &str, file: &str, bounds: &[u64]) -> (Fx, PineArgv) {
    let root = std::env::temp_dir().join(format!("cli-pine-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let mut plan = String::from("# pvm-cutplan-1\n");
    for k in 0..bounds.len() - 1 {
        let dir = root.join(format!("c{k}"));
        write_cut(&dir, bounds[k] * SEC, bounds[k + 1] * SEC);
        plan.push_str(&format!(
            "{k}\trun-{EA}\t{EA}\t{}\t{}\t{}\n",
            bounds[k] * SEC,
            bounds[k + 1] * SEC,
            dir.display()
        ));
    }
    std::fs::write(root.join("plan.tsv"), plan).unwrap();
    let src = std::fs::read(script_dir().join(file)).unwrap();
    let sha: String = core_crypto::sha256(&src)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(
        root.join("manifest.toml"),
        format!(
            "[[script]]\nsha256 = \"{sha}\"\nfile = \"{file}\"\ndialect = 6\nenabled = 1\n\
             mode = \"bar_close\"\nsymbol = \"hyperliquid:btc\"\ntimeframe_s = 60\n\
             venue = \"hyperliquid\"\nmax_bars_back = 5000\ncoalesce_ms = 250\n\
             coalesce_bps = 0\ninsn_budget = 2000000\nqty_notional_1e6 = 12000000\n\
             limit_primitive = \"ioc\"\ncross_ticks = 2\ncap_instance_1e6 = 12000000\n\
             cap_day_1e6 = 480000000\nmax_open = 4\ncollection_cap = 4096\n\
             str_persist_bytes = 4096\nstr_cell_bytes = 128\nstr_scratch_bytes = 8192\n\
             intent_cap = 32\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("screen-universe.tsv"),
        "hyperliquid:btc\t-\thyperliquid:BTC\t4\tperp\thl\t10000\t1000000\t1000\t10000000\t\
         BTC\tBTC\tUSDC\tcrypto\n",
    )
    .unwrap();
    let argv = PineArgv {
        manifest: root.join("manifest.toml"),
        pine_dir: Some(script_dir()),
        screen_universe: Some(root.join("screen-universe.tsv")),
        cut_plan: Some(root.join("plan.tsv")),
        cut_wait_s: 900,
        split: Some("70/30".to_owned()),
        fee_bps: vec!["hl.perp:2:5".to_owned()],
        null_arm: vec!["sign-flip".to_owned()],
        null_seed: Some(7),
        null_replicates: 1,
        trial_id: Some("ab".repeat(32)),
        out_dir: Some(root.join("out")),
        ..PineArgv::default()
    };
    (Fx { root }, argv)
}

fn field(json: &str, key: &str) -> i128 {
    let k = format!("\"{key}\":");
    let at = json.find(&k).unwrap_or_else(|| panic!("{key} in {json}")) + k.len();
    let tail = &json[at..];
    let end = tail.find([',', '}']).unwrap();
    tail[..end].parse().unwrap()
}

/// I15: one member and one broker across cuts — a position opened in one
/// cut is managed and closed in the next.
#[test]
fn a_hold_opened_in_one_cut_closes_in_the_next() {
    let (fx, a) = fixture("i15", "hold.pine", &[0, 90, 150, 270, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{} {}", x.stderr, x.stdout);
    assert_eq!(field(&x.stdout, "cuts"), 4);
    assert!(field(&x.stdout, "closed_trades") >= 2, "{}", x.stdout);
    let plan = std::fs::read(fx.root.join("plan.tsv")).unwrap();
    let plan = super::super::cuts::parse_cut_plan(&plan).unwrap();
    let edges: Vec<i64> = plan
        .rows
        .iter()
        .map(|r| (r.wall_to_ns() / 1_000_000) as i64)
        .collect();
    let crossed = x
        .closed_ms
        .iter()
        .any(|(e, t)| edges.iter().any(|b| *e < *b && *t >= *b));
    assert!(
        crossed,
        "a trade spans a cut edge: {:?} edges {edges:?}",
        x.closed_ms
    );
    assert!(fx.root.join("out/fees.tsv").is_file());
}

/// I16: a script fault stops the replay at that record and exits 4 with
/// the `(FaultKind, pc, bar, sha)` print; every arm's row is written
/// FAULTED and no series (T15).
#[test]
fn a_script_fault_aborts_writes_faulted_rows_and_exits_4() {
    let (fx, a) = fixture("i16", "fault.pine", &[0, 90, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 4, "{} {}", x.stderr, x.stdout);
    assert!(x.stderr.contains("InsnBudget"), "{}", x.stderr);
    assert!(x.stderr.contains("sha "), "{}", x.stderr);
    assert!(x.stdout.contains("\"faulted\":true"), "{}", x.stdout);
    let rows = pine_screen::rows::RowsFile::open(&fx.root.join("out/rows.t2")).unwrap();
    assert_eq!(rows.recs().len(), 1);
    let s = rows.recs()[0].summary;
    assert_ne!(s.flags & pine_ir::trial_flag::FAULTED, 0);
    assert_eq!((s.faults, s.obs_days, s.net_bps_1e4), (1, 0, 0));
    let ret = pine_screen::ret::ret_path(&fx.root.join("out/ret"), &[0xab; 32]);
    assert!(!ret.exists(), "a FAULTED row has no series");
    assert!(x.stdout.contains("\"ret_sha256\":\"\""), "{}", x.stdout);
}

/// I17: one `RowRec` per arm in trial-id order in one `RowsHdr { tier: 2 }`
/// file with the run's notional and fee digest; one series per non-FAULTED
/// arm with `obs_days` values. (The nulls' rows follow the signal's from T16.)
#[test]
fn one_row_and_one_series_per_arm_in_trial_id_order() {
    let (fx, a) = fixture("i17a", "hold.pine", &[0, 90, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{} {}", x.stderr, x.stdout);
    let rows = pine_screen::rows::RowsFile::open(&fx.root.join("out/rows.t2")).unwrap();
    let h = rows.hdr();
    assert_eq!(h.tier, 2);
    assert_eq!(h.sizing_notional_1e6, 12_000_000);
    assert_eq!(h.fee_digest, pine_screen::ids::fee_digest(&["hl.perp:2:5"]));
    assert_eq!(rows.recs().len(), field(&x.stdout, "rows") as usize);
    assert_eq!(rows.recs()[0].trial_id, [0xab; 32], "signal first");
    let s = rows.recs()[0].summary;
    assert_eq!(s.obs_days, 1, "the fixture spans one UTC day");
    let ret = pine_screen::ret::ret_path(&fx.root.join("out/ret"), &[0xab; 32]);
    let days = pine_screen::ret::read_ret(&ret).unwrap();
    assert_eq!(days.len(), s.obs_days as usize);
    let sha: String = pine_screen::ret::ret_sha256(&days)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(
        x.stdout.contains(&format!("\"ret_sha256\":\"{sha}\"")),
        "{}",
        x.stdout
    );
}

/// I17: the row is `TrialAccum::finish`'s — `FIXED_NOTIONAL` at the
/// manifest's notional, `gross = net + fee` exactly, the series sums to the
/// net, every closed broker trade counted, the fee from the R8 commission.
#[test]
fn rows_come_from_trial_accum_with_impl07s_identities() {
    let (fx, a) = fixture("i17b", "hold.pine", &[0, 90, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{} {}", x.stderr, x.stdout);
    let rows = pine_screen::rows::RowsFile::open(&fx.root.join("out/rows.t2")).unwrap();
    let s = rows.recs()[0].summary;
    assert_ne!(s.flags & pine_ir::trial_flag::FIXED_NOTIONAL, 0);
    assert_eq!(s.flags & pine_ir::trial_flag::FAULTED, 0);
    assert_eq!(s.sizing_notional_1e6, 12_000_000);
    assert_eq!(s.gross_bps_1e4, s.net_bps_1e4 + s.fee_bps_1e4);
    assert!(s.closed >= 1, "{}", x.stdout);
    assert_eq!(i128::from(s.closed), field(&x.stdout, "closed_trades"));
    assert!(s.fee_bps_1e4 > 0, "R8: the broker booked the harness fees");
    let ret = pine_screen::ret::ret_path(&fx.root.join("out/ret"), &[0xab; 32]);
    let days = pine_screen::ret::read_ret(&ret).unwrap();
    let sum: f64 = days.iter().sum();
    assert_eq!((sum * 10_000.0).round() as i64, s.net_bps_1e4);
    assert_eq!(field(&x.stdout, "net_bps_1e4"), i128::from(s.net_bps_1e4));
}

/// pin:Bp-fee-drain-per-cut (risk-review nit): the model's pine fee charges
/// are drained per cut, so no drain holds the whole run's fills.
#[test]
fn fee_charges_drain_per_cut_not_per_run() {
    let (_fx, a) = fixture("drain", "hold.pine", &[0, 90, 150, 270, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{} {}", x.stderr, x.stdout);
    let rows = field(&x.stdout, "fee_rows") as usize;
    assert!(rows >= 4, "{}", x.stdout);
    assert!(x.max_fee_drain >= 1);
    assert!(
        x.max_fee_drain < rows,
        "one drain carried {} of {rows} charges",
        x.max_fee_drain
    );
}

/// T16: the null arms are parsed at boot — a `--null-seeds` file of the
/// wrong length refuses before any record (exit 6, nothing written), and
/// `--trial-id` derives one id per null arm.
#[test]
fn null_arms_are_parsed_before_the_first_record() {
    let (fx, mut a) = fixture("nullcfg", "hold.pine", &[0, 90]);
    std::fs::write(fx.root.join("seeds.txt"), "1\n2\n3\n").unwrap();
    a.null_seed = None;
    a.null_seeds = Some(fx.root.join("seeds.txt"));
    a.null_replicates = 2;
    let x = run_pine_main(&a);
    assert_eq!(x.code, 6, "{}", x.stderr);
    assert!(x.stderr.contains("need 2"), "{}", x.stderr);
    assert!(!fx.root.join("out").exists(), "nothing written");
    a.null_seeds = None;
    a.null_seed = Some(3);
    a.trial_ids = None;
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{}", x.stderr);
    assert_eq!(
        field(&x.stdout, "trials"),
        3,
        "the signal + 2 sign-flip replicates"
    );
}

/// pin:Bp-trial-ids-count: `--trial-ids` must name the signal and every
/// null arm.
#[test]
fn trial_ids_must_name_every_arm() {
    let (fx, mut a) = fixture("tids", "hold.pine", &[0, 90]);
    std::fs::write(fx.root.join("ids.txt"), format!("{}\n", "cd".repeat(32))).unwrap();
    a.trial_id = None;
    a.trial_ids = Some(fx.root.join("ids.txt"));
    let x = run_pine_main(&a);
    assert_eq!(x.code, 6, "{}", x.stderr);
    assert!(x.stderr.contains("need 2"), "{}", x.stderr);
    std::fs::write(
        fx.root.join("ids.txt"),
        format!("{}\n{}\n", "cd".repeat(32), "ef".repeat(32)),
    )
    .unwrap();
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{}", x.stderr);
}

/// `--rows-out` / `--ret-dir` / `--report-out` land where named; the report
/// file holds the stdout line.
#[test]
fn rows_ret_and_report_paths_are_honoured() {
    let (fx, mut a) = fixture("paths", "hold.pine", &[0, 90, 330]);
    a.rows_out = Some(fx.root.join("elsewhere/r.t2"));
    a.ret_dir = Some(fx.root.join("series"));
    a.report_out = Some(fx.root.join("report.jsonl"));
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{} {}", x.stderr, x.stdout);
    assert!(fx.root.join("elsewhere/r.t2").is_file());
    assert!(pine_screen::ret::ret_path(&fx.root.join("series"), &[0xab; 32]).is_file());
    let rep = std::fs::read_to_string(fx.root.join("report.jsonl")).unwrap();
    assert_eq!(rep.trim_end(), x.stdout);
}

/// I18: a manifest with two scripts refuses before any record (exit 6),
/// nothing written.
#[test]
fn a_manifest_with_two_scripts_exits_6() {
    let (fx, a) = fixture("two", "hold.pine", &[0, 90]);
    let m = std::fs::read_to_string(&a.manifest).unwrap();
    std::fs::write(
        &a.manifest,
        format!("{m}{}", m.replace("hyperliquid:btc", "hyperliquid:eth")),
    )
    .unwrap();
    let x = run_pine_main(&a);
    assert_eq!(x.code, 6, "{}", x.stderr);
    assert!(!fx.root.join("out").exists(), "nothing written");
}

/// pin:Bp-screen-universe-flag: no screen universe, no facts — exit 6.
#[test]
fn a_missing_screen_universe_exits_6() {
    let (fx, mut a) = fixture("nouni", "hold.pine", &[0, 90]);
    a.screen_universe = Some(fx.root.join("absent.tsv"));
    let x = run_pine_main(&a);
    assert_eq!(x.code, 6, "{}", x.stderr);
    assert!(x.stderr.contains("screen-universe"), "{}", x.stderr);
    assert!(!fx.root.join("out").exists());
}

/// R8 through the drive: the broker's commission is the model's fee sum
/// (the `fees.tsv` ledger's).
#[test]
fn the_broker_commission_is_the_fee_ledger_sum() {
    let (_fx, a) = fixture("r8", "hold.pine", &[0, 90, 330]);
    let x = run_pine_main(&a);
    assert_eq!(x.code, 0, "{}", x.stderr);
    let fee = field(&x.stdout, "fee_1e12");
    assert!(fee > 0, "{}", x.stdout);
    assert_eq!(field(&x.stdout, "commission_1e12"), fee, "{}", x.stdout);
}
