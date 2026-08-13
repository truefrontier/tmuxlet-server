mod auth;
mod backend;
mod config;
mod env;
mod exec;
mod http;
mod http_client;
mod log;
mod proto;
mod pty;
mod route;
mod stats;

use backend::Backend;
use config::Config;
use env::Env;
use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tiny_http::Server;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_CONFIG: &str = "~/.tmuxlet/server.toml";
const DEFAULT_LISTEN: &str = "127.0.0.1:3456";

struct Args {
    config: String,
    validate: bool,
    check_backends: bool,
    allow_remote: bool,
    /// §10: print the resolved policy and leg order for a model, then exit.
    explain: Option<String>,
    /// §12: score a JSONL corpus offline and sweep confidence_threshold.
    replay: Option<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        config: DEFAULT_CONFIG.to_string(),
        validate: false,
        check_backends: false,
        allow_remote: false,
        explain: None,
        replay: None,
    };
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--version" | "-V" => {
                println!("tmuxlet-server {VERSION}");
                return ExitCode::SUCCESS;
            }
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            // §10: --dry-run is an alias, purely for muscle-memory parity with
            // the neighbouring tmuxlet CLI.
            "--validate" | "--dry-run" => args.validate = true,
            "--check-backends" => args.check_backends = true,
            "--allow-remote-unauthenticated" => args.allow_remote = true,
            "--config" => {
                i += 1;
                match argv.get(i) {
                    Some(p) => args.config = p.clone(),
                    None => {
                        eprintln!("tmuxlet-server: --config requires a path");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--explain" => {
                i += 1;
                match argv.get(i) {
                    Some(m) => args.explain = Some(m.clone()),
                    None => {
                        eprintln!("tmuxlet-server: --explain requires a model id");
                        return ExitCode::FAILURE;
                    }
                }
            }
            "--replay" => {
                i += 1;
                match argv.get(i) {
                    Some(f) => args.replay = Some(f.clone()),
                    None => {
                        eprintln!("tmuxlet-server: --replay requires a .jsonl path");
                        return ExitCode::FAILURE;
                    }
                }
            }
            other => {
                if let Some(p) = other.strip_prefix("--config=") {
                    args.config = p.to_string();
                } else if let Some(m) = other.strip_prefix("--explain=") {
                    args.explain = Some(m.to_string());
                } else if let Some(f) = other.strip_prefix("--replay=") {
                    args.replay = Some(f.to_string());
                } else {
                    eprintln!("tmuxlet-server: unknown argument: {other}");
                    return ExitCode::FAILURE;
                }
            }
        }
        i += 1;
    }

    if let Some(file) = &args.replay {
        return run_replay(file);
    }
    if let Some(model) = &args.explain {
        return run_explain(&args.config, model);
    }
    if args.validate || args.check_backends {
        return run_validate(&args);
    }

    run_serve(&args.config, args.allow_remote)
}

/// Capture the backend environment per config (S-9 timeout-bounded).
fn capture_env(cfg: &Config) -> Env {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    Env::capture(
        &cfg.server.env_source,
        &shell,
        cfg.server.env_capture_timeout_secs,
    )
}

