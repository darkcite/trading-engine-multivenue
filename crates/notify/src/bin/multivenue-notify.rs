// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! `multivenue-notify` — the one-way Telegram notifier (NOTIFY lane).
//!
//! ```text
//! multivenue-notify run       [--config PATH]   poll and send until killed (absent config = off, exit 0)
//! multivenue-notify send      [--loud] [--source NAME] [--dir DIR] [TEXT…]
//!                                               queue an AI-session message (stdin when no TEXT)
//! multivenue-notify test-send [--config PATH]   send one test line now (honours dry_run)
//! multivenue-notify check     [--config PATH]   validate config, keys (presence only), universe, /state
//! multivenue-notify replay    --config PATH --run-dir DIR
//!                                               print the fill/settlement messages a run's capture yields
//! ```
//!
//! The default config is `~/multivenue/notify.toml`; the default inbox for
//! `send` is `~/multivenue/state/notify/inbox`. Exit codes: 0 ok (or off),
//! 1 runtime failure, 2 usage, 3 config refused.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use core_io::pmlr::SlotKind;
use notify::capture::{FillRec, Tail};
use notify::config::{self, Config};
use notify::model::Model;
use notify::names::Names;
use notify::run::{self, log};
use notify::telegram::Outcome;
use notify::{clock, envfile, inbox, state};

const USAGE: &str =
    "usage: multivenue-notify run|send|test-send|check|replay [options] (see --help)";

fn help() -> &'static str {
    "multivenue-notify — the one-way Telegram notifier (NOTIFY lane)

  run       [--config PATH]   poll /state + the run captures + the inbox, send until killed;
                              an absent config means off (exit 0)
  send      [--loud] [--source NAME] [--dir DIR] [TEXT...]
                              queue a message for the notifier that owns DIR (default
                              ~/multivenue/state/notify/inbox); TEXT from stdin when omitted
  test-send [--config PATH]   send one test line now (dry_run prints it instead)
  check     [--config PATH]   validate the config, the .env keys (presence only), the universe
                              names and /state; prints no secret
  replay    --config PATH --run-dir DIR
                              print the FILL / SETTLED messages a run's engine-fills.pmlr yields

  default config: ~/multivenue/notify.toml"
}

struct Args {
    verb: String,
    config: Option<PathBuf>,
    loud: bool,
    source: Option<String>,
    dir: Option<PathBuf>,
    run_dir: Option<PathBuf>,
    text: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let verb = it.next().ok_or(USAGE)?;
    let mut a = Args {
        verb,
        config: None,
        loud: false,
        source: None,
        dir: None,
        run_dir: None,
        text: Vec::new(),
    };
    let mut rest_is_text = false;
    while let Some(x) = it.next() {
        if rest_is_text {
            a.text.push(x);
            continue;
        }
        match x.as_str() {
            "--config" => {
                a.config = Some(config::expand(&it.next().ok_or("--config needs a path")?))
            }
            "--loud" => a.loud = true,
            "--source" => a.source = Some(it.next().ok_or("--source needs a name")?),
            "--dir" => a.dir = Some(config::expand(&it.next().ok_or("--dir needs a path")?)),
            "--run-dir" => {
                a.run_dir = Some(config::expand(&it.next().ok_or("--run-dir needs a path")?))
            }
            "--" => rest_is_text = true,
            s if s.starts_with("--") => return Err(format!("unknown option {s}")),
            _ => a.text.push(x),
        }
    }
    Ok(a)
}

