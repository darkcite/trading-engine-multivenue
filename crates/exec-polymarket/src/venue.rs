// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The venue gate (PM15-6c, plan D-8): read once at boot, before the arm
//! may arm — any miss refuses it, naming what to fix.
//!
//! | read | wanted |
//! |---|---|
//! | `GET /version` | `{"version":2}` (CLOB V2: Exchange V2's order struct) |
//! | `GET /auth/ban-status/closed-only` | `{"closed_only":false}` (L2) |
//! | `GET /balance-allowance?asset_type=COLLATERAL` | readable; the CTF Exchange V2 allowance at least the session bound (L2) |
//!
//! The market's own law (`negRisk:false`, the tick, the taker delay, the
//! fee) is discovery's, window by window (`ingress_polymarket::gamma15`).

use core_net::Method;

use crate::account::PmAccount;
use crate::clob::PmClob;
use crate::gw::l2_request;
use crate::response::read_balance;

/// What the gate read.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct VenueFacts {
    /// The pUSD balance ×1e6.
    pub balance_1e6: i64,
    /// The CTF Exchange V2 allowance ×1e6 (`i64::MAX`: unlimited).
    pub allowance_1e6: i64,
}

/// Read the gate at wall second `wall_s`; `min_allowance_1e6` is the
/// session's bound (an allowance under it refuses).
///
/// # Errors
/// The first read that misses, as the boot's refusal text.
pub fn check_venue<W: PmClob>(
    clob: &mut W,
    acct: &PmAccount,
    wall_s: u64,
    min_allowance_1e6: i64,
) -> Result<VenueFacts, String> {
    let (http, range) = clob
        .request(Method::Get, b"/version", &[], 0)
        .map_err(|e| format!("GET /version: {e}"))?;
    let body = &clob.resp()[range];
    if http != 200 || !squeeze_eq(body, br#"{"version":2}"#) {
        // COPY: the answer's first bytes into the refusal's text — boot,
        // once, on the way out.
        return Err(format!(
            "GET /version answered {http} {:?}: the arm speaks CLOB V2 (version 2) alone",
            String::from_utf8_lossy(&body[..body.len().min(80)])
        ));
    }
    let (http, range) = l2_request(
        clob,
        acct,
        Method::Get,
        b"/auth/ban-status/closed-only",
        0,
        wall_s,
    )
    .map_err(|e| format!("GET /auth/ban-status/closed-only: {e}"))?;
    let body = &clob.resp()[range];
    if http != 200 || !squeeze_eq(body, br#"{"closed_only":false}"#) {
        // COPY: the answer's first bytes into the refusal's text — boot,
        // once, on the way out.
        return Err(format!(
            "GET /auth/ban-status/closed-only answered {http} {:?}: the account must be open \
             (not banned, not closed-only) and the API credentials its own",
            String::from_utf8_lossy(&body[..body.len().min(80)])
        ));
    }
    let mut target = [0u8; 80];
    let mut at = crate::num::put(
        &mut target,
        0,
        b"/balance-allowance?asset_type=COLLATERAL&signature_type=",
    );
    at = crate::num::put_u64(&mut target, at, u64::from(acct.sig_type));
    let (http, range) = l2_request(clob, acct, Method::Get, &target[..at], 0, wall_s)
        .map_err(|e| format!("GET /balance-allowance: {e}"))?;
    let read = if http == 200 {
        read_balance(&clob.resp()[range])
    } else {
        None
    };
    let Some((balance_1e6, allowance_1e6)) = read else {
        return Err(format!(
            "GET /balance-allowance answered {http} or did not read: the account's pUSD must be \
             readable"
        ));
    };
    if allowance_1e6 < min_allowance_1e6 {
        return Err(format!(
            "the CTF Exchange V2 may spend ${} of the account's pUSD, under the session bound \
             ${}: approve pUSD for 0xE111180000d2663C0091e4f400237545B87B996B first",
            allowance_1e6 / 1_000_000,
            min_allowance_1e6 / 1_000_000
        ));
    }
    Ok(VenueFacts {
        balance_1e6,
        allowance_1e6,
    })
}

/// `a` equals `want` once JSON whitespace is ignored.
fn squeeze_eq(a: &[u8], want: &[u8]) -> bool {
    let mut j = 0usize;
    let mut i = 0usize;
    while i < a.len() {
        let c = a[i];
        i += 1;
        if c == b' ' || c == b'\n' || c == b'\r' || c == b'\t' {
            continue;
        }
        if j >= want.len() || want[j] != c {
            return false;
        }
        j += 1;
    }
    j == want.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_is_compared_without_its_whitespace() {
        assert!(squeeze_eq(b"{ \"version\": 2 }\n", br#"{"version":2}"#));
        assert!(!squeeze_eq(br#"{"version":3}"#, br#"{"version":2}"#));
        assert!(!squeeze_eq(br#"{"version":2,"x":1}"#, br#"{"version":2}"#));
        assert!(!squeeze_eq(b"", br#"{"version":2}"#));
    }
}
