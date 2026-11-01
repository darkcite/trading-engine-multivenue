// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `--replay <spool>`: serves a recorded session at its recorded pace, with
//! order entry disabled. `InstrDef`/`EngineStatus` frames are kept for late
//! joiners, and the last book snapshot per instrument and side for snapshots.

use crate::source::{route, Source};
use crate::spool::{Reader, SpoolErr};
use term_proto::Kind;

pub struct Replay {
    r: Reader,
    pending: Option<(u64, Vec<u8>)>,
    hello: Vec<u8>,
    snaps: Vec<(u16, u8, Vec<u8>)>,
    pub done: bool,
}

impl Replay {
    pub fn open(path: &std::path::Path) -> Result<Replay, SpoolErr> {
        Ok(Replay { r: Reader::open(path)?, pending: None, hello: Vec::new(), snaps: Vec::new(), done: false })
    }

    fn keep(&mut self, f: &[u8]) {
        match Kind::from_u8(f[0]) {
            Some(Kind::InstrDef) | Some(Kind::EngineStatus) => self.hello.extend_from_slice(f),
            Some(Kind::BookSnap) => {
                if let Some((_, instr)) = route(f) {
                    let side = f[term_proto::HDR_LEN + 2];
                    self.snaps.retain(|(i, s, _)| !(*i == instr && *s == side));
                    self.snaps.push((instr, side, f.to_vec()));
                }
            }
            _ => {}
        }
    }
}

impl Source for Replay {
    fn hello(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.hello);
    }
    fn snapshot(&mut self, instr: u16, out: &mut Vec<u8>) {
        for (i, _, f) in &self.snaps {
            if *i == instr {
                out.extend_from_slice(f);
            }
        }
    }
    fn tick(&mut self, t_ns: u64, out: &mut Vec<u8>) {
        loop {
            if self.pending.is_none() {
                match self.r.next_record() {
                    Ok(Some((t, f))) => self.pending = Some((t, f.to_vec())),
                    Ok(None) => {
                        self.done = true;
                        return;
                    }
                    Err(e) => {
                        eprintln!("replay: stopping on {e:?}");
                        self.done = true;
                        return;
                    }
                }
            }
            let Some((t, f)) = self.pending.take() else { return };
            if t > t_ns {
                self.pending = Some((t, f));
                return;
            }
            self.keep(&f);
            out.extend_from_slice(&f);
        }
    }
}
