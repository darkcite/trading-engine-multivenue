// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! **DNS re-resolve, off the gateway thread (BX7, carried).**
//!
//! Binance's hosts are CDN-fronted and rotate their addresses. A
//! connection resolves its host once, when it is built; a socket that
//! keeps failing to dial may be dialling an address the host no longer
//! serves. After [`RERESOLVE_AFTER`] failed establishments in a row on a
//! host — and again after every [`RERESOLVE_AFTER`] more — the gateway asks
//! this module's cold thread to resolve the host again, and hands the
//! answer to every connection on that host (`set_addr`: the next dial goes
//! there; a live socket is left alone).
//!
//! `getaddrinfo` blocks — for seconds when a resolver times out — so it
//! never runs on the gateway thread. (The HL gateway resolves on its own
//! thread, but only while no socket of it carries posts; here another lane
//! may be trading.) The thread holds the host names — the connections'
//! own, interned for the process's life: the table (references only) moves
//! to it by value and no name is copied; the gateway sends a host's index
//! on one SPSC ring and reads `(index, address)` back on another — no
//! lock, and nothing allocated on the gateway's side after the boot. A
//! host is asked at most once at a time; an answer that is not an address
//! leaves the next streak to ask again. [`Resolver::spawn`] returns once
//! the thread serves: its start-up — std's thread bookkeeping, which
//! allocates on some platforms — is the boot's, never the run's.

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use core_ring::{Consumer, Producer, Ring};

/// Failed establishments in a row before a host is resolved again (as the
/// HL gateway's).
pub const RERESOLVE_AFTER: u32 = 3;
/// The distinct hosts one gateway dials: per lane its WS API, its stream
/// host and its REST host, and SAPI's — a full mainnet arm (USDⓈ-M,
/// COIN-M, spot and options, BX9) dials nine: `ws-fapi`, `fstream`,
/// `fapi`, `ws-dapi`, `dstream`, `dapi`, `ws-api`, `api` (spot's REST and
/// SAPI's) and `eapi` (options' WS pair names it too; their stream is
/// `fstream`'s). One spare.
pub const DNS_HOSTS: usize = 10;
/// The rings' room: every host asked at once, past twice over.
const DNS_RING: usize = 32;
// `Resolver::pending` holds a bit per host.
const _: () = assert!(DNS_HOSTS <= 16);
/// How long the cold thread sleeps when nothing is asked.
const IDLE: Duration = Duration::from_millis(20);
/// How long [`Resolver::spawn`] waits for the thread to serve.
const START_WITHIN: Duration = Duration::from_secs(5);
/// The thread's state: spawned, not serving yet.
const STARTING: u8 = 0;
/// Serving.
const SERVING: u8 = 1;
/// Asked to leave (the resolver dropped, or its start given up on).
const STOPPING: u8 = 2;

/// One answer: the host's index and its first address, `None` when the
/// name did not resolve.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    /// The host's index ([`Resolver::host`]).
    pub host: u8,
    /// The address, or `None`.
    pub addr: Option<SocketAddr>,
}

/// Why the resolver could not start.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DnsErr {
    /// More than [`DNS_HOSTS`] hosts.
    TooManyHosts,
    /// The thread could not be spawned, or did not serve within
    /// [`START_WITHIN`].
    Spawn,
}

/// The gateway's end of the resolver.
pub struct Resolver {
    ask: Producer<u8, DNS_RING>,
    answers: Consumer<Answer, DNS_RING>,
    /// The thread's state (`STARTING`, `SERVING`, `STOPPING`).
    state: Arc<AtomicU8>,
    hosts: usize,
    /// Bit `h`: host `h` is asked and not answered yet.
    pending: u16,
    /// Host `h`'s streak when it was last asked (0: none since its last
    /// established socket).
    asked_at: [u32; DNS_HOSTS],
    /// Answers that were addresses.
    resolved: u64,
}

