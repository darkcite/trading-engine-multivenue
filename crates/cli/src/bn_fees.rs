// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **BX11 — `multivenue-engine bn-fees <run-dir>…`** (BX-14): the Binance
//! commission, measured from the arm's own journal.
//!
//! Reads `binance-exec.pmlr` in each run directory given (or in every
//! `run-*` under a root), never writes anything. Every [`J_FEE`] record —
//! one per trade, the commission the venue charged — of an order of the
//! arm's (the member's, or an earlier boot's) is counted in its **class**:
//! the product × perpetual / dated × TradFi × maker / taker. Excluded,
//! and counted apart: a settlement's, a liquidation's or an ADL's (not the
//! maker/taker schedule); a commission in another asset than the row's
//! settlement asset (BNB — the BNB burn is on — or another: no rate without
//! a price for it) or naming none; a trade of no notional; a Binance Stocks
//! order's fee at the platform's $0.35 minimum (BX10: `FEE_FLAG_EQ_FLOOR`
//! — a floor, not a rate). Fills booked
//! without their trade (`J_FILL_UNSEEN`, `J_FILL_ADOPTED`) or lost
//! (`J_FILL_LOST`) carry no `J_FEE`: nothing to exclude.
//!
//! Per class: the trades, the notional and the commission in the
//! settlement asset, the rate (`Σ commission ÷ Σ notional`, in bps), and
//! whether the class has reached `N` commission-bearing trades (BX-14:
//! "N commission-bearing fills, counted in fills, never days"; N = 30
//! unless the operator rules). It prints the `fees.toml` rows the measured
//! classes support — `[fees.bn]`'s `perp` and `dated`, whole bps rounded UP
//! (the file's law L3), a rebate as 0 (the file's grammar is unsigned, and
//! 0 is a rebate's safe side) — and never writes `fees.toml` (the
//! operator's ruling, Q2). TradFi's classes are shown and make no row: the
//! file has no TradFi class (a TradFi perpetual is charged as `perp`). A
//! journal named twice (a root and a run under it) is read once. Journals
//! written before BX11 carry no `J_FEE`: they measure nothing.
//!
//! COPY-DOCTRINE: a cold operator tool over files, never reachable from
//! the engine loop: it allocates and copies freely.

use std::path::{Path, PathBuf};

use core_io::pmlr::SlotKind;
use core_io::PmlrReader;
use core_types::ExecRecord;
use exec_binance::inst::PRODUCT_COINM;
use exec_binance::journal::{
    fee_fields, FEE_ASSET_BNB, FEE_ASSET_NONE, FEE_ASSET_SETTLE, FEE_FLAG_EQ_FLOOR, J_FEE, TRADE_BY_MEMBER, TRADE_BY_ORPHAN,
};

use crate::bn_demo::{product_word, Line};
use crate::bn_live::JOURNAL_FILE;

/// BX-14's `N`: commission-bearing trades per class before its row is
/// measured.
pub const FEE_N: u64 = 30;

/// One fee class.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Class {
    /// `PRODUCT_*`.
    pub product: u8,
    /// A dated row.
    pub dated: bool,
    /// A TradFi row.
    pub tradfi: bool,
    /// The trade was a maker.
    pub maker: bool,
}

impl Class {
    /// The `fees.toml` class word: `perp`, `dated` (TradFi perpetuals are
    /// their own: `tradfi`).
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        if self.tradfi {
            "tradfi"
        } else if self.dated {
            "dated"
        } else {
            "perp"
        }
    }
}

/// A class's sums: trades, notional and commission ×1e9 in the settlement
/// asset.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Commission-bearing trades.
    pub n: u64,
    /// Σ notional ×1e9.
    pub notional_1e9: i128,
    /// Σ commission ×1e9 (a rebate negative).
    pub commission_1e9: i128,
}

impl Tally {
    /// The rate in bps ×1e4 (`Σ commission ÷ Σ notional × 1e8`), `None`
    /// with no notional.
    #[must_use]
    pub fn rate_bps_1e4(&self) -> Option<i128> {
        (self.notional_1e9 > 0).then(|| self.commission_1e9 * 100_000_000 / self.notional_1e9)
    }

