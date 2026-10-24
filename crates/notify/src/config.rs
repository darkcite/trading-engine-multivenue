// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `notify.toml` — one per engine instance; absent = off (R-NT-4). The
//! contract is `notify.toml.example` at the repo root. Unknown keys and
//! tables are refused, so a typo never silently disables an alert.
//!
//! No alert threshold has a built-in default (ruling 2026-10-24): a key
//! left out turns its alert off. The operating parameters (`poll_ms`,
//! `daily_utc_hour`, `hourly_when_live`, `inbox_max_per_hour`) keep the
//! defaults the plan states.

use std::path::{Path, PathBuf};

use crate::clock;
use crate::toml::{self, TVal, Table};

/// `GET` target for `/state`: loopback only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateUrl {
    /// `127.0.0.1` or `localhost`.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Path, e.g. `/state`.
    pub path: String,
}

/// How a forward-test watch counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchUnit {
    /// Each venue fill row counts (the PR spec's word "fills").
    Fill,
    /// Each settled instance counts once.
    Instance,
}

/// One `[[slot]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotCfg {
    /// Strategy-set slot, 0–15.
    pub slot: u8,
    /// `true`: paper fills are ignored entirely.
    pub live_only: bool,
    /// Report order attempts (coalesced per instance).
    pub attempts: bool,
}

/// One `[[watch]]`: a forward test's kill tallies (reports, never acts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchCfg {
    /// Short name printed in messages, e.g. `P2`.
    pub name: String,
    /// The slot it watches.
    pub slot: u8,
    /// Window start, seconds since the epoch.
    pub start_s: i64,
    /// Counting unit.
    pub unit: WatchUnit,
    /// "N losses within the first `first_n`": the window size.
    pub first_n: Option<u32>,
    /// …and the loss count that meets the criterion.
    pub max_losses_in_first: Option<u32>,
    /// The hit rate is judged once this many units are settled.
    pub hit_after: Option<u32>,
    /// …and meets the criterion below this percentage.
    pub min_hit_pct: Option<u32>,
    /// Realised gross net (USD, negative) at or below which it is met.
    pub loss_net_usd: Option<i64>,
}

/// A parsed `notify.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Message prefix (`SA`, `MAIN`).
    pub tag: String,
    /// `/state` (required when any slot is configured).
    pub state_url: Option<StateUrl>,
    /// The instance's `universe.toml` (family names).
    pub universe: Option<PathBuf>,
    /// The `.env` holding `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`.
    pub env_file: Option<PathBuf>,
    /// The notifier's own state directory.
    pub state_dir: PathBuf,
    /// Render to the log, send nothing.
    pub dry_run: bool,
    /// Poll period.
    pub poll_ms: u64,
    /// DARK after `/state` has been silent this long.
    pub dark_after_s: Option<u64>,
    /// A hold rings once it has lasted this long.
    pub hold_loud_s: Option<u64>,
    /// A live boot not seeded after this long rings.
    pub seed_warn_s: Option<u64>,
    /// The request budget below which a warning rings.
    pub budget_warn: Option<i64>,
    /// Session-net levels (USD, negative) that ring once each per boot.
    pub session_warn_usd: Vec<i64>,
    /// The venue agent's validity end, seconds since the epoch.
    pub agent_valid_until_s: Option<i64>,
    /// Days-left values that ring.
    pub agent_warn_days: Vec<i64>,
    /// Hourly one-liner while a slot is live (R-NT-7).
    pub hourly_when_live: bool,
    /// The daily digest's UTC hour (R-NT-7: 0).
    pub daily_utc_hour: u8,
    /// AI-session inbox directories.
    pub inbox: Vec<PathBuf>,
    /// Inbox messages accepted per hour; the rest are counted.
    pub inbox_max_per_hour: u32,
    /// The slots it reports on.
    pub slots: Vec<SlotCfg>,
    /// Forward-test watches.
    pub watches: Vec<WatchCfg>,
}

/// A config failure, spelled for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigErr(pub String);

impl std::fmt::Display for ConfigErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "notify.toml: {}", self.0)
    }
}

fn cerr(s: impl Into<String>) -> ConfigErr {
    ConfigErr(s.into())
}

/// Expand a leading `~/` against `$HOME`.
#[must_use]
pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return Path::new(&h).join(rest);
        }
    }
    PathBuf::from(p)
}

