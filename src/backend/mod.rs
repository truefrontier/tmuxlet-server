pub mod api;
pub mod cli;
pub mod tmuxlet;

use crate::config;
use crate::env::Env;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct DispatchResult {
    pub content: String,
    pub model_label: String,
}

#[derive(Debug)]
pub enum BackendError {
    Timeout(String),
    Spawn(String, String),
    Exit(String, i32, String),
    Backend(String, String),
    /// name, status code, and the first chars of the upstream error body (U-7).
    Http(String, u16, String),
    Parse(String, String),
    /// U-20: backend at its max_concurrent cap; the chain advances.
    Busy(String),
    /// §4.3: the prompt exceeds this backend's context window. Not a health
    /// signal — the backend is fine, the prompt does not fit. Carries the backend
    /// name and the upstream's own detail, so the 400 we return is actionable
    /// ("maximum context length is 32768 tokens, requested 41002").
    Overflow(String, String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BackendError::Timeout(n) => write!(f, "[{n}] timed out"),
            BackendError::Spawn(n, e) => write!(f, "[{n}] spawn failed: {e}"),
            BackendError::Exit(n, c, s) => write!(f, "[{n}] exited with {c}: {s}"),
            BackendError::Backend(n, m) => write!(f, "[{n}] backend error: {m}"),
            BackendError::Http(n, s, d) => {
                if d.is_empty() {
                    write!(f, "[{n}] HTTP {s}")
                } else {
                    write!(f, "[{n}] HTTP {s}: {d}")
                }
            }
            BackendError::Parse(n, m) => write!(f, "[{n}] parse error: {m}"),
            BackendError::Busy(n) => write!(f, "[{n}] busy (max_concurrent reached)"),
            BackendError::Overflow(n, d) => {
                if d.is_empty() {
                    write!(f, "[{n}] context overflow")
                } else {
                    write!(f, "[{n}] context overflow: {d}")
                }
            }
        }
    }
}

impl BackendError {
    /// The backend name this error is attributed to (for U-23 error grouping).
    pub fn backend_name(&self) -> &str {
        match self {
            BackendError::Timeout(n)
            | BackendError::Spawn(n, _)
            | BackendError::Exit(n, _, _)
            | BackendError::Backend(n, _)
            | BackendError::Http(n, _, _)
            | BackendError::Parse(n, _)
            | BackendError::Busy(n)
            | BackendError::Overflow(n, _) => n,
        }
    }

    /// A short error class for the U-23 compact 503 summary.
    pub fn class(&self) -> &'static str {
        match self {
            BackendError::Timeout(_) => "timeout",
            BackendError::Spawn(_, _) => "spawn",
            BackendError::Exit(_, _, _) => "exit",
            BackendError::Backend(_, _) => "backend error",
            BackendError::Http(_, s, _) => match s {
                429 => "rate limited",
                _ => "http error",
            },
            BackendError::Parse(_, _) => "parse error",
            BackendError::Busy(_) => "busy",
            BackendError::Overflow(_, _) => "context overflow",
        }
    }
}

/// §4.3: does this upstream response say "your prompt does not fit"?
///
/// Deliberately narrow: HTTP **400** *and* a body naming a context error. 413 and
/// 422 do **not** count — they are too ambiguous across upstreams, and a false
/// positive silently skips a healthy leg without ever cooling it, which is a
/// worse failure than the one this exists to fix.
///
/// **Only `api` backends can reach this today.** The rule keys on an HTTP status,
/// and `BackendError::Http` is constructed in `backend/api.rs` alone. A `tmuxlet`
/// or `cli` backend that overflows surfaces as `Exit`/`Backend` with no status
/// and still cools exactly as before. The common local-model shape
/// (Ollama/llama.cpp behind `type = "api"` on 127.0.0.1) *is* covered; widening
/// to text-pattern-only detection with no status to gate on is exactly the
/// false-positive risk the narrow rule avoids.
pub fn is_context_overflow(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    let b = body.to_ascii_lowercase();
    b.contains("context_length_exceeded")
        || b.contains("maximum context length")
        || b.contains("prompt is too long")
        || b.contains("context window")
}

pub struct TmuxletBackend {
    pub name: String,
    /// F-4: `tmuxlet` resolved to an absolute path from the captured env PATH at
    /// startup, so spawn failures are deterministic rather than PATH-dependent.
    pub bin: PathBuf,
    pub target: String,
    pub target_args: Vec<String>,
    pub cwd: PathBuf,
    pub allow_empty: bool,
    /// S-3: resolved env allowlist (per-backend or server default).
    pub env_pass: Option<Vec<String>>,
    /// S-4: use tmuxlet's stdin form (`-p ... -`) when the installed tmuxlet
    /// supports it (probed once at startup); otherwise argv.
    pub use_stdin: bool,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
    /// U-14: prompt shaping mode.
    pub prompt_mode: crate::openai::PromptMode,
}

