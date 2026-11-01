// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `multivenue-terminal [--port P] [--bundle DIR]
//!  (--synthetic SEED | --replay SPOOL | --live [--hl COIN]… [--bn SYMBOL]…)
//!  [--record SPOOL] [--no-door] [--probe SECS]` — see `terminal::server`.
//! `--probe SECS` runs the source alone (no server) and prints what it made.

use std::path::PathBuf;
use std::time::Duration;

use terminal::bundle::Bundle;
use terminal::server::{Config, Server};

fn usage() -> ! {
    eprintln!("usage: multivenue-terminal [--port 9393] [--bundle terminal-ui/dist] (--synthetic SEED | --replay SPOOL | --live [--hl BTC] [--bn BTCUSDT]) [--record SPOOL] [--no-door] [--probe SECS] [--extra-ca PEM] [--data DIR] [--state URL] [--door SOCK --door-key ENV (--door-epoch FILE | --state URL) --route I:SLOT:SYM…]");
    std::process::exit(2)
}

fn main() -> std::io::Result<()> {
    let mut port = 9393u16;
    let mut bundle: Option<PathBuf> = None;
    let mut seed: Option<u64> = None;
    let mut replay: Option<PathBuf> = None;
    let mut record: Option<PathBuf> = None;
    let mut no_door = false;
    let mut live = false;
    let mut hl: Vec<String> = Vec::new();
    let mut bn: Vec<String> = Vec::new();
    let mut probe: Option<u64> = None;
    let mut extra_ca: Vec<PathBuf> = Vec::new();
    let mut door_sock: Option<PathBuf> = None;
    let mut door_key: Option<PathBuf> = None;
    let mut door_epoch: Option<PathBuf> = None;
    let mut routes: Vec<terminal::door::Route> = Vec::new();
    let mut state_url: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        match k.as_str() {
            "--port" => port = a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--bundle" => bundle = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--synthetic" => seed = Some(a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--replay" => replay = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--record" => record = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--no-door" => no_door = true,
            "--live" => live = true,
            "--hl" => hl.push(a.next().unwrap_or_else(|| usage())),
            "--bn" => bn.push(a.next().unwrap_or_else(|| usage())),
            "--data" => data_dir = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--state" => state_url = Some(a.next().unwrap_or_else(|| usage())),
            "--door" => door_sock = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--door-key" => door_key = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--door-epoch" => door_epoch = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--route" => routes.push(a.next().and_then(|v| terminal::door::Route::parse(&v)).unwrap_or_else(|| usage())),
            "--extra-ca" => extra_ca.push(a.next().unwrap_or_else(|| usage()).into()),
            "--probe" => probe = Some(a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            _ => usage(),
        }
    }
    if [seed.is_some(), replay.is_some(), live].iter().filter(|x| **x).count() != 1 {
        eprintln!("exactly one of --synthetic, --replay and --live");
        usage();
    }
    if live && hl.is_empty() && bn.is_empty() {
        hl.push("BTC".into());
        bn.push("BTCUSDT".into());
    }
    if let Some(secs) = probe {
        if live {
            let mut src = terminal::live::Live::new(&terminal::live::LiveCfg { hl, bn, extra_ca })?;
            terminal::probe::run(&mut src, secs);
            eprintln!("live stats: {:?}", src.stats);
        } else if let Some(s) = seed {
            terminal::probe::run(&mut terminal::synth::Synthetic::new(s), secs);
        }
        return Ok(());
    }
    let bundle = match bundle {
        Some(d) => Bundle::load(&d).unwrap_or_else(|e| {
            eprintln!("bundle refused: {e}");
            std::process::exit(1)
        }),
        None => Bundle::none(),
    };
    let token = terminal::auth::random_secret()?;
    // Replay never takes orders; until the door lands (M5) nothing does.
    let order_entry = !no_door && replay.is_none();
    // The door: --door SOCK --door-key term.env --door-epoch term.epoch
    // --route instr:slot:sym (repeatable). T-L1: the terminal still holds
    // no venue key; the door key only authenticates it to the engine.
    let door = match (door_sock, order_entry) {
        (Some(sock), true) => {
            let key = terminal::door::read_key(&door_key.unwrap_or_else(|| usage())).unwrap_or_else(|e| {
                eprintln!("door refused: {e}");
                std::process::exit(1)
            });
            // The epoch: /state's boot.boot_wall_ns when --state is given
            // (T-L11), else the file the engine wrote beside the socket.
            let from_state = state_url.as_deref().and_then(|u| notify::config::parse_state_url(u).ok()).and_then(|u| notify::state::fetch(&u, Duration::from_secs(2)).ok()).map(|v| v.boot_wall_ns);
            let from_file = door_epoch.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| s.trim().parse().ok());
            let epoch: u64 = from_state.or(from_file).unwrap_or_else(|| {
                eprintln!("door refused: no epoch (pass --state URL or --door-epoch FILE)");
                std::process::exit(1)
            });
            Some(terminal::door::DoorClient::connect(&sock, key, epoch, routes)?)
        }
        _ => None,
    };
    let state = match state_url.as_deref().map(notify::config::parse_state_url) {
        Some(Ok(u)) => Some(terminal::state::StatePoller::spawn(u)),
        Some(Err(e)) => {
            eprintln!("--state refused: {e:?}");
            std::process::exit(2)
        }
        None => None,
    };
    let cfg = Config { port, token, bundle, record, tick: Duration::from_millis(100), order_entry, door, state, data_dir };
    let mut srv = Server::bind(cfg)?;
    // The one-time login token goes to the operator's terminal only.
    println!("TERM http://127.0.0.1:{}/  login token: {}", srv.port(), String::from_utf8_lossy(&token));
    if live {
        let mut src = terminal::live::Live::new(&terminal::live::LiveCfg { hl, bn, extra_ca })?;
        return srv.run(&mut src, |_| false);
    }
    match (seed, replay) {
        (Some(s), _) => srv.run(&mut terminal::synth::Synthetic::new(s), |_| false),
        (_, Some(p)) => {
            let mut r = terminal::replay::Replay::open(&p).unwrap_or_else(|e| {
                eprintln!("replay refused: {e:?}");
                std::process::exit(1)
            });
            srv.run(&mut r, |_| false)
        }
        _ => unreachable!(),
    }
}
