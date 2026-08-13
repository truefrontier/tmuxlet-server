use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: Server,
    #[serde(default)]
    pub backends: HashMap<String, Backend>,
    #[serde(default)]
    pub chains: HashMap<String, Chain>,
    /// §5: auto-routers (name -> routable model id).
    #[serde(default)]
    pub routers: HashMap<String, Router>,
}

#[derive(Debug, Deserialize)]
pub struct Server {
    pub listen: String,
    pub default_chain: String,
    #[serde(default = "default_timeout")]
    pub request_timeout_secs: u64,
    #[serde(default = "default_env_source")]
    pub env_source: String,
    /// U-8: log level (error|warn|info|debug).
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Watchdog for shell env capture (S-9): a blocking `~/.zshrc` must not hang
    /// startup forever. On expiry the capture is abandoned and the process env
    /// is used instead.
    #[serde(default = "default_env_capture_timeout")]
    pub env_capture_timeout_secs: u64,
    /// S-1: enable bearer-token auth. When true, a 32-byte token is generated
    /// to ~/.tmuxlet/token (0600) unless `auth_token_env` is set.
    #[serde(default)]
    pub auth: bool,
    /// S-1: read the token from this env var (wins over the token file).
    #[serde(default)]
    pub auth_token_env: Option<String>,
    /// S-3: server-level default env allowlist for spawned backends (globs).
    #[serde(default)]
    pub env_pass: Option<Vec<String>>,
    /// S-5: cap on a single upstream response body.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: u64,
    /// P-1: max concurrent in-flight requests (worker threads). Default
    /// max(16, cores).
    #[serde(default)]
    pub workers: Option<usize>,
    /// P-6: optional total time budget across a chain's legs (seconds).
    #[serde(default)]
    pub chain_budget_secs: Option<u64>,
    /// P-9: enable failure-aware backend cooldown.
    #[serde(default = "default_true")]
    pub cooldown: bool,
    /// P-9: base cooldown for rate-limit / http failures (seconds).
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
    /// U-3: reject unknown models instead of falling back to default_chain.
    #[serde(default)]
    pub strict_models: bool,
    /// U-10: browser CORS allowlist. Empty (default) = no CORS headers, current
    /// behavior. When set, matching Origins are echoed and OPTIONS is answered.
    #[serde(default)]
    pub cors_origins: Vec<String>,
    /// §4.2: show the classifier the opening user task as well as the latest
    /// turn, instead of the latest turn alone. **Server-wide** — the only
    /// classifier key that lives outside `[routers.<name>]`, deliberately, so one
    /// line pins v0.2.0 behavior for every router rather than five. The
    /// neighbouring `classifier_max_chars` *is* per-router; do not assume this
    /// one follows it. Opt-out, because it changes routing quality.
    #[serde(default = "default_true")]
    pub classifier_include_opening: bool,
}

fn default_timeout() -> u64 {
    1800
} // deviation #5: align with tmuxlet's own print-mode default
fn default_env_source() -> String {
    "shell".into()
}
fn default_log_level() -> String {
    "info".into()
}
fn default_env_capture_timeout() -> u64 {
    15
}
fn default_max_response_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_true() -> bool {
    true
}
fn default_cooldown_secs() -> u64 {
    60
}
fn default_classifier_timeout() -> u64 {
    5
}
fn default_classifier_max_chars() -> usize {
    4000
}

