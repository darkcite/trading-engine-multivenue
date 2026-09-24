// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! The run loop over `TestTransport`, against the model node: handshake,
//! both subscriptions, the pinned snapshot read through the driver's own
//! frames, held events, Live, and every resync path.
//!
//! COPY-DOCTRINE: test-only module (declared under `#[cfg(test)]` in
//! run_loop.rs); every copy here assembles a fixture frame.

use super::*;
use crate::pools::{PoolEntry, PoolFamily};
use crate::snapshot::ReadKind;
use crate::testnode::FakePool;
use core_amm::payload::{decode, PoolEvent};
use core_net::{
    expected_accept as accept_pub, sec_websocket_key_from_seed as key_pub, TestTransport,
};
use core_ring::{Consumer, Ring};
use core_types::NullCapture;

const B: u64 = 46_650_000;
const CAP: usize = 4096;

fn pools() -> Vec<FakePool> {
    vec![
        FakePool::from_positions(
            PoolFamily::UniswapV3,
            [0x10; 20],
            -230_543,
            10,
            500,
            &[(-231_000, -230_000, 5_000_000), (-240_000, -220_000, 7)],
        ),
        FakePool::from_positions(
            PoolFamily::Slipstream,
            [0x20; 20],
            12_345,
            100,
            2_500,
            &[(10_000, 15_000, 42_000)],
        ),
        FakePool::from_positions(
            PoolFamily::Algebra,
            [0x30; 20],
            -297_448,
            1,
            1_234,
            &[(-297_500, -297_400, 77_000), (-299_000, -296_000, 5)],
        ),
    ]
}

fn driver(ps: &[FakePool]) -> Driver {
    let mut e = Vec::new();
    for (i, p) in ps.iter().enumerate() {
        e.push(PoolEntry {
            address: p.address,
            sym: 100 + i as u32,
            family: p.family,
            dec0: 18,
            dec1: 6,
        });
    }
    Driver::new(42, PoolTable::new(&e).unwrap(), 4_000)
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = vec![0x81];
    if body.len() <= 125 {
        out.push(body.len() as u8);
    } else {
        assert!(body.len() <= u16::MAX as usize);
        out.push(126);
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(body);
    out
}

/// Every masked client frame's body in the transport's outgoing bytes.
fn client_bodies(t: &mut TestTransport) -> Vec<String> {
    let mut buf = vec![0u8; 1 << 20];
    let n = t.drain_outgoing(&mut buf);
    let b = &buf[..n];
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        assert_eq!(b[i] & 0x0f, 0x2, "binary frames");
        assert!(b[i + 1] & 0x80 != 0, "client frames are masked");
        let mut len = (b[i + 1] & 0x7f) as usize;
        let mut h = 2;
        if len == 126 {
            len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            h = 4;
        } else if len == 127 {
            len = u64::from_be_bytes(b[i + 2..i + 10].try_into().unwrap()) as usize;
            h = 10;
        }
        let mask = [b[i + h], b[i + h + 1], b[i + h + 2], b[i + h + 3]];
        let body: Vec<u8> = (0..len).map(|k| b[i + h + 4 + k] ^ mask[k & 3]).collect();
        out.push(String::from_utf8(body).unwrap());
        i += h + 4 + len;
    }
    out
}

fn field<'a>(s: &'a str, key: &str) -> &'a str {
    let at = s.find(key).unwrap_or_else(|| panic!("{key} in {s}")) + key.len();
    let rest = &s[at..];
    let end = rest.find(['"', ',', '}']).unwrap();
    &rest[..end]
}

