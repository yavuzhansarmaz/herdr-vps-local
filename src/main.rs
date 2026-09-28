//! herdr-vps-local: local experience on remote machines.
//! Mirrors repos between your laptop and remote machines (VPS, Raspberry
//! Pi, ...) via Mutagen.
//!
//! Used as a Herdr plugin: the `ensure` startup hook recreates any missing
//! sync session after a reboot, so remote work (Herdr agents on a VPS)
//! keeps showing up locally and vice versa.
//!
//! Subcommands:
//!   ensure   create missing sync sessions (idempotent)
//!   status   show configured sessions and their state
//!
//! Config: $HERDR_PLUGIN_CONFIG_DIR/config.toml (or --config PATH).
//! See config.toml.example.

use serde::Deserialize;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const LOG: &str = "[vps-local]";

#[derive(Debug, Deserialize, Default)]
struct Config {
    mutagen: Option<String>,
    #[serde(default)]
    sessions: Vec<Session>,
}

#[derive(Debug, Deserialize)]
struct Session {
    name: String,
    alpha: String,
    beta: String,
    mode: Option<String>,
    #[serde(default)]
    ignore: Vec<String>,
}

fn log(msg: &str) {
    println!("{LOG} {msg}");
}

fn warn(msg: &str) {
    eprintln!("{LOG} WARNING: {msg}");
}

fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn mutagen_bin(cfg: &Config) -> String {
    if let Ok(v) = env::var("MUTAGEN_BIN") {
        if !v.trim().is_empty() {
            return v;
        }
    }
    cfg.mutagen.clone().unwrap_or_else(|| "mutagen".to_string())
}

/// Run mutagen, capturing stdout+stderr. Returns (success, combined output).
fn run(bin: &str, args: &[String]) -> (bool, String) {
    match Command::new(bin).args(args).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                text.push_str(&err);
            }
            (out.status.success(), text)
        }
        Err(e) => (false, e.to_string()),
    }
}

fn session_exists(bin: &str, name: &str) -> bool {
    let (ok, _) = run(bin, &["sync".into(), "list".into(), name.into()]);
    ok
}

fn ensure(cfg: &Config) -> i32 {
    let bin = mutagen_bin(cfg);
    let (ok, out) = run(&bin, &["daemon".into(), "start".into()]);
    if !ok {
        eprintln!("{LOG} ERROR: cannot start mutagen daemon: {}", out.trim());
        return 1;
    }
    if cfg.sessions.is_empty() {
        log("no sessions configured; nothing to do");
        return 0;
    }
    let mut failed = 0;
    for s in &cfg.sessions {
        if session_exists(&bin, &s.name) {
            // Best effort: resume in case it was paused.
            let (rok, rout) = run(&bin, &["sync".into(), "resume".into(), s.name.clone()]);
            if rok {
                log(&format!("session '{}' exists; resumed", s.name));
            } else if rout.to_lowercase().contains("paused") {
                log(&format!("session '{}' exists (already running)", s.name));
            } else {
                warn(&format!(
                    "session '{}' exists; resume said: {}",
                    s.name,
                    rout.trim()
                ));
            }
            continue;
        }
        let alpha = expand_tilde(&s.alpha);
        if !PathBuf::from(&alpha).exists() {
            match fs::create_dir_all(&alpha) {
                Ok(()) => log(&format!("created local dir {alpha}")),
                Err(e) => {
                    eprintln!(
                        "{LOG} ERROR: session '{}': cannot create {alpha}: {e}",
                        s.name
                    );
                    failed += 1;
                    continue;
                }
            }
        }
        let mut args = vec![
            "sync".to_string(),
            "create".to_string(),
            "--name".to_string(),
            s.name.clone(),
        ];
        if let Some(m) = &s.mode {
            args.push("--mode".to_string());
            args.push(m.clone());
        }
        for ig in &s.ignore {
            args.push("--ignore".to_string());
            args.push(ig.clone());
        }
        args.push(alpha);
        args.push(s.beta.clone());
        let (ok, out) = run(&bin, &args);
        if ok {
            log(&format!(
                "session '{}' created ({} <-> {})",
                s.name, s.alpha, s.beta
            ));
        } else {
            eprintln!("{LOG} ERROR: session '{}': {}", s.name, out.trim());
            failed += 1;
        }
    }
    if failed > 0 {
        1
    } else {
        0
    }
}

fn status(cfg: &Config) -> i32 {
    let bin = mutagen_bin(cfg);
    if cfg.sessions.is_empty() {
        log("no sessions configured");
        return 0;
    }
    for s in &cfg.sessions {
        if session_exists(&bin, &s.name) {
            let (_, out) = run(&bin, &["sync".into(), "list".into(), s.name.clone()]);
            let state = out
                .lines()
                .find_map(|l| l.trim().strip_prefix("Status:").map(|v| v.trim().to_string()))
                .unwrap_or_else(|| "exists".to_string());
            log(&format!(
                "{}: {} ({} <-> {})",
                s.name, state, s.alpha, s.beta
            ));
        } else {
            log(&format!("{}: missing ({} <-> {})", s.name, s.alpha, s.beta));
        }
    }
    0
}

fn load_config(path: Option<&str>) -> Config {
    let p = path.map(str::to_string).or_else(|| {
        env::var("HERDR_PLUGIN_CONFIG_DIR")
            .ok()
            .map(|d| format!("{d}/config.toml"))
    });
    let Some(p) = p else {
        warn("no config file (set HERDR_PLUGIN_CONFIG_DIR or --config); continuing empty");
        return Config::default();
    };
    match fs::read_to_string(&p) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => {
                log(&format!("loaded config from {p}"));
                c
            }
            Err(e) => {
                warn(&format!("ignoring invalid config {p}: {e}"));
                Config::default()
            }
        },
        Err(_) => {
            warn(&format!("config not found at {p}; continuing empty"));
            Config::default()
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: herdr-vps-local [--config PATH] [--mutagen BIN] <ensure|status>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut config_path: Option<String> = None;
    let mut mutagen_override: Option<String> = None;
    let mut sub: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                config_path = args.get(i).cloned();
            }
            "--mutagen" => {
                i += 1;
                mutagen_override = args.get(i).cloned();
            }
            s => {
                if sub.is_none() {
                    sub = Some(s.to_string());
                }
            }
        }
        i += 1;
    }
    let Some(sub) = sub else { usage() };
    let mut cfg = load_config(config_path.as_deref());
    if mutagen_override.is_some() {
        cfg.mutagen = mutagen_override;
    }
    let code = match sub.as_str() {
        "ensure" => ensure(&cfg),
        "status" => status(&cfg),
        _ => usage(),
    };
    std::process::exit(code);
}
