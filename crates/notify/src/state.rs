// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `/state`: one plain-HTTP GET on loopback per poll, decoded into the few
//! fields the notifier acts on. A missing key reads as zero / empty —
//! schema drift degrades a message, it never stops the notifier. The
//! request is built by `core_net::http1` and the answer framed by it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use core_net::http1::{self, BodyFraming, DechunkResult, HttpResult};

use crate::config::StateUrl;
use crate::json::Json;

/// The `/state` body cap (a full snapshot is ≈ 140 KB).
pub const MAX_STATE_BYTES: usize = 4 << 20;

/// The exec block, as far as the notifier reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecView {
    /// An exec artifact is configured.
    pub configured: bool,
    /// The first reconciliation agreed.
    pub seeded: bool,
    /// `exec.HALT` exists.
    pub halt_file_present: bool,
    /// Halts so far.
    pub halts: u64,
    /// The router's refusals by name, summed below.
    pub refused: Vec<(String, u64)>,
    /// Reconciliations that failed.
    pub recon_failed: u64,
    /// Legs that drifted.
    pub recon_drift_legs: u64,
    /// Legs unseen.
    pub recon_unseen_legs: u64,
    /// The venue request budget left.
    pub budget_remaining: i64,
    /// The session anchor (account worth at arming), ×1e6.
    pub anchor_1e6: i64,
    /// Session P&L against the anchor (fees included), ×1e6.
    pub session_pnl_1e6: i64,
    /// Per slot: the halt word (`none` when trading).
    pub halted: Vec<String>,
    /// Per slot: the hold word (`none` when not held).
    pub held: Vec<String>,
    /// Holds so far.
    pub holds: u64,
    /// Slots an arm serves (from `exec.arms.*.slot`).
    pub arm_slots: Vec<u8>,
    /// The HL gateway's measured fees this session, ×1e8.
    pub gw_fees_1e8: Option<i64>,
    /// An `exec.arms.hyperliquid_gw` block is present.
    pub gw_present: bool,
}

impl ExecView {
    /// Every refusal counter summed.
    #[must_use]
    pub fn refused_total(&self) -> u64 {
        self.refused.iter().map(|(_, n)| *n).sum()
    }
}

/// The parts of `/state` the notifier reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateView {
    /// The engine clock at the snapshot.
    pub mono_ns: u64,
    /// The wall clock at the snapshot.
    pub wall_ns: u64,
    /// The engine pid.
    pub pid: u64,
    /// `boot.git_sha`.
    pub git_sha: String,
    /// `boot.boot_wall_ns` (identifies a boot).
    pub boot_wall_ns: u64,
    /// The capture directory.
    pub run_dir: String,
    /// `--paper`.
    pub paper: bool,
    /// Configured slot mask.
    pub configured_mask: u64,
    /// The exec block.
    pub exec: ExecView,
}

impl StateView {
    /// Is `slot` trading live: not a paper boot, an exec artifact, and an
    /// arm that serves it.
    #[must_use]
    pub fn slot_live(&self, slot: u8) -> bool {
        !self.paper && self.exec.configured && self.exec.arm_slots.contains(&slot)
    }

    /// Engine-clock ns → wall ns, by this snapshot's own pair.
    #[must_use]
    pub fn mono_to_wall_ns(&self, mono: u64) -> u64 {
        if mono > 1_500_000_000_000_000_000 {
            return mono; // already a wall stamp
        }
        (i128::from(self.wall_ns) - i128::from(self.mono_ns) + i128::from(mono)).max(0) as u64
    }

    /// A boot's identity.
    #[must_use]
    pub fn boot_id(&self) -> String {
        format!("{}:{}", self.git_sha, self.boot_wall_ns)
    }
}

fn u(j: &Json, path: &str) -> u64 {
    j.path(path)
        .and_then(Json::as_i64)
        .map_or(0, |v| v.max(0) as u64)
}

fn i(j: &Json, path: &str) -> i64 {
    j.path(path).and_then(Json::as_i64).unwrap_or(0)
}

fn words(j: &Json, path: &str) -> Vec<String> {
    j.path(path).map_or_else(Vec::new, |a| {
        a.items()
            .iter()
            .map(|w| w.as_str().unwrap_or("?").to_string())
            .collect()
    })
}

