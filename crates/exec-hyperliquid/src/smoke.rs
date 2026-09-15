// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The testnet smoke — proof that this binary can sign something the
//! venue actually verifies.
//!
//! O-E3 makes testnet a **permanent CI target, not a phase that ends**:
//! a relinked binary that cannot sign a testnet order must never be
//! allowed to sign a mainnet one. So this runs before every armed
//! restart, not once.
//!
//! ## It cannot reach mainnet. By construction.
//!
//! [`run`] refuses unless [`HlConfig::is_testnet`] — which requires
//! BOTH the testnet host and the testnet `source`. There is no flag,
//! no environment variable and no argument that points this at
//! production. A smoke that could touch mainnet would be a live-order
//! path reachable from a cron job.
//!
//! ## Two phases, and the first needs no money
//!
//! | phase | what it proves | needs |
//! |---|---|---|
//! | **A. signature verified** | a GOOD signature is accepted by the venue's recovery | an agent wallet |
//! | **B. signature enforced** | a CORRUPTED signature is REJECTED | an agent wallet |
//! | **C. lifecycle** | place → modify → cancel round-trips | faucet USDC |
//!
//! Phases A and B are the heart of E3's gate and they cost nothing:
//! they cancel an order id that does not exist.
//!
//! ## What counts as "the venue verified us"
//!
//! The obvious answer — `status:"ok"` — is not the only one, and
//! insisting on it would make this gate need an account it does not
//! need. Three answers are possible and all three are informative:
//!
//! | the venue says | what happened | phase A |
//! |---|---|---|
//! | `{"status":"ok",…"Order was never placed…"}` | recovered us, found us registered, judged the order | **verified** |
//! | `{"status":"err",…"User or API Wallet 0xOURS does not exist."}` | **recovered us**, found no account | **verified** |
//! | `{"status":"err","response":"Unable to recover signer."}` | recovery failed | FAILED |
//!
//! The middle row is the one that matters. To name our address the
//! venue must have run the entire recovery — msgpack → keccak →
//! EIP-712 digest → secp256k1 recover — and landed on the same twenty
//! bytes we did. **That is stronger evidence than `status:"ok"`, and
//! it needs no registered agent, no account and no faucet.** The gate
//! therefore asks for one thing, a private key, which means it can run
//! from the first minute of a testnet setup rather than the last.
//!
//! Phase B is then the one that carries the weight, because without it
//! a permissive path would look exactly like a correct one. It flips
//! one bit of `s` and requires that the venue does NOT come back with
//! our address: a venue that still names us, after being handed a
//! signature we deliberately broke, is not deriving the signer from
//! the signature at all — and then phase A proved nothing.

use std::sync::Arc;

use crate::action::{encode_cancel, CancelWire, MAX_ACTION};
use crate::config::HlConfig;
use crate::http::{HlHttp, MAX_REQ_BODY};
use crate::request::{cancel_json, envelope};
use crate::response::{scan, HlResponse};
use crate::sign::{sign_action, Vault};

/// Exit codes. **The shell gate and the binary agree here or they do
/// not agree at all** — `scripts/exec-smoke.sh` branches on these, and
/// a code invented in one place and read in the other is how a gate
/// comes to report green on a failure.
///
/// Every non-zero code refuses the armed restart. They differ so an
/// operator woken at 08:33Z knows in one line whether the venue was
/// unreachable or this binary cannot sign.
pub const EXIT_PASS: i32 = 0;
/// The smoke could not run at all: bad config, encode or signing
/// failure, or an answer that did not parse. Says nothing about the
/// binary's signing — which is why it is not [`EXIT_NOT_VERIFIED`].
pub const EXIT_FAILED: i32 = 1;
/// **Phase A failed.** The venue would not verify a signature this
/// binary produced. This binary must not sign a mainnet order.
///
/// The diagnostic codes start at 20, not at 2, and the gap is
/// deliberate: **clap exits 2 on any argument error**, so a release
/// binary too old to have the `exec-smoke` arm at all would have
/// exited 2 and been reported as "the venue would not verify our
/// signature" — sending an operator to hunt a signing bug when the
/// real answer was "rebuild". A stale binary and a broken signer are
/// not the same emergency.
pub const EXIT_NOT_VERIFIED: i32 = 20;
/// **Phase B failed — the loudest code here.** A deliberately
/// corrupted signature was ACCEPTED. Either the venue is not
/// verifying, or this is not the venue. Every other result from the
/// run is meaningless.
pub const EXIT_CORRUPT_ACCEPTED: i32 = 21;
/// The venue could not be reached. Fail-closed: the restart is still
/// refused, because an unproven binary is an unproven binary whatever
/// the reason.
pub const EXIT_UNREACHABLE: i32 = 22;
/// **The offline self-test failed.** This binary's own encoders no
/// longer reproduce the SDK's bytes. Needs no network to diagnose and
/// no venue to blame: the fault is in this artifact.
pub const EXIT_SELFTEST: i32 = 23;

