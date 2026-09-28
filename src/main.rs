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
//!   add      append a session to the config and create it immediately
//!
//! Config: $HERDR_PLUGIN_CONFIG_DIR/config.toml (or --config PATH).
//! See config.toml.example.

use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const LOG: &str = "[vps-local]";

#[derive(Debug, Deserialize, Serialize, Default)]
struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    mutagen: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sessions: Vec<Session>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Session {
    name: String,
    alpha: String,
    beta: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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

/// Preflight check: can the Mutagen binary be executed at all? Prints one
/// friendly line and returns false when it cannot (missing from PATH or a
/// bad --mutagen override), so callers fail fast instead of surfacing a
/// raw OS spawn error.
fn check_mutagen(bin: &str) -> bool {
    if Command::new(bin).arg("--version").output().is_ok() {
        return true;
    }
    eprintln!("{LOG} ERROR: Mutagen is required but '{bin}' could not be executed; install Mutagen from https://mutagen.io or via your package manager, make sure it is on PATH (or pass --mutagen PATH), and re-run.");
    false
}

/// Last few non-empty lines of command output, joined onto one line so the
/// detail stays readable. Falls back to a placeholder when empty.
fn stderr_tail(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return "(no detail)".to_string();
    }
    let start = lines.len().saturating_sub(3);
    lines[start..].join(" | ")
}

/// True when Mutagen's error text suggests the remote is unreachable or
/// SSH authentication failed (case-insensitive keyword match).
fn looks_like_ssh_failure(output: &str) -> bool {
    let lower = output.to_lowercase();
    [
        "ssh",
        "permission denied",
        "host key",
        "connection refused",
        "timed out",
        "no such host",
        "resolve",
    ]
    .iter()
    .any(|k| lower.contains(k))
}

/// Remote target of a Mutagen beta URL: the part before the first ':'.
fn beta_target(beta: &str) -> &str {
    beta.split_once(':').map(|(t, _)| t).unwrap_or(beta)
}

/// Machine-readable template for `mutagen sync list --template`.
///
/// Mutagen renders the template over a list of public-model sessions (its
/// `pkg/api/models/synchronization/session.go`), one `|`-separated record
/// per line: `name|status|paused|conflicts|excluded`. `.SessionState` is an
/// embedded pointer that is nil while the session is paused, so the conflict
/// counts are guarded by `{{if .SessionState}}`; without the guard the
/// template fails on paused sessions. `excluded` counts conflicts omitted
/// from the reported list by API truncation.
const STATUS_TEMPLATE: &str = "{{range .}}{{.Name}}|{{.Status}}|{{.Paused}}|{{if .SessionState}}{{len .SessionState.Conflicts}}|{{.SessionState.ExcludedConflicts}}{{else}}0|0{{end}}\n{{end}}";

/// Per-session state parsed from STATUS_TEMPLATE output.
struct SessionState {
    /// Raw Mutagen status enum (e.g. "Watching"), see describe_status.
    status: String,
    paused: bool,
    /// Reported plus truncated (excluded) conflicts.
    conflicts: u64,
}

/// Query one session via the machine-readable template. Returns None when
/// the session does not exist or the output cannot be parsed (e.g. a Mutagen
/// build without `--template` support).
fn query_session_state(bin: &str, name: &str) -> Option<SessionState> {
    let (ok, out) = run(
        bin,
        &[
            "sync".to_string(),
            "list".to_string(),
            "--template".to_string(),
            STATUS_TEMPLATE.to_string(),
            name.to_string(),
        ],
    );
    if !ok {
        return None;
    }
    let mut fallback: Option<SessionState> = None;
    for line in out.lines() {
        let f: Vec<&str> = line.split('|').collect();
        if f.len() != 5 {
            continue;
        }
        let conflicts = f[3].parse::<u64>().unwrap_or(0)
            + f[4].trim().parse::<u64>().unwrap_or(0);
        let st = SessionState {
            status: f[1].to_string(),
            paused: f[2] == "true",
            conflicts,
        };
        if f[0] == name {
            return Some(st);
        }
        if fallback.is_none() {
            fallback = Some(st);
        }
    }
    fallback
}