const REFUSED_KEYS: &[&str] = &[
    "refused_halted",
    "refused_unseeded",
    "refused_short",
    "refused_unpriced",
    "refused_held",
];
const GW_REFUSED_KEYS: &[&str] = &[
    "refused_local",
    "refused_budget",
    "refused_leverage",
    "throttled",
];

impl StateView {
    /// Decode a parsed `/state` document.
    #[must_use]
    pub fn from_json(j: &Json) -> StateView {
        let mut refused: Vec<(String, u64)> = REFUSED_KEYS
            .iter()
            .map(|k| ((*k).to_string(), u(j, &format!("exec.{k}"))))
            .collect();
        let mut arm_slots = Vec::new();
        let mut gw_fees_1e8 = None;
        let mut gw_present = false;
        let mut gw_budget = None;
        if let Some(arms) = j.path("exec.arms") {
            for (name, arm) in arms.members() {
                if let Some(s) = arm.get("slot").and_then(Json::as_i64) {
                    if (0..16).contains(&s) && !arm_slots.contains(&(s as u8)) {
                        arm_slots.push(s as u8);
                    }
                }
                if name == "hyperliquid_gw" {
                    gw_present = true;
                    gw_fees_1e8 = arm.get("fees_1e8").and_then(Json::as_i64);
                    if arm.get("budget_known").and_then(Json::as_i64) == Some(1) {
                        gw_budget = arm.get("budget_remaining").and_then(Json::as_i64);
                    }
                    for k in GW_REFUSED_KEYS {
                        refused.push((
                            format!("gw.{k}"),
                            arm.get(k)
                                .and_then(Json::as_i64)
                                .map_or(0, |v| v.max(0) as u64),
                        ));
                    }
                }
            }
        }
        StateView {
            mono_ns: u(j, "now.mono_ns"),
            wall_ns: u(j, "now.wall_ns"),
            pid: u(j, "boot.pid"),
            git_sha: j
                .path("boot.git_sha")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            boot_wall_ns: u(j, "boot.boot_wall_ns"),
            run_dir: j
                .path("boot.run_dir")
                .and_then(Json::as_str)
                .unwrap_or("")
                .to_string(),
            paper: u(j, "boot.paper") != 0,
            configured_mask: u(j, "boot.configured_mask"),
            exec: ExecView {
                configured: u(j, "exec.configured") != 0,
                seeded: u(j, "exec.seeded") != 0,
                halt_file_present: u(j, "exec.halt_file_present") != 0,
                halts: u(j, "exec.halts"),
                refused,
                recon_failed: u(j, "exec.arm_recon_failed"),
                recon_drift_legs: u(j, "exec.arm_recon_drift_legs"),
                recon_unseen_legs: u(j, "exec.arm_recon_unseen_legs"),
                // The gateway transport keeps the venue budget in its own
                // block; `arm_budget_remaining` is the blocking arm's.
                budget_remaining: match (i(j, "exec.arm_budget_remaining"), gw_budget) {
                    (0, Some(g)) => g,
                    (b, _) => b,
                },
                anchor_1e6: i(j, "exec.arm_pnl_anchor_usd_1e6"),
                session_pnl_1e6: i(j, "exec.arm_session_pnl_usd_1e6"),
                halted: words(j, "exec.halted"),
                held: words(j, "exec.held"),
                holds: u(j, "exec.holds"),
                arm_slots,
                gw_fees_1e8,
                gw_present,
            },
        }
    }
}

/// Why a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchErr {
    /// Nothing listens, or the socket failed.
    Net(String),
    /// Not a 200, or not HTTP.
    Http(String),
    /// Not JSON.
    Json(String),
}

impl std::fmt::Display for FetchErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchErr::Net(s) => write!(f, "/state unreachable: {s}"),
            FetchErr::Http(s) => write!(f, "/state answered badly: {s}"),
            FetchErr::Json(s) => write!(f, "/state is not JSON: {s}"),
        }
    }
}

