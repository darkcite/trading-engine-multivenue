// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! Where market data comes from. The server routes every frame a source
//! emits by its kind and instrument; a source never sees a client.

use term_proto::ctl::*;
use term_proto::Kind;

pub trait Source {
    /// Frames every client gets on connect: `InstrDef`s, `EngineStatus`.
    fn hello(&mut self, out: &mut Vec<u8>);
    /// A fresh picture of one instrument: book snapshots, bar history.
    fn snapshot(&mut self, instr: u16, out: &mut Vec<u8>);
    /// Advance to `t_ns` (monotonic since boot) and append new frames.
    fn tick(&mut self, t_ns: u64, out: &mut Vec<u8>);
}

/// The subscription channel a market-data frame belongs to, and its
/// instrument (every md body starts with `instr u16`).
pub fn route(frame: &[u8]) -> Option<(u32, u16)> {
    let kind = Kind::from_u8(*frame.first()?)?;
    let ch = match kind {
        Kind::BookSnap | Kind::BookDelta => CH_BOOK,
        Kind::TradeBatch | Kind::Marker => CH_TRADES,
        Kind::Bar => CH_BARS,
        Kind::FootprintCells => CH_FOOTPRINT,
        Kind::VapArray => CH_VAP,
        Kind::FlowPoint => CH_FLOW,
        Kind::HeatCol => CH_HEAT,
        Kind::IndicatorPoint => CH_IND,
        _ => return None,
    };
    let b = frame.get(term_proto::HDR_LEN..term_proto::HDR_LEN + 2)?;
    Some((ch, u16::from_le_bytes([b[0], b[1]])))
}

/// Split concatenated frames, validating each.
pub fn frames(mut buf: &[u8]) -> impl Iterator<Item = Result<&[u8], term_proto::Error>> {
    core::iter::from_fn(move || {
        if buf.is_empty() {
            return None;
        }
        match term_proto::decode(buf) {
            Ok((_, n)) => {
                let (f, rest) = buf.split_at(n);
                buf = rest;
                Some(Ok(f))
            }
            Err(e) => {
                buf = &[];
                Some(Err(e))
            }
        }
    })
}
