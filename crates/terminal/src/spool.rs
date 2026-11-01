// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! The market-data recorder and its reader (TERM plan §4.6, T1a).
//! A spool file is `b"TERMSPOOL"` · term-proto `VER` u8 · 6 zero bytes, then
//! records of `t_ns u64 · len u32 · frame` (little-endian, `t_ns` monotonic
//! from the recording's start). A spool of another wire version is refused.

use std::io::{BufReader, BufWriter, Read, Write};

pub const MAGIC: &[u8; 9] = b"TERMSPOOL";
pub const HEADER_LEN: usize = 16;

pub struct Recorder {
    w: BufWriter<std::fs::File>,
}

impl Recorder {
    pub fn create(path: &std::path::Path) -> std::io::Result<Recorder> {
        let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&[term_proto::VER, 0, 0, 0, 0, 0, 0])?;
        Ok(Recorder { w })
    }
    pub fn record(&mut self, t_ns: u64, frame: &[u8]) -> std::io::Result<()> {
        self.w.write_all(&t_ns.to_le_bytes())?;
        self.w.write_all(&(frame.len() as u32).to_le_bytes())?;
        self.w.write_all(frame)
    }
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

#[derive(Debug)]
pub enum SpoolErr {
    Io(std::io::Error),
    NotASpool,
    /// The spool was written by another term-proto version.
    Version(u8),
    /// A record that does not decode as a term-proto frame.
    BadRecord(u64),
}

impl From<std::io::Error> for SpoolErr {
    fn from(e: std::io::Error) -> SpoolErr {
        SpoolErr::Io(e)
    }
}

pub struct Reader {
    r: BufReader<std::fs::File>,
    buf: Vec<u8>,
    n: u64,
}

impl Reader {
    pub fn open(path: &std::path::Path) -> Result<Reader, SpoolErr> {
        let mut r = BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
        let mut h = [0u8; HEADER_LEN];
        r.read_exact(&mut h).map_err(|_| SpoolErr::NotASpool)?;
        if &h[..9] != MAGIC {
            return Err(SpoolErr::NotASpool);
        }
        if h[9] != term_proto::VER {
            return Err(SpoolErr::Version(h[9]));
        }
        Ok(Reader { r, buf: vec![0u8; term_proto::MAX_FRAME], n: 0 })
    }

    /// The next record: its time and a validated frame, or `None` at EOF.
    pub fn next_record(&mut self) -> Result<Option<(u64, &[u8])>, SpoolErr> {
        let mut h = [0u8; 12];
        match self.r.read_exact(&mut h) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let t = u64::from_le_bytes(h[..8].try_into().unwrap_or_default());
        let len = u32::from_le_bytes(h[8..].try_into().unwrap_or_default()) as usize;
        self.n += 1;
        if len > term_proto::MAX_FRAME {
            return Err(SpoolErr::BadRecord(self.n));
        }
        self.r.read_exact(&mut self.buf[..len])?;
        match term_proto::decode(&self.buf[..len]) {
            Ok((_, used)) if used == len => Ok(Some((t, &self.buf[..len]))),
            _ => Err(SpoolErr::BadRecord(self.n)),
        }
    }
}