/// Answer every request the driver sent; returns the reply frames.
fn answer_all(bodies: &[String], ps: &[FakePool], snap_block: u64) -> Vec<u8> {
    let mut out = Vec::new();
    for body in bodies {
        let id: u64 = field(body, "\"id\":").parse().unwrap();
        let reply = if body.contains("eth_blockNumber") {
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"0x{snap_block:x}"}}"#)
        } else if body.contains("\"newHeads\"") {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":"0x9cef478923ff08bf67fde6c64013158d"}}"#
            )
        } else if body.contains("\"logs\"") {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":"0x1111478923ff08bf67fde6c640131500"}}"#
            )
        } else {
            assert!(body.contains("eth_call"), "unexpected request {body}");
            let to = field(body, "\"to\":\"");
            let data = field(body, "\"data\":\"");
            let block = u64::from_str_radix(&field(body, "},\"")[2..], 16).unwrap();
            let to_addr = crate::hex::hex_fixed::<20>(to.as_bytes(), 0).unwrap().0;
            // The two `decimals()` reads are addressed to a TOKEN.
            let (p, token) = match ps.iter().find(|p| p.address == to_addr) {
                Some(p) => (p, None),
                None => {
                    let p = ps
                        .iter()
                        .find(|p| p.tokens().0 == to_addr || p.tokens().1 == to_addr)
                        .unwrap();
                    (p, Some(p.tokens().1 == to_addr))
                }
            };
            let kind = match &data[2..10] {
                "313ce567" => match token {
                    Some(true) => ReadKind::Dec1,
                    Some(false) => ReadKind::Dec0,
                    None => panic!("decimals() sent to a pool"),
                },
                "0dfe1681" => ReadKind::Token0,
                "d21220a7" => ReadKind::Token1,
                "3850c7bd" | "e76c01e4" => ReadKind::Head,
                "1a686502" => ReadKind::Liquidity,
                "ddca3f43" => ReadKind::Fee,
                "d0c93a7c" => ReadKind::Spacing,
                "050a4d21" => ReadKind::Prev,
                "d5c35a7e" => ReadKind::Next,
                "5339c296" => ReadKind::Bitmap,
                "f30dba93" => ReadKind::Tick,
                s => panic!("selector {s}"),
            };
            let arg = if data.len() == 74 {
                crate::hex::word_i24(&data.as_bytes()[10..]).unwrap()
            } else {
                0
            };
            let r = p.answer(kind, arg, block != snap_block);
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":"{r}"}}"#)
        };
        out.extend_from_slice(&frame(reply.as_bytes()));
    }
    out
}

fn head_push(n: u64) -> Vec<u8> {
    frame(format!(r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"0x9cef478923ff08bf67fde6c64013158d","result":{{"number":"0x{n:x}","timestamp":"0x68d2a1f3","baseFeePerGas":"0x5f5e100"}}}}}}"#).as_bytes())
}

fn swap_push(addr: [u8; 20], block: u64, log_index: u64, removed: bool, a0: i128) -> Vec<u8> {
    let w = |v: i128| {
        if v < 0 {
            format!("{}{:032x}", "f".repeat(32), v as u128)
        } else {
            format!("{:064x}", v as u128)
        }
    };
    let (s, _) = core_amm::sqrt_at_tick(-230_500);
    let data = format!(
        "{}{}{}{}{}",
        w(a0),
        w(-7),
        w(s as i128),
        w(123),
        w(-230_500)
    );
    let a: String = addr.iter().map(|b| format!("{b:02x}")).collect();
    let body = format!(
        r#"{{"jsonrpc":"2.0","method":"eth_subscription","params":{{"subscription":"0x1111478923ff08bf67fde6c640131500","result":{{"address":"0x{a}","topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67","0x{z}","0x{z}"],"data":"0x{data}","blockNumber":"0x{block:x}","logIndex":"0x{log_index:x}","removed":{removed}}}}}}}"#,
        z = "0".repeat(64)
    );
    frame(body.as_bytes())
}

struct Rig {
    t: TestTransport,
    d: Driver,
    status: IngressStatus,
}

impl Rig {
    fn step(&mut self, prod: &mut Producer<Signal, CAP>) -> io::Result<()> {
        drive_one(
            &mut self.t,
            &mut self.d,
            b"rpc.example",
            b"/",
            prod,
            &self.status,
            &mut NullCapture,
        )
    }
}

/// Handshake + subscriptions → AwaitHead.
fn to_await_head(ps: &[FakePool], prod: &mut Producer<Signal, CAP>) -> Rig {
    let mut r = Rig {
        t: TestTransport::with_capacity(1 << 20),
        d: driver(ps),
        status: IngressStatus::new(),
    };
    note_transport_ready(&mut r.d, Status::Ready);
    r.step(prod).unwrap();
    let mut scratch = vec![0u8; 8192];
    let _ = r.t.drain_outgoing(&mut scratch);
    let accept = accept_pub(&key_pub(42));
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&accept);
    resp.extend_from_slice(b"\r\n\r\n");
    r.t.inject_incoming(&resp);
    r.step(prod).unwrap();
    assert_eq!(r.d.state(), State::Steady);
    r.d.suppress_polling_for_test();
    let bodies = client_bodies(&mut r.t);
    assert_eq!(bodies.len(), 2, "newHeads + logs");
    assert!(bodies[0].contains("\"newHeads\""));
    assert!(bodies[1].contains("\"logs\""));
    for p in ps {
        let a: String = p.address.iter().map(|b| format!("{b:02x}")).collect();
        assert!(bodies[1].contains(&a), "every pool subscribed");
    }
    assert!(bodies[1].contains("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"));
    r.t.inject_incoming(&answer_all(&bodies, ps, B));
    r.step(prod).unwrap();
    assert_eq!(r.d.phase(), Phase::AwaitHead);
    r
}