fn known(t: &Table, allowed: &[&str], what: &str) -> Result<(), ConfigErr> {
    for (k, _, line) in &t.entries {
        if !allowed.contains(&k.as_str()) {
            return Err(cerr(format!("line {line}: unknown key '{k}' in {what}")));
        }
    }
    Ok(())
}

fn s(t: &Table, k: &str) -> Result<Option<String>, ConfigErr> {
    match t.get(k) {
        None => Ok(None),
        Some(TVal::Str(v)) => Ok(Some(v.clone())),
        Some(_) => Err(cerr(format!("'{k}' must be a string"))),
    }
}

fn i(t: &Table, k: &str) -> Result<Option<i64>, ConfigErr> {
    match t.get(k) {
        None => Ok(None),
        Some(TVal::Int(v)) => Ok(Some(*v)),
        Some(_) => Err(cerr(format!("'{k}' must be an integer"))),
    }
}

fn b(t: &Table, k: &str) -> Result<Option<bool>, ConfigErr> {
    match t.get(k) {
        None => Ok(None),
        Some(TVal::Bool(v)) => Ok(Some(*v)),
        Some(_) => Err(cerr(format!("'{k}' must be true or false"))),
    }
}

fn ints(t: &Table, k: &str) -> Result<Vec<i64>, ConfigErr> {
    match t.get(k) {
        None => Ok(Vec::new()),
        Some(TVal::Arr(a)) => a
            .iter()
            .map(|v| match v {
                TVal::Int(n) => Ok(*n),
                _ => Err(cerr(format!("'{k}' must be an array of integers"))),
            })
            .collect(),
        Some(_) => Err(cerr(format!("'{k}' must be an array of integers"))),
    }
}

fn strs(t: &Table, k: &str) -> Result<Vec<String>, ConfigErr> {
    match t.get(k) {
        None => Ok(Vec::new()),
        Some(TVal::Arr(a)) => a
            .iter()
            .map(|v| match v {
                TVal::Str(n) => Ok(n.clone()),
                _ => Err(cerr(format!("'{k}' must be an array of strings"))),
            })
            .collect(),
        Some(_) => Err(cerr(format!("'{k}' must be an array of strings"))),
    }
}

fn pos_u64(t: &Table, k: &str) -> Result<Option<u64>, ConfigErr> {
    match i(t, k)? {
        None => Ok(None),
        Some(v) if v > 0 => Ok(Some(v as u64)),
        Some(_) => Err(cerr(format!("'{k}' must be > 0"))),
    }
}

/// Parse `http://127.0.0.1:9191/state`.
///
/// # Errors
/// Anything but plain `http` to a loopback host.
pub fn parse_state_url(u: &str) -> Result<StateUrl, ConfigErr> {
    let rest = u
        .strip_prefix("http://")
        .ok_or_else(|| cerr("state_url must be http://127.0.0.1:<port>/state"))?;
    let (hostport, path) = match rest.find('/') {
        Some(j) => (&rest[..j], &rest[j..]),
        None => (rest, "/state"),
    };
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| cerr("state_url needs a port"))?;
    if host != "127.0.0.1" && host != "localhost" {
        return Err(cerr(
            "state_url must be loopback (127.0.0.1): /state never leaves the host",
        ));
    }
    let port: u16 = port
        .parse()
        .map_err(|_| cerr("state_url port is not a number"))?;
    Ok(StateUrl {
        host: host.to_string(),
        port,
        path: path.to_string(),
    })
}

const ROOT_KEYS: &[&str] = &[
    "tag",
    "state_url",
    "universe",
    "env_file",
    "state_dir",
    "dry_run",
    "poll_ms",
    "dark_after_s",
    "hold_loud_s",
    "seed_warn_s",
    "budget_warn",
    "session_warn_usd",
    "agent_valid_until",
    "agent_warn_days",
    "hourly_when_live",
    "daily_utc_hour",
    "inbox",
    "inbox_max_per_hour",
];
const SLOT_KEYS: &[&str] = &["slot", "live_only", "attempts"];
const WATCH_KEYS: &[&str] = &[
    "name",
    "slot",
    "start",
    "unit",
    "first_n",
    "max_losses_in_first",
    "hit_after",
    "min_hit_pct",
    "loss_net_usd",
];

fn slot_no(v: Option<i64>, what: &str) -> Result<u8, ConfigErr> {
    match v {
        Some(n) if (0..16).contains(&n) => Ok(n as u8),
        Some(n) => Err(cerr(format!("{what}: slot {n} is outside 0–15"))),
        None => Err(cerr(format!("{what}: 'slot' is required"))),
    }
}