/// Why a smoke run did not complete.
#[derive(Debug)]
pub enum SmokeErr {
    /// The configuration is not the testnet one. **This is the guard
    /// that makes mainnet unreachable from here.**
    NotTestnet(String),
    /// The configuration itself was refused.
    Config(crate::config::ConfigErr),
    /// Encoding overflowed.
    Encode,
    /// Signing failed.
    Sign,
    /// The network cycle failed.
    Http(crate::http::HttpErr),
    /// The venue's answer could not be understood. Fail-closed: an
    /// unreadable answer is never a pass.
    Unreadable,
    /// **Phase A failed**: a good signature was not verified. The
    /// message is the venue's own.
    SignatureNotVerified(String),
    /// **Phase B failed**: a CORRUPTED signature was ACCEPTED. The
    /// venue is not checking us, or we are not reaching the venue.
    CorruptSignatureAccepted,
    /// **Phase B failed**: the venue named OUR OWN address after being
    /// handed a signature we deliberately corrupted.
    CorruptSignatureRecoveredUs(String),
    /// **The offline self-test failed** — see [`crate::selftest`]. The
    /// binary did not get as far as the network, and should not.
    SelfTest(crate::selftest::SelfTestErr),
}

impl core::fmt::Display for SmokeErr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SmokeErr::NotTestnet(h) => write!(
                f,
                "exec-smoke: refusing to run against {h} — the smoke is TESTNET ONLY, and \
                 requires both the testnet host and HYPERLIQUID_SOURCE=b"
            ),
            SmokeErr::Config(e) => write!(f, "exec-smoke: {e}"),
            SmokeErr::SelfTest(e) => write!(f, "exec-smoke: {e}"),
            SmokeErr::Encode => write!(f, "exec-smoke: action did not fit its buffer"),
            SmokeErr::Sign => write!(f, "exec-smoke: signing failed"),
            SmokeErr::Http(e) => write!(f, "exec-smoke: {e}"),
            SmokeErr::Unreadable => write!(f, "exec-smoke: the venue's answer did not parse"),
            SmokeErr::SignatureNotVerified(m) => write!(
                f,
                "exec-smoke: PHASE A FAILED — the venue could not verify a signature this binary \
                 produced: {m:?}. This binary must not be allowed to sign a mainnet order."
            ),
            SmokeErr::CorruptSignatureAccepted => write!(
                f,
                "exec-smoke: PHASE B FAILED — a DELIBERATELY CORRUPTED signature was ACCEPTED. \
                 Either the venue is not verifying us or we are not talking to the venue. \
                 Every other result from this run is meaningless."
            ),
            SmokeErr::CorruptSignatureRecoveredUs(m) => write!(
                f,
                "exec-smoke: PHASE B FAILED — a DELIBERATELY CORRUPTED signature still produced \
                 OUR OWN address: {m:?}. The signer is not being derived from the signature, so \
                 phase A proved nothing and neither does anything else in this run."
            ),
        }
    }
}