/// Answer reads until the driver is Live.
fn to_live(r: &mut Rig, ps: &[FakePool], prod: &mut Producer<Signal, CAP>, mid: &[Vec<u8>]) {
    let mut injected_mid = false;
    let mut guard = 0;
    while r.d.phase() != Phase::Live {
        guard += 1;
        assert!(guard < 200, "stuck in {:?}", r.d.phase());
        let bodies = client_bodies(&mut r.t);
        let mut replies = answer_all(&bodies, ps, B);
        if !injected_mid && !bodies.is_empty() {
            for m in mid {
                replies.extend_from_slice(m);
            }
            injected_mid = true;
        }
        r.t.inject_incoming(&replies);
        r.step(prod).unwrap();
    }
}

fn drain(cons: &mut Consumer<Signal, CAP>) -> Vec<(u32, PoolEvent)> {
    let mut v = Vec::new();
    while let Some(s) = cons.try_pop_ref() {
        assert_eq!(s.source, SIGNAL_SOURCE_HYPEREVM);
        v.push((s.sym, decode(&s.payload).expect("payload decodes")));
    }
    v
}

#[test]
fn the_full_lifecycle_snapshot_then_held_events_then_live() {
    let ps = pools();
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);

    // A log at B−1 arrives before the pinning head (held, then covered);
    // the head pins B.
    let mut early = swap_push([0x10; 20], B - 1, 0, false, 5);
    early.extend_from_slice(&head_push(B));
    r.t.inject_incoming(&early);
    r.step(&mut prod).unwrap();
    assert_eq!(r.d.phase(), Phase::Reading);

    // Mid-snapshot: a swap at B (covered) and one at B+1 (held → emitted).
    let mid = vec![
        swap_push([0x10; 20], B, 1, false, 6),
        swap_push([0x10; 20], B + 1, 0, false, 7),
        head_push(B + 1),
    ];
    to_live(&mut r, &ps, &mut prod, &mid);

    let sig = drain(&mut cons);
    // Three snapshots (each SNAPSHOT … STATE), then B+1's SWAP, STATE and HEAD.
    let snaps = sig
        .iter()
        .filter(|x| matches!(x.1, PoolEvent::Snapshot { .. }))
        .count();
    assert_eq!(snaps, 3);
    let tail = &sig[sig.len() - 3..];
    assert_eq!(
        tail[0],
        (
            100,
            PoolEvent::Swap {
                block: B + 1,
                amount0: 7,
                amount1: -7
            }
        )
    );
    assert!(matches!(
        tail[1],
        (
            100,
            PoolEvent::State {
                tick: -230_500,
                snapshot: false,
                ..
            }
        )
    ));
    assert!(matches!(tail[2], (SYMBOL_ID_NONE, PoolEvent::Head { block, .. }) if block == B + 1));
    assert!(
        sig.iter()
            .all(|x| !matches!(x.1, PoolEvent::Swap { block, .. } if block <= B)),
        "covered events never emitted"
    );
    assert_eq!(
        r.d.counters().held_covered,
        2 * 2,
        "two swaps × (SWAP + STATE) covered"
    );
    assert_eq!(r.d.counters().snapshots, 1);

    // Live: straight through.
    r.t.inject_incoming(&swap_push([0x30; 20], B + 2, 3, false, -9));
    r.step(&mut prod).unwrap();
    let sig = drain(&mut cons);
    assert_eq!(
        sig[0],
        (
            102,
            PoolEvent::Swap {
                block: B + 2,
                amount0: -9,
                amount1: -7
            }
        )
    );
    assert_eq!(sig.len(), 2);

    // A retracted log: GAP, and a fresh snapshot on the same connection.
    r.t.inject_incoming(&swap_push([0x30; 20], B + 2, 3, true, -9));
    r.step(&mut prod).unwrap();
    let sig = drain(&mut cons);
    assert_eq!(sig, vec![(SYMBOL_ID_NONE, PoolEvent::Gap { block: B + 2 })]);
    assert_eq!(r.d.phase(), Phase::AwaitHead);
    assert_eq!(r.d.counters().resyncs, 1);
    assert_eq!(r.d.sub_count(), 2, "subscriptions survive a resync");
}

