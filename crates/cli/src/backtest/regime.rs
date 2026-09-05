// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # backtest::regime — regime-aware replay (RG3, plan §4.8)
//!
//! The harness half of the regime lane: `backtest` and `audit-pnl`
//! instantiate the SAME `core_regime::RegimeState` the engine runs,
//! seed it from a `regime-seed.tsv`, feed it the replay's ticks and
//! funding prints, poll its 1 s timer on the virtual clock, replay the
//! window's `SetRegime` frames from `ai-cmds.pmlr`, and hand the
//! resulting [`RegimeView`] to the bare `VmStrategy` exactly as the
//! strategy set does live (`VmStrategy::set_regime_view`). The words
//! the harness judges are therefore the engine's own code over the
//! captured minutes — never the worker's reference.
//!
//! ## `--regime` law
//!
//! * absent → the DEFAULT artifact (`~/multivenue/regime.toml`) when it
//!   exists and resolves on this root; otherwise the replay is
//!   regime-blind (stderr says so) — the frozen worker argv passes no
//!   flag, so a default artifact can never turn a backtest into an
//!   error;
//! * `--regime <path>` → that artifact; any refusal is a usage error;
//! * `--regime off` → every row evaluates as `LABEL_ANY` (the tails are
//!   stripped before the table is handed to the vm) — the on/off delta
//!   is the first number any labelled ruleset must show.
//!
//! Without a usable artifact the vm keeps its boot view (every word
//! UNKNOWN): labelled rows fail closed, exactly as they would live.
//!
//! ## Seed
//!
//! `--regime-seed <path>` wins; else `<first run dir>/regime-seed.tsv`
//! (the window root's own seed, written by `window_root` from
//! `candles.db` — derived data, never a capture window); else the
//! detector warms live and the stderr tell says `seed=absent`.
//!
//! ## `SetRegime` frames
//!
//! A frame captured BEFORE the run's first tick (a window root carries
//! the pre-window declaration that was still in force at the cut) is
//! clamped to the anchor with its TTL shortened by the clamp; a frame
//! whose TTL had already run out at the anchor is dropped and counted.
//!
//! DOCTRINE: offline tooling (`audit_replay.rs`) — allocates freely,
//! never loaded by the engine loop.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_io::{PmlrReader, SlotKind};
use core_regime::{RegimeState, RegimeView};
use core_time::WallAnchor;
use core_types::regime::{dim_byte_name, dim_name, DIM_COUNT, REGIME_PROFILES};
use core_types::{AiCmd, AiCmdKind, ChannelEvent, ChannelId, RegimeWord, SymbolId, Tick};

use crate::backtest::{pmlr_version_accepted, HarnessError, MIN_PMLR_VERSION};
use crate::regime_boot::{resolve_regime_file, seed_rows_for};

/// The `--regime` flag, parsed. The `Default` TRAIT value is [`Off`]
/// (hermetic for library callers and tests — never the operator's home
/// directory); the bin maps an ABSENT flag to [`Auto`] through
/// [`RegimeMode::parse`].
///
/// [`Off`]: RegimeMode::Off
/// [`Auto`]: RegimeMode::Auto
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RegimeMode {
    /// No flag: the default artifact if usable, else regime-blind.
    Auto,
    /// `--regime off`: strip every row's regime tail (evaluate as ANY).
    #[default]
    Off,
    /// `--regime <path>`: this artifact, refusals fatal.
    Artifact(PathBuf),
}

impl RegimeMode {
    /// Parse the flag value (`None` = absent ⇒ [`RegimeMode::Auto`]).
    pub fn parse(flag: Option<&str>) -> Self {
        match flag {
            None => Self::Auto,
            Some("off") => Self::Off,
            Some(p) => Self::Artifact(PathBuf::from(p)),
        }
    }

    /// True for `--regime off`.
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Off)
    }
}