/// The body of a complete HTTP/1.1 response in `buf` (dechunked in place).
///
/// # Errors
/// [`FetchErr::Http`].
pub fn body_of(buf: &mut [u8]) -> Result<std::ops::Range<usize>, FetchErr> {
    match http1::read_response(buf) {
        HttpResult::Complete {
            status,
            body_start,
            body_end,
            framing,
            ..
        } => {
            if status != 200 {
                return Err(FetchErr::Http(format!("status {status}")));
            }
            match framing {
                BodyFraming::ContentLength(n) => {
                    let end = body_start + usize::try_from(n).unwrap_or(usize::MAX);
                    if end > buf.len() {
                        return Err(FetchErr::Http("body shorter than Content-Length".into()));
                    }
                    Ok(body_start..end)
                }
                BodyFraming::CloseDelimited => Ok(body_start..buf.len().max(body_end)),
                BodyFraming::Chunked => match http1::dechunk_in_place(&mut buf[body_start..]) {
                    DechunkResult::Complete { length } => Ok(body_start..body_start + length),
                    _ => Err(FetchErr::Http("bad chunked body".into())),
                },
            }
        }
        HttpResult::Incomplete => Err(FetchErr::Http("truncated response".into())),
        HttpResult::Malformed => Err(FetchErr::Http("malformed response".into())),
    }
}

/// GET `path` on the `/state` host and return the body.
///
/// # Errors
/// [`FetchErr`].
fn get(url: &StateUrl, path: &str, timeout: Duration) -> Result<Vec<u8>, FetchErr> {
    let addr: SocketAddr = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| FetchErr::Net(e.to_string()))?
        .next()
        .ok_or_else(|| FetchErr::Net("no address".into()))?;
    let mut sock =
        TcpStream::connect_timeout(&addr, timeout).map_err(|e| FetchErr::Net(e.to_string()))?;
    sock.set_read_timeout(Some(timeout))
        .map_err(|e| FetchErr::Net(e.to_string()))?;
    sock.set_write_timeout(Some(timeout))
        .map_err(|e| FetchErr::Net(e.to_string()))?;
    let mut req = [0u8; 512];
    let n = http1::write_get_request(
        &mut req,
        url.host.as_bytes(),
        path.as_bytes(),
        b"multivenue-notify",
    )
    .map_err(|_| FetchErr::Http("request does not fit".into()))?;
    sock.write_all(&req[..n])
        .map_err(|e| FetchErr::Net(e.to_string()))?;
    let mut buf = Vec::with_capacity(256 << 10);
    let mut chunk = [0u8; 64 << 10];
    loop {
        match sock.read(&mut chunk) {
            Ok(0) => break,
            Ok(k) => {
                buf.extend_from_slice(&chunk[..k]);
                if buf.len() > MAX_STATE_BYTES {
                    return Err(FetchErr::Http("body over the cap".into()));
                }
                // A framed answer may complete before the close.
                if let HttpResult::Complete {
                    framing: BodyFraming::ContentLength(len),
                    body_start,
                    ..
                } = http1::read_response(&buf)
                {
                    if buf.len() >= body_start + usize::try_from(len).unwrap_or(usize::MAX) {
                        break;
                    }
                }
            }
            Err(e) => return Err(FetchErr::Net(e.to_string())),
        }
    }
    let range = body_of(&mut buf)?;
    buf.truncate(range.end);
    buf.drain(..range.start);
    Ok(buf)
}

/// The `/metrics` gauge `name`, if present.
#[must_use]
pub fn metric(text: &str, name: &str) -> Option<i64> {
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(' ')?;
        (k == name).then(|| v.trim().split(['.', 'e', 'E']).next()?.parse().ok())?
    })
}

impl StateView {
    /// The Hyperliquid gateway keeps its session anchor and the account's
    /// worth in `/metrics` only (`exec.arm_pnl_*` on `/state` are the
    /// blocking transport's). Fill them in from the metrics text.
    pub fn apply_metrics(&mut self, text: &str) {
        let anchor = metric(text, "engine_exec_hlgw_pnl_anchor_usd_1e6").unwrap_or(0);
        let worth = metric(text, "engine_exec_hlgw_worth_usd_1e6").unwrap_or(0);
        if self.exec.anchor_1e6 == 0 && anchor > 0 && worth > 0 {
            self.exec.anchor_1e6 = anchor;
            self.exec.session_pnl_1e6 = worth - anchor;
        }
    }
}