#[test]
fn a_ring_drop_in_live_forces_a_resync() {
    let ps = pools();
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);
    r.t.inject_incoming(&head_push(B));
    r.step(&mut prod).unwrap();
    to_live(&mut r, &ps, &mut prod, &[]);
    let _ = drain(&mut cons);
    // Fill the ring to the brim, then deliver a swap: it cannot be pushed.
    let filler = Signal::new(0, 0, core_types::LatencyClass::Warm, 0, [0; 40]);
    while prod.try_push_ref(&filler) {}
    r.t.inject_incoming(&swap_push([0x10; 20], B + 5, 0, false, 1));
    r.step(&mut prod).unwrap();
    assert_eq!(r.status.ring_drops_total(), 1);
    assert_eq!(r.d.phase(), Phase::Live, "the resync waits for room");
    while cons.try_pop_ref().is_some() {}
    r.step(&mut prod).unwrap();
    assert_eq!(r.d.phase(), Phase::AwaitHead);
    let sig = drain(&mut cons);
    assert_eq!(sig, vec![(SYMBOL_ID_NONE, PoolEvent::Gap { block: B })]);
}

#[test]
fn an_uncarriable_event_stales_only_its_pool() {
    let ps = pools();
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);
    r.t.inject_incoming(&head_push(B));
    r.step(&mut prod).unwrap();
    to_live(&mut r, &ps, &mut prod, &[]);
    let _ = drain(&mut cons);
    // amount0 = 2^127 as a uint256: beyond int128 → refused.
    let mut push = swap_push([0x20; 20], B + 3, 0, false, 1);
    let at = push
        .windows(10)
        .position(|w| w == b"\"data\":\"0x")
        .unwrap()
        + 10;
    push[at..at + 64].copy_from_slice(format!("{}8{}", "0".repeat(32), "0".repeat(31)).as_bytes());
    r.t.inject_incoming(&push);
    r.step(&mut prod).unwrap();
    let sig = drain(&mut cons);
    assert_eq!(
        sig,
        vec![(101, PoolEvent::Gap { block: B })],
        "the snapshot block is the last delivered"
    );
    assert_eq!(r.d.counters().logs_out_of_range, 1);
    assert_eq!(r.d.phase(), Phase::Live, "the other pools keep streaming");
}

#[test]
fn a_dishonest_archive_ends_the_session_with_its_own_verdict() {
    let mut ps = pools();
    for p in ps.iter_mut() {
        p.probe_sqrt = p.sqrt;
    }
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);
    r.t.inject_incoming(&head_push(B));
    r.step(&mut prod).unwrap();
    let mut failed = false;
    for _ in 0..50 {
        let bodies = client_bodies(&mut r.t);
        r.t.inject_incoming(&answer_all(&bodies, &ps, B));
        if r.step(&mut prod).is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "the probe must end the session");
    assert!(r.d.archive_dishonest);
    assert!(
        drain(&mut cons).is_empty(),
        "nothing from a dishonest snapshot reaches the ring"
    );
}

#[test]
fn a_reconnect_announces_the_break() {
    let ps = pools();
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);
    r.t.inject_incoming(&head_push(B));
    r.step(&mut prod).unwrap();
    to_live(&mut r, &ps, &mut prod, &[]);
    r.t.inject_incoming(&swap_push([0x10; 20], B + 9, 0, false, 1));
    r.step(&mut prod).unwrap();
    let _ = drain(&mut cons);
    r.d.reset_for_reconnect(43);
    r.d.set_state(State::AwaitingWsUpgrade);
    let accept = accept_pub(&key_pub(43));
    let mut resp = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ".to_vec();
    resp.extend_from_slice(&accept);
    resp.extend_from_slice(b"\r\n\r\n");
    let _ = client_bodies(&mut r.t);
    r.t.inject_incoming(&resp);
    r.step(&mut prod).unwrap();
    assert_eq!(
        drain(&mut cons),
        vec![(SYMBOL_ID_NONE, PoolEvent::Gap { block: B + 9 })]
    );
    assert_eq!(r.d.phase(), Phase::Subscribing);
}

