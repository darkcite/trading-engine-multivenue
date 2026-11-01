// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)
//! `multivenue-terminal [--port P] [--bundle DIR] (--synthetic SEED | --replay SPOOL)
//!  [--record SPOOL] [--no-door]` — see `terminal::server`.

use std::path::PathBuf;
use std::time::Duration;

use terminal::bundle::Bundle;
use terminal::server::{Config, Server};

fn usage() -> ! {
    eprintln!("usage: multivenue-terminal [--port 9393] [--bundle terminal-ui/dist] (--synthetic SEED | --replay SPOOL) [--record SPOOL] [--no-door]");
    std::process::exit(2)
}

fn main() -> std::io::Result<()> {
    let mut port = 9393u16;
    let mut bundle: Option<PathBuf> = None;
    let mut seed: Option<u64> = None;
    let mut replay: Option<PathBuf> = None;
    let mut record: Option<PathBuf> = None;
    let mut no_door = false;
    let mut a = std::env::args().skip(1);
    while let Some(k) = a.next() {
        match k.as_str() {
            "--port" => port = a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--bundle" => bundle = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--synthetic" => seed = Some(a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())),
            "--replay" => replay = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--record" => record = Some(a.next().unwrap_or_else(|| usage()).into()),
            "--no-door" => no_door = true,
            _ => usage(),
        }
    }
    if seed.is_some() == replay.is_some() {
        eprintln!("exactly one of --synthetic and --replay");
        usage();
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