fn opt_u32(t: &Table, k: &str) -> Result<Option<u32>, ConfigErr> {
    match i(t, k)? {
        None => Ok(None),
        Some(v) if v > 0 && v <= i64::from(u32::MAX) => Ok(Some(v as u32)),
        Some(_) => Err(cerr(format!("'{k}' must be a positive integer"))),
    }
}

impl Config {
    /// Parse the text of a `notify.toml`.
    ///
    /// # Errors
    /// [`ConfigErr`] naming the key or line.
    pub fn parse(src: &str) -> Result<Config, ConfigErr> {
        let doc = toml::parse(src).map_err(|e| cerr(e.to_string()))?;
        if let Some(name) = doc.sections.keys().next() {
            return Err(cerr(format!("unknown table [{name}]")));
        }
        for name in doc.arrays.keys() {
            if name != "slot" && name != "watch" {
                return Err(cerr(format!("unknown table [[{name}]]")));
            }
        }
        let r = &doc.root;
        known(r, ROOT_KEYS, "the top level")?;
        let tag = s(r, "tag")?.ok_or_else(|| cerr("'tag' is required"))?;
        if tag.is_empty() || tag.len() > 16 {
            return Err(cerr("'tag' must be 1–16 characters"));
        }
        let state_url = s(r, "state_url")?
            .map(|u| parse_state_url(&u))
            .transpose()?;
        let state_dir = expand(&s(r, "state_dir")?.ok_or_else(|| cerr("'state_dir' is required"))?);
        let dry_run = b(r, "dry_run")?.unwrap_or(false);
        let env_file = s(r, "env_file")?.map(|p| expand(&p));
        if !dry_run && env_file.is_none() {
            return Err(cerr("'env_file' is required unless dry_run = true"));
        }
        let poll_ms = match i(r, "poll_ms")? {
            None => 1000,
            Some(v) if (200..=60_000).contains(&v) => v as u64,
            Some(_) => return Err(cerr("'poll_ms' must be 200–60000")),
        };
        let session_warn_usd = ints(r, "session_warn_usd")?;
        if session_warn_usd.iter().any(|&v| v >= 0) {
            return Err(cerr("'session_warn_usd' levels must be negative (a loss)"));
        }
        let agent_valid_until_s = match s(r, "agent_valid_until")? {
            None => None,
            Some(t) => Some(
                clock::parse_rfc3339_z(&t)
                    .ok_or_else(|| cerr("'agent_valid_until' must be YYYY-MM-DDTHH:MM:SSZ"))?,
            ),
        };
        let agent_warn_days = ints(r, "agent_warn_days")?;
        if agent_warn_days.iter().any(|&d| d < 0) {
            return Err(cerr("'agent_warn_days' must be ≥ 0"));
        }
        let daily_utc_hour = match i(r, "daily_utc_hour")? {
            None => 0,
            Some(h) if (0..24).contains(&h) => h as u8,
            Some(_) => return Err(cerr("'daily_utc_hour' must be 0–23")),
        };
        let inbox_max_per_hour = match i(r, "inbox_max_per_hour")? {
            None => 60,
            Some(v) if (1..=3600).contains(&v) => v as u32,
            Some(_) => return Err(cerr("'inbox_max_per_hour' must be 1–3600")),
        };
        let mut slots = Vec::new();
        for t in doc.arrays.get("slot").map_or(&[][..], Vec::as_slice) {
            known(t, SLOT_KEYS, "[[slot]]")?;
            let slot = slot_no(i(t, "slot")?, "[[slot]]")?;
            if slots.iter().any(|x: &SlotCfg| x.slot == slot) {
                return Err(cerr(format!("[[slot]] {slot} listed twice")));
            }
            slots.push(SlotCfg {
                slot,
                live_only: b(t, "live_only")?.unwrap_or(true),
                attempts: b(t, "attempts")?.unwrap_or(true),
            });
        }
        if !slots.is_empty() && state_url.is_none() {
            return Err(cerr(
                "'state_url' is required when a [[slot]] is configured",
            ));
        }
        let mut watches = Vec::new();
        for t in doc.arrays.get("watch").map_or(&[][..], Vec::as_slice) {
            known(t, WATCH_KEYS, "[[watch]]")?;
            let name = s(t, "name")?.ok_or_else(|| cerr("[[watch]]: 'name' is required"))?;
            let slot = slot_no(i(t, "slot")?, "[[watch]]")?;
            if !slots.iter().any(|x| x.slot == slot) {
                return Err(cerr(format!(
                    "[[watch]] {name}: slot {slot} has no [[slot]] entry"
                )));
            }
            let start = s(t, "start")?.ok_or_else(|| cerr("[[watch]]: 'start' is required"))?;
            let start_s = clock::parse_rfc3339_z(&start)
                .ok_or_else(|| cerr("[[watch]]: 'start' must be YYYY-MM-DDTHH:MM:SSZ"))?;
            let unit = match s(t, "unit")?.as_deref() {
                None | Some("fill") => WatchUnit::Fill,
                Some("instance") => WatchUnit::Instance,
                Some(u) => {
                    return Err(cerr(format!(
                        "[[watch]]: unit '{u}' is not fill or instance"
                    )))
                }
            };
            let first_n = opt_u32(t, "first_n")?;
            let max_losses_in_first = opt_u32(t, "max_losses_in_first")?;
            if first_n.is_some() != max_losses_in_first.is_some() {
                return Err(cerr(
                    "[[watch]]: 'first_n' and 'max_losses_in_first' go together",
                ));
            }
            let hit_after = opt_u32(t, "hit_after")?;
            let min_hit_pct = opt_u32(t, "min_hit_pct")?;
            if hit_after.is_some() != min_hit_pct.is_some() {
                return Err(cerr("[[watch]]: 'hit_after' and 'min_hit_pct' go together"));
            }
            if min_hit_pct.is_some_and(|p| p > 100) {
                return Err(cerr("[[watch]]: 'min_hit_pct' must be ≤ 100"));
            }
            let loss_net_usd = i(t, "loss_net_usd")?;
            if loss_net_usd.is_some_and(|v| v >= 0) {
                return Err(cerr("[[watch]]: 'loss_net_usd' must be negative"));
            }
            watches.push(WatchCfg {
                name,
                slot,
                start_s,
                unit,
                first_n,
                max_losses_in_first,
                hit_after,
                min_hit_pct,
                loss_net_usd,
            });
        }
        Ok(Config {
            tag,
            state_url,
            universe: s(r, "universe")?.map(|p| expand(&p)),
            env_file,
            state_dir,
            dry_run,
            poll_ms,
            dark_after_s: pos_u64(r, "dark_after_s")?,
            hold_loud_s: pos_u64(r, "hold_loud_s")?,
            seed_warn_s: pos_u64(r, "seed_warn_s")?,
            budget_warn: i(r, "budget_warn")?,
            session_warn_usd,
            agent_valid_until_s,
            agent_warn_days,
            hourly_when_live: b(r, "hourly_when_live")?.unwrap_or(true),
            daily_utc_hour,
            inbox: strs(r, "inbox")?.iter().map(|p| expand(p)).collect(),
            inbox_max_per_hour,
            slots,
            watches,
        })
    }