/// `--validate` (strict: lint warnings are errors) and/or `--check-backends`
/// (probe cli/tmuxlet/api reachability). Both may be requested together.
fn run_validate(args: &Args) -> ExitCode {
    let path = config::tilde_path(&args.config);
    let (cfg, warnings) = match config::load(&path) {
        Ok(cw) => cw,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut failed = false;
    if args.validate {
        if warnings.is_empty() {
            println!(
                "OK ({} backends, {} chains)",
                cfg.backends.len(),
                cfg.chains.len()
            );
        } else {
            for w in &warnings {
                eprintln!("tmuxlet-server: {w}");
            }
            failed = true;
        }
    }
    if args.check_backends {
        let env = capture_env(&cfg);
        if !check_backends(&cfg, &env) {
            failed = true;
        }
    }
    // S-2: strict validate flags an unauthenticated non-loopback bind.
    if args.validate
        && let Err(e) = bind_policy(&cfg.server.listen, cfg.server.auth, args.allow_remote)
    {
        eprintln!("tmuxlet-server: {e}");
        failed = true;
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
/// U-5 `--check-backends`: per backend, print PASS/FAIL and a detail line.
/// Returns true iff every backend passed.
fn check_backends(cfg: &Config, env: &Env) -> bool {
    let mut all_ok = true;
    let mut names: Vec<&String> = cfg.backends.keys().collect();
    names.sort();
    for name in names {
        let b = &cfg.backends[name];
        let (ok, detail) = match b {
            config::Backend::Cli { bin, .. } => {
                let path = std::path::PathBuf::from(bin);
                if backend::is_executable_file(&path) {
                    (true, path.display().to_string())
                } else {
                    (
                        false,
                        format!("{} not found or not executable", path.display()),
                    )
                }
            }
            config::Backend::Tmuxlet { .. } => {
                let resolved = backend::resolve_program("tmuxlet", env);
                match Command::new(&resolved)
                    .arg("--version")
                    .env_clear()
                    .envs(env.as_pairs())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                {
                    Ok(s) if s.success() => (true, resolved.display().to_string()),
                    Ok(s) => (
                        false,
                        format!("{} exited {}", resolved.display(), s.code().unwrap_or(-1)),
                    ),
                    Err(e) => (false, format!("tmuxlet not runnable ({e})")),
                }
            }
            config::Backend::Api { base_url, .. } => {
                let (_scheme, host, port, _path) = backend::api::split_url(base_url);
                let addr = format!("{host}:{port}");
                match addr.to_socket_addrs().ok().and_then(|mut it| it.next()) {
                    Some(sa) => match TcpStream::connect_timeout(&sa, Duration::from_secs(3)) {
                        Ok(_) => (true, format!("connected {addr}")),
                        Err(e) => (false, format!("cannot connect {addr} ({e})")),
                    },
                    None => (false, format!("cannot resolve {addr}")),
                }
            }
        };
        println!(
            "{} {name} ({}) — {detail}",
            if ok { "PASS" } else { "FAIL" },
            b.type_str()
        );
        if !ok {
            all_ok = false;
        }
    }
    all_ok
}

fn listen_is_loopback(listen: &str) -> bool {
    match listen.to_socket_addrs() {
        Ok(addrs) => {
            let addrs: Vec<_> = addrs.collect();
            !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback())
        }
        Err(_) => false,
    }
}

/// S-2: refuse to expose an unauthenticated server to the network.
fn bind_policy(listen: &str, auth_on: bool, allow_remote: bool) -> Result<(), String> {
    if !listen_is_loopback(listen) && !auth_on && !allow_remote {
        return Err(format!(
            "refusing to bind non-loopback address '{listen}' without auth — set `auth = true` in server.toml (a token is generated to ~/.tmuxlet/token), or pass --allow-remote-unauthenticated to override"
        ));
    }
    Ok(())
}

/// S-4: probe whether the installed tmuxlet advertises a stdin form in its help
/// text. Conservative: any failure or ambiguity selects argv mode (never fails
/// a request).
fn probe_tmuxlet_stdin(env: &Env) -> bool {
    let bin = backend::resolve_program("tmuxlet", env);
    match Command::new(&bin)
        .arg("--help")
        .env_clear()
        .envs(env.as_pairs())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
            text.to_ascii_lowercase().contains("stdin")
        }
        Err(_) => false,
    }
}