pub struct ApiBackend {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub extra_body: serde_json::Value,
    pub timeout: Option<Duration>,
    pub allow_empty: bool,
    /// S-5: cap on the upstream response body (server-level).
    pub max_response_bytes: u64,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
    /// U-24: custom CA PEM (tilde-expanded) trusted in addition to webpki roots.
    pub ca_file: Option<PathBuf>,
}

pub struct CliBackend {
    pub name: String,
    pub bin: PathBuf,
    pub args: Vec<String>,
    pub pty: bool,
    pub cwd: PathBuf,
    /// U-12: (cols, rows).
    pub pty_size: (u16, u16),
    pub allow_empty: bool,
    /// S-3: resolved env allowlist (per-backend or server default).
    pub env_pass: Option<Vec<String>>,
    /// S-4: write the prompt to stdin instead of argv (plain mode).
    pub stdin_prompt: bool,
    /// U-20: max simultaneous dispatches (None = unlimited).
    pub max_concurrent: Option<usize>,
    /// U-14: prompt shaping mode.
    pub prompt_mode: crate::openai::PromptMode,
}

pub enum Backend {
    Tmuxlet(TmuxletBackend),
    Api(ApiBackend),
    Cli(CliBackend),
}

/// Resolve a program name to an absolute path using the captured env's PATH
/// (F-4). A name already containing a path separator is used verbatim. Falls
/// back to the bare name (spawn will then surface a deterministic failure).
pub fn resolve_program(program: &str, env: &Env) -> PathBuf {
    if program.contains('/') {
        return PathBuf::from(program);
    }
    if let Some(path) = env.get("PATH") {
        for dir in path.split(':').filter(|d| !d.is_empty()) {
            let candidate = Path::new(dir).join(program);
            // Skip a non-executable shadow (e.g. a 0644 file named `tmuxlet`)
            // and keep scanning, matching execvp/shell PATH semantics (F-4).
            if is_executable_file(&candidate) {
                return candidate;
            }
        }
    }
    PathBuf::from(program)
}

/// True if `p` is a regular file with any exec bit set (F-4). This is a
/// permission-bit check, not an `access(2)` call: it can accept a file the
/// current (non-root) server user cannot actually execute (e.g. exec bit only
/// in `group`/`other`). Exact executability requires `faccessat`, which isn't
/// worth the extra plumbing for a localhost preflight — a genuinely
/// non-executable target fails loudly at spawn time on the next chain leg.
#[cfg(unix)]
pub(crate) fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Compute a poll deadline `now + timeout` that cannot panic on an absurd
/// timeout (e.g. a huge configured `request_timeout_secs`). `Instant` has no
/// wall-clock maximum, so an overflow falls back to ~1 year out — effectively
/// unbounded for a localhost tool, and never an already-expired deadline.
pub(crate) fn poll_deadline(timeout: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(timeout).unwrap_or_else(|| {
        now.checked_add(Duration::from_secs(365 * 24 * 3600))
            .unwrap_or(now)
    })
}

/// Server-level values threaded into each runtime backend at build time.
pub struct ServerDefaults {
    pub max_response_bytes: u64,
    /// S-3 server-level env allowlist (used when a backend defines none).
    pub env_pass: Option<Vec<String>>,
    /// S-4: whether the installed tmuxlet supports the stdin form.
    pub tmuxlet_stdin: bool,
}