impl Resolver {
    /// Start the cold thread `bn-dns` over the first `n` of `names` (each
    /// once, at most [`DNS_HOSTS`]; the connections' interned names), every
    /// host on `port` — and return once it serves, so its start-up is the
    /// boot's. At boot: the thread (and its name) is the only allocation.
    ///
    /// # Errors
    /// Too many hosts, or the thread could not be spawned or did not serve
    /// within [`START_WITHIN`].
    pub fn spawn(names: [&'static str; DNS_HOSTS], n: usize, port: u16) -> Result<Self, DnsErr> {
        if n > DNS_HOSTS {
            return Err(DnsErr::TooManyHosts);
        }
        let (ask, asked) = Ring::<u8, DNS_RING>::new().split();
        let (answer, answers) = Ring::<Answer, DNS_RING>::new().split();
        let state = Arc::new(AtomicU8::new(STARTING));
        let theirs = Arc::clone(&state);
        // COPY: the host table (≤ 160 B of `&'static str`, no name's bytes)
        // moved into the thread's closure — boot only, once — rejected: a
        // table shared with the gateway (an `Arc` and a lock, or a lifetime
        // the thread outlives, to save 160 B once).
        thread::Builder::new()
            .name(String::from("bn-dns"))
            .spawn(move || serve(&names[..n], port, asked, answer, &theirs))
            .map_err(|_| DnsErr::Spawn)?;
        let t0 = Instant::now();
        while state.load(Ordering::Acquire) == STARTING {
            // Never serving: it leaves at once if it ever starts.
            if t0.elapsed() > START_WITHIN && state.compare_exchange(STARTING, STOPPING, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                return Err(DnsErr::Spawn);
            }
            thread::sleep(Duration::from_micros(50));
        }
        Ok(Self {
            ask,
            answers,
            state,
            hosts: n,
            pending: 0,
            asked_at: [0; DNS_HOSTS],
            resolved: 0,
        })
    }

    /// Ask again for every host whose worst streak (`streak[h]`, its
    /// connections' failed establishments in a row) has reached
    /// [`RERESOLVE_AFTER`] more than when it was last asked; a host back
    /// at 0 starts over. A full ring asks at the next call.
    #[inline]
    pub fn ask_where_due(&mut self, streak: &[u32; DNS_HOSTS]) {
        let mut h = 0;
        while h < self.hosts {
            let s = streak[h];
            if s == 0 {
                self.asked_at[h] = 0;
            } else if s >= self.asked_at[h].saturating_add(RERESOLVE_AFTER) && self.pending & (1 << h) == 0 && self.ask.try_push_ref(&(h as u8)) {
                self.pending |= 1 << h;
                self.asked_at[h] = s;
            }
            h += 1;
        }
    }

    /// The next answer, if one came: its host is no longer pending.
    #[inline]
    pub fn answer(&mut self) -> Option<Answer> {
        let a = *self.answers.try_pop_ref()?;
        self.pending &= !(1u16 << a.host);
        self.resolved += u64::from(a.addr.is_some());
        Some(a)
    }

    /// Answers that were addresses, so far.
    #[must_use]
    pub const fn resolved(&self) -> u64 {
        self.resolved
    }
}

impl Drop for Resolver {
    fn drop(&mut self) {
        // The thread leaves after its current resolution, if any.
        self.state.store(STOPPING, Ordering::Release);
    }
}

/// The cold thread: serving (unless `spawn` gave up on its start), resolve
/// what is asked, answer, sleep when idle.
fn serve(names: &[&'static str], port: u16, mut asked: Consumer<u8, DNS_RING>, mut answer: Producer<Answer, DNS_RING>, state: &AtomicU8) {
    if state.compare_exchange(STARTING, SERVING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    while state.load(Ordering::Acquire) == SERVING {
        let Some(h) = asked.try_pop_ref().map(|g| *g) else {
            thread::sleep(IDLE);
            continue;
        };
        let Some(name) = names.get(usize::from(h)) else {
            continue;
        };
        // The first address, as the connections resolve at construction.
        let addr = (*name, port).to_socket_addrs().ok().and_then(|mut it| it.next());
        // The ring holds every host twice over: an answer always fits.
        let _ = answer.try_push_ref(&Answer { host: h, addr });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn wait_answer(r: &mut Resolver) -> Answer {
        let t0 = Instant::now();
        loop {
            if let Some(a) = r.answer() {
                return a;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "no answer");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// A host is asked at RERESOLVE_AFTER failures, again only RERESOLVE_AFTER
    /// later and never while its question is open; back at 0 it starts
    /// over. The cold thread answers with the first address; it serves
    /// before `spawn` returns, and leaves once the resolver is dropped.
    #[test]
    fn a_failing_host_is_resolved_again_on_the_cold_thread() {
        let mut names = [""; DNS_HOSTS];
        names[0] = "localhost";
        names[1] = "no-such-host.invalid";
        // Serving before `spawn` returns, every time: its start-up is the
        // boot's (gate 107 measures after it).
        let mut k = 0;
        while k < 32 {
            let r = Resolver::spawn([""; DNS_HOSTS], 0, 1).expect("spawn");
            assert_eq!(r.state.load(Ordering::Acquire), SERVING, "serving before spawn returns");
            k += 1;
        }
        let mut r = Resolver::spawn(names, 2, 4_443).expect("spawn");
        let state = Arc::clone(&r.state);
        let mut s = [0u32; DNS_HOSTS];
        s[0] = RERESOLVE_AFTER - 1;
        r.ask_where_due(&s);
        assert_eq!(r.pending, 0, "not yet");
        s[0] = RERESOLVE_AFTER;
        r.ask_where_due(&s);
        r.ask_where_due(&s);
        assert_eq!(r.pending, 1, "asked once");
        let a = wait_answer(&mut r);
        assert_eq!((a.host, a.addr.map(|x| (x.ip().is_loopback(), x.port()))), (0, Some((true, 4_443))));
        assert_eq!((r.pending, r.resolved()), (0, 1));
        s[0] = 2 * RERESOLVE_AFTER - 1;
        r.ask_where_due(&s);
        assert_eq!(r.pending, 0, "the next ask is RERESOLVE_AFTER later");
        // A name that does not resolve is answered with none.
        s[1] = RERESOLVE_AFTER;
        r.ask_where_due(&s);
        let a = wait_answer(&mut r);
        assert_eq!((a.host, a.addr), (1, None));
        assert_eq!(r.resolved(), 1);
        // Back at 0, a host starts over.
        s[0] = 0;
        r.ask_where_due(&s);
        s[0] = RERESOLVE_AFTER;
        r.ask_where_due(&s);
        assert_eq!(r.pending, 1);
        let _ = wait_answer(&mut r);
        drop(r);
        assert_eq!(state.load(Ordering::Acquire), STOPPING, "dropped: the thread leaves");
        assert_eq!(Resolver::spawn([""; DNS_HOSTS], DNS_HOSTS + 1, 1).err(), Some(DnsErr::TooManyHosts));
    }
}
