// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The notifier's own state, in its state directory (never git, never the
//! vault — operator state, like the worker's TSVs):
//!
//! * `cursor.tsv` — the run directory and the records consumed per capture;
//! * `positions.tsv` — open instance positions (a settlement can arrive
//!   after a restart of either process);
//! * `settled.tsv` — append-only: every settled instance (the watches and
//!   the digests read it);
//! * `kv.tsv` — small keyed values: the last boot seen, digest marks, the
//!   period counters, alerts already rung.
//!
//! Every rewrite is a write to a temporary file and a rename.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the capture tails stand.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cursor {
    /// The run directory being tailed.
    pub run_dir: String,
    /// Records consumed from `engine-fills.pmlr`.
    pub fills: u64,
    /// …from `engine-orders.pmlr`.
    pub orders: u64,
    /// …from `engine-events.pmlr`.
    pub events: u64,
}

/// An open position in one instance of one leg.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Position {
    /// Slot.
    pub slot: u8,
    /// Paper fills.
    pub paper: bool,
    /// Net contracts ×1e6.
    pub qty: i64,
    /// Contracts bought ×1e6.
    pub buy_qty: i64,
    /// Paid for them ×1e12.
    pub buy_cost: i128,
    /// Net cash ×1e12 (− paid, + received).
    pub cash: i128,
    /// Fill rows.
    pub fills: u32,
    /// First fill, s.
    pub first_ts_s: i64,
}

/// A settled instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settled {
    /// When the settlement fill came, s.
    pub settle_ts_s: i64,
    /// The instance's expiry, s.
    pub expiry_s: i64,
    /// The leg.
    pub sym: u32,
    /// Slot.
    pub slot: u8,
    /// Paper.
    pub paper: bool,
    /// Contracts bought ×1e6.
    pub buy_qty: i64,
    /// Paid ×1e12.
    pub buy_cost: i128,
    /// Paid out at settlement (and any sale) ×1e12.
    pub proceeds: i128,
    /// Proceeds − cost ×1e12 (gross of fees).
    pub gross: i128,
    /// Settled at 1.
    pub won: bool,
    /// Fill rows.
    pub fills: u32,
    /// First fill, s.
    pub first_ts_s: i64,
}

/// Everything persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Store {
    /// The directory.
    pub dir: PathBuf,
}