impl Backend {
    pub fn from_config(
        name: &str,
        c: &config::Backend,
        env: &Env,
        defaults: &ServerDefaults,
    ) -> Backend {
        let home = || std::env::var("HOME").unwrap_or_else(|_| ".".into());
        // S-3: a per-backend env_pass overrides the server-level default.
        let resolve_env_pass =
            |own: &Option<Vec<String>>| own.clone().or_else(|| defaults.env_pass.clone());
        match c {
            config::Backend::Tmuxlet {
                target,
                target_args,
                cwd,
                allow_empty,
                env_pass,
                max_concurrent,
                prompt_mode,
            } => Backend::Tmuxlet(TmuxletBackend {
                name: name.into(),
                bin: resolve_program("tmuxlet", env),
                target: target.clone(),
                target_args: target_args.clone(),
                cwd: PathBuf::from(cwd.clone().unwrap_or_else(home)),
                allow_empty: *allow_empty,
                env_pass: resolve_env_pass(env_pass),
                use_stdin: defaults.tmuxlet_stdin,
                max_concurrent: *max_concurrent,
                prompt_mode: prompt_mode
                    .as_deref()
                    .and_then(crate::openai::PromptMode::parse)
                    .unwrap_or_default(),
            }),
            config::Backend::Api {
                base_url,
                model,
                api_key_env,
                extra_body,
                timeout_secs,
                allow_empty,
                max_concurrent,
                ca_file,
            } => Backend::Api(ApiBackend {
                name: name.into(),
                base_url: base_url.clone(),
                model: model.clone(),
                api_key_env: api_key_env.clone(),
                extra_body: extra_body.clone(),
                timeout: timeout_secs.map(Duration::from_secs),
                allow_empty: *allow_empty,
                max_response_bytes: defaults.max_response_bytes,
                max_concurrent: *max_concurrent,
                ca_file: ca_file.clone().map(PathBuf::from),
            }),
            config::Backend::Cli {
                bin,
                args,
                pty,
                cwd,
                pty_size,
                allow_empty,
                env_pass,
                stdin_prompt,
                max_concurrent,
                prompt_mode,
            } => Backend::Cli(CliBackend {
                name: name.into(),
                bin: PathBuf::from(bin),
                args: args.clone(),
                pty: *pty,
                cwd: PathBuf::from(cwd.clone().unwrap_or_else(home)),
                pty_size: pty_size
                    .as_ref()
                    .filter(|v| v.len() == 2)
                    .map(|v| (v[0], v[1]))
                    .unwrap_or((200, 50)),
                allow_empty: *allow_empty,
                env_pass: resolve_env_pass(env_pass),
                stdin_prompt: *stdin_prompt,
                max_concurrent: *max_concurrent,
                prompt_mode: prompt_mode
                    .as_deref()
                    .and_then(crate::openai::PromptMode::parse)
                    .unwrap_or_default(),
            }),
        }
    }