fn run_serve(config_path: &str, allow_remote: bool) -> ExitCode {
    // 1. rustls ring CryptoProvider — REQUIRED before any TLS use (api backend).
    let _ = rustls::crypto::ring::default_provider().install_default();

    // 2. Load + validate config. Lint warnings are non-fatal at serve time
    //    (UX invariant 2: an upgrade must not brick a running service).
    let path = config::tilde_path(config_path);
    let (cfg, warnings) = match config::load(&path) {
        Ok(cw) => cw,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    // U-8: honor log_level for the rest of startup + request handling.
    log::set_level(&cfg.server.log_level);
    for w in &warnings {
        log::warn(w);
    }

    // 3. Capture environment for backends (S-9 timeout-bounded).
    let environment = capture_env(&cfg);
    for b in cfg.backends.values() {
        if let config::Backend::Api {
            api_key_env: Some(k),
            ..
        } = b
            && environment.get(k).is_none()
        {
            eprintln!("[warn] api_key_env {k} is unset");
        }
    }

    // 4. Resolve the auth token (S-1).
    let token_file = config::tilde_path("~/.tmuxlet/token");
    let auth_token = match auth::resolve(
        cfg.server.auth,
        cfg.server.auth_token_env.as_deref(),
        &environment,
        &token_file,
    ) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    // 5. Bind policy (S-2): refuse to expose an unauthenticated server.
    if let Err(e) = bind_policy(&cfg.server.listen, auth_token.is_some(), allow_remote) {
        eprintln!("tmuxlet-server: {e}");
        return ExitCode::FAILURE;
    }
    let loopback = listen_is_loopback(&cfg.server.listen);
    if !loopback {
        eprintln!(
            "[warn] bound to a non-loopback address ({}); tiny_http has no TLS — use an SSH tunnel for remote access",
            cfg.server.listen
        );
    }

    // 6. Build the runtime backends once (P-3), resolving tmuxlet's path from the
    //    captured env PATH (F-4). Probe tmuxlet's stdin form once (S-4).
    let has_tmuxlet = cfg
        .backends
        .values()
        .any(|b| matches!(b, config::Backend::Tmuxlet { .. }));
    let defaults = backend::ServerDefaults {
        max_response_bytes: cfg.server.max_response_bytes,
        env_pass: cfg.server.env_pass.clone(),
        tmuxlet_stdin: has_tmuxlet && probe_tmuxlet_stdin(&environment),
    };
    let backends: HashMap<String, Backend> = cfg
        .backends
        .iter()
        .map(|(name, b)| {
            (
                name.clone(),
                Backend::from_config(name, b, &environment, &defaults),
            )
        })
        .collect();

    // 7. Bind.
    let listen = cfg.server.listen.clone();
    let server = match Server::http(&listen) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("tmuxlet-server: failed to bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!(
        "tmuxlet-server {VERSION} listening on http://{listen} ({} backends, {} chains, auth {})",
        cfg.backends.len(),
        cfg.chains.len(),
        if auth_token.is_some() { "on" } else { "off" }
    );

    // 8. Signal handler → unblock workers for graceful shutdown.
    {
        let server = Arc::clone(&server);
        let _ = ctrlc::set_handler(move || {
            eprintln!("shutting down...");
            server.unblock();
        });
    }

    // P-1: workers = configured, else max(16, cores) — the workload is blocking
    // I/O, so cores alone under-provisions (one long tmuxlet turn pins a worker).
    // `.max(1)`: workers = 0 would spawn no threads and the server would bind,
    // print "listening", then exit immediately (lint warns; serve stays alive).
    let workers = cfg
        .server
        .workers
        .unwrap_or_else(|| {
            let cores = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            cores.max(16)
        })
        .max(1);
    let sessions = Arc::new(route::session::SessionStore::new(
        cfg.server.session_max,
        cfg.server.session_ttl_secs,
    ));
    // U-22: the counters and the session map are sibling Arcs, so a SIGHUP reload
    // rebuilds State around them instead of dropping every conversation's
    // affinity and every counter on the floor.
    let live = http::Live::new(Arc::clone(&sessions));
    let state = Arc::new(http::State::new(
        cfg,
        environment,
        backends,
        auth_token,
        !loopback,
        &live,
    ));
    let holder: http::StateHolder = Arc::new(RwLock::new(state));
    install_reload_handler();
    spawn_reload_watcher(
        config_path.to_string(),
        Arc::clone(&holder),
        live,
        loopback,
        allow_remote,
    );
    http::serve(server, holder, workers);
    ExitCode::SUCCESS
}

/// U-22: set by the SIGHUP handler, drained by the watcher thread.
///
/// A signal handler may only do async-signal-safe work, so it does exactly one
/// thing: store a flag. Everything real — file I/O, validation, allocation —
/// happens on an ordinary thread.
static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sighup(_: CInt) {
    RELOAD_REQUESTED.store(true, Ordering::Relaxed);
}

/// `libc::c_int` without depending on `libc` directly — `nix` re-exports it.
type CInt = nix::libc::c_int;

fn install_reload_handler() {
    // SAFETY: `on_sighup` only stores to an AtomicBool, which is
    // async-signal-safe. SIGHUP's default action is to terminate, so failing to
    // install this would make `kill -HUP` kill the server — warn loudly.
    let res = unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGHUP,
            nix::sys::signal::SigHandler::Handler(on_sighup),
        )
    };
    if let Err(e) = res {
        log::warn(&format!(
            "could not install the SIGHUP handler ({e}); config reload is unavailable and kill -HUP will terminate the server"
        ));
    }
}