/// Mirror of Mutagen's Status.Description() in
/// pkg/synchronization/state.go, plus its "[Paused]" rendering for paused
/// sessions. Unknown enum values pass through verbatim so new Mutagen
/// releases keep working.
fn describe_status(raw: &str, paused: bool) -> String {
    if paused {
        return "[Paused]".to_string();
    }
    match raw {
        "Disconnected" => "Disconnected",
        "HaltedOnRootEmptied" => "Halted due to one-sided root emptying",
        "HaltedOnRootDeletion" => "Halted due to root deletion",
        "HaltedOnRootTypeChange" => "Halted due to root type change",
        "ConnectingAlpha" => "Connecting to alpha",
        "ConnectingBeta" => "Connecting to beta",
        "Watching" => "Watching for changes",
        "Scanning" => "Scanning files",
        "WaitingForRescan" => "Waiting 5 seconds for rescan",
        "Reconciling" => "Reconciling changes",
        "StagingAlpha" => "Staging files on alpha",
        "StagingBeta" => "Staging files on beta",
        "Transitioning" => "Applying changes",
        "Saving" => "Saving archive",
        other => other,
    }
    .to_string()
}

/// Legacy fallback: extract the `Status:` line from human-readable
/// `mutagen sync list` output. Used only when the template query fails but
/// the session exists (old Mutagen without `--template`). Falls back to
/// "exists" when no status line is found.
fn parse_human_status_line(output: &str) -> String {
    output
        .lines()
        .find_map(|l| l.trim().strip_prefix("Status:").map(|v| v.trim().to_string()))
        .unwrap_or_else(|| "exists".to_string())
}

fn ensure(cfg: &Config) -> i32 {
    let bin = mutagen_bin(cfg);
    if !check_mutagen(&bin) {
        return 1;
    }
    let (ok, out) = run(&bin, &["daemon".into(), "start".into()]);
    if !ok {
        eprintln!(
            "{LOG} ERROR: could not start the Mutagen daemon: {}",
            stderr_tail(&out)
        );
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
            if looks_like_ssh_failure(&out) {
                let target = beta_target(&s.beta);
                eprintln!("{LOG} HINT: session '{}': cannot reach the remote; check that `ssh {target}` works non-interactively (key auth, accepted host keys), then re-run ensure", s.name);
            }
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
    if !check_mutagen(&bin) {
        return 1;
    }
    if cfg.sessions.is_empty() {
        log("no sessions configured");
        return 0;
    }
    for s in &cfg.sessions {
        match query_session_state(&bin, &s.name) {
            Some(st) => {
                let state = describe_status(&st.status, st.paused);
                log(&format!(
                    "{}: {}, conflicts: {} ({} <-> {})",
                    s.name, state, st.conflicts, s.alpha, s.beta
                ));
            }
            None if session_exists(&bin, &s.name) => {
                // Template unsupported by this Mutagen; legacy line format.
                let (_, out) = run(&bin, &["sync".into(), "list".into(), s.name.clone()]);
                let state = parse_human_status_line(&out);
                log(&format!(
                    "{}: {} ({} <-> {})",
                    s.name, state, s.alpha, s.beta
                ));
            }
            None => {
                log(&format!("{}: missing ({} <-> {})", s.name, s.alpha, s.beta));
            }
        }
    }
    0
}

/// Resolve the config file path from --config or $HERDR_PLUGIN_CONFIG_DIR.
fn resolve_config_path(path: Option<&str>) -> Option<String> {
    path.map(str::to_string).or_else(|| {
        env::var("HERDR_PLUGIN_CONFIG_DIR")
            .ok()
            .map(|d| format!("{d}/config.toml"))
    })
}

fn load_config(path: Option<&str>) -> Config {
    let Some(p) = resolve_config_path(path) else {
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

struct AddArgs {
    name: String,
    alpha: String,
    beta: String,
    mode: Option<String>,
    ignore: Vec<String>,
}

fn parse_add_args(tokens: &[String]) -> Result<AddArgs, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut mode: Option<String> = None;
    let mut ignore: Vec<String> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = tokens[i].as_str();
        if t == "--mode" {
            i += 1;
            match tokens.get(i) {
                Some(v) => mode = Some(v.clone()),
                None => return Err("add: --mode needs a value".to_string()),
            }
        } else if let Some(v) = t.strip_prefix("--mode=") {
            if v.is_empty() {
                return Err("add: --mode needs a value".to_string());
            }
            mode = Some(v.to_string());
        } else if t == "--ignore" {
            i += 1;
            match tokens.get(i) {
                Some(v) => ignore.push(v.clone()),
                None => return Err("add: --ignore needs a value".to_string()),
            }
        } else if let Some(v) = t.strip_prefix("--ignore=") {
            if v.is_empty() {
                return Err("add: --ignore needs a value".to_string());
            }
            ignore.push(v.to_string());
        } else if t.starts_with("--") {
            return Err(format!("add: unknown option '{t}'"));
        } else {
            positional.push(tokens[i].clone());
        }
        i += 1;
    }
    if positional.len() != 3 {
        return Err("add: expected <name> <alpha> <beta>".to_string());
    }
    let name = positional[0].clone();
    let alpha = positional[1].clone();
    let beta = positional[2].clone();
    if name.trim().is_empty() {
        return Err("add: session name must not be empty".to_string());
    }
    if alpha.trim().is_empty() {
        return Err("add: alpha (local path) must not be empty".to_string());
    }
    if beta.trim().is_empty() {
        return Err("add: beta (remote endpoint) must not be empty".to_string());
    }
    Ok(AddArgs {
        name,
        alpha,
        beta,
        mode,
        ignore,
    })
}

/// Load the config for `add`: a missing file starts empty, but an existing
/// file that fails to parse is an error (never overwrite it blindly).
fn load_config_strict(path: &str) -> Result<Config, String> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str::<Config>(&text)
            .map_err(|e| format!("invalid config {path}: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log(&format!("config not found at {path}; creating it"));
            Ok(Config::default())
        }
        Err(e) => Err(format!("cannot read config {path}: {e}")),
    }
}