impl SmokeErr {
    /// The process exit code this failure deserves.
    #[must_use]
    pub fn code(&self) -> i32 {
        match self {
            SmokeErr::Http(_) => EXIT_UNREACHABLE,
            SmokeErr::SignatureNotVerified(_) => EXIT_NOT_VERIFIED,
            SmokeErr::CorruptSignatureAccepted | SmokeErr::CorruptSignatureRecoveredUs(_) => {
                EXIT_CORRUPT_ACCEPTED
            }
            SmokeErr::SelfTest(_) => EXIT_SELFTEST,
            SmokeErr::NotTestnet(_)
            | SmokeErr::Config(_)
            | SmokeErr::Encode
            | SmokeErr::Sign
            | SmokeErr::Unreadable => EXIT_FAILED,
        }
    }
}

impl std::error::Error for SmokeErr {}

/// What a completed smoke observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeReport {
    /// The host it ran against.
    pub host: String,
    /// The agent address whose signature was verified.
    pub agent: String,
    /// Phase A: a good signature was verified by the venue.
    pub signature_verified: bool,
    /// Whether the venue also found the agent REGISTERED — a
    /// `status:"ok"` rather than a recovery that named an address it
    /// has no account for. Reported, never gated on: registration is
    /// a fact about the account, not about this binary's signing.
    pub agent_registered: bool,
    /// Phase B: a corrupted signature was rejected by the venue.
    pub corruption_rejected: bool,
    /// The venue's message on the corrupted attempt, for the log.
    pub corruption_message: String,
    /// SDK vectors the offline self-test reproduced before any packet
    /// left the host.
    pub selftest_rows: u32,
}

impl SmokeReport {
    /// Both signature phases passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.signature_verified && self.corruption_rejected
    }

    /// One line of JSON for CI, matching the stdout-purity law the
    /// other report-producing arms follow.
    ///
    /// Hand-rolled because this crate has no `serde_json` and will not
    /// grow one for five fields — but `corruption_message` is the
    /// VENUE's text, so it is escaped rather than trusted.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(256);
        s.push_str("{\"host\":\"");
        json_escape(&mut s, &self.host);
        s.push_str("\",\"agent\":\"");
        json_escape(&mut s, &self.agent);
        s.push_str("\",\"signature_verified\":");
        s.push_str(if self.signature_verified { "true" } else { "false" });
        s.push_str(",\"agent_registered\":");
        s.push_str(if self.agent_registered { "true" } else { "false" });
        s.push_str(",\"selftest_rows\":");
        push_u32(&mut s, self.selftest_rows);
        s.push_str(",\"corruption_rejected\":");
        s.push_str(if self.corruption_rejected { "true" } else { "false" });
        s.push_str(",\"corruption_message\":\"");
        json_escape(&mut s, &self.corruption_message);
        s.push_str("\",\"passed\":");
        s.push_str(if self.passed() { "true" } else { "false" });
        s.push('}');
        s
    }
}

/// Render a `u32` without `format!`.
fn push_u32(out: &mut String, mut v: u32) {
    if v == 0 {
        out.push('0');
        return;
    }
    let mut d = [0u8; 10];
    let mut i = d.len();
    while v > 0 {
        i -= 1;
        d[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    for b in &d[i..] {
        out.push(*b as char);
    }
}

/// Escape a string into a JSON string body (no surrounding quotes).
fn json_escape(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str("\\u");
                for shift in [12u32, 8, 4, 0] {
                    let nib = ((c as u32) >> shift) & 0xF;
                    out.push(char::from_digit(nib, 16).unwrap_or('0'));
                }
            }
            c => out.push(c),
        }
    }
}

/// An order id that cannot exist, so cancelling it moves nothing.
/// The point is the signature, not the cancel.
const NONEXISTENT_OID: u64 = 1;