/// U-22: re-load, validate, and swap `Arc<State>`.
///
/// **A bad config must never brick a running service** (UX invariant 2): a reload
/// that fails to parse or validate is logged and discarded, and the old state
/// keeps serving. `listen` and `workers` are load-bearing at startup only — the
/// socket is already bound and the threads already spawned — so a change to
/// either is reported and ignored rather than silently having no effect.
fn spawn_reload_watcher(
    config_path: String,
    holder: http::StateHolder,
    live: http::Live,
    loopback: bool,
    allow_remote: bool,
) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(500));
            if !RELOAD_REQUESTED.swap(false, Ordering::Relaxed) {
                continue;
            }
            log::info("SIGHUP: reloading config");
            match reload_state(&config_path, &live, loopback, allow_remote, &holder) {
                Ok(msg) => log::info(&format!("SIGHUP: {msg}")),
                Err(e) => log::error(&format!(
                    "SIGHUP: reload REJECTED, keeping the running config — {e}"
                )),
            }
        }
    });
}

fn reload_state(
    config_path: &str,
    live: &http::Live,
    loopback: bool,
    allow_remote: bool,
    holder: &http::StateHolder,
) -> Result<String, String> {
    let path = config::tilde_path(config_path);
    let (cfg, warnings) = config::load(&path)?;
    for w in &warnings {
        log::warn(&format!("SIGHUP: {w}"));
    }
    // S-2 still applies on reload: a config that turns auth off while bound to a
    // public address must not be allowed to take effect.
    let auth_token = auth::resolve(
        cfg.server.auth,
        cfg.server.auth_token_env.as_deref(),
        &capture_env(&cfg),
        &config::tilde_path("~/.tmuxlet/token"),
    )?;
    bind_policy(&cfg.server.listen, auth_token.is_some(), allow_remote)?;

    let old = holder
        .read()
        .map(|g| Arc::clone(&g))
        .map_err(|_| "state lock is poisoned".to_string())?;
    if cfg.server.listen != old.cfg.server.listen {
        log::warn(&format!(
            "SIGHUP: listen changed to {} but the socket is already bound to {}; restart to move it",
            cfg.server.listen, old.cfg.server.listen
        ));
    }
    if cfg.server.workers != old.cfg.server.workers {
        log::warn("SIGHUP: workers changed but the pool is already spawned; restart to resize it");
    }
    log::set_level(&cfg.server.log_level);

    let environment = capture_env(&cfg);
    let has_tmuxlet = cfg
        .backends
        .values()
        .any(|b| matches!(b, config::Backend::Tmuxlet { .. }));
    let defaults = backend::ServerDefaults {
        max_response_bytes: cfg.server.max_response_bytes,
        env_pass: cfg.server.env_pass.clone(),
        tmuxlet_stdin: has_tmuxlet && probe_tmuxlet_stdin(&environment),
    };
    let backends: HashMap<String, Backend> = cfg
        .backends
        .iter()
        .map(|(name, b)| {
            (
                name.clone(),
                Backend::from_config(name, b, &environment, &defaults),
            )
        })
        .collect();
    let n_backends = backends.len();
    let n_chains = cfg.chains.len();
    let fresh = Arc::new(http::State::new(
        cfg,
        environment,
        backends,
        auth_token,
        !loopback,
        live,
    ));
    match holder.write() {
        Ok(mut g) => *g = fresh,
        Err(e) => *e.into_inner() = fresh,
    }
    Ok(format!(
        "swapped in {n_backends} backends and {n_chains} chains; counters, sessions, health and in-flight slots preserved"
    ))
}