    /// The `[[slot]]` entry for `slot`.
    #[must_use]
    pub fn slot(&self, slot: u8) -> Option<&SlotCfg> {
        self.slots.iter().find(|s| s.slot == slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
tag = "SA"
state_url = "http://127.0.0.1:9192/state"
universe = "/tmp/u.toml"
env_file = "/tmp/.env"
state_dir = "/tmp/notify"
poll_ms = 1000
hold_loud_s = 30
session_warn_usd = [-250, -400]
agent_valid_until = "2027-04-18T07:14:00Z"
agent_warn_days = [14, 7, 1]
inbox = ["/tmp/in1", "/tmp/in2"]
[[slot]]
slot = 3
[[watch]]
name = "P2"
slot = 3
start = "2026-10-24T05:19:25Z"
first_n = 20
max_losses_in_first = 2
hit_after = 30
min_hit_pct = 93
loss_net_usd = -500
"#;

    #[test]
    fn parses_the_full_shape() {
        let c = Config::parse(FULL).unwrap();
        assert_eq!(c.tag, "SA");
        assert_eq!(c.state_url.as_ref().unwrap().port, 9192);
        assert_eq!(
            c.slots,
            vec![SlotCfg {
                slot: 3,
                live_only: true,
                attempts: true
            }]
        );
        assert_eq!(c.hold_loud_s, Some(30));
        assert_eq!(c.dark_after_s, None);
        assert_eq!(c.budget_warn, None);
        assert_eq!(c.session_warn_usd, vec![-250, -400]);
        assert_eq!(c.inbox.len(), 2);
        assert!(c.hourly_when_live);
        assert_eq!(c.daily_utc_hour, 0);
        assert_eq!(c.inbox_max_per_hour, 60);
        let w = &c.watches[0];
        assert_eq!(
            (w.first_n, w.max_losses_in_first, w.hit_after, w.min_hit_pct),
            (Some(20), Some(2), Some(30), Some(93))
        );
        assert_eq!(w.unit, WatchUnit::Fill);
        assert_eq!(w.loss_net_usd, Some(-500));
    }

    #[test]
    fn inbox_only_needs_no_state_url() {
        let c = Config::parse(
            "tag = \"MAIN\"\nstate_dir = \"/tmp/x\"\ndry_run = true\ninbox = [\"/tmp/i\"]\n",
        )
        .unwrap();
        assert!(c.slots.is_empty());
        assert!(c.state_url.is_none());
        assert!(c.env_file.is_none());
    }

    #[test]
    fn refusals() {
        let base = "tag = \"SA\"\nstate_dir = \"/tmp/x\"\ndry_run = true\n";
        for (extra, why) in [
            ("typo_key = 1\n", "unknown key"),
            ("[extra]\n", "unknown table"),
            ("[[slot]]\nslot = 3\n", "state_url"),
            ("state_url = \"http://10.0.0.1:9191/state\"\n", "loopback"),
            ("session_warn_usd = [250]\n", "negative"),
            ("poll_ms = 10\n", "poll_ms"),
            ("agent_valid_until = \"tomorrow\"\n", "agent_valid_until"),
            ("state_url = \"http://127.0.0.1:9191/state\"\n[[slot]]\nslot = 16\n", "outside"),
            ("state_url = \"http://127.0.0.1:9191/state\"\n[[slot]]\nslot = 3\n[[slot]]\nslot = 3\n", "twice"),
            (
                "state_url = \"http://127.0.0.1:9191/state\"\n[[slot]]\nslot = 3\n[[watch]]\nname = \"x\"\nslot = 4\nstart = \"2026-10-24T05:19:25Z\"\n",
                "no [[slot]]",
            ),
            (
                "state_url = \"http://127.0.0.1:9191/state\"\n[[slot]]\nslot = 3\n[[watch]]\nname = \"x\"\nslot = 3\nstart = \"2026-10-24T05:19:25Z\"\nfirst_n = 20\n",
                "go together",
            ),
        ] {
            let e = Config::parse(&format!("{base}{extra}")).unwrap_err();
            assert!(e.0.contains(why), "{extra:?} -> {e}");
        }
        let e = Config::parse("tag = \"SA\"\nstate_dir = \"/tmp/x\"\n").unwrap_err();
        assert!(e.0.contains("env_file"));
    }
}

#[cfg(test)]
mod example {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../notify.toml.example");

    #[test]
    fn the_example_is_the_contract() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.tag, "SA");
        assert_eq!(
            c.slots,
            vec![SlotCfg {
                slot: 3,
                live_only: true,
                attempts: true
            }]
        );
        assert!(c.watches.is_empty());
        assert_eq!(
            (c.dark_after_s, c.hold_loud_s, c.budget_warn),
            (None, None, None)
        );
    }

    #[test]
    fn every_documented_key_parses_when_uncommented() {
        let un: String = EXAMPLE
            .lines()
            .map(|l| {
                let body = l.strip_prefix("# ").unwrap_or(l);
                let is_key = body
                    .split_once(" = ")
                    .is_some_and(|(k, _)| k.bytes().all(|c| c.is_ascii_lowercase() || c == b'_'));
                if l.starts_with("# ") && (is_key || body == "[[watch]]") {
                    body.to_string()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let c = Config::parse(&un).unwrap();
        assert_eq!(c.dark_after_s, Some(60));
        assert_eq!(c.hold_loud_s, Some(30));
        assert_eq!(c.seed_warn_s, Some(180));
        assert_eq!(c.budget_warn, Some(1000));
        assert_eq!(c.session_warn_usd, vec![-250, -400]);
        assert_eq!(c.agent_warn_days, vec![14, 7, 1]);
        assert_eq!(c.inbox.len(), 2);
        assert_eq!(c.watches.len(), 1);
        assert_eq!(c.watches[0].min_hit_pct, Some(93));
    }
}
