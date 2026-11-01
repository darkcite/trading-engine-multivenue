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
    eprintln!("usage: multivenue-terminal [--port 9393] [--bundle terminal-ui/dist] (--synthetic SEED | --replay SPOOL | --live [--hl BTC] [--bn BTCUSDT]) [--record SPOOL] [--no-door] [--probe SECS] [--extra-ca PEM]");
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
    let cfg = Config { port, token, bundle, record, tick: Duration::from_millis(100), order_entry };
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