    /// The rate in whole bps, rounded UP (`fees.toml`'s L3), `None` with no
    /// notional.
    #[must_use]
    pub fn ceil_bps(&self) -> Option<i128> {
        let d = self.notional_1e9;
        (d > 0).then(|| {
            let num = self.commission_1e9 * 10_000;
            num.div_euclid(d) + i128::from(num.rem_euclid(d) != 0)
        })
    }
}

/// What was read and left out.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Excluded {
    /// A settlement's, a liquidation's or an ADL's commission.
    pub not_ours: u64,
    /// A commission naming no asset.
    pub no_asset: u64,
    /// Paid in BNB (the BNB burn is on).
    pub bnb: u64,
    /// Paid in another asset.
    pub other_asset: u64,
    /// A trade of no notional.
    pub no_notional: u64,
    /// BX10: a Binance Stocks fee at the $0.35 minimum — a floor, no rate.
    pub eq_floor: u64,
}

/// The measurement over every journal read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Measure {
    /// The classes, sorted.
    pub classes: Vec<(Class, Tally)>,
    /// What was left out.
    pub excluded: Excluded,
    /// Journal records read, and the `J_FEE` among them.
    pub records: u64,
    /// See [`Self::records`].
    pub fees: u64,
}

impl Measure {
    /// Count one journal record.
    pub fn add(&mut self, r: &ExecRecord) {
        self.records += 1;
        if r.kind != J_FEE {
            return;
        }
        self.fees += 1;
        let (maker, asset, by, product, dated, tradfi) = fee_fields(r.code);
        let x = &mut self.excluded;
        if by != TRADE_BY_MEMBER && by != TRADE_BY_ORPHAN {
            x.not_ours += 1;
        } else if asset == FEE_ASSET_NONE {
            x.no_asset += 1;
        } else if asset == FEE_ASSET_BNB {
            x.bnb += 1;
        } else if asset != FEE_ASSET_SETTLE {
            x.other_asset += 1;
        } else if r.b <= 0 {
            x.no_notional += 1;
        } else if r.code & FEE_FLAG_EQ_FLOOR != 0 {
            x.eq_floor += 1;
        } else {
            let class = Class { product, dated, tradfi, maker };
            let t = match self.classes.binary_search_by(|(c, _)| c.cmp(&class)) {
                Ok(i) => &mut self.classes[i].1,
                Err(i) => {
                    self.classes.insert(i, (class, Tally::default()));
                    &mut self.classes[i].1
                }
            };
            t.n += 1;
            t.notional_1e9 += i128::from(r.b);
            t.commission_1e9 += i128::from(r.c);
        }
    }

    /// The class's tally, if any trade was counted in it.
    #[must_use]
    pub fn tally(&self, class: Class) -> Option<Tally> {
        self.classes.iter().find(|(c, _)| *c == class).map(|(_, t)| *t)
    }

    /// **The `fees.toml` rows the measurement supports**: `<kind> =
    /// "<maker>:<taker>"` for `[fees.bn]`'s `perp` and `dated` whose USDⓈ-M
    /// maker AND taker classes reached `n` — whole bps rounded up, a rebate
    /// as 0 (the file's grammar is unsigned). COIN-M shares the `bn` rows
    /// until its own schedule differs (the file's note), and the file has
    /// no TradFi class: their classes are reported, never a row here.
    #[must_use]
    pub fn rows(&self, n: u64) -> Vec<String> {
        let mut out = Vec::new();
        for dated in [false, true] {
            let get = |maker| self.tally(Class { product: exec_binance::inst::PRODUCT_USDM, dated, tradfi: false, maker });
            if let (Some(m), Some(t)) = (get(true), get(false)) {
                if let (true, Some(mb), Some(tb)) = (m.n >= n && t.n >= n, m.ceil_bps(), t.ceil_bps()) {
                    let kind = Class { product: 0, dated, tradfi: false, maker: false }.kind();
                    out.push(format!("{kind} = \"{}:{}\"", mb.max(0), tb.max(0)));
                }
            }
        }
        out
    }
}