/// Per-profile name for reports (`fast` / `slow`).
pub fn profile_name(p: u8) -> &'static str {
    match p {
        0 => "fast",
        1 => "slow",
        _ => "?",
    }
}

/// Decode a word for reports — `trend=bull shape=trend … source=measured`
/// (populated dimensions only, `unknown` for the mark, `?` for a
/// malformed byte, `empty` for the empty word). Mirrors
/// `claude_worker.regime.describe` so the nightly merge keys on one
/// string.
pub fn word_string(w: RegimeWord) -> String {
    let mut out = String::with_capacity(96);
    let mut d = 0u8;
    while d < DIM_COUNT {
        let name = dim_byte_name(d, w.dim(d));
        if !name.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(dim_name(d));
            out.push('=');
            out.push_str(name);
        }
        d += 1;
    }
    if out.is_empty() {
        out.push_str("empty");
    }
    out
}

/// The harness-side detector + its bookkeeping.
pub struct RegimeReplay {
    state: Box<RegimeState>,
    fund_ref: SymbolId,
    next_timer_virt: u64,
    minutes_seen: u64,
    /// Minutes judged per `(profile, effective word)` — sampled on
    /// every minute roll after the judge.
    pub minutes_by_word: BTreeMap<(u8, u64), u64>,
    /// `SetRegime` frames applied.
    pub declared_applied: u64,
    /// Seed rows applied at build.
    pub seed_rows: u32,
    /// Artifact members absent from this root (dropped).
    pub members_dropped: usize,
    /// SHA-256 of the artifact bytes.
    pub hash: [u8; 32],
    /// Where the seed came from (stderr tell).
    pub seed_source: String,
}