/// A pool with 300 initialised ticks around its price (the live WHYPE /
/// USDC shape: nearly every tick of a 10-spaced grid initialised).
fn dense_pool() -> Vec<FakePool> {
    let mut pos = Vec::new();
    let mut k = 1i32;
    while k <= 150 {
        pos.push((-230_550 - 10 * k, -230_530 + 10 * k, 1_000 + k as u128));
        k += 1;
    }
    vec![FakePool::from_positions(
        PoolFamily::UniswapV3,
        [0x10; 20],
        -230_543,
        10,
        500,
        &pos,
    )]
}

/// HYPARB H3b live-smoke finding: the archive endpoint answers a deep
/// burst of reads OUT OF ORDER, and a read can stay unanswered while
/// hundreds of later ids complete. The driver keeps at most
/// `MAX_READS_IN_FLIGHT` reads out, skips the pending slot a straggler
/// still holds instead of colliding with it (the old behaviour ended the
/// session), and completes the snapshot once the straggler is answered.
#[test]
fn a_dense_snapshot_is_read_in_a_bounded_window_around_a_straggler() {
    let ps = dense_pool();
    let ring = Ring::<Signal, CAP>::new();
    let (mut prod, mut cons) = ring.split();
    let mut r = to_await_head(&ps, &mut prod);
    r.t.inject_incoming(&head_push(B));
    r.step(&mut prod).unwrap();
    assert_eq!(r.d.phase(), Phase::Reading);

    let mut held: Option<(u64, String)> = None;
    let mut released = false;
    let mut max_id = 0u64;
    let mut rounds = 0;
    while r.d.phase() != Phase::Live {
        rounds += 1;
        assert!(rounds < 1_000, "stuck in {:?}", r.d.phase());
        assert!(r.d.reads_in_flight() <= MAX_READS_IN_FLIGHT);
        let mut bodies = client_bodies(&mut r.t);
        let calls = bodies.iter().filter(|b| b.contains("eth_call")).count();
        assert!(calls <= MAX_READS_IN_FLIGHT, "{calls} reads in one round");
        for b in &bodies {
            max_id = max_id.max(field(b, "\"id\":").parse().unwrap());
        }
        // Hold back the first `ticks()` read until it is the only read
        // left; answer the rest newest-first.
        if held.is_none() {
            if let Some(i) = bodies.iter().position(|b| b.contains("f30dba93")) {
                let b = bodies.remove(i);
                held = Some((field(&b, "\"id\":").parse().unwrap(), b));
            }
        }
        if !released && r.d.reads_in_flight() == 1 && bodies.is_empty() {
            if let Some((_, b)) = &held {
                bodies.push(b.clone());
                released = true;
            }
        }
        bodies.reverse();
        r.t.inject_incoming(&answer_all(&bodies, &ps, B));
        r.step(&mut prod).unwrap();
    }
    let (straggler, _) = held.expect("a read was held");
    assert!(released, "the straggler was answered last");
    assert!(
        max_id > straggler + PENDING_CAP as u64,
        "later ids wrapped onto the straggler's slot ({straggler} → {max_id})"
    );
    let sig = drain(&mut cons);
    let ticks = sig
        .iter()
        .filter(|x| matches!(x.1, PoolEvent::Tick { .. }))
        .count();
    assert_eq!(ticks, 300, "every initialised tick emitted");
    assert_eq!(r.d.snapshot_counters().pools_ok, 1);
}

#[test]
fn alloc_id_skips_a_slot_a_straggler_still_holds() {
    let ps = pools();
    let mut d = driver(&ps);
    record_pending(&mut d, 5, RpcKind::EthCall).unwrap();
    d.next_id = 5 + PENDING_CAP as u64;
    assert_eq!(alloc_id(&mut d), 6 + PENDING_CAP as u64, "261 folds onto 5");
    assert_eq!(alloc_id(&mut d), 7 + PENDING_CAP as u64);
    // Id 0 is reserved: the counter never hands it out.
    d.next_id = u64::MAX;
    assert_eq!(alloc_id(&mut d), u64::MAX);
    assert_eq!(d.next_id, 1);
}

#[test]
fn a_snapshot_stalls_only_with_reads_out_and_no_answer() {
    assert!(!snapshot_stalled(0, 0, SNAP_STALL_NS * 10));
    assert!(!snapshot_stalled(3, 1_000, 1_000 + SNAP_STALL_NS));
    assert!(snapshot_stalled(3, 1_000, 1_001 + SNAP_STALL_NS));
    assert!(
        !snapshot_stalled(3, 5_000, 1_000),
        "a clock behind progress is not a stall"
    );
}