fn print_help() {
    println!(
        "tmuxlet-server {VERSION}\n\nUsage:\n  tmuxlet-server [--config FILE]    Start the server (default config: {DEFAULT_CONFIG})\n  tmuxlet-server --validate         Validate config (strict) and exit\n  tmuxlet-server --dry-run          Alias for --validate\n  tmuxlet-server --explain MODEL    Print the policy, cascade and leg order for a model\n  tmuxlet-server --replay FILE      Score a JSONL conversation corpus offline (§12)\n  tmuxlet-server --check-backends   Probe each backend's reachability and exit\n  tmuxlet-server --version          Print version\n  tmuxlet-server --help             Print this help\n\nConfig is TOML at {DEFAULT_CONFIG} (override with --config FILE or --config=FILE).\nA fresh config listens on {DEFAULT_LISTEN}. Set `auth = true` for a bearer token\n(written to ~/.tmuxlet/token); --allow-remote-unauthenticated permits a\nnon-loopback bind without auth."
    );
}

/// §10 `--explain <model>`: print the resolved policy, the cascade it will walk,
/// and the leg order, without starting the server.
///
/// This is the debugging tool the config format has needed since the five-router
/// framework landed: "which backend will `do` actually hit, and why" was
/// previously only answerable by sending a request and reading a header.
fn run_explain(config_path: &str, model: &str) -> ExitCode {
    let path = config::tilde_path(config_path);
    let (cfg, _warnings) = match config::load(&path) {
        Ok(cw) => cw,
        Err(e) => {
            eprintln!("tmuxlet-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!("model: {model}");
    match cfg.routers.get(model) {
        Some(r) if r.kind == "signals" => {
            let chain = r.chain.as_deref().unwrap_or(&cfg.server.default_chain);
            println!("policy: signals (§7)");
            println!(
                "  confidence_threshold={} reserve_threshold={} recent_turn_window={}",
                r.confidence_threshold, r.reserve_threshold, r.recent_turn_window
            );
            println!("  chain: {chain}");
            print_legs(&cfg, chain, "    ");
            println!("cascade (§7.7), first match wins:");
            println!("  1 affinity   (not used by a signals router: rungs 2-6 depend on live");
            println!("               quota pressure, so replaying them would be wrong. Affinity");
            println!("               applies to the classifier at rung 7.)");
            println!("  2 quota      leg 0 pressure == 1.0 -> start one leg deeper");
            println!("  3 override   compacted context or a critical tool error -> leg 0");
            println!(
                "  4 static     pressure < {} -> leg 0  [the common path]",
                r.reserve_threshold
            );
            println!("  5 settled    rationing + tests passed -> shed one leg");
            println!(
                "  6 dimensions rationing + confidence >= {} -> score decides",
                r.confidence_threshold
            );
            match &r.classifier {
                Some(c) => println!("  7 classifier rationing + sub-threshold -> ask '{c}'"),
                None => println!("  7 classifier (no classifier configured; skipped)"),
            }
            println!("  8 fall_open  shed one leg, subject to the agentic constraint");
        }
        Some(r) => {
            println!("policy: classifier (§5)");
            println!(
                "  classifier={} fallback_class={}",
                r.classifier.as_deref().unwrap_or("-"),
                r.fallback_class.as_deref().unwrap_or("-")
            );
            let mut classes: Vec<(&String, &String)> = r.routes.iter().collect();
            classes.sort();
            for (class, target) in classes {
                println!("  class '{class}' -> {target}");
                print_legs(&cfg, target, "    ");
            }
        }
        None => {
            let known = cfg.chains.contains_key(model) || cfg.backends.contains_key(model);
            if known {
                println!("policy: static — the model names a chain or backend");
            } else if cfg.server.strict_models {
                println!(
                    "policy: static — UNKNOWN model, and strict_models = true, so this is a 404"
                );
                return ExitCode::FAILURE;
            } else {
                println!(
                    "policy: default — unknown model falls back to default_chain '{}'",
                    cfg.server.default_chain
                );
            }
            let target = if known {
                model
            } else {
                &cfg.server.default_chain
            };
            print_legs(&cfg, target, "  ");
        }
    }
    ExitCode::SUCCESS
}

/// Print a chain's legs with the economics that decide whether one can be shed.
fn print_legs(cfg: &Config, target: &str, indent: &str) {
    let legs: Vec<String> = match cfg.chains.get(target) {
        Some(c) => c.order.clone(),
        None if cfg.backends.contains_key(target) => vec![target.to_string()],
        None => {
            println!("{indent}(unresolvable: '{target}' is not a chain or backend)");
            return;
        }
    };
    for (i, name) in legs.iter().enumerate() {
        match cfg.backends.get(name) {
            Some(b) => println!(
                "{indent}{i}. {name} [{}, cost={}, agentic={}]",
                b.type_str(),
                b.cost_class(),
                b.is_agentic()
            ),
            None => println!("{indent}{i}. {name} (UNDEFINED)"),
        }
    }
}

/// §12 `--replay <file.jsonl>`: score a captured corpus offline and sweep
/// `confidence_threshold`.
///
/// No model calls, no server — which is exactly why `route::signals` and
/// `route::scorer` are pure functions over `&Value`. This is the harness that
/// turns the thresholds from guesses into measurements.
fn run_replay(file: &str) -> ExitCode {
    const THRESHOLDS: [f64; 5] = [0.0, 0.3, 0.5, 0.7, 0.9];
    let path = config::tilde_path(file);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tmuxlet-server: cannot read {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let mut scored: Vec<(String, route::scorer::Score)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("tmuxlet-server: {}:{}: {e}", path.display(), n + 1);
                return ExitCode::FAILURE;
            }
        };
        let id = v
            .get("id")
            .and_then(|s| s.as_str())
            .unwrap_or("-")
            .to_string();
        let messages = v
            .get("messages")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let sig = route::signals::extract(&messages, route::signals::DEFAULT_RECENT_WINDOW);
        let sc = route::scorer::score(&sig);
        let turns = messages.as_array().map_or(0, |a| a.len());
        println!(
            "conversation {id}: turns={turns} score={:+.3} confidence={:.3} signals=w/e/r/t={}/{}/{}/{} sev={:.2} depth={} compacted={}",
            sc.score,
            sc.confidence,
            sig.recent_write_count,
            sig.recent_edit_count,
            sig.recent_read_count,
            sig.recent_todowrite_count,
            sig.severity,
            sig.turn_depth,
            sig.compacted,
        );
        scored.push((id, sc));
    }
    if scored.is_empty() {
        eprintln!("tmuxlet-server: {} has no conversations", path.display());
        return ExitCode::FAILURE;
    }
    // The routing split per threshold. "shed" is a turn the scorer would move off
    // leg 0 *while rationing*; below reserve_threshold none of them move at all.
    for t in THRESHOLDS {
        let shed = scored
            .iter()
            .filter(|(_, sc)| !(sc.confidence >= t && sc.score > 0.0))
            .count();
        println!("threshold {t:.2}: keep={} shed={shed}", scored.len() - shed);
    }
    ExitCode::SUCCESS
}