/// Read one journal file into `m`.
///
/// # Errors
/// The file is not a readable `binance-exec.pmlr` (another kind, a version
/// this build does not read, a malformed file).
pub fn read_journal(path: &Path, m: &mut Measure) -> Result<u64, String> {
    let r = PmlrReader::<ExecRecord>::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if r.slot_kind() != SlotKind::Exec {
        return Err(format!("{}: not an execution journal ({:?})", path.display(), r.slot_kind()));
    }
    if !crate::backtest::pmlr_version_accepted(r.version()) {
        return Err(format!("{}: PMLR version {} this build does not read", path.display(), r.version()));
    }
    let recs = r.records();
    let mut i = 0;
    while i < recs.len() {
        m.add(&recs[i]);
        i += 1;
    }
    Ok(recs.len() as u64)
}

/// A rate ×1e4 bps as a decimal word, its sign its own (−0.5 bp is
/// `-0.5000`, never `0.5000`).
fn rate_word(r: i128) -> String {
    let (sign, a) = if r < 0 { ("-", -r) } else { ("", r) };
    format!("{sign}{}.{:04}", a / 10_000, a % 10_000)
}

/// **Measure** the journals under `dirs` (each a run directory or a root of
/// `run-*` directories) and print the report; `n` is BX-14's N.
///
/// # Errors
/// A directory with no run, a journal that cannot be read, or no journal
/// at all.
pub fn run(dirs: &[PathBuf], n: u64) -> Result<Measure, String> {
    let mut m = Measure::default();
    let mut journals = 0u32;
    let mut read: Vec<PathBuf> = Vec::new();
    for d in dirs {
        if !d.is_dir() {
            return Err(format!("{} is not a directory", d.display()));
        }
        let runs = crate::backtest::discover_runs(d).map_err(|e| e.to_string())?;
        for run in runs {
            let path = run.path.join(JOURNAL_FILE);
            if !path.is_file() {
                continue;
            }
            // A root and a run under it, or a directory named twice: one
            // journal, counted once.
            let canon = std::fs::canonicalize(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if read.contains(&canon) {
                continue;
            }
            read.push(canon);
            let k = read_journal(&path, &mut m)?;
            journals += 1;
            Line::new("fees", "journal").s("path", &path.display().to_string()).n("records", k as i64).emit();
        }
    }
    if journals == 0 {
        return Err(format!("no {JOURNAL_FILE} under {dirs:?}"));
    }
    for (c, t) in &m.classes {
        let rate = t.rate_bps_1e4().map_or_else(|| String::from("-"), rate_word);
        Line::new("fees", "class")
            .s("product", product_word(c.product))
            .s("kind", c.kind())
            .s("side", if c.maker { "maker" } else { "taker" })
            .n("trades", t.n as i64)
            .s("notional_1e9", &t.notional_1e9.to_string())
            .s("commission_1e9", &t.commission_1e9.to_string())
            .s("rate_bps", &rate)
            .n("ceil_bps", t.ceil_bps().map_or(-1, |b| b as i64))
            .b("reached", t.n >= n)
            .b("coinm_shares_bn_rows", c.product == PRODUCT_COINM)
            .emit();
    }
    let x = m.excluded;
    Line::new("fees", "excluded")
        .n("not_ours", x.not_ours as i64)
        .n("no_asset", x.no_asset as i64)
        .n("bnb", x.bnb as i64)
        .n("other_asset", x.other_asset as i64)
        .n("no_notional", x.no_notional as i64)
        .n("eq_floor", x.eq_floor as i64)
        .emit();
    let rows = m.rows(n);
    Line::new("fees", "fees_toml")
        .n("n", n as i64)
        .s("section", "[fees.bn]")
        .s("rows", &rows.join("; "))
        .b("written", false)
        .emit();
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exec_binance::inst::{PRODUCT_COINM, PRODUCT_USDM};
    use exec_binance::journal::{FEE_ASSET_OTHER, J_FILL, TRADE_BY_LIQUIDATION, TRADE_BY_SETTLEMENT};

    /// A `J_FEE` as the gateway writes it (`fee_code`'s bits).
    fn fee(maker: bool, asset: u8, by: u8, product: u8, dated: bool, notional_1e9: i64, commission_1e9: i64) -> ExecRecord {
        let code = i32::from(maker)
            | (i32::from(asset) << 1)
            | (i32::from(by) << 4)
            | (i32::from(product) << 8)
            | (i32::from(dated) << 12);
        ExecRecord { kind: J_FEE, b: notional_1e9, c: commission_1e9, code, ..ExecRecord::default() }
    }

    /// The classes and the exclusions: the arm's trades counted by product
    /// × kind × side; the venue's own (settlement, liquidation), a fee in
    /// BNB or another asset, one naming none, one of no notional — counted
    /// apart; other records read past. The `fees.toml` row only once both
    /// sides reached N, rounded UP. Break-and-watch: counting a
    /// liquidation's fee as a taker's moves the taker rate below.
    #[test]
    fn the_classes_and_the_exclusions() {
        let mut m = Measure::default();
        m.add(&ExecRecord { kind: J_FILL, ..ExecRecord::default() });
        // USDⓈ-M perpetual: 2 bp maker (130 USDT, 0.026), 4.5 bp taker.
        for _ in 0..30 {
            m.add(&fee(true, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, 130_000_000_000, 26_000_000));
            m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_ORPHAN, PRODUCT_USDM, false, 100_000_000_000, 45_000_000));
        }
        m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_LIQUIDATION, PRODUCT_USDM, false, 100_000_000_000, 1_000_000_000));
        m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_SETTLEMENT, PRODUCT_USDM, false, 100_000_000_000, 1_000_000_000));
        m.add(&fee(false, FEE_ASSET_BNB, TRADE_BY_MEMBER, PRODUCT_USDM, false, 100_000_000_000, 1));
        m.add(&fee(false, FEE_ASSET_OTHER, TRADE_BY_MEMBER, PRODUCT_USDM, false, 100_000_000_000, 1));
        m.add(&fee(false, FEE_ASSET_NONE, TRADE_BY_MEMBER, PRODUCT_USDM, false, 100_000_000_000, 0));
        m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, 0, 5));
        // COIN-M perpetual, in the coin: 5 bp taker, short of N.
        m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_COINM, false, 4_615_384, 2_307));
        // USDⓈ-M dated: maker only.
        m.add(&fee(true, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, true, 100_000_000_000, 20_000_000));
        assert_eq!((m.records, m.fees), (69, 68));
        assert_eq!(m.excluded, Excluded { not_ours: 2, no_asset: 1, bnb: 1, other_asset: 1, no_notional: 1, eq_floor: 0 });
        let maker = m.tally(Class { product: PRODUCT_USDM, dated: false, tradfi: false, maker: true }).unwrap();
        let taker = m.tally(Class { product: PRODUCT_USDM, dated: false, tradfi: false, maker: false }).unwrap();
        assert_eq!((maker.n, maker.rate_bps_1e4(), maker.ceil_bps()), (30, Some(20_000), Some(2)));
        assert_eq!((taker.n, taker.rate_bps_1e4(), taker.ceil_bps()), (30, Some(45_000), Some(5)), "4.5 bp rounds up");
        let cm = m.tally(Class { product: PRODUCT_COINM, dated: false, tradfi: false, maker: false }).unwrap();
        assert_eq!((cm.n, cm.ceil_bps()), (1, Some(5)));
        assert_eq!(m.rows(30), vec![String::from("perp = \"2:5\"")], "dated has no taker side");
        assert!(m.rows(31).is_empty(), "short of N: no row");
        // The parser review's P-3: TradFi at N makes no row (the file has no
        // such class), and a rebate past 1 bp prints 0, never a signed bp.
        let tradfi = |maker| {
            let code = i32::from(maker) | (i32::from(FEE_ASSET_SETTLE) << 1) | (i32::from(PRODUCT_USDM) << 8) | (1 << 13);
            ExecRecord { kind: J_FEE, b: 100_000_000_000, c: -15_000_000, code, ..ExecRecord::default() }
        };
        for _ in 0..30 {
            m.add(&tradfi(true));
            m.add(&tradfi(false));
            m.add(&fee(true, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, true, 100_000_000_000, -15_000_000));
            m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, true, 100_000_000_000, -15_000_000));
        }
        let t = m.tally(Class { product: PRODUCT_USDM, dated: false, tradfi: true, maker: true }).unwrap();
        assert_eq!((t.n, t.ceil_bps()), (30, Some(-1)));
        assert_eq!(m.rows(30), vec![String::from("perp = \"2:5\""), String::from("dated = \"0:0\"")]);
    }

    /// The rate's sign is its own (the parser review's P-4).
    #[test]
    fn a_rates_word_keeps_its_sign() {
        assert_eq!(rate_word(-5_000), "-0.5000");
        assert_eq!(rate_word(-1), "-0.0001");
        assert_eq!(rate_word(45_000), "4.5000");
        assert_eq!(rate_word(-25_000), "-2.5000");
        assert_eq!(rate_word(0), "0.0000");
    }

    /// A rebate rounds up toward zero; no notional is no rate.
    #[test]
    fn a_rebate_rounds_up_and_no_notional_has_no_rate() {
        let t = Tally { n: 1, notional_1e9: 100_000_000_000, commission_1e9: -5_000_000 };
        assert_eq!((t.rate_bps_1e4(), t.ceil_bps()), (Some(-5_000), Some(0)), "-0.5 bp → 0");
        assert_eq!(Tally::default().rate_bps_1e4(), None);
        assert_eq!(Tally::default().ceil_bps(), None);
    }

    /// BX10: a Binance Stocks fee at the $0.35 minimum is a floor, never a
    /// rate — counted apart; one past $350 of notional is the class's.
    /// Break-and-watch: counting the floored fee makes the rate 350 bp.
    #[test]
    fn an_equity_fee_at_the_floor_is_no_rate() {
        let mut m = Measure::default();
        let mut floored = fee(false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, exec_binance::inst::PRODUCT_EQUITY, false, 10_000_000_000, 350_000_000);
        floored.code |= FEE_FLAG_EQ_FLOOR;
        m.add(&floored);
        m.add(&fee(false, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, exec_binance::inst::PRODUCT_EQUITY, false, 1_000_000_000_000, 1_000_000_000));
        assert_eq!(m.excluded.eq_floor, 1);
        let t = m.tally(Class { product: exec_binance::inst::PRODUCT_EQUITY, dated: false, tradfi: false, maker: false }).unwrap();
        assert_eq!((t.n, t.ceil_bps()), (1, Some(10)), "10 bps past the floor");
    }

    /// A journal file is read whole, and refused when it is another kind.
    #[test]
    fn a_journal_file_is_read_and_another_kind_refused() {
        let dir = std::env::temp_dir().join(format!("bn-fees-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(JOURNAL_FILE);
        {
            let mut cap = core_io::SlotCapture::<ExecRecord>::open(&path, SlotKind::Exec, 1).unwrap();
            cap.append(&fee(true, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, 100_000_000_000, 20_000_000));
            cap.append(&ExecRecord { kind: J_FILL, ..ExecRecord::default() });
            cap.flush_all().unwrap();
        }
        let mut m = Measure::default();
        assert_eq!(read_journal(&path, &mut m), Ok(2));
        assert_eq!((m.records, m.fees, m.classes.len()), (2, 1, 1));
        // Another kind of 64 B slots (a fills capture) is no journal.
        let other = dir.join("fills.pmlr");
        {
            let mut cap = core_io::SlotCapture::<ExecRecord>::open(&other, SlotKind::Fill, 1).unwrap();
            cap.append(&ExecRecord::default());
            cap.flush_all().unwrap();
        }
        assert!(read_journal(&other, &mut m).is_err(), "another kind is refused");
        assert_eq!(m.records, 2, "nothing of it counted");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The parser review's P-5: a journal named twice — a root and a run
    /// under it — is read once, or every class reaches N at half its trades.
    #[test]
    fn a_journal_named_twice_is_read_once() {
        let root = std::env::temp_dir().join(format!("bn-fees-twice-{}", std::process::id()));
        let rd = root.join("run-1790000000000000000");
        std::fs::create_dir_all(&rd).unwrap();
        {
            let mut cap = core_io::SlotCapture::<ExecRecord>::open(rd.join(JOURNAL_FILE), SlotKind::Exec, 1).unwrap();
            cap.append(&fee(true, FEE_ASSET_SETTLE, TRADE_BY_MEMBER, PRODUCT_USDM, false, 100_000_000_000, 20_000_000));
            cap.flush_all().unwrap();
        }
        let m = run(&[root.clone(), rd.clone(), root.clone()], 30).unwrap();
        assert_eq!((m.records, m.fees), (1, 1));
        std::fs::remove_dir_all(&root).unwrap();
    }
}