fn load(path: &Path) -> Result<Option<Config>, ExitCode> {
    match std::fs::read_to_string(path) {
        Ok(t) => match Config::parse(&t) {
            Ok(c) => Ok(Some(c)),
            Err(e) => {
                log(&format!("{} refused: {e}", path.display()));
                Err(ExitCode::from(3))
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            log(&format!("{} unreadable: {e}", path.display()));
            Err(ExitCode::from(3))
        }
    }
}

fn verb_send(a: &Args) -> ExitCode {
    let body = if a.text.is_empty() {
        let mut s = String::new();
        if std::io::stdin().read_to_string(&mut s).is_err() {
            eprintln!("send: stdin is not UTF-8 text");
            return ExitCode::from(2);
        }
        s
    } else {
        a.text.join(" ")
    };
    if body.trim().is_empty() {
        eprintln!("send: empty message");
        return ExitCode::from(2);
    }
    let dir = a
        .dir
        .clone()
        .unwrap_or_else(|| config::expand("~/multivenue/state/notify/inbox"));
    match inbox::write_message(&dir, a.loud, a.source.as_deref(), &body) {
        Ok(p) => {
            println!("queued {}", p.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("send: cannot write into {}: {e}", dir.display());
            ExitCode::from(1)
        }
    }
}

fn verb_test_send(cfg: &Config) -> ExitCode {
    let mut t = match run::transport_for(cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("test-send: {e}");
            return ExitCode::from(1);
        }
    };
    let host = std::env::var("HOST")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    let text = format!(
        "[{}] test message from multivenue-notify{}",
        cfg.tag,
        if host.is_empty() {
            String::new()
        } else {
            format!(" on {host}")
        }
    );
    match t.send(&text, false) {
        Outcome::Sent => {
            println!(
                "sent{}",
                if cfg.dry_run {
                    " (dry run: printed above, nothing left the host)"
                } else {
                    ""
                }
            );
            ExitCode::SUCCESS
        }
        o => {
            eprintln!("test-send failed: {o:?}");
            ExitCode::from(1)
        }
    }
}

fn verb_check(path: &Path, cfg: &Config) -> ExitCode {
    let mut ok = true;
    println!(
        "config     {} (tag {}, {} slot(s), {} watch(es))",
        path.display(),
        cfg.tag,
        cfg.slots.len(),
        cfg.watches.len()
    );
    println!(
        "mode       {}",
        if cfg.dry_run {
            "dry run (nothing is sent)"
        } else {
            "sending"
        }
    );
    match &cfg.env_file {
        Some(p) => match envfile::read(p) {
            Ok(_) => println!(
                "keys       TELEGRAM_BOT_TOKEN and TELEGRAM_CHAT_ID present in {}",
                p.display()
            ),
            Err(e) => {
                println!("keys       {e} ({})", p.display());
                ok &= cfg.dry_run;
            }
        },
        None => println!("keys       no env_file (dry run)"),
    }
    let names = Names::load(cfg.universe.as_deref());
    println!(
        "universe   {} rolling famil{}: {}",
        names.families.len(),
        if names.families.len() == 1 {
            "y"
        } else {
            "ies"
        },
        names
            .families
            .iter()
            .map(|f| f.label.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if let Some(u) = &cfg.state_url {
        match state::fetch(u, std::time::Duration::from_secs(2)) {
            Ok(s) => {
                let live: Vec<String> = cfg
                    .slots
                    .iter()
                    .filter(|c| s.slot_live(c.slot))
                    .map(|c| format!("s{}", c.slot))
                    .collect();
                println!(
                    "state      {}:{} answers: {} pid {} run {} · live: {}",
                    u.host,
                    u.port,
                    s.git_sha,
                    s.pid,
                    s.run_dir,
                    if live.is_empty() {
                        "none".into()
                    } else {
                        live.join(",")
                    }
                );
            }
            Err(e) => {
                println!("state      {e}");
                ok = false;
            }
        }
    }
    for d in &cfg.inbox {
        println!(
            "inbox      {} ({})",
            d.display(),
            if d.is_dir() {
                "exists"
            } else {
                "created on the first message"
            }
        );
    }
    println!("state_dir  {}", cfg.state_dir.display());
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn verb_replay(cfg: Config, run_dir: &Path) -> ExitCode {
    let names = Names::load(cfg.universe.as_deref());
    let now_s = (clock::now_wall_ns() / 1_000_000_000) as i64;
    let mut m = Model::new(
        cfg,
        names,
        Default::default(),
        Vec::new(),
        Default::default(),
        now_s,
    );
    m.assume_live();
    let mut tail = Tail::new(run_dir.join(notify::FILLS_FILE), SlotKind::Fill, 0);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        buf.clear();
        match tail.poll(4096, &mut buf) {
            Ok(0) => break,
            Ok(_) => {
                for s in &buf {
                    m.on_fill(&FillRec::decode(s), now_s, false, &mut out);
                }
            }
            Err(e) => {
                eprintln!("replay: {e}");
                return ExitCode::from(1);
            }
        }
    }
    for msg in out {
        println!(
            "{} {}",
            if msg.loud { "LOUD  " } else { "silent" },
            msg.text.replace('\n', "\n       ")
        );
    }
    println!(
        "{} open position(s) at the end of the capture",
        m.positions.len()
    );
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    let a = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if a.verb == "--help" || a.verb == "-h" || a.verb == "help" {
        println!("{}", help());
        return ExitCode::SUCCESS;
    }
    if a.verb == "send" {
        return verb_send(&a);
    }
    let path = a
        .config
        .clone()
        .unwrap_or_else(|| config::expand("~/multivenue/notify.toml"));
    let cfg = match load(&path) {
        Ok(Some(c)) => c,
        Ok(None) => {
            log(&format!(
                "{} absent — notifications are off",
                path.display()
            ));
            return if a.verb == "run" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(3)
            };
        }
        Err(code) => return code,
    };
    match a.verb.as_str() {
        "run" => match run::run(cfg) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                log(&format!("stopped: {e}"));
                ExitCode::from(1)
            }
        },
        "test-send" => verb_test_send(&cfg),
        "check" => verb_check(&path, &cfg),
        "replay" => match &a.run_dir {
            Some(d) => verb_replay(cfg, d),
            None => {
                eprintln!("replay needs --run-dir DIR");
                ExitCode::from(2)
            }
        },
        other => {
            eprintln!("unknown verb '{other}'\n{USAGE}");
            ExitCode::from(2)
        }
    }
}