#[derive(Debug, Deserialize)]
pub struct Chain {
    pub order: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Router {
    /// A defined backend (not a router) that classifies the task.
    pub classifier: String,
    #[serde(default = "default_classifier_timeout")]
    pub classifier_timeout_secs: u64,
    #[serde(default = "default_classifier_max_chars")]
    pub classifier_max_chars: usize,
    /// Class used when classification fails/times out/answers garbage.
    pub fallback_class: String,
    #[serde(default)]
    pub classifier_prompt: Option<String>,
    /// class -> chain or backend.
    #[serde(default)]
    pub routes: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Backend {
    Tmuxlet {
        target: String,
        #[serde(default)]
        target_args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
        /// U-6: treat empty output as failure so the chain advances. Set true to
        /// accept an empty completion as success.
        #[serde(default)]
        allow_empty: bool,
        /// S-3: env allowlist for the spawned child (globs); overrides the
        /// server-level default.
        #[serde(default)]
        env_pass: Option<Vec<String>>,
        /// U-20: max simultaneous dispatches to this backend.
        #[serde(default)]
        max_concurrent: Option<usize>,
        /// U-14: prompt shaping — "transcript" (default) | "last_user".
        #[serde(default)]
        prompt_mode: Option<String>,
    },
    Api {
        base_url: String,
        model: String,
        #[serde(default)]
        api_key_env: Option<String>,
        #[serde(default)]
        extra_body: JsonValue,
        #[serde(default)]
        timeout_secs: Option<u64>,
        #[serde(default)]
        allow_empty: bool,
        /// U-20: max simultaneous dispatches to this backend.
        #[serde(default)]
        max_concurrent: Option<usize>,
        /// U-24: custom CA PEM path (tilde-expanded) trusted for this backend's
        /// TLS in addition to the webpki roots.
        #[serde(default)]
        ca_file: Option<String>,
    },
    Cli {
        bin: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        pty: bool,
        /// U-11: working directory for the child (both plain and PTY modes).
        /// Tilde-expanded. Defaults to `$HOME`.
        #[serde(default)]
        cwd: Option<String>,
        /// U-12: PTY dimensions as `[cols, rows]`. Defaults to `[200, 50]`.
        #[serde(default)]
        pty_size: Option<Vec<u16>>,
        #[serde(default)]
        allow_empty: bool,
        /// S-3: env allowlist for the spawned child (globs); overrides the
        /// server-level default.
        #[serde(default)]
        env_pass: Option<Vec<String>>,
        /// S-4: write the prompt to the child's stdin instead of argv (plain
        /// mode; a `{prompt}` placeholder still takes precedence).
        #[serde(default)]
        stdin_prompt: bool,
        /// U-20: max simultaneous dispatches to this backend.
        #[serde(default)]
        max_concurrent: Option<usize>,
        /// U-14: prompt shaping — "transcript" (default) | "last_user".
        #[serde(default)]
        prompt_mode: Option<String>,
    },
}

impl Backend {
    /// The `type` discriminant as it appears in config and on `/v1/backends`.
    /// Single source of truth so the label can't drift between call sites.
    pub fn type_str(&self) -> &'static str {
        match self {
            Backend::Tmuxlet { .. } => "tmuxlet",
            Backend::Api { .. } => "api",
            Backend::Cli { .. } => "cli",
        }
    }
}

pub fn load(path: &Path) -> Result<(Config, Vec<String>), String> {
    let text = fs::read_to_string(path)
        .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
    let mut cfg =
        parse(&text).map_err(|e| format!("failed to parse config {}: {e}", path.display()))?;
    expand_paths(&mut cfg);
    validate(&cfg)?;
    let warnings = lint(&cfg, &text);
    Ok((cfg, warnings))
}

pub fn parse(text: &str) -> Result<Config, String> {
    toml::from_str(text).map_err(|e| e.to_string())
}

/// Hard errors: conditions under which the server cannot (or must not) serve.
/// These fail both `--validate` and `serve`.
pub fn validate(cfg: &Config) -> Result<(), String> {
    for (cname, chain) in &cfg.chains {
        if chain.order.is_empty() {
            return Err(format!("chain '{cname}' has an empty order"));
        }
        for b in &chain.order {
            if !cfg.backends.contains_key(b) {
                return Err(format!(
                    "chain '{cname}' references undefined backend '{b}'"
                ));
            }
        }
    }
    if !cfg.chains.contains_key(&cfg.server.default_chain)
        && !cfg.backends.contains_key(&cfg.server.default_chain)
    {
        return Err(format!(
            "server.default_chain '{}' is not a defined chain or backend",
            cfg.server.default_chain
        ));
    }
    // U-5: an unresolvable listen address can never bind — catch it here rather
    // than at a 2am service restart.
    if cfg.server.listen.as_str().to_socket_addrs().is_err() {
        return Err(format!(
            "server.listen '{}' is not a valid address:port (e.g. \"127.0.0.1:3456\")",
            cfg.server.listen
        ));
    }
    // §5.3: auto-router validation.
    for (rname, r) in &cfg.routers {
        if cfg.chains.contains_key(rname) || cfg.backends.contains_key(rname) {
            return Err(format!(
                "router '{rname}' collides with a chain or backend name — rename one"
            ));
        }
        if !cfg.backends.contains_key(&r.classifier) {
            return Err(format!(
                "router '{rname}' classifier '{}' is not a defined backend",
                r.classifier
            ));
        }
        if r.routes.is_empty() {
            return Err(format!("router '{rname}' has no [routers.{rname}.routes]"));
        }
        for (class, target) in &r.routes {
            if !cfg.chains.contains_key(target) && !cfg.backends.contains_key(target) {
                return Err(format!(
                    "router '{rname}' route '{class}' -> '{target}' is not a defined chain or backend"
                ));
            }
        }
        if !r.routes.contains_key(&r.fallback_class) {
            return Err(format!(
                "router '{rname}' fallback_class '{}' is not one of its routes",
                r.fallback_class
            ));
        }
    }
    Ok(())
}

/// Soft issues (U-4 unknown keys, U-5 depth): errors under `--validate`,
/// warnings at serve time (UX invariant 2 — an upgrade must never brick a
/// running service over a nit). Returns human-readable messages, each naming
/// its fix (UX invariant 3).
pub fn lint(cfg: &Config, text: &str) -> Vec<String> {
    let mut out = Vec::new();

    // U-4: unknown keys (serde silently ignores them). Diff the parsed TOML
    // table against the known key set; suggest the nearest neighbor.
    for (key, suggestion) in unknown_keys(text) {
        match suggestion {
            Some(s) => out.push(format!("unknown config key '{key}' — did you mean '{s}'?")),
            None => out.push(format!("unknown config key '{key}'")),
        }
    }

    // U-5: chain/backend name collision — the router silently prefers the chain.
    for name in cfg.chains.keys() {
        if cfg.backends.contains_key(name) {
            out.push(format!(
                "'{name}' is both a chain and a backend; the chain shadows the backend in routing — rename one"
            ));
        }
    }

    // U-5: request_timeout_secs = 0 means "no deadline" for api but "instant
    // timeout" for cli/tmuxlet.
    if cfg.server.request_timeout_secs == 0 {
        out.push(
            "server.request_timeout_secs = 0 means 'no deadline' for api backends but 'instant timeout' for cli/tmuxlet — omit the key for the default (1800)"
                .into(),
        );
    }

    // P-1: workers = 0 spawns no threads; serve clamps to 1, but name the fix.
    if cfg.server.workers == Some(0) {
        out.push(
            "server.workers = 0 spawns no worker threads and would serve nothing — set workers >= 1 or omit the key for the default"
                .into(),
        );
    }

    // U-5: unknown env_source value is silently treated as "shell".
    if !matches!(cfg.server.env_source.as_str(), "shell" | "process") {
        out.push(format!(
            "server.env_source '{}' is neither 'shell' nor 'process' — it will be treated as 'shell'",
            cfg.server.env_source
        ));
    }

    // U-8: an unknown log_level is silently treated as info (and debug — the
    // useful one — is easy to miss). Flag it, matching the env_source arm.
    if !matches!(
        cfg.server.log_level.as_str(),
        "error" | "warn" | "info" | "debug"
    ) {
        out.push(format!(
            "server.log_level '{}' is not one of error|warn|info|debug — it will be treated as 'info'",
            cfg.server.log_level
        ));
    }

    // U-12: pty_size must be [cols, rows].
    for (bname, b) in &cfg.backends {
        if let Backend::Cli {
            pty_size: Some(ps), ..
        } = b
            && ps.len() != 2
        {
            out.push(format!(
                "backends.{bname}.pty_size must be [cols, rows] (2 elements), got {} — using the default [200, 50]",
                ps.len()
            ));
        }
        // U-14: prompt_mode, if set, must be transcript | last_user.
        let pm = match b {
            Backend::Cli { prompt_mode, .. } | Backend::Tmuxlet { prompt_mode, .. } => {
                prompt_mode.as_deref()
            }
            _ => None,
        };
        if let Some(m) = pm
            && crate::openai::PromptMode::parse(m).is_none()
        {
            out.push(format!(
                "backends.{bname}.prompt_mode '{m}' is neither 'transcript' nor 'last_user' — using 'transcript'"
            ));
        }
        // U-24: a configured ca_file must exist, or TLS to that upstream fails.
        if let Backend::Api {
            ca_file: Some(p), ..
        } = b
            && !tilde_path(p).exists()
        {
            out.push(format!(
                "backends.{bname}.ca_file '{p}' does not exist — create it or drop the key to use the system CAs"
            ));
        }
    }

    // U-10: cors_origins matching is exact string equality (http.rs handle), so an
    // entry that can never equal a browser's Origin header is silently dead.
    for o in &cfg.server.cors_origins {
        if o == "*" {
            out.push(
                "server.cors_origins entry \"*\" is not a wildcard — matching is exact; list each origin explicitly, e.g. http://localhost:5173"
                    .into(),
            );
        } else if o == "null" {
            out.push(
                "server.cors_origins entry \"null\" matches the spoofable \"Origin: null\" (sandboxed iframes, file://); remove it unless you truly mean to allow those"
                    .into(),
            );
        } else if !o.starts_with("http://") && !o.starts_with("https://") {
            out.push(format!(
                "server.cors_origins '{o}' has no scheme — a browser Origin is scheme://host[:port], e.g. http://{o}"
            ));
        } else if o.ends_with('/') {
            out.push(format!(
                "server.cors_origins '{o}' has a trailing slash — a browser Origin has none, e.g. {}",
                o.trim_end_matches('/')
            ));
        } else if o.chars().any(|c| c.is_ascii_uppercase()) {
            out.push(format!(
                "server.cors_origins '{o}' has uppercase characters — browsers send the Origin lowercased, so it can never match; use '{}'",
                o.to_ascii_lowercase()
            ));
        }
    }

    out
}

// ---- U-4 unknown-key detection ----

const TOP_KEYS: &[&str] = &["server", "backends", "chains", "routers"];
const SERVER_KEYS: &[&str] = &[
    "listen",
    "default_chain",
    "request_timeout_secs",
    "env_source",
    "log_level",
    "env_capture_timeout_secs",
    "auth",
    "auth_token_env",
    "env_pass",
    "max_response_bytes",
    "workers",
    "chain_budget_secs",
    "cooldown",
    "cooldown_secs",
    "strict_models",
    "cors_origins",
    "classifier_include_opening",
];
const CHAIN_KEYS: &[&str] = &["order"];
const TMUXLET_KEYS: &[&str] = &[
    "type",
    "target",
    "target_args",
    "cwd",
    "allow_empty",
    "env_pass",
    "max_concurrent",
    "prompt_mode",
];
const API_KEYS: &[&str] = &[
    "type",
    "base_url",
    "model",
    "api_key_env",
    "extra_body",
    "timeout_secs",
    "allow_empty",
    "max_concurrent",
    "ca_file",
];
const CLI_KEYS: &[&str] = &[
    "type",
    "bin",
    "args",
    "pty",
    "cwd",
    "pty_size",
    "allow_empty",
    "env_pass",
    "stdin_prompt",
    "max_concurrent",
    "prompt_mode",
];
const ROUTER_KEYS: &[&str] = &[
    "classifier",
    "classifier_timeout_secs",
    "classifier_max_chars",
    "fallback_class",
    "classifier_prompt",
    "routes",
];

/// Walk the parsed TOML table one level deep per known section and report keys
/// not in the known set, with the nearest known key as a suggestion.
fn unknown_keys(text: &str) -> Vec<(String, Option<String>)> {
    use toml::Value;
    let mut out = Vec::new();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(_) => return out, // parse errors are surfaced elsewhere
    };
    for (k, _) in &table {
        if !TOP_KEYS.contains(&k.as_str()) {
            out.push((k.clone(), nearest(k, TOP_KEYS)));
        }
    }
    if let Some(Value::Table(server)) = table.get("server") {
        for (k, _) in server {
            if !SERVER_KEYS.contains(&k.as_str()) {
                out.push((format!("server.{k}"), nearest(k, SERVER_KEYS)));
            }
        }
    }
    if let Some(Value::Table(chains)) = table.get("chains") {
        for (cname, cval) in chains {
            if let Value::Table(c) = cval {
                for (k, _) in c {
                    if !CHAIN_KEYS.contains(&k.as_str()) {
                        out.push((format!("chains.{cname}.{k}"), nearest(k, CHAIN_KEYS)));
                    }
                }
            }
        }
    }
    if let Some(Value::Table(backends)) = table.get("backends") {
        for (bname, bval) in backends {
            if let Value::Table(b) = bval {
                let ty = b.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let allowed: &[&str] = match ty {
                    "tmuxlet" => TMUXLET_KEYS,
                    "api" => API_KEYS,
                    "cli" => CLI_KEYS,
                    _ => &["type"],
                };
                for (k, _) in b {
                    if !allowed.contains(&k.as_str()) {
                        out.push((format!("backends.{bname}.{k}"), nearest(k, allowed)));
                    }
                }
            }
        }
    }
    if let Some(Value::Table(routers)) = table.get("routers") {
        for (rname, rval) in routers {
            if let Value::Table(r) = rval {
                for (k, _) in r {
                    // `routes` is a free-form class->target map; don't validate
                    // its keys.
                    if !ROUTER_KEYS.contains(&k.as_str()) {
                        out.push((format!("routers.{rname}.{k}"), nearest(k, ROUTER_KEYS)));
                    }
                }
            }
        }
    }
    out
}