/// Run the signature phases (A and B) against testnet.
///
/// `asset` is the venue asset id the probe cancel names; it need not
/// exist for the probe to be meaningful, because the cancel is
/// expected to fail on the ORDER, not on the signature.
pub fn run(cfg: &HlConfig, tls: Arc<rustls::ClientConfig>, asset: u32) -> Result<SmokeReport, SmokeErr> {
    // THE GUARD. Nothing below can run against mainnet.
    if !cfg.is_testnet() {
        return Err(SmokeErr::NotTestnet(cfg.host.clone()));
    }

    // Offline first. If this binary's own encoders no longer agree
    // with the venue's SDK, the network phases would test the one
    // action they send and pronounce the whole artifact healthy.
    let st = crate::selftest::run().map_err(SmokeErr::SelfTest)?;

    let sk = cfg.secret_key().map_err(SmokeErr::Config)?;
    let mut http = HlHttp::new(&cfg.host, 443, tls).map_err(SmokeErr::Http)?;

    let cancels = [CancelWire {
        asset,
        oid: NONEXISTENT_OID,
    }];
    let mut mp = [0u8; MAX_ACTION];
    let mp_n = encode_cancel(&mut mp, &cancels).map_err(|_| SmokeErr::Encode)?;
    let mut aj = [0u8; MAX_ACTION];
    let aj_n = cancel_json(&mut aj, &cancels).map_err(|_| SmokeErr::Encode)?;

    // ---- Phase A: a GOOD signature ---------------------------------
    let nonce_a = now_ms();
    let sig = sign_action(&sk, &mp[..mp_n], nonce_a, Vault::None, None, cfg.network)
        .map_err(|_| SmokeErr::Sign)?;
    let mut body = [0u8; MAX_REQ_BODY];
    let n = envelope(&mut body, &aj[..aj_n], nonce_a, &sig, None, None)
        .map_err(|_| SmokeErr::Encode)?;
    let (_status, range) = http.post(&body[..n]).map_err(SmokeErr::Http)?;
    let resp = http.resp();
    let slice = &resp[range];
    let agent_hex = crate::config::hex20(&cfg.agent_addr);
    let mut agent_registered = false;
    let signature_verified = match scan(slice).map_err(|_| SmokeErr::Unreadable)? {
        // `status: ok` at all means the venue RECOVERED our signer,
        // found an account for it and then judged the order. Whether
        // the (nonexistent) order could be cancelled is beside the
        // point — the recovery is what phase A asks about.
        HlResponse::Ok(_) => {
            agent_registered = true;
            true
        }
        HlResponse::Err { msg } => {
            let text = String::from_utf8_lossy(msg.of(slice)).to_string();
            // The venue can only put OUR address in its answer by
            // having recovered it from OUR signature. An unregistered
            // agent therefore still proves the whole chain.
            if contains_ci(text.as_bytes(), agent_hex.as_bytes()) {
                true
            } else {
                return Err(SmokeErr::SignatureNotVerified(text));
            }
        }
    };

    // ---- Phase B: a CORRUPTED signature ----------------------------
    // Flip one bit of `s`. The result is still a well-formed
    // signature, so the venue must do real work to reject it — which
    // is the point. A malformed blob would only prove it can parse.
    let nonce_b = now_ms().max(nonce_a + 1);
    let mut bad = sign_action(&sk, &mp[..mp_n], nonce_b, Vault::None, None, cfg.network)
        .map_err(|_| SmokeErr::Sign)?;
    bad[40] ^= 0x01;
    let n = envelope(&mut body, &aj[..aj_n], nonce_b, &bad, None, None)
        .map_err(|_| SmokeErr::Encode)?;
    let (_status, range) = http.post(&body[..n]).map_err(SmokeErr::Http)?;
    let resp = http.resp();
    let slice = &resp[range];
    let (corruption_rejected, corruption_message) =
        match scan(slice).map_err(|_| SmokeErr::Unreadable)? {
            HlResponse::Err { msg } => {
                let text = String::from_utf8_lossy(msg.of(slice)).to_string();
                // A broken signature must not recover to us. If it
                // does, the venue is not deriving the signer from the
                // signature and phase A proved nothing.
                if contains_ci(text.as_bytes(), agent_hex.as_bytes()) {
                    return Err(SmokeErr::CorruptSignatureRecoveredUs(text));
                }
                (true, text)
            }
            // The venue accepted a signature we deliberately broke.
            HlResponse::Ok(_) => return Err(SmokeErr::CorruptSignatureAccepted),
        };

    Ok(SmokeReport {
        host: cfg.host.clone(),
        agent: agent_hex,
        signature_verified,
        agent_registered,
        corruption_rejected,
        corruption_message,
        selftest_rows: st.rows,
    })
}