fn atomic_write(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn num<T: std::str::FromStr>(s: Option<&str>) -> Option<T> {
    s?.parse().ok()
}

impl Store {
    /// A store in `dir` (created, mode 0700).
    ///
    /// # Errors
    /// The directory cannot be created.
    pub fn open(dir: &Path) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(Store {
            dir: dir.to_path_buf(),
        })
    }

    fn p(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The cursor, if one was saved.
    #[must_use]
    pub fn load_cursor(&self) -> Option<Cursor> {
        let t = std::fs::read_to_string(self.p("cursor.tsv")).ok()?;
        let line = t.lines().next()?;
        let mut it = line.split('\t');
        Some(Cursor {
            run_dir: it.next()?.to_string(),
            fills: num(it.next())?,
            orders: num(it.next())?,
            events: num(it.next())?,
        })
    }

    /// Save the cursor.
    ///
    /// # Errors
    /// I/O.
    pub fn save_cursor(&self, c: &Cursor) -> std::io::Result<()> {
        atomic_write(
            &self.p("cursor.tsv"),
            &format!("{}\t{}\t{}\t{}\n", c.run_dir, c.fills, c.orders, c.events),
        )
    }

    /// Open positions keyed by `(sym, expiry_s)`.
    #[must_use]
    pub fn load_positions(&self) -> BTreeMap<(u32, i64), Position> {
        let mut m = BTreeMap::new();
        let Ok(t) = std::fs::read_to_string(self.p("positions.tsv")) else {
            return m;
        };
        for line in t.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 10 {
                continue;
            }
            let (Some(sym), Some(exp)) = (num::<u32>(Some(f[0])), num::<i64>(Some(f[1]))) else {
                continue;
            };
            let p = Position {
                slot: num(Some(f[2])).unwrap_or(0),
                paper: f[3] == "1",
                qty: num(Some(f[4])).unwrap_or(0),
                buy_qty: num(Some(f[5])).unwrap_or(0),
                buy_cost: num(Some(f[6])).unwrap_or(0),
                cash: num(Some(f[7])).unwrap_or(0),
                fills: num(Some(f[8])).unwrap_or(0),
                first_ts_s: num(Some(f[9])).unwrap_or(0),
            };
            m.insert((sym, exp), p);
        }
        m
    }

    /// Rewrite the open positions.
    ///
    /// # Errors
    /// I/O.
    pub fn save_positions(&self, m: &BTreeMap<(u32, i64), Position>) -> std::io::Result<()> {
        let mut s = String::new();
        for ((sym, exp), p) in m {
            s.push_str(&format!(
                "{sym}\t{exp}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                p.slot,
                u8::from(p.paper),
                p.qty,
                p.buy_qty,
                p.buy_cost,
                p.cash,
                p.fills,
                p.first_ts_s
            ));
        }
        atomic_write(&self.p("positions.tsv"), &s)
    }

    /// Every settled instance, in file order.
    #[must_use]
    pub fn load_settled(&self) -> Vec<Settled> {
        let Ok(t) = std::fs::read_to_string(self.p("settled.tsv")) else {
            return Vec::new();
        };
        t.lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split('\t').collect();
                if f.len() != 12 {
                    return None;
                }
                Some(Settled {
                    settle_ts_s: num(Some(f[0]))?,
                    expiry_s: num(Some(f[1]))?,
                    sym: num(Some(f[2]))?,
                    slot: num(Some(f[3]))?,
                    paper: f[4] == "1",
                    buy_qty: num(Some(f[5]))?,
                    buy_cost: num(Some(f[6]))?,
                    proceeds: num(Some(f[7]))?,
                    gross: num(Some(f[8]))?,
                    won: f[9] == "1",
                    fills: num(Some(f[10]))?,
                    first_ts_s: num(Some(f[11]))?,
                })
            })
            .collect()
    }

    /// Append one settled instance.
    ///
    /// # Errors
    /// I/O.
    pub fn append_settled(&self, s: &Settled) -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.p("settled.tsv"))?;
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            s.settle_ts_s,
            s.expiry_s,
            s.sym,
            s.slot,
            u8::from(s.paper),
            s.buy_qty,
            s.buy_cost,
            s.proceeds,
            s.gross,
            u8::from(s.won),
            s.fills,
            s.first_ts_s
        )
    }

    /// The keyed values.
    #[must_use]
    pub fn load_kv(&self) -> BTreeMap<String, String> {
        let Ok(t) = std::fs::read_to_string(self.p("kv.tsv")) else {
            return BTreeMap::new();
        };
        t.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Rewrite the keyed values.
    ///
    /// # Errors
    /// I/O.
    pub fn save_kv(&self, kv: &BTreeMap<String, String>) -> std::io::Result<()> {
        let mut s = String::new();
        for (k, v) in kv {
            s.push_str(k);
            s.push('\t');
            s.push_str(&v.replace(['\n', '\t'], " "));
            s.push('\n');
        }
        atomic_write(&self.p("kv.tsv"), &s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("notify-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn everything_round_trips() {
        let s = Store::open(&dir("rt")).unwrap();
        assert!(s.load_cursor().is_none());
        let c = Cursor {
            run_dir: "/a/run-1".into(),
            fills: 3,
            orders: 7,
            events: 9,
        };
        s.save_cursor(&c).unwrap();
        assert_eq!(s.load_cursor(), Some(c));

        let mut m = BTreeMap::new();
        m.insert(
            (0x0400_1000, 900),
            Position {
                slot: 3,
                paper: false,
                qty: 42_000_000,
                buy_qty: 42_000_000,
                buy_cost: 39_480_000_000_000,
                cash: -39_480_000_000_000,
                fills: 2,
                first_ts_s: 880,
            },
        );
        s.save_positions(&m).unwrap();
        assert_eq!(s.load_positions(), m);

        let st = Settled {
            settle_ts_s: 905,
            expiry_s: 900,
            sym: 0x0400_1000,
            slot: 3,
            paper: false,
            buy_qty: 42_000_000,
            buy_cost: 39_480_000_000_000,
            proceeds: 42_000_000_000_000,
            gross: 2_520_000_000_000,
            won: true,
            fills: 2,
            first_ts_s: 880,
        };
        s.append_settled(&st).unwrap();
        s.append_settled(&st).unwrap();
        assert_eq!(s.load_settled(), vec![st.clone(), st]);

        let mut kv = BTreeMap::new();
        kv.insert("boot".to_string(), "57bcdf6d:1".to_string());
        kv.insert("x".to_string(), "a\tb\nc".to_string());
        s.save_kv(&kv).unwrap();
        let back = s.load_kv();
        assert_eq!(back["boot"], "57bcdf6d:1");
        assert_eq!(back["x"], "a b c");
    }
}