/// Nearest known key by Levenshtein distance, if within a small threshold.
fn nearest(key: &str, candidates: &[&str]) -> Option<String> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        if *c == "type" {
            continue; // never suggest the discriminant
        }
        let d = levenshtein(key, c);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    // Only suggest when the typo is close (<= a third of the key length, min 2).
    best.and_then(|(d, c)| {
        let threshold = (key.len() / 3).max(2);
        (d <= threshold).then(|| c.to_string())
    })
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn expand_tilde(s: &str) -> String {
    if s == "~" {
        return env::var("HOME").unwrap_or_else(|_| s.to_string());
    }
    if let Some(rest) = s.strip_prefix("~/")
        && let Ok(home) = env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    s.to_string()
}

fn expand_paths(cfg: &mut Config) {
    for b in cfg.backends.values_mut() {
        match b {
            Backend::Tmuxlet { cwd: Some(c), .. } => *c = expand_tilde(c),
            Backend::Cli { bin, cwd, .. } => {
                *bin = expand_tilde(bin);
                if let Some(c) = cwd {
                    *c = expand_tilde(c);
                }
            }
            Backend::Api {
                ca_file: Some(c), ..
            } => *c = expand_tilde(c),
            _ => {}
        }
    }
}

pub fn tilde_path(s: &str) -> PathBuf {
    PathBuf::from(expand_tilde(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "default"
request_timeout_secs = 1800
env_source = "shell"
log_level = "info"

[backends.agy]
type = "cli"
bin = "/Users/kk/.antigravity/antigravity/bin/agy"
args = ["-p"]
pty = true

[backends.ollama-kimi]
type = "api"
base_url = "http://127.0.0.1:11434/v1"
model = "kimi-k2.6:cloud"
extra_body = { think = true }
timeout_secs = 120

[backends.claude-thinking]
type = "tmuxlet"
target = "claude"
target_args = ["--effort", "max"]
cwd = "~"

[chains.default]
order = ["agy", "ollama-kimi", "claude-thinking"]
"#;

    #[test]
    fn parses_all_three_backend_types_and_extra_body() {
        let cfg = parse(SAMPLE).expect("sample must parse");
        assert_eq!(cfg.server.request_timeout_secs, 1800);
        assert_eq!(cfg.server.env_capture_timeout_secs, 15);
        assert_eq!(cfg.backends.len(), 3);
        match &cfg.backends["ollama-kimi"] {
            Backend::Api {
                extra_body,
                timeout_secs,
                ..
            } => {
                assert_eq!(extra_body["think"], serde_json::json!(true));
                assert_eq!(*timeout_secs, Some(120));
            }
            _ => panic!("ollama-kimi should be an api backend"),
        }
        assert_eq!(
            cfg.chains["default"].order,
            vec!["agy", "ollama-kimi", "claude-thinking"]
        );
    }

    #[test]
    fn validation_rejects_undefined_backend_in_chain() {
        let bad = format!("{SAMPLE}\n[chains.broken]\norder = [\"ghost\"]\n");
        let cfg = parse(&bad).unwrap();
        let err = validate(&cfg).unwrap_err();
        assert!(
            err.contains("broken") && err.contains("ghost"),
            "got: {err}"
        );
    }

    #[test]
    fn validation_rejects_unknown_default_chain() {
        let mut cfg = parse(SAMPLE).unwrap();
        cfg.server.default_chain = "missing".into();
        assert!(validate(&cfg).unwrap_err().contains("default_chain"));
    }

    #[test]
    fn validation_rejects_unparseable_listen() {
        let mut cfg = parse(SAMPLE).unwrap();
        cfg.server.listen = "not-an-address".into();
        assert!(validate(&cfg).unwrap_err().contains("listen"));
    }

    #[test]
    fn tilde_expands_to_home() {
        let cfg = parse(SAMPLE).unwrap();
        if let Ok(home) = std::env::var("HOME") {
            // expand_paths runs inside load(); call it directly here.
            let mut cfg = cfg;
            expand_paths(&mut cfg);
            match &cfg.backends["claude-thinking"] {
                Backend::Tmuxlet { cwd: Some(c), .. } => assert_eq!(*c, home),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn lint_flags_unknown_key_with_suggestion() {
        let bad = SAMPLE.replace("request_timeout_secs = 1800", "request_timeout_sec = 1800");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("request_timeout_sec") && w.contains("request_timeout_secs")),
            "expected a did-you-mean suggestion, got: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_unknown_backend_key() {
        let bad = SAMPLE.replace("pty = true", "pty = true\nptysize = [100, 40]");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings.iter().any(|w| w.contains("backends.agy.ptysize")),
            "got: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_missing_ca_file() {
        // U-24: a ca_file that doesn't exist is a validate-error / serve-warning,
        // and the key itself is recognized (no unknown-key noise).
        let bad = SAMPLE.replace(
            "timeout_secs = 120",
            "timeout_secs = 120\nca_file = \"/nonexistent/tmuxlet-ca.pem\"",
        );
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("backends.ollama-kimi.ca_file")),
            "missing ca_file not flagged: {warnings:?}"
        );
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("unknown") && w.contains("ca_file")),
            "ca_file should be a known key: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_name_collision_and_zero_timeout() {
        let bad = format!("{SAMPLE}\n[chains.agy]\norder = [\"ollama-kimi\"]\n")
            .replace("request_timeout_secs = 1800", "request_timeout_secs = 0");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("both a chain and a backend")),
            "collision: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("request_timeout_secs = 0")),
            "zero timeout: {warnings:?}"
        );
    }

    #[test]
    fn lint_flags_zero_workers() {
        // P-1: workers = 0 spawns no threads; lint names the fix (serve clamps).
        let bad = SAMPLE.replace("log_level = \"info\"", "log_level = \"info\"\nworkers = 0");
        let cfg = parse(&bad).unwrap();
        let warnings = lint(&cfg, &bad);
        assert!(
            warnings.iter().any(|w| w.contains("server.workers = 0")),
            "zero workers not flagged: {warnings:?}"
        );
    }

    #[test]
    fn clean_config_lints_clean() {
        let cfg = parse(SAMPLE).unwrap();
        assert!(lint(&cfg, SAMPLE).is_empty(), "{:?}", lint(&cfg, SAMPLE));
    }

    #[test]
    fn cli_cwd_tilde_expands() {
        let src = SAMPLE.replace("pty = true", "pty = true\ncwd = \"~\"");
        let mut cfg = parse(&src).unwrap();
        expand_paths(&mut cfg);
        if let Ok(home) = std::env::var("HOME") {
            match &cfg.backends["agy"] {
                Backend::Cli { cwd: Some(c), .. } => assert_eq!(*c, home),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn lint_flags_unknown_log_level() {
        // B8: 'trace' is not a real level (logger has error|warn|info|debug).
        let bad = SAMPLE.replace("log_level = \"info\"", "log_level = \"trace\"");
        let cfg = parse(&bad).unwrap();
        let w = lint(&cfg, &bad);
        assert!(
            w.iter()
                .any(|x| x.contains("log_level") && x.contains("error|warn|info|debug")),
            "trace should be flagged: {w:?}"
        );
    }

    #[test]
    fn lint_flags_bad_cors_origins() {
        // B7: each shape that can never match exact-equality is flagged.
        let base = SAMPLE.replace(
            "log_level = \"info\"",
            "log_level = \"info\"\ncors_origins = [\"*\", \"http://localhost:5173/\", \"localhost:3000\", \"null\", \"http://LocalHost:5173\"]",
        );
        let cfg = parse(&base).unwrap();
        let w = lint(&cfg, &base);
        assert!(
            w.iter().any(|x| x.contains("not a wildcard")),
            "star: {w:?}"
        );
        assert!(
            w.iter().any(|x| x.contains("trailing slash")),
            "slash: {w:?}"
        );
        assert!(w.iter().any(|x| x.contains("no scheme")), "scheme: {w:?}");
        assert!(w.iter().any(|x| x.contains("null")), "null: {w:?}");
        assert!(w.iter().any(|x| x.contains("uppercase")), "case: {w:?}");
    }

    #[test]
    fn lint_flags_invalid_prompt_mode_value() {
        // U-14: prompt_mode must be transcript | last_user; an unknown value is a
        // validate-error / serve-warning naming the fallback.
        let bad = SAMPLE.replace("pty = true", "pty = true\nprompt_mode = \"verbatim\"");
        let cfg = parse(&bad).unwrap();
        let w = lint(&cfg, &bad);
        assert!(
            w.iter()
                .any(|x| x.contains("prompt_mode") && x.contains("transcript")),
            "invalid prompt_mode not flagged: {w:?}"
        );
    }

    #[test]
    fn lint_flags_prompt_mode_on_an_api_backend_as_unknown() {
        // U-14: prompt_mode is meaningless for api backends (they pass raw JSON),
        // so it is not an API key and must surface as an unknown key.
        let bad = SAMPLE.replace(
            "model = \"kimi-k2.6:cloud\"",
            "model = \"kimi-k2.6:cloud\"\nprompt_mode = \"last_user\"",
        );
        let cfg = parse(&bad).unwrap();
        let w = lint(&cfg, &bad);
        assert!(
            w.iter()
                .any(|x| x.contains("backends.ollama-kimi.prompt_mode")),
            "prompt_mode on an api backend should be an unknown key: {w:?}"
        );
    }

    #[test]
    fn classifier_include_opening_defaults_true_and_lints_clean() {
        // §4.2 / O5: the key is server-wide and opt-out. Omitting it means true,
        // setting it under [server] is a known key, and the same name under a
        // router is *not* — pinning the home against a move back to per-router,
        // which would silently ignore an operator's [server] line.
        let cfg = parse(SAMPLE).unwrap();
        assert!(
            cfg.server.classifier_include_opening,
            "an omitted classifier_include_opening must default to true"
        );

        let opted_out = SAMPLE.replace(
            "log_level = \"info\"",
            "log_level = \"info\"\nclassifier_include_opening = false",
        );
        let cfg = parse(&opted_out).unwrap();
        assert!(
            !cfg.server.classifier_include_opening,
            "an explicit false must survive parsing"
        );
        let w = lint(&cfg, &opted_out);
        assert!(
            !w.iter().any(|x| x.contains("classifier_include_opening")),
            "server.classifier_include_opening must be a known server key: {w:?}"
        );

        let on_router = format!(
            r#"{SAMPLE}
[routers.x]
classifier = "agy"
fallback_class = "general"
classifier_include_opening = false

[routers.x.routes]
general = "default"
"#
        );
        let cfg = parse(&on_router).unwrap();
        let w = lint(&cfg, &on_router);
        assert!(
            w.iter()
                .any(|x| x.contains("routers.x.classifier_include_opening")),
            "the key has no per-router home and must lint as unknown: {w:?}"
        );
    }

    #[test]
    fn example_config_parses_validates_and_lints_clean() {
        assert_shipped_example_clean("examples/server.toml");
    }

    #[test]
    fn router_framework_example_parses_validates_and_lints_clean() {
        assert_shipped_example_clean("examples/server-router-framework.toml");
    }

    // D1 / invariant 4: every shipped example must always parse, validate, and
    // lint with zero warnings — catches a typo in an active config or a
    // reference key that drifted out of the *_KEYS arrays.
    fn assert_shipped_example_clean(rel: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{rel} must exist"));
        let cfg = parse(&text).expect("example config must parse");
        validate(&cfg).expect("example config must validate");
        let warnings = lint(&cfg, &text);
        assert!(
            warnings.is_empty(),
            "{rel} must lint clean, got: {warnings:?}"
        );
    }
}
