// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **The boot vectors (BX-3).**
//!
//! COPY-DOCTRINE: a boot self-test, run once before the gateway thread
//! exists; every copy here is into a stack buffer compared and dropped.
//!
//! The signer's own RFC 8032 known answers run first
//! ([`crate::config::BnConfig::signer`]). These prove the RENDERS the
//! signatures are made over, byte for byte, against answers computed
//! outside this code (the Python `cryptography` package 46.0.7 on OpenSSL,
//! from the RFC 8032 §7.1 TEST 1 secret key — a published test key, never
//! a real one):
//!
//! 1. the `session.logon` frame (the sorted payload and its base64
//!    signature);
//! 2. a signed REST query (the parameters, `&signature=` and the
//!    percent-encoded base64);
//! 3. the client order id's layout and its round trip;
//! 4. BX-5 quantization and the fixed-decimal renderer.
//!
//! Any mismatch refuses the boot: an order signed over the wrong bytes is
//! refused by the venue at best and, at worst, is a different order.

use crate::cid::{CidClass, CidPrefix, CID_LEN};
use crate::num::{ceil_to, floor_to, render_fixed, Magic, RENDER_MAX};
use crate::rest::QueryWriter;
use crate::wsapi::{queue_logon, render_parts, FrameSink, Part};

/// RFC 8032 §7.1 TEST 1: the secret key.
const TEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];
/// The docs' example API key (public).
const TEST_KEY: &[u8] = b"vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A";
const TEST_TS: u64 = 1_790_000_000_000;

const LOGON_FRAME: &[u8] = b"{\"id\":1,\"method\":\"session.logon\",\"params\":{\"apiKey\":\"vmPUZE6mv9SD5VNHk4HlWFsOr6aKE2zvsw0MuIgwCIPy6utIco14y7Ju91duEh8A\",\"signature\":\"AK5cQnftUDgn1+SvQdJq9hk/doR/bpF1V7jBa3XACPH+lG3/s5NLG1I9NfKtzHqtHekL/R+F1BK7GNSSirR9Dg==\",\"timestamp\":1790000000000}}";

const REST_QUERY: &[u8] = b"symbol=BTCUSDT&countdownTime=30000&recvWindow=1000&timestamp=1790000000000&signature=wz1ZLq%2F5K1CrZ5UBRxY%2BBnOFh0MSHi9NwObMLNDas%2F9oxKDQI%2FI3EiNkQ%2F%2Fz14t4pveEOmXFRDz%2FkPa9jG4oBg%3D%3D";

/// Which vector failed. Each refuses the boot.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SelfTestErr {
    /// The test signer could not be built.
    Signer,
    /// The `session.logon` frame.
    Logon,
    /// The signed REST query.
    Rest,
    /// The client order id.
    Cid,
    /// Quantization.
    Quantize,
    /// The fixed-decimal renderer.
    Render,
}

/// A fixed buffer that takes one frame's plain text.
struct Buf {
    b: [u8; 512],
    n: usize,
}

impl FrameSink for Buf {
    fn queue_parts(&mut self, parts: &[Part<'_>]) -> Result<(), core_net::WsErr> {
        self.n = render_parts(parts, &mut self.b)?;
        Ok(())
    }
}

/// **Run every boot vector** (module docs).
pub fn run() -> Result<(), SelfTestErr> {
    let signer = signer_ed25519::Ed25519Signer::from_seed(&TEST_SEED).map_err(|_| SelfTestErr::Signer)?;

    let mut f = Buf { b: [0; 512], n: 0 };
    queue_logon(&mut f, 1, TEST_KEY, TEST_TS, &signer).map_err(|_| SelfTestErr::Logon)?;
    if &f.b[..f.n] != LOGON_FRAME {
        return Err(SelfTestErr::Logon);
    }

    let mut win = [0u8; 512];
    let mut w = QueryWriter::new(&mut win);
    w.put(b"symbol=BTCUSDT&countdownTime=")
        .uint(30_000)
        .put(b"&recvWindow=")
        .uint(1_000)
        .put(b"&timestamp=")
        .uint(TEST_TS);
    let n = w.sign(&signer).map_err(|_| SelfTestErr::Rest)?;
    if &win[..n] != REST_QUERY {
        return Err(SelfTestErr::Rest);
    }

    let p = CidPrefix::new(0x6512_ab0f);
    let mut id = [0u8; CID_LEN];
    p.render(3, 0x0123_4567_89ab_cdef, &mut id);
    if &id != b"mv6512ab0f30123456789abcdef00000"
        || p.classify(&id)
            != (CidClass::Ours {
                slot: 3,
                client_oid: 0x0123_4567_89ab_cdef,
            })
    {
        return Err(SelfTestErr::Cid);
    }

    let tick = Magic::new(100_000);
    if floor_to(65_000_150_000, 100_000, tick) != 65_000_100_000
        || ceil_to(65_000_150_000, 100_000, tick) != 65_000_200_000
        || floor_to(12_345, 1_000, Magic::new(1_000)) != 12_000
    {
        return Err(SelfTestErr::Quantize);
    }

    let mut r = [0u8; RENDER_MAX];
    let a = render_fixed(65_000_100_000, 1, &mut r);
    if &r[..a] != b"65000.1" {
        return Err(SelfTestErr::Render);
    }
    let b = render_fixed(12_000, 3, &mut r);
    if &r[..b] != b"0.012" {
        return Err(SelfTestErr::Render);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_vector_holds() {
        assert_eq!(super::run(), Ok(()));
    }
}