/// GET `/state` and decode it; with a gateway arm and no anchor on
/// `/state`, the anchor and worth come from `/metrics` on the same port.
///
/// # Errors
/// [`FetchErr`] (a failed `/metrics` read is not an error).
pub fn fetch(url: &StateUrl, timeout: Duration) -> Result<StateView, FetchErr> {
    let body = get(url, &url.path, timeout)?;
    let j = Json::parse(&body).map_err(|e| FetchErr::Json(e.to_string()))?;
    let mut sv = StateView::from_json(&j);
    if sv.exec.configured && sv.exec.gw_present && sv.exec.anchor_1e6 == 0 {
        if let Ok(m) = get(url, "/metrics", timeout) {
            sv.apply_metrics(&String::from_utf8_lossy(&m));
        }
    }
    Ok(sv)
}

#[cfg(test)]
pub(crate) mod testkit {
    //! COPY-DOCTRINE: test-only (declared under `#[cfg(test)]`).

    /// A `/state` body in the engine's own shape, trimmed to what we read.
    pub fn body(
        boot_wall_ns: u64,
        run_dir: &str,
        live_slot: Option<u8>,
        halted3: &str,
        held3: &str,
        session_pnl_1e6: i64,
    ) -> String {
        let arms = live_slot.map_or(String::new(), |s| {
            format!(r#","arms":{{"hyperliquid_gw":{{"slot":{s},"ready":1,"ws_up":1,"refused_local":0,"refused_budget":0,"refused_leverage":0,"throttled":0,"fees_1e8":12300000}}}}"#)
        });
        let mut halted = vec!["\"none\"".to_string(); 16];
        halted[3] = format!("\"{halted3}\"");
        let mut held = vec!["\"none\"".to_string(); 16];
        held[3] = format!("\"{held3}\"");
        format!(
            r#"{{"v":3,"seq":1,"now":{{"mono_ns":"1000000000","wall_ns":"1792846648000000000","uptime_s":1}},"boot":{{"pid":33607,"git_sha":"57bcdf6d","binary_mtime_ns":"1","boot_wall_ns":"{boot_wall_ns}","run_epoch_ns":"1","run_dir":"{run_dir}","strategy":"bin15","strategy_kind":"set","paper":0,"requested_mask":8,"configured_mask":8,"enabled_mask":8,"halted":0}},"slots":[],"exec":{{"configured":{conf},"seeded":1,"adopted_from_file":0,"halt_file_present":0,"halts":0,"refused_halted":0,"refused_unseeded":0,"arm_recon_ok":5,"arm_recon_failed":0,"arm_recon_drift_legs":0,"arm_recon_unseen_legs":0,"arm_budget_remaining":9990,"arm_pnl_anchor_usd_1e6":1248830000,"arm_session_pnl_usd_1e6":{session_pnl_1e6},"refused_short":0,"refused_unpriced":0{arms},"halted":[{halted}],"held":[{held}],"holds":0,"refused_held":0}}}}"#,
            conf = u8::from(live_slot.is_some()),
            halted = halted.join(","),
            held = held.join(","),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_engine_shape() {
        let b = testkit::body(
            1_792_800_000_000_000_000,
            "/x/runs/run-1",
            Some(3),
            "none",
            "ws-gap",
            -12_500_000,
        );
        let j = Json::parse(b.as_bytes()).unwrap();
        let s = StateView::from_json(&j);
        assert_eq!(s.git_sha, "57bcdf6d");
        assert_eq!(s.pid, 33607);
        assert_eq!(s.run_dir, "/x/runs/run-1");
        assert!(s.slot_live(3));
        assert!(!s.slot_live(6));
        assert_eq!(s.exec.halted[3], "none");
        assert_eq!(s.exec.held[3], "ws-gap");
        assert_eq!(s.exec.anchor_1e6, 1_248_830_000);
        assert_eq!(s.exec.session_pnl_1e6, -12_500_000);
        assert_eq!(s.exec.gw_fees_1e8, Some(12_300_000));
        assert_eq!(s.exec.refused_total(), 0);
        assert_eq!(s.mono_to_wall_ns(2_000_000_000), 1_792_846_649_000_000_000);
        assert_eq!(
            s.mono_to_wall_ns(1_792_846_649_000_000_000),
            1_792_846_649_000_000_000
        );
    }

    #[test]
    fn the_gateway_budget_and_metrics_anchor_fill_in() {
        let b = testkit::body(1, "/r", Some(3), "none", "none", 0)
            .replace(
                "\"arm_budget_remaining\":9990",
                "\"arm_budget_remaining\":0",
            )
            .replace(
                "\"arm_pnl_anchor_usd_1e6\":1248830000",
                "\"arm_pnl_anchor_usd_1e6\":0",
            )
            .replace(
                "\"fees_1e8\":12300000",
                "\"fees_1e8\":12300000,\"budget_known\":1,\"budget_remaining\":9962",
            );
        let mut s = StateView::from_json(&Json::parse(b.as_bytes()).unwrap());
        assert!(s.exec.gw_present);
        assert_eq!(s.exec.budget_remaining, 9962);
        assert_eq!(s.exec.anchor_1e6, 0);
        let m = "# TYPE engine_exec_hlgw_pnl_anchor_usd_1e6 gauge\nengine_exec_hlgw_pnl_anchor_usd_1e6 1248829952\nengine_exec_hlgw_anchor_kept 1\nengine_exec_hlgw_worth_usd_1e6 1261102712\n";
        assert_eq!(metric(m, "engine_exec_hlgw_anchor_kept"), Some(1));
        assert_eq!(metric(m, "missing"), None);
        s.apply_metrics(m);
        assert_eq!(s.exec.anchor_1e6, 1_248_829_952);
        assert_eq!(s.exec.session_pnl_1e6, 12_272_760);
        // An anchor already on /state (the blocking transport) wins.
        let mut t = s.clone();
        t.exec.anchor_1e6 = 5;
        t.exec.session_pnl_1e6 = 7;
        t.apply_metrics(m);
        assert_eq!((t.exec.anchor_1e6, t.exec.session_pnl_1e6), (5, 7));
    }

    #[test]
    fn a_paper_boot_is_never_live() {
        let b = testkit::body(1, "/r", None, "none", "none", 0);
        let s = StateView::from_json(&Json::parse(b.as_bytes()).unwrap());
        assert!(!s.slot_live(3));
        assert!(!s.exec.configured);
    }

    #[test]
    fn missing_keys_read_as_zero() {
        let s = StateView::from_json(&Json::parse(b"{}").unwrap());
        assert_eq!(
            s,
            StateView {
                exec: ExecView {
                    refused: s.exec.refused.clone(),
                    ..ExecView::default()
                },
                ..StateView::default()
            }
        );
    }

    #[test]
    fn frames_content_length_and_chunked_answers() {
        let mut a = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec();
        let r = body_of(&mut a).unwrap();
        assert_eq!(&a[r], b"{}");
        let mut c =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\n{\r\n1\r\n}\r\n0\r\n\r\n"
                .to_vec();
        let r = body_of(&mut c).unwrap();
        assert_eq!(&c[r], b"{}");
        let mut e = b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n".to_vec();
        assert!(matches!(body_of(&mut e), Err(FetchErr::Http(_))));
    }

    #[test]
    fn fetches_over_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = testkit::body(7, "/r/run-9", Some(3), "none", "none", 0);
        let h = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut req = [0u8; 1024];
            let _ = s.read(&mut req).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).unwrap();
        });
        let url = StateUrl {
            host: "127.0.0.1".into(),
            port,
            path: "/state".into(),
        };
        let s = fetch(&url, Duration::from_secs(2)).unwrap();
        h.join().unwrap();
        assert_eq!(s.run_dir, "/r/run-9");
        assert_eq!(s.boot_wall_ns, 7);
    }

    #[test]
    fn pins_the_state_words() {
        assert_eq!(engine_snapshot::hold_word(0), "none");
        assert_eq!(engine_snapshot::halt_reason_word(0), "none");
    }
}