/// The offline half on its own: no network, no credentials, no venue.
///
/// This is what CI runs on every commit touching `exec-hyperliquid` or
/// `signer-eip712`. CI has no testnet key and should not have one, so
/// without this the plan's "runs in CI on every commit" bullet could
/// not be honoured at all — and the half that CI *can* run is the half
/// that catches LAW E-3 across every action type.
pub fn self_test_only() -> Result<crate::selftest::SelfTestReport, SmokeErr> {
    crate::selftest::run().map_err(SmokeErr::SelfTest)
}

/// ASCII-case-insensitive substring search.
///
/// Case-insensitive because the venue may echo an address in EIP-55
/// checksummed form while [`crate::config::hex20`] renders lowercase,
/// and a case mismatch here would turn the strongest evidence this
/// module can collect into a phase-A failure.
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return false;
    }
    let last = hay.len() - needle.len();
    for i in 0..=last {
        let mut j = 0usize;
        while j < needle.len() && hay[i + j].eq_ignore_ascii_case(&needle[j]) {
            j += 1;
        }
        if j == needle.len() {
            return true;
        }
    }
    false
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Scope, HOST_MAINNET, HOST_TESTNET};

    const KEY: [u8; 32] = [0x33; 32];
    const ADDR: [u8; 20] = [0x44; 20];

    /// The venue writes `corruption_message`. A quote or a backslash
    /// in it must not be able to break the line CI parses.
    #[test]
    fn the_venues_text_cannot_break_the_json() {
        let r = SmokeReport {
            host: "api.hyperliquid-testnet.xyz".to_owned(),
            agent: "0xabc".to_owned(),
            signature_verified: true,
            agent_registered: false,
            selftest_rows: 25,
            corruption_rejected: true,
            corruption_message: "he said \"no\"\\ and\nthen \u{1}stopped".to_owned(),
        };
        let j = r.to_json();
        assert!(j.starts_with('{') && j.ends_with('}'), "{j}");
        assert!(!j.contains('\n'), "a raw newline would split the line: {j}");
        assert!(j.contains(r#"\"no\""#), "{j}");
        assert!(j.contains(r"\\ and"), "{j}");
        assert!(j.contains(r"\u0001"), "{j}");
        assert!(j.contains(r#""passed":true"#), "{j}");
        // Quotes are balanced once escaping is undone: count the
        // UNESCAPED ones — six field delimiters plus four values.
        let mut unescaped = 0usize;
        let b: Vec<char> = j.chars().collect();
        for i in 0..b.len() {
            if b[i] == '"' && (i == 0 || b[i - 1] != '\\') {
                unescaped += 1;
            }
        }
        assert_eq!(unescaped % 2, 0, "unbalanced quotes: {j}");
    }

    /// The venue may echo an address checksummed. A case mismatch
    /// here would turn the strongest evidence this module collects —
    /// the venue naming our own recovered address — into a phase-A
    /// failure, and an operator would go hunting a signing bug that
    /// does not exist.
    #[test]
    fn the_address_match_ignores_case_and_nothing_else() {
        let ours = b"0xfcad0b19bb29d4674531d6f115237e16afce377c";
        let checksummed =
            b"User or API Wallet 0xFCAd0B19bB29D4674531d6f115237E16AfCE377c does not exist.";
        assert!(contains_ci(checksummed, ours));
        assert!(contains_ci(b"Unable to recover signer.", b"recover"));
        // A DIFFERENT address must not match — phase B depends on it.
        let other =
            b"User or API Wallet 0x0000000000000000000000000000000000000001 does not exist.";
        assert!(!contains_ci(other, ours));
        // Degenerate inputs never panic and never match.
        assert!(!contains_ci(b"", ours));
        assert!(!contains_ci(b"short", ours));
        assert!(!contains_ci(b"anything", b""));
        // A needle at the very end still matches (the off-by-one).
        assert!(contains_ci(b"xxabc", b"abc"));
        assert!(!contains_ci(b"xxabc", b"abcd"));
    }

    /// `scripts/exec-smoke.sh` branches on these literals. If one
    /// moves, the gate silently mis-reports — so they are pinned here
    /// and the script quotes this test by name.
    #[test]
    fn the_exit_codes_are_pinned() {
        assert_eq!(EXIT_PASS, 0);
        assert_eq!(EXIT_FAILED, 1);
        assert_eq!(EXIT_NOT_VERIFIED, 20);
        assert_eq!(EXIT_CORRUPT_ACCEPTED, 21);
        assert_eq!(EXIT_UNREACHABLE, 22);
        assert_eq!(EXIT_SELFTEST, 23);
        // 2 is clap's argument-error code. A binary too old to know
        // the `exec-smoke` arm exits 2, and must never be reportable
        // as a signing failure.
        for c in [EXIT_NOT_VERIFIED, EXIT_CORRUPT_ACCEPTED, EXIT_UNREACHABLE] {
            assert_ne!(c, 2, "collides with clap's argument-error exit");
        }
        // Only success is zero. A gate that exits 0 on a failure is
        // worse than no gate: it launders the failure into a green.
        for e in [
            SmokeErr::NotTestnet(String::new()),
            SmokeErr::Encode,
            SmokeErr::Sign,
            SmokeErr::Unreadable,
            SmokeErr::SignatureNotVerified(String::new()),
            SmokeErr::CorruptSignatureAccepted,
            SmokeErr::SelfTest(crate::selftest::SelfTestErr::KeyOrder("order")),
        ] {
            assert_ne!(e.code(), EXIT_PASS, "{e:?} exits zero");
        }
    }

    /// **The guard.** No configuration reachable from an operator can
    /// point this at production.
    #[test]
    fn the_smoke_refuses_every_non_testnet_configuration() {
        let tls = core_net::TlsTransport::default_client_config();

        // Mainnet host + mainnet source: a valid config, refused here.
        let m = HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR).expect("valid mainnet cfg");
        let e = run(&m, tls.clone(), 0).expect_err("mainnet must be refused");
        assert!(matches!(e, SmokeErr::NotTestnet(_)), "{e:?}");
        assert!(e.to_string().contains("TESTNET ONLY"), "{e}");

        // A local double is not the testnet venue either — the smoke
        // asserts against the real one or not at all.
        let l = HlConfig::new(Scope::Testnet, "localhost", 'b', KEY, ADDR).expect("valid local cfg");
        assert!(matches!(
            run(&l, tls, 0).expect_err("a double is not testnet"),
            SmokeErr::NotTestnet(_)
        ));
    }

    /// The testnet config is the ONLY one that gets past the guard —
    /// asserted without a network by checking the predicate the guard
    /// uses.
    #[test]
    fn only_the_testnet_configuration_passes_the_guard() {
        assert!(HlConfig::new(Scope::Testnet, HOST_TESTNET, 'b', KEY, ADDR)
            .expect("cfg")
            .is_testnet());
        assert!(!HlConfig::new(Scope::Live, HOST_MAINNET, 'a', KEY, ADDR)
            .expect("cfg")
            .is_testnet());
    }

    #[test]
    fn a_report_passes_only_when_both_phases_did() {
        let base = SmokeReport {
            host: HOST_TESTNET.to_owned(),
            agent: "0x".to_owned(),
            signature_verified: true,
            agent_registered: false,
            selftest_rows: 25,
            corruption_rejected: true,
            corruption_message: String::new(),
        };
        assert!(base.passed());
        assert!(!SmokeReport {
            signature_verified: false,
            ..base.clone()
        }
        .passed());
        assert!(
            !SmokeReport {
                corruption_rejected: false,
                ..base
            }
            .passed(),
            "a venue that accepts a broken signature fails the smoke"
        );
    }

    /// The failure messages are what an operator reads at 3 a.m.
    #[test]
    fn the_failure_messages_say_what_to_do() {
        let a = SmokeErr::SignatureNotVerified("Unable to recover signer.".to_owned());
        assert!(a.to_string().contains("must not be allowed to sign a mainnet order"));
        let b = SmokeErr::CorruptSignatureAccepted;
        assert!(b.to_string().contains("meaningless"));
    }
}