/// Write the config back, creating parent directories when needed.
fn save_config(path: &str, cfg: &Config) -> Result<(), String> {
    if let Some(parent) = PathBuf::from(path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create config dir {}: {e}", parent.display()))?;
        }
    }
    let text = toml::to_string(cfg).map_err(|e| format!("cannot encode config: {e}"))?;
    fs::write(path, text).map_err(|e| format!("cannot write config {path}: {e}"))?;
    Ok(())
}

fn add(
    config_path: Option<&str>,
    mutagen_override: Option<&str>,
    a: &AddArgs,
) -> i32 {
    let Some(p) = resolve_config_path(config_path) else {
        eprintln!("{LOG} ERROR: add: no config file (set HERDR_PLUGIN_CONFIG_DIR or --config)");
        return 1;
    };
    let mut cfg = match load_config_strict(&p) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{LOG} ERROR: add: {e}");
            return 1;
        }
    };
    // Preflight with the same binary resolution ensure() will use, before
    // touching the config file.
    let probe_cfg = Config {
        mutagen: mutagen_override
            .map(str::to_string)
            .or_else(|| cfg.mutagen.clone()),
        sessions: Vec::new(),
    };
    if !check_mutagen(&mutagen_bin(&probe_cfg)) {
        return 1;
    }
    if cfg.sessions.iter().any(|s| s.name == a.name) {
        eprintln!("{LOG} ERROR: add: session '{}' already exists in {p}", a.name);
        return 1;
    }
    cfg.sessions.push(Session {
        name: a.name.clone(),
        alpha: a.alpha.clone(),
        beta: a.beta.clone(),
        mode: a.mode.clone(),
        ignore: a.ignore.clone(),
    });
    if let Err(e) = save_config(&p, &cfg) {
        eprintln!("{LOG} ERROR: add: {e}");
        return 1;
    }
    log(&format!("added session '{}' to {p}", a.name));
    // Apply the CLI override only in memory so a transient flag is not
    // persisted to the config file.
    if let Some(m) = mutagen_override {
        cfg.mutagen = Some(m.to_string());
    }
    ensure(&cfg)
}

fn usage() -> ! {
    eprintln!("usage: herdr-vps-local [--config PATH] [--mutagen BIN] <ensure|status>");
    eprintln!(
        "       herdr-vps-local [--config PATH] [--mutagen BIN] add <name> <alpha> <beta> [--mode MODE] [--ignore PATTERN]..."
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let mut config_path: Option<String> = None;
    let mut mutagen_override: Option<String> = None;
    let mut rest: Vec<String> = Vec::new();
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
                rest.push(s.to_string());
            }
        }
        i += 1;
    }
    let Some(sub) = rest.first().cloned() else {
        usage()
    };
    let code = match sub.as_str() {
        "ensure" | "status" => {
            let mut cfg = load_config(config_path.as_deref());
            if mutagen_override.is_some() {
                cfg.mutagen = mutagen_override;
            }
            if sub == "ensure" {
                ensure(&cfg)
            } else {
                status(&cfg)
            }
        }
        "add" => {
            let parsed = match parse_add_args(&rest[1..]) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("{LOG} ERROR: {e}");
                    usage();
                }
            };
            add(
                config_path.as_deref(),
                mutagen_override.as_deref(),
                &parsed,
            )
        }
        _ => usage(),
    };
    std::process::exit(code);
}