    /// The backend's configured name. Exercised by tests; production logging
    /// uses the router-resolved name directly.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        match self {
            Backend::Tmuxlet(b) => &b.name,
            Backend::Api(b) => &b.name,
            Backend::Cli(b) => &b.name,
        }
    }

    /// U-6: whether an empty completion is acceptable as success.
    pub fn allow_empty(&self) -> bool {
        match self {
            Backend::Tmuxlet(b) => b.allow_empty,
            Backend::Api(b) => b.allow_empty,
            Backend::Cli(b) => b.allow_empty,
        }
    }

    /// A per-backend timeout override (api backends only), else None.
    pub fn timeout_override(&self) -> Option<Duration> {
        match self {
            Backend::Api(b) => b.timeout,
            _ => None,
        }
    }

    /// U-20: max simultaneous dispatches to this backend (None = unlimited).
    pub fn max_concurrent(&self) -> Option<usize> {
        match self {
            Backend::Tmuxlet(b) => b.max_concurrent,
            Backend::Api(b) => b.max_concurrent,
            Backend::Cli(b) => b.max_concurrent,
        }
    }

    /// U-14: prompt shaping mode (api backends pass raw JSON, so Transcript).
    pub fn prompt_mode(&self) -> crate::openai::PromptMode {
        match self {
            Backend::Tmuxlet(b) => b.prompt_mode,
            Backend::Cli(b) => b.prompt_mode,
            Backend::Api(_) => crate::openai::PromptMode::Transcript,
        }
    }

    pub fn dispatch(
        &self,
        prompt: &str,
        raw_messages: &serde_json::Value,
        env: &Env,
        timeout: Duration,
    ) -> Result<DispatchResult, BackendError> {
        match self {
            Backend::Tmuxlet(b) => tmuxlet::dispatch(b, prompt, env, timeout),
            Backend::Api(b) => api::dispatch(b, raw_messages, env, timeout),
            Backend::Cli(b) => cli::dispatch(b, prompt, env, timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_name_the_backend() {
        assert!(
            BackendError::Timeout("agy".into())
                .to_string()
                .contains("agy")
        );
        assert!(
            BackendError::Http("ollama".into(), 500, String::new())
                .to_string()
                .contains("500")
        );
    }

    #[test]
    fn http_error_includes_body_detail() {
        let e = BackendError::Http("or".into(), 401, "invalid key".into());
        assert!(e.to_string().contains("invalid key"));
        assert_eq!(e.class(), "http error");
        assert_eq!(
            BackendError::Http("or".into(), 429, String::new()).class(),
            "rate limited"
        );
    }

    #[test]
    fn overflow_error_classifies_and_names_its_backend() {
        let e =
            BackendError::Overflow("agy".into(), "maximum context length is 8192 tokens".into());
        assert_eq!(e.class(), "context overflow");
        assert_eq!(e.backend_name(), "agy");
        let s = e.to_string();
        assert!(
            s.contains("agy") && s.contains("8192 tokens"),
            "must name both the backend and the upstream detail: {s}"
        );
        assert_eq!(
            BackendError::Overflow("agy".into(), String::new()).to_string(),
            "[agy] context overflow",
            "an empty detail must not leave a dangling ': '"
        );
    }

    #[test]
    fn is_context_overflow_requires_400_plus_a_body_match() {
        // §4.3: a hit needs both the 400 and a body naming a context error; the
        // body match is case-insensitive.
        let hits: &[(u16, &str)] = &[
            (400, "context_length_exceeded"),
            (400, "maximum context length"),
            (400, "prompt is too long"),
            (400, "context window"),
            (400, "This model's Maximum Context Length is 8192 tokens"),
        ];
        // The 413 and 422 rows encode a deliberate decision, not an oversight:
        // those statuses are too ambiguous across upstreams, and a false positive
        // silently skips a healthy leg without cooling it. A future "helpful"
        // widening has to break this test to undo the choice.
        let misses: &[(u16, &str)] = &[
            (413, "maximum context length"),
            (422, "maximum context length"),
            (400, "invalid api key"),
            (500, "maximum context length"),
            (200, "maximum context length"),
        ];
        for (status, body) in hits {
            assert!(
                is_context_overflow(*status, body),
                "{status} + {body:?} must read as overflow"
            );
        }
        for (status, body) in misses {
            assert!(
                !is_context_overflow(*status, body),
                "{status} + {body:?} must not read as overflow"
            );
        }
    }
    #[test]
    fn builds_runtime_backend_from_config() {
        let cfg = crate::config::parse(
            r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "d"

[backends.t]
type = "tmuxlet"
target = "claude"

[chains.d]
order = ["t"]
"#,
        )
        .unwrap();
        let env = crate::env::Env::capture("process", "", 5);
        let defaults = ServerDefaults {
            max_response_bytes: 1024,
            env_pass: None,
            tmuxlet_stdin: false,
        };
        let b = Backend::from_config("t", &cfg.backends["t"], &env, &defaults);
        assert_eq!(b.name(), "t");
        assert!(matches!(b, Backend::Tmuxlet(_)));
    }

    #[test]
    #[cfg(unix)]
    fn is_executable_file_requires_the_exec_bit() {
        // F-4: a regular-but-non-executable shadow must not count as resolvable.
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tmuxlet-exec-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shadow = dir.join("tool");
        std::fs::write(&shadow, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable_file(&shadow), "0644 file is not executable");
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable_file(&shadow), "0755 file is executable");
        assert!(!is_executable_file(&dir.join("missing")), "absent path");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invalid_prompt_mode_falls_back_to_transcript() {
        // U-14: an unrecognized prompt_mode value is accepted at serve time and
        // defaults to Transcript (lint warns; serve never bricks over it).
        let cfg = crate::config::parse(
            r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "d"

[backends.c]
type = "cli"
bin = "/bin/echo"
prompt_mode = "verbatim"

[chains.d]
order = ["c"]
"#,
        )
        .unwrap();
        let env = crate::env::Env::capture("process", "", 5);
        let defaults = ServerDefaults {
            max_response_bytes: 1024,
            env_pass: None,
            tmuxlet_stdin: false,
        };
        let b = Backend::from_config("c", &cfg.backends["c"], &env, &defaults);
        assert_eq!(b.prompt_mode(), crate::openai::PromptMode::Transcript);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_program_keeps_scanning_past_a_nonexecutable_shadow() {
        // F-4: PATH = dirA(0644 shadow) : dirB(0755 real) must resolve to dirB,
        // matching execvp semantics (a non-executable shadow is skipped, not a
        // hard stop).
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("tmuxlet-path-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir_a = root.join("a");
        let dir_b = root.join("b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        std::fs::write(dir_a.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir_a.join("tool"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::write(dir_b.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(dir_b.join("tool"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let mut m = HashMap::new();
        m.insert(
            "PATH".to_string(),
            format!("{}:{}", dir_a.display(), dir_b.display()),
        );
        let env = crate::env::Env::from_map(m);
        assert_eq!(
            resolve_program("tool", &env),
            dir_b.join("tool"),
            "must skip the shadow in dirA and resolve to dirB"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn is_executable_file_follows_symlinks() {
        // A symlink (Homebrew-style PATHs) must resolve to its target: metadata()
        // follows the link, unlike symlink_metadata().
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("tmuxlet-symlink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("real");
        std::fs::write(&target, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.join("link");
        symlink(&target, &link).unwrap();
        assert!(
            is_executable_file(&link),
            "a symlink to a 0755 target must resolve as executable"
        );
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !is_executable_file(&link),
            "a symlink to a 0644 target is not executable"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
