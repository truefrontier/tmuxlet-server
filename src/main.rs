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
use std::sync::Arc;
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
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        config: DEFAULT_CONFIG.to_string(),
        validate: false,
        check_backends: false,
        allow_remote: false,
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
            other => {
                if let Some(p) = other.strip_prefix("--config=") {
                    args.config = p.to_string();
                } else {
                    eprintln!("tmuxlet-server: unknown argument: {other}");
                    return ExitCode::FAILURE;
                }
            }
        }
        i += 1;
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
    let state = Arc::new(http::State::new(
        cfg,
        environment,
        backends,
        auth_token,
        !loopback,
        // U-22: the counters and the session map are built here and handed in as
        // sibling Arcs, so a future SIGHUP reload can rebuild State around them
        // instead of dropping every conversation's affinity on the floor.
        Arc::new(stats::Stats::new()),
        sessions,
    ));
    http::serve(server, state, workers);
    ExitCode::SUCCESS
}

fn print_help() {
    println!(
        "tmuxlet-server {VERSION}\n\nUsage:\n  tmuxlet-server [--config FILE]    Start the server (default config: {DEFAULT_CONFIG})\n  tmuxlet-server --validate         Validate config (strict) and exit\n  tmuxlet-server --dry-run          Alias for --validate\n  tmuxlet-server --check-backends   Probe each backend's reachability and exit\n  tmuxlet-server --version          Print version\n  tmuxlet-server --help             Print this help\n\nConfig is TOML at {DEFAULT_CONFIG} (override with --config FILE or --config=FILE).\nA fresh config listens on {DEFAULT_LISTEN}. Set `auth = true` for a bearer token\n(written to ~/.tmuxlet/token); --allow-remote-unauthenticated permits a\nnon-loopback bind without auth."
    );
}