impl RegimeReplay {
    /// Build the detector for a replay whose virtual clock starts at
    /// `first_virt` == wall `first_wall`. `resolve` maps §9.4
    /// descriptors to the replay's syms; `default_seed` is the window
    /// root's own seed path. `Ok(None)` = regime-blind replay (Default
    /// mode with no usable artifact, or Off); Artifact-mode refusals
    /// are [`HarnessError::Usage`]. Every decision is reported.
    pub fn build(
        mode: &RegimeMode,
        seed_flag: Option<&Path>,
        default_seed: Option<&Path>,
        resolve: &dyn Fn(&str) -> Option<SymbolId>,
        first_virt: u64,
        first_wall: u64,
        report: &mut dyn FnMut(&str),
    ) -> Result<Option<Self>, HarnessError> {
        let (path, explicit): (PathBuf, bool) = match mode {
            RegimeMode::Off => {
                report("regime: off — every row evaluates as ANY (tails stripped)");
                return Ok(None);
            }
            RegimeMode::Artifact(p) => (p.clone(), true),
            RegimeMode::Auto => {
                let p = match core_config::regime::default_regime_path() {
                    Ok(p) => PathBuf::from(p),
                    Err(_) => {
                        report("regime: no default artifact path (HOME unset) — regime-blind");
                        return Ok(None);
                    }
                };
                if !p.exists() {
                    report("regime: no artifact — regime-blind (labelled rows fail closed; pass --regime <toml> or --regime off)");
                    return Ok(None);
                }
                (p, false)
            }
        };
        let refuse =
            |why: String, report: &mut dyn FnMut(&str)| -> Result<Option<Self>, HarnessError> {
                if explicit {
                    Err(HarnessError::Usage(format!(
                        "--regime {}: {why}",
                        path.display()
                    )))
                } else {
                    report(&format!(
                        "regime: default artifact {} unusable on this root ({why}) — regime-blind",
                        path.display()
                    ));
                    Ok(None)
                }
            };
        let (file, bytes) = match core_config::regime::load(&path) {
            Ok(x) => x,
            Err(e) => return refuse(e.to_string(), report),
        };
        let resolved = match resolve_regime_file(&file, &bytes, resolve, true) {
            Ok(r) => r,
            Err(e) => return refuse(e, report),
        };
        let mut state = RegimeState::new_boxed();
        let anchor = WallAnchor::new(first_virt, first_wall);
        if let Err(e) = state.configure(&resolved.params, anchor, first_virt) {
            return refuse(format!("{e:?}"), report);
        }
        // Seed: flag → window root → absent.
        let mut seed_rows = 0u32;
        let mut seed_source = String::from("absent");
        let seed_path: Option<(PathBuf, bool)> = match seed_flag {
            Some(p) => Some((p.to_path_buf(), true)),
            None => default_seed
                .filter(|p| p.exists())
                .map(|p| (p.to_path_buf(), false)),
        };
        let mut seed_dropped = 0usize;
        if let Some((sp, flagged)) = seed_path {
            match core_config::regime::load_seed(&sp) {
                Ok(lines) => {
                    let (rows, dropped) = seed_rows_for(&lines, resolve, &resolved.params);
                    seed_dropped = dropped;
                    seed_rows = state.seed(&rows);
                    seed_source = sp.display().to_string();
                }
                Err(e) if flagged => {
                    return Err(HarnessError::Usage(format!(
                        "--regime-seed {}: {e}",
                        sp.display()
                    )));
                }
                Err(e) => {
                    report(&format!(
                        "regime: seed {} unreadable ({e}) — warming live",
                        sp.display()
                    ));
                }
            }
        }
        let _ = state.refresh_effective(first_virt);
        let hash_hex: String = resolved.hash[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        report(&format!(
            "regime: artifact {} sha256={hash_hex}… members={} (dropped {}) seed={seed_source} \
             seed_rows={seed_rows} (dropped {seed_dropped}) fast={} slow={}",
            path.display(),
            resolved.params.n_members,
            resolved.members_dropped,
            word_string(state.effective(0)),
            word_string(state.effective(1)),
        ));
        Ok(Some(Self {
            fund_ref: resolved.params.fund_ref,
            state,
            next_timer_virt: first_virt,
            minutes_seen: 0,
            minutes_by_word: BTreeMap::new(),
            declared_applied: 0,
            seed_rows,
            members_dropped: resolved.members_dropped,
            hash: resolved.hash,
            seed_source,
        }))
    }

    /// The detector's current view (hand it to the vm on `true` from
    /// [`Self::on_time`] / [`Self::on_set_regime`]).
    #[inline]
    pub fn view(&self) -> RegimeView {
        self.state.view()
    }

    /// Effective word of profile `p`.
    #[inline]
    pub fn effective(&self, p: u8) -> RegimeWord {
        self.state.effective(p)
    }

    /// Minutes judged since build.
    #[inline]
    pub fn minutes_judged(&self) -> u64 {
        self.state.minutes_judged()
    }

    /// The detector (tests / tells).
    #[inline]
    pub fn state(&self) -> &RegimeState {
        &self.state
    }

    /// One replay tick (stale ticks are ignored by the detector's law).
    #[inline]
    pub fn on_tick(&mut self, t: &Tick) {
        self.state.on_tick(t);
    }

    /// One venue event: the funding reference's prints feed the latch
    /// (Funding on every venue; Hyperliquid rides AssetCtx — the set's
    /// law verbatim).
    #[inline]
    pub fn on_event(&mut self, e: &ChannelEvent) {
        if e.sym == self.fund_ref
            && (e.channel == ChannelId::Funding as u8 || e.channel == ChannelId::AssetCtx as u8)
        {
            self.state.on_funding(e.v0, e.venue_time_ms);
        }
    }

    /// A captured `SetRegime` frame at virtual `now` (shape-checked at
    /// capture). Returns true when an effective word changed.
    pub fn on_set_regime(&mut self, cmd: &AiCmd, now: u64) -> bool {
        debug_assert_eq!(cmd.kind(), Some(AiCmdKind::SetRegime));
        self.state.set_declared(
            cmd.param_id as u8,
            RegimeWord(cmd.px as u64),
            now,
            cmd.ttl_ns,
        );
        self.declared_applied += 1;
        self.state.refresh_effective(now) != 0
    }

    /// The 1 s timer on the virtual clock: rolls minutes, refreshes
    /// the effective words, samples the per-word minute histogram.
    /// Returns true when the vm's view must be refreshed (a word
    /// changed, or a minute rolled — REL may have moved).
    pub fn on_time(&mut self, now: u64) -> bool {
        if now < self.next_timer_virt {
            return false;
        }
        self.next_timer_virt = now.saturating_add(1_000_000_000);
        let changed = self.state.on_timer(now);
        let judged = self.state.minutes_judged();
        let rolled = judged != self.minutes_seen;
        if rolled {
            let n = judged - self.minutes_seen;
            self.minutes_seen = judged;
            let mut p = 0u8;
            while (p as usize) < REGIME_PROFILES {
                *self
                    .minutes_by_word
                    .entry((p, self.state.effective(p).0))
                    .or_insert(0) += n;
                p += 1;
            }
        }
        changed != 0 || rolled
    }
}

/// Load a run's `SetRegime` frames from `ai-cmds.pmlr` (absent file =
/// none). Frames stamped before `anchor_ts` (the run's first tick) are
/// clamped to it with their TTL shortened; those already expired at
/// the anchor are dropped. Returns `(frames, dropped)`.
pub fn load_set_regime_frames(
    run_dir: &Path,
    epoch_ns: u64,
    anchor_ts: u64,
) -> Result<(Vec<AiCmd>, u64), HarnessError> {
    let path = run_dir.join("ai-cmds.pmlr");
    if !path.is_file() {
        return Ok((Vec::new(), 0));
    }
    let reader = PmlrReader::<AiCmd>::open(&path)
        .map_err(|e| HarnessError::Capture(format!("{}: {e}", path.display())))?;
    if reader.slot_kind() != SlotKind::AiCmd {
        return Err(HarnessError::Capture(format!(
            "{}: slot_kind {:?} is not AiCmd",
            path.display(),
            reader.slot_kind()
        )));
    }
    if !pmlr_version_accepted(reader.version()) {
        return Err(HarnessError::Capture(format!(
            "{}: PMLR v{} — accepted v{}..=v{}",
            path.display(),
            reader.version(),
            MIN_PMLR_VERSION,
            core_io::VERSION
        )));
    }
    if reader.epoch_ns() != epoch_ns {
        return Err(HarnessError::Capture(format!(
            "{}: header epoch_ns {} != directory epoch_ns {epoch_ns} (§3.1 cross-check)",
            path.display(),
            reader.epoch_ns()
        )));
    }
    let mut out = Vec::new();
    let mut dropped = 0u64;
    for c in reader.records() {
        if c.kind() != Some(AiCmdKind::SetRegime) || c.validate_shape().is_err() {
            continue;
        }
        let mut cmd = *c;
        if cmd.ts_ns < anchor_ts {
            let delta = anchor_ts - cmd.ts_ns;
            if cmd.ttl_ns <= delta {
                dropped += 1;
                continue;
            }
            cmd.ttl_ns -= delta;
            cmd.ts_ns = anchor_ts;
        }
        out.push(cmd);
    }
    Ok((out, dropped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_types::regime::{
        DIM_TREND, DIM_VOL, FUND_POS, LEVEL_NORMAL, SHAPE_TREND, SOURCE_MEASURED, STRETCH_NEUTRAL,
        TREND_BULL, VOL_LOW,
    };
    use core_types::{VenueId, AI_SIDE_NONE, STRATEGY_SLOT_NONE, SYMBOL_ID_NONE};

    #[test]
    fn mode_parses_absent_off_and_paths() {
        assert_eq!(RegimeMode::parse(None), RegimeMode::Auto);
        assert_eq!(
            RegimeMode::default(),
            RegimeMode::Off,
            "hermetic trait default"
        );
        assert_eq!(RegimeMode::parse(Some("off")), RegimeMode::Off);
        assert!(RegimeMode::parse(Some("off")).is_off());
        assert_eq!(
            RegimeMode::parse(Some("/tmp/r.toml")),
            RegimeMode::Artifact(PathBuf::from("/tmp/r.toml"))
        );
        assert_eq!(profile_name(0), "fast");
        assert_eq!(profile_name(1), "slow");
        assert_eq!(profile_name(2), "?");
    }

    #[test]
    fn word_string_mirrors_the_worker_describe() {
        let w = RegimeWord::from_values(
            TREND_BULL,
            SHAPE_TREND,
            VOL_LOW,
            FUND_POS,
            LEVEL_NORMAL,
            STRETCH_NEUTRAL,
            SOURCE_MEASURED,
        );
        assert_eq!(
            word_string(w),
            "trend=bull shape=trend vol=low fund=pos level=normal stretch=neutral source=measured"
        );
        assert_eq!(
            word_string(w.with_dim_unknown(DIM_VOL)),
            "trend=bull shape=trend vol=unknown fund=pos level=normal stretch=neutral source=measured"
        );
        assert_eq!(
            word_string(RegimeWord::EMPTY.with_dim(DIM_TREND, TREND_BULL)),
            "trend=bull"
        );
        assert_eq!(word_string(RegimeWord::EMPTY), "empty");
        assert_eq!(
            word_string(RegimeWord::UNKNOWN),
            "trend=unknown shape=unknown vol=unknown fund=unknown level=unknown stretch=unknown source=unknown"
        );
    }

    fn set_regime(ts: u64, ttl: u64) -> AiCmd {
        AiCmd::new(
            ts,
            1,
            SYMBOL_ID_NONE,
            (1u64 << 2) as i64,
            0,
            ttl,
            AiCmdKind::SetRegime,
            VenueId::Ai,
            STRATEGY_SLOT_NONE,
            AI_SIDE_NONE,
            0,
            0,
        )
    }

    #[test]
    fn set_regime_frames_clamp_to_the_anchor_and_drop_expired() {
        let dir = std::env::temp_dir().join(format!(
            "rg3-frames-{}-{}",
            std::process::id(),
            core_time::now_ns()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        const EPOCH: u64 = 7;
        {
            let mut c = core_io::SlotCapture::<AiCmd>::open(
                dir.join("ai-cmds.pmlr"),
                SlotKind::AiCmd,
                EPOCH,
            )
            .unwrap();
            let hb = AiCmd::new(
                50,
                1,
                SYMBOL_ID_NONE,
                0,
                0,
                0,
                AiCmdKind::Heartbeat,
                VenueId::Ai,
                STRATEGY_SLOT_NONE,
                AI_SIDE_NONE,
                0,
                0,
            );
            c.append(&hb);
            c.append(&set_regime(100, 50)); // expired at anchor 200
            c.append(&set_regime(150, 100)); // clamped: ts 200, ttl 50
            c.append(&set_regime(300, 100)); // in-window, untouched
            c.flush_all().unwrap();
        }
        let (frames, dropped) = load_set_regime_frames(&dir, EPOCH, 200).unwrap();
        assert_eq!(dropped, 1);
        assert_eq!(frames.len(), 2);
        assert_eq!((frames[0].ts_ns, frames[0].ttl_ns), (200, 50));
        assert_eq!((frames[1].ts_ns, frames[1].ttl_ns), (300, 100));
        // Absent file = none; wrong epoch = capture error.
        let empty = std::env::temp_dir().join(format!("rg3-none-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(load_set_regime_frames(&empty, 1, 0).unwrap().0.len(), 0);
        assert!(load_set_regime_frames(&dir, 8, 200).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
