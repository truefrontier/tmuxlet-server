//! HTTP transport: parse a request, hand it to `route` and `exec`, respond.
//!
//! Everything about *which* backend answers lives in `route/`; everything about
//! *running* it lives in `exec/`. What is left here is the wire.

use crate::backend::{Backend, BackendError};
use crate::config::Config;
use crate::env::Env;
use crate::exec::chain::{self, BackendHealth, ChainCtx, StreamOutcome};
use crate::log;
use crate::proto::{anthropic, openai};
use crate::route::policy::{ClassifierPolicy, Policy, SignalPolicy, StaticPolicy};
use crate::route::session::{self, SessionKey, SessionStore};
use crate::route::{Decision, DecisionSource, RequestCtx};
use crate::stats::Stats;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

pub enum Route {
    Health,
    Models,
    Backends,
    Chat,
    /// §5.2: traffic counters.
    Stats,
    /// §5.2: `POST /v1/stats/reset`, for benchmarking runs.
    StatsReset,
    /// §5.3: Prometheus exposition, only when `[server] metrics = true`.
    Metrics,
    /// §9.1: Anthropic Messages, text-only.
    Messages,
    Reserved,
    NotFound,
    /// U-17: a known path reached with the wrong method. Carries the `Allow` value.
    MethodNotAllowed(&'static str),
}

pub fn classify(method: &str, path: &str) -> Route {
    // U-17: HEAD routes like GET; the handler emits headers with an empty body.
    let m = if method == "HEAD" { "GET" } else { method };
    match (m, path) {
        ("GET", "/health") => Route::Health,
        ("GET", "/v1/models") => Route::Models,
        ("GET", "/v1/backends") => Route::Backends,
        ("GET", "/v1/stats") => Route::Stats,
        ("GET", "/metrics") => Route::Metrics,
        ("POST", "/v1/stats/reset") => Route::StatsReset,
        ("POST", "/v1/chat/completions") => Route::Chat,
        ("POST", "/v1/messages") => Route::Messages,
        // U-17: known path, wrong method → 405 with an Allow header.
        (_, "/health")
        | (_, "/v1/models")
        | (_, "/v1/backends")
        | (_, "/v1/stats")
        | (_, "/metrics") => Route::MethodNotAllowed("GET, HEAD"),
        (_, "/v1/chat/completions") | (_, "/v1/messages") | (_, "/v1/stats/reset") => {
            Route::MethodNotAllowed("POST")
        }
        _ if path == "/" || path.starts_with("/ui") || path.starts_with("/api") => Route::Reserved,
        _ => Route::NotFound,
    }
}

/// U-2: channel-backed SSE body. `read` blocks on the channel, so the coordinator
/// thread can trickle `: keepalive` frames while dispatch runs, then push the
/// final frames. EOF when the sender drops.
pub struct ChannelSseBody {
    rx: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    pos: usize,
}

impl Read for ChannelSseBody {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.current.len() {
                let n = (self.current.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.rx.recv() {
                Ok(chunk) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Err(_) => return Ok(0),
            }
        }
    }
}

pub struct State {
    pub cfg: Config,
    pub env: Env,
    /// P-3: runtime backends built once at startup (with F-4 path resolution).
    pub backends: HashMap<String, Backend>,
    /// S-1: expected bearer token; None disables auth.
    pub auth_token: Option<String>,
    /// S-6: redact per-backend error detail (true when bound non-loopback).
    pub redact_errors: bool,
    /// P-9 / U-13: per-backend health.
    pub health: Mutex<HashMap<String, BackendHealth>>,
    /// U-20: in-flight dispatch counts per backend.
    pub active: Mutex<HashMap<String, usize>>,
    /// §5.2 counters. A **sibling `Arc`, not a field of `Config`**, on purpose:
    /// U-22's SIGHUP reload swaps `Arc<State>`, and traffic history must survive
    /// the swap rather than resetting every time someone edits the config.
    pub stats: Arc<Stats>,
    /// §6.2: bounded per-conversation state. Another sibling `Arc` for the same
    /// U-22 reason as `stats` — a config reload must not drop every session's
    /// affinity and overflow exclusions on the floor.
    pub sessions: Arc<SessionStore>,
    /// §3.2: policy per routable model id, chosen once at startup rather than per
    /// request. A model with no entry gets `static_policy`.
    policies: HashMap<String, Box<dyn Policy>>,
    static_policy: StaticPolicy,
}

impl State {
    pub fn new(
        cfg: Config,
        env: Env,
        backends: HashMap<String, Backend>,
        auth_token: Option<String>,
        redact_errors: bool,
        stats: Arc<Stats>,
        sessions: Arc<SessionStore>,
    ) -> State {
        // §3.2: policy selection is a startup decision. `type = "signals"` builds
        // the quota-aware cascade; anything else is the §5 auto-router. Everything
        // that is not a router shares one StaticPolicy.
        let policies: HashMap<String, Box<dyn Policy>> = cfg
            .routers
            .iter()
            .map(|(name, r)| {
                let p: Box<dyn Policy> = if r.kind == "signals" {
                    Box::new(SignalPolicy {
                        router: name.clone(),
                    })
                } else {
                    Box::new(ClassifierPolicy {
                        router: name.clone(),
                    })
                };
                (name.clone(), p)
            })
            .collect();
        // §7.5 tiers 2 and 3 are startup facts, so seed them before serving.
        for (name, b) in &cfg.backends {
            if let Some(n) = b.quota_calls() {
                stats.set_limit_hint(name, Some(n));
            }
            if let Some(w) = b.quota_window_secs() {
                stats.set_quota_window(name, w);
            }
        }
        State {
            cfg,
            env,
            backends,
            auth_token,
            redact_errors,
            health: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            stats,
            sessions,
            policies,
            static_policy: StaticPolicy,
        }
    }

    pub fn policy_for(&self, model: &str) -> &dyn Policy {
        match self.policies.get(model) {
            Some(p) => p.as_ref(),
            None => &self.static_policy,
        }
    }

    /// §5.2: every configured backend and its cost class, so the stats report
    /// covers backends nothing has dispatched to yet. Sorted for stable output.
    fn backend_costs(&self) -> BTreeMap<String, &'static str> {
        self.cfg
            .backends
            .iter()
            .map(|(name, b)| (name.clone(), b.cost_class()))
            .collect()
    }

    #[cfg(test)]
    pub fn for_test(cfg: Config) -> State {
        let sessions = Arc::new(SessionStore::new(
            cfg.server.session_max,
            cfg.server.session_ttl_secs,
        ));
        State::new(
            cfg,
            Env::capture("process", "/bin/sh", 1),
            HashMap::new(),
            None,
            false,
            Arc::new(Stats::new()),
            sessions,
        )
    }
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

thread_local! {
    // U-10: per-request CORS decision, set once at the top of handle() (each
    // worker thread serves one request to completion before the next), so every
    // response builder can add the headers without threading them through every
    // call site. `enabled` = the server has cors_origins configured, so
    // `Vary: Origin` must go on EVERY response for cache correctness behind a
    // shared proxy — even when this request's Origin didn't match. `origin` = the
    // allowlisted Origin to echo via Access-Control-Allow-Origin, if it matched.
    static CORS: std::cell::RefCell<CorsState> =
        const { std::cell::RefCell::new(CorsState::DISABLED) };
}
struct CorsState {
    enabled: bool,
    origin: Option<String>,
}

impl CorsState {
    const DISABLED: CorsState = CorsState {
        enabled: false,
        origin: None,
    };
}

fn set_cors(enabled: bool, origin: Option<String>) {
    CORS.with(|c| *c.borrow_mut() = CorsState { enabled, origin });
}

/// U-10: when CORS is enabled, add `Vary: Origin` to every response (cache
/// correctness), plus `Access-Control-Allow-Origin` when this request's Origin
/// was allowlisted. A no-op when CORS is disabled.
fn apply_cors<R: Read>(resp: Response<R>) -> Response<R> {
    CORS.with(|c| {
        let s = c.borrow();
        if !s.enabled {
            return resp;
        }
        let resp = resp.with_header(header("Vary", "Origin"));
        match s.origin.as_deref() {
            Some(o) => resp.with_header(header("Access-Control-Allow-Origin", o)),
            None => resp,
        }
    })
}

fn respond_json(req: Request, status: u16, body: String) {
    respond_json_with(req, status, body, &[]);
}

/// `respond_json` plus caller-supplied headers, for the §5.1 provenance set.
fn respond_json_with(req: Request, status: u16, body: String, extra: &[(&'static str, String)]) {
    let mut resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_status_code(status);
    for (k, v) in extra {
        resp = resp.with_header(header(k, v));
    }
    let _ = req.respond(apply_cors(resp));
}

fn respond_err(req: Request, status: u16, msg: &str, etype: &str, code: Option<&str>) {
    respond_json(
        req,
        status,
        openai::ErrorEnvelope::new(msg, etype, code).to_json(),
    );
}

/// §9.1: the same error in whichever envelope the caller's endpoint speaks.
///
/// An OpenAI-shaped body coming back from `/v1/messages` is a contract break: an
/// Anthropic client parses `{"type":"error","error":{...}}` and will not find
/// anything where we put it.
fn respond_err_shaped(
    req: Request,
    anthropic_shape: bool,
    status: u16,
    msg: &str,
    etype: &str,
    code: Option<&str>,
) {
    if anthropic_shape {
        respond_json(
            req,
            status,
            anthropic::error_envelope(etype, msg).to_string(),
        );
    } else {
        respond_err(req, status, msg, etype, code);
    }
}

fn respond_head(req: Request) {
    // U-17: HEAD mirrors GET headers with an empty body.
    let resp =
        Response::empty(StatusCode(200)).with_header(header("Content-Type", "application/json"));
    let _ = req.respond(apply_cors(resp));
}

fn respond_405(req: Request, allow: &str) {
    // U-17: wrong method on a known path — the Allow header names the fix.
    let body = openai::ErrorEnvelope::new(
        format!("method not allowed on this path; use: {allow}"),
        "invalid_request_error",
        Some("method_not_allowed"),
    )
    .to_json();
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_header(header("Allow", allow))
        .with_status_code(405);
    let _ = req.respond(apply_cors(resp));
}

pub fn handle(mut req: Request, state: &Arc<State>) {
    let method = match req.method() {
        Method::Get => "GET",
        Method::Post => "POST",
        Method::Head => "HEAD",
        Method::Options => "OPTIONS",
        _ => "OTHER",
    };
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let auth_header = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str().to_string());
    // U-10: resolve the request Origin against the allowlist and record it for
    // this request's responses (apply_cors reads it via the thread-local).
    let origin = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_string());
    let allowed = origin.filter(|o| state.cfg.server.cors_origins.iter().any(|a| a == o));
    let cors_enabled = !state.cfg.server.cors_origins.is_empty();
    set_cors(cors_enabled, allowed.clone());
    if method == "OPTIONS" && cors_enabled {
        // CORS preflight: 204. apply_cors echoes the Origin (ACAO) and adds Vary;
        // the preflight-only headers (allowed methods/headers, max-age) go on when
        // the Origin matched the allowlist.
        let mut resp = Response::empty(StatusCode(204));
        if allowed.is_some() {
            resp = resp
                .with_header(header("Access-Control-Allow-Methods", "GET, POST, OPTIONS"))
                .with_header(header(
                    "Access-Control-Allow-Headers",
                    "authorization, content-type",
                ))
                .with_header(header("Access-Control-Max-Age", "600"));
        }
        let _ = req.respond(apply_cors(resp));
        return;
    }
    // §6.1: lowercase (name, value) pairs, so session key derivation does not
    // need the tiny_http types.
    let header_session = session::key_from_headers(
        &req.headers()
            .iter()
            .map(|h| {
                (
                    h.field.as_str().as_str().to_ascii_lowercase(),
                    h.value.as_str().to_string(),
                )
            })
            .collect::<Vec<_>>(),
    );
    let route = classify(method, &path);
    // S-1: gate protected routes; /health stays open for liveness probes.
    // /v1/stats and /metrics are gated too — they name backends and expose
    // traffic volume, which is not something to hand an unauthenticated caller
    // just because a scraper finds it convenient.
    if let Some(token) = &state.auth_token
        && matches!(
            route,
            Route::Models
                | Route::Chat
                | Route::Messages
                | Route::Backends
                | Route::Stats
                | Route::StatsReset
                | Route::Metrics
        )
        && !crate::auth::check_bearer(auth_header.as_deref(), token)
    {
        log::warn(&format!(
            "401 on {path}: token mismatch — the expected token is in ~/.tmuxlet/token (or the auth_token_env var)"
        ));
        return respond_err(
            req,
            401,
            "missing or invalid bearer token",
            "invalid_request_error",
            Some("unauthorized"),
        );
    }
    // U-17: HEAD carries no body — respond with headers only for GET-family routes.
    if method == "HEAD"
        && matches!(
            route,
            Route::Health | Route::Models | Route::Backends | Route::Stats
        )
    {
        return respond_head(req);
    }
    match route {
        Route::MethodNotAllowed(allow) => respond_405(req, allow),
        Route::Health => {
            let body = format!(
                "{{\"status\":\"ok\",\"version\":\"{}\",\"backends\":{},\"chains\":{}}}",
                env!("CARGO_PKG_VERSION"),
                state.cfg.backends.len(),
                state.cfg.chains.len()
            );
            respond_json(req, 200, body);
        }
        Route::Models => {
            // U-9: deterministic order — routers + chains first, then backends.
            let mut head_ids: Vec<String> = state
                .cfg
                .routers
                .keys()
                .chain(state.cfg.chains.keys())
                .cloned()
                .collect();
            head_ids.sort();
            let mut backend_ids: Vec<String> = state.cfg.backends.keys().cloned().collect();
            backend_ids.sort();
            let ids = head_ids.into_iter().chain(backend_ids);
            respond_json(
                req,
                200,
                serde_json::to_string(&openai::model_list(ids)).unwrap(),
            );
        }
        Route::Backends => respond_backends(req, state),
        Route::Stats => {
            let mut body = state.stats.to_json(&state.backend_costs());
            // §6.2: the session map is bounded, and an operator should be able to
            // see the bound holding rather than take it on faith. Not in §5.2 —
            // it did not exist when §5.2 was written.
            if let Some(obj) = body.as_object_mut() {
                obj.insert(
                    "sessions".into(),
                    serde_json::json!({
                        "entries": state.sessions.len(),
                        "max": state.cfg.server.session_max,
                        "ttl_secs": state.cfg.server.session_ttl_secs,
                    }),
                );
            }
            respond_json(req, 200, serde_json::to_string(&body).unwrap());
        }
        Route::StatsReset => {
            state.stats.reset();
            respond_json(req, 200, r#"{"reset":true}"#.to_string());
        }
        Route::Metrics => {
            if !state.cfg.server.metrics {
                // Off by default. A 404 rather than a 403 so a scraper pointed at
                // the wrong server sees "no such endpoint", which is true.
                return respond_err(
                    req,
                    404,
                    "metrics are disabled; set [server] metrics = true to enable /metrics",
                    "invalid_request_error",
                    Some("metrics_disabled"),
                );
            }
            let labels: Vec<&'static str> =
                DecisionSource::ALL.iter().map(|d| d.as_str()).collect();
            let body = state.stats.to_prometheus(&state.backend_costs(), &labels);
            let resp = Response::from_string(body)
                .with_header(header(
                    "Content-Type",
                    "text/plain; version=0.0.4; charset=utf-8",
                ))
                .with_status_code(200);
            let _ = req.respond(apply_cors(resp));
        }
        Route::Reserved => respond_err(
            req,
            501,
            "not implemented in V1",
            "server_error",
            Some("not_implemented"),
        ),
        Route::NotFound => respond_err(
            req,
            404,
            "no such route",
            "invalid_request_error",
            Some("unknown_route"),
        ),
        Route::Chat | Route::Messages => {
            let anthropic_shape = matches!(route, Route::Messages);
            // Cap the body read so a malformed/huge payload can't exhaust memory.
            const MAX_BODY: u64 = 16 * 1024 * 1024; // 16 MiB
            let mut raw = String::new();
            if req
                .as_reader()
                .take(MAX_BODY + 1)
                .read_to_string(&mut raw)
                .is_err()
            {
                return respond_err(
                    req,
                    400,
                    "could not read body",
                    "invalid_request_error",
                    Some("read_error"),
                );
            }
            if raw.len() as u64 > MAX_BODY {
                return respond_err(
                    req,
                    413,
                    "request body too large",
                    "invalid_request_error",
                    Some("payload_too_large"),
                );
            }
            // §6.1: the id has to come off the raw request, so it is read here
            // and handed down — `handle_chat` has already given the Request away
            // by the time it needs it.
            handle_chat(req, &raw, state, header_session, anthropic_shape);
        }
    }
}

/// U-13: backend status from the P-9 health records.
fn respond_backends(req: Request, state: &Arc<State>) {
    let mut names: Vec<&String> = state.cfg.backends.keys().collect();
    names.sort();
    // Snapshot the in-flight counts (U-20) before taking the health lock, so the
    // two leaf locks are never held at once.
    let active: HashMap<String, usize> = state.active.lock().unwrap().clone();
    let health = state.health.lock().unwrap();
    let now = Instant::now();
    let entries: Vec<serde_json::Value> = names
        .iter()
        .map(|name| {
            let h = health.get(*name);
            let cooling = h.and_then(|x| x.cooling_until).filter(|u| *u > now);
            // U-20/B4: report "busy" when the backend is at its concurrency cap
            // right now. (Busy is deliberately excluded from P-9 cooldown — a
            // saturated backend recovers the instant a slot frees.)
            let busy = state
                .backends
                .get(*name)
                .and_then(|b| b.max_concurrent())
                .zip(active.get(*name))
                .is_some_and(|(max, count)| *count >= max);
            let st = if cooling.is_some() {
                "cooling"
            } else if busy {
                "busy"
            } else if h.is_some() {
                "ok"
            } else {
                "unknown"
            };
            serde_json::json!({
                "name": name,
                "type": state.cfg.backends[*name].type_str(),
                "state": st,
                "consecutive_failures": h.map(|x| x.consecutive_failures).unwrap_or(0),
                "last_error": h.and_then(|x| x.last_error.clone()),
                "last_latency_ms": h.and_then(|x| x.last_latency_ms),
                "cooling_secs": cooling.map(|u| u.duration_since(now).as_secs()),
            })
        })
        .collect();
    drop(health);
    respond_json(req, 200, serde_json::to_string(&entries).unwrap());
}

/// B3/U-8: one closing info line per chat request — success OR total failure.
/// status: ok = first candidate answered, fallback = a later one did, fail = none.
fn log_request_summary(
    reqid: &str,
    model: &str,
    route_label: Option<&str>,
    elapsed_ms: u64,
    winner: Option<&str>,
    first: Option<&str>,
) {
    let status = if winner.is_none() {
        "fail"
    } else if winner == first {
        "ok"
    } else {
        "fallback"
    };
    log::info(&format!(
        "{reqid} model={model} route={} elapsed_ms={elapsed_ms} backend={} status={status}",
        route_label.unwrap_or("-"),
        winner.unwrap_or("-"),
    ));
}

/// §5.1: provenance on every response, router-driven or not. V1 emitted
/// `x-tmuxlet-route` only on the router path, so a plain chain request carried no
/// clue about which backend had answered.
/// `winner` is `"-"` when no leg answered — the same convention the closing log
/// line uses, so `x-tmuxlet-route` keeps its shape on a 503 or an all-overflow
/// 400 instead of vanishing exactly when someone is trying to debug it.
fn decision_headers(
    decision: &Decision,
    model: &str,
    winner: &str,
    redact: bool,
) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("x-tmuxlet-route", decision.route_header(model, winner)),
        ("x-tmuxlet-backend", crate::route::header_safe(winner)),
        ("x-tmuxlet-decision", decision.source.as_str().to_string()),
    ];
    if let Some(r) = decision.rationale_header(redact) {
        h.push(("x-tmuxlet-rationale", r));
    }
    h
}

fn handle_chat(
    req: Request,
    raw: &str,
    state: &Arc<State>,
    header_session: Option<SessionKey>,
    anthropic_shape: bool,
) {
    let reqid = log::next_reqid();
    let req_start = Instant::now();
    // §9.1: an Anthropic request is normalized to the OpenAI shape right here, so
    // routing, the classifier, signal extraction, prompt shaping, and the `api`
    // backends downstream all see exactly one wire format. Only the *response*
    // envelope differs, which is why the translation is ~250 LOC rather than
    // Switchyard's 11.5k-line crate: we refuse tool-call round-tripping.
    let normalized;
    let raw = if anthropic_shape {
        match normalize_anthropic(raw) {
            Ok(v) => {
                normalized = v;
                normalized.as_str()
            }
            Err((status, body)) => return respond_json(req, status, body),
        }
    } else {
        raw
    };
    // P-2: parse the body once to a Value, then deserialize the typed view.
    let raw_value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return respond_err_shaped(
                req,
                anthropic_shape,
                400,
                &format!("parse error: {e}"),
                "invalid_request_error",
                Some("parse_error"),
            );
        }
    };
    let parsed: openai::ChatRequest = match openai::ChatRequest::deserialize(&raw_value) {
        Ok(p) => p,
        Err(e) => {
            return respond_err_shaped(
                req,
                anthropic_shape,
                400,
                &format!("parse error: {e}"),
                "invalid_request_error",
                Some("parse_error"),
            );
        }
    };
    if parsed.messages.is_empty() {
        return respond_err_shaped(
            req,
            anthropic_shape,
            400,
            "messages must not be empty",
            "invalid_request_error",
            Some("missing_messages"),
        );
    }

    // ---- The decision layer (§3) ----
    let model = parsed.model.clone();
    // §6.1: a header id wins. The message-hash fallback is per-router and opt-in,
    // because it collides across independent conversations that open with the
    // same prompt — templated agent harnesses do exactly that.
    let sess = header_session.or_else(|| {
        state
            .cfg
            .routers
            .get(&model)
            .filter(|r| r.message_hash_fallback)
            .and_then(|_| session::key_from_messages(&parsed.messages))
    });
    let ctx = RequestCtx {
        model: &model,
        messages: &parsed.messages,
        raw: &raw_value,
        cfg: &state.cfg,
        reqid: &reqid,
        session: sess.as_ref(),
    };
    let decision = match state.policy_for(&model).decide(&ctx, state) {
        Ok(d) => d,
        Err(e) => {
            // No Decision exists here — routing itself failed — so there is no
            // provenance to report beyond the error envelope's own code.
            state.stats.record_request(false);
            return respond_err_shaped(
                req,
                anthropic_shape,
                e.status,
                &e.message,
                e.etype,
                Some(e.code),
            );
        }
    };
    if decision.source == DecisionSource::Default {
        log::warn(&format!("{reqid} unknown model '{model}' → default chain"));
    }
    state.stats.record_decision(decision.source.as_str());

    // §8: the gate belongs to the chain the decision landed on, not to the model
    // the client asked for — a router's classes can point at chains with
    // different gates.
    let gate_cfg = decision
        .chain
        .as_ref()
        .and_then(|c| state.cfg.chains.get(c))
        .and_then(|c| c.gate.as_ref());

    let prompt = openai::flatten_to_prompt(&parsed.messages);
    let last_user = openai::last_user_prompt(&parsed.messages); // U-14
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    if parsed.stream {
        stream_with_keepalive(
            req,
            state,
            decision,
            prompt,
            last_user,
            raw_value,
            default_to,
            reqid,
            req_start,
            model,
            sess,
            anthropic_shape,
        );
        return;
    }

    let names_ref: Vec<&str> = decision.legs.iter().map(|s| s.as_str()).collect();
    let first = decision.legs.first().map(|s| s.as_str());
    match chain::run(
        state,
        &ChainCtx {
            legs: &names_ref,
            prompt: &prompt,
            last_user: &last_user,
            raw: &raw_value,
            default_to,
            reqid: &reqid,
            session: sess.as_ref(),
            gate: gate_cfg,
        },
    ) {
        Ok((result, winner)) => {
            state.stats.record_request(true);
            log_request_summary(
                &reqid,
                &model,
                decision.label.as_deref(),
                req_start.elapsed().as_millis() as u64,
                Some(&winner),
                first,
            );
            let body = if anthropic_shape {
                anthropic::build_message(
                    format!("msg_{}", result.model_label),
                    result.model_label,
                    result.content,
                )
                .to_string()
            } else {
                serde_json::to_string(&openai::build_completion(
                    format!("chatcmpl-{}", result.model_label),
                    result.model_label,
                    result.content,
                ))
                .unwrap()
            };
            let mut resp = Response::from_string(body)
                .with_header(header("Content-Type", "application/json"))
                .with_status_code(StatusCode(200));
            for (k, v) in decision_headers(&decision, &model, &winner, state.redact_errors) {
                resp = resp.with_header(header(k, &v));
            }
            let _ = req.respond(apply_cors(resp));
        }
        Err(errors) => {
            state.stats.record_request(false);
            log_request_summary(
                &reqid,
                &model,
                decision.label.as_deref(),
                req_start.elapsed().as_millis() as u64,
                None,
                first,
            );
            let (status, body) = chain_error_response(&errors, state.redact_errors);
            let body = if anthropic_shape {
                anthropic::error_envelope("api_error", &chain_error_message(&errors)).to_string()
            } else {
                body
            };
            respond_json_with(
                req,
                status,
                body,
                &decision_headers(&decision, &model, "-", state.redact_errors),
            );
        }
    }
}

/// §9.1: turn an Anthropic Messages body into the OpenAI body the rest of the
/// server speaks. `Err` carries a ready-to-send `(status, body)` in Anthropic's
/// own error envelope — a client on this endpoint should not get an OpenAI-shaped
/// error back.
fn normalize_anthropic(raw: &str) -> Result<String, (u16, String)> {
    let parsed: anthropic::MessagesRequest = serde_json::from_str(raw).map_err(|e| {
        (
            400,
            anthropic::error_envelope("invalid_request_error", &format!("parse error: {e}"))
                .to_string(),
        )
    })?;
    // Refuse loudly rather than silently returning prose where the caller
    // expected a tool_use block. Silent wrongness is the failure mode worth
    // refusing; the README states the same ceiling.
    if parsed.has_tools() {
        return Err((
            400,
            anthropic::error_envelope(
                "invalid_request_error",
                "this endpoint is text-only: `tools` is not supported and no tool_use blocks will be returned. Use /v1/chat/completions with a backend that speaks tools.",
            )
            .to_string(),
        ));
    }
    Ok(serde_json::json!({
        "model": parsed.model(),
        "stream": parsed.stream(),
        "messages": openai::messages_to_json(&parsed.to_openai_messages()),
    })
    .to_string())
}

/// A3: run the streaming dispatch, converting a panic into an in-band chain
/// error. Without this, a panic on the dispatch thread drops `rtx` silently and
/// the coordinator ends the already-committed 200 SSE stream at clean EOF — no
/// error, no `data: [DONE]`, indistinguishable from an empty success. The server
/// is built to survive dispatch panics (F-3), so map them to the same 503 body
/// the non-streaming path returns.
fn dispatch_or_panic_error<T, F>(reqid: &str, f: F) -> Result<T, Vec<BackendError>>
where
    F: FnOnce() -> Result<T, Vec<BackendError>>,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => {
            log::error(&format!(
                "{reqid} streaming dispatch panicked; emitting in-band 503"
            ));
            Err(vec![BackendError::Backend(
                "chain".to_string(),
                "internal panic during dispatch".to_string(),
            )])
        }
    }
}

/// U-2: stream `: keepalive` comment frames every 15s while the chain runs on a
/// separate thread, then the final completion (or an in-band error) frames.
#[allow(clippy::too_many_arguments)]
fn stream_with_keepalive(
    req: Request,
    state: &Arc<State>,
    decision: Decision,
    prompt: String,
    last_user: String,
    raw_value: serde_json::Value,
    default_to: Duration,
    reqid: String,
    req_start: Instant,
    model: String,
    session: Option<SessionKey>,
    anthropic_shape: bool,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (rtx, rrx) = mpsc::channel::<Result<StreamOutcome, Vec<BackendError>>>();
    let redact = state.redact_errors;
    let state_dispatch = Arc::clone(state);
    let stats = Arc::clone(&state.stats);
    let reqid2 = reqid.clone();
    let names = decision.legs.clone();
    let first = names.first().cloned();
    // The dispatch thread holds its own Arc<State>, so re-resolve the gate there
    // rather than smuggling a borrow of `state.cfg` across the spawn.
    let gate_chain = decision.chain.clone();
    let model2 = model.clone();
    // §9.2: the dispatch thread relays deltas straight onto the SSE channel as
    // they arrive. `first` flips on the first token — the commit point — and the
    // role-priming frame goes out with it so the client sees a well-formed
    // OpenAI stream rather than bare content deltas.
    // §9.1: an Anthropic request never relays live. Its event sequence differs
    // from OpenAI's and cannot be re-shaped from a relayed chunk after the fact,
    // so the whole reply is buffered and emitted as Anthropic events at the end.
    // Without this a client got OpenAI `chat.completion.chunk` frames live and
    // *then* the Anthropic events — two protocols in one stream.
    let relay_live = !anthropic_shape;
    let tx_deltas = tx.clone();
    // One id and one model for every frame of a relayed stream. The backend's own
    // model_label is not known until the first token arrives, and switching it
    // mid-stream would break any client that keys on `id`.
    let stream_id = format!("chatcmpl-{model}");
    let tail_id = stream_id.clone();
    let tail_model = model.clone();
    std::thread::spawn(move || {
        let names_ref: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let mut primed = false;
        let r = dispatch_or_panic_error(&reqid2, || {
            let mut on_delta = |d: &str| {
                if !relay_live {
                    // Buffered: dispatch_stream still accumulates the content, so
                    // the reply is complete — it just is not relayed.
                    return true;
                }
                if !primed {
                    primed = true;
                    if tx_deltas
                        .send(openai::stream_role_frame(&stream_id, &model2).into_bytes())
                        .is_err()
                    {
                        return false;
                    }
                }
                tx_deltas
                    .send(openai::stream_delta_frame(&stream_id, &model2, d).into_bytes())
                    .is_ok()
            };
            chain::run_streaming(
                &state_dispatch,
                &ChainCtx {
                    legs: &names_ref,
                    prompt: &prompt,
                    last_user: &last_user,
                    raw: &raw_value,
                    default_to,
                    reqid: &reqid2,
                    session: session.as_ref(),
                    gate: gate_chain
                        .as_ref()
                        .and_then(|c| state_dispatch.cfg.chains.get(c))
                        .and_then(|c| c.gate.as_ref()),
                },
                &mut on_delta,
            )
        });
        let _ = rtx.send(r);
    });
    let route_for_log = decision.label.clone();
    // §5.1: the route header value is built here, before `model` moves into the
    // coordinator thread. The winner is deliberately absent — see below.
    let route_header = decision.route_header_prefix(&model);
    std::thread::spawn(move || {
        let final_result = loop {
            match rrx.recv_timeout(Duration::from_secs(15)) {
                Ok(r) => break r,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if tx.send(b": keepalive\n\n".to_vec()).is_err() {
                        // Client gone mid-stream. §5.2: the decision was already
                        // counted, so returning without this would leave
                        // `requests.total` permanently short of
                        // `sum(decisions)` — a drift an operator would read as a
                        // counter bug rather than as cancelled streams.
                        stats.record_request(false);
                        log_request_summary(
                            &reqid,
                            &model,
                            route_for_log.as_deref(),
                            req_start.elapsed().as_millis() as u64,
                            None,
                            first.as_deref(),
                        );
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // The dispatch thread dropped its sender without a result.
                    // dispatch_or_panic_error (A3) makes this unreachable today;
                    // count it rather than silently losing the request.
                    stats.record_request(false);
                    return;
                }
            }
        };
        let elapsed_ms = req_start.elapsed().as_millis() as u64;
        match final_result {
            Ok(StreamOutcome {
                result,
                winner,
                streamed,
            }) => {
                stats.record_request(true);
                log_request_summary(
                    &reqid,
                    &model,
                    route_for_log.as_deref(),
                    elapsed_ms,
                    Some(&winner),
                    first.as_deref(),
                );
                let id = format!("chatcmpl-{}", result.model_label);
                if anthropic_shape {
                    // §9.1: Anthropic's event sequence differs from OpenAI's and
                    // its frames carry an `event:` line as well as `data:`, so a
                    // relayed OpenAI stream cannot be re-shaped after the fact.
                    // The Anthropic surface therefore always buffers.
                    for f in anthropic::stream_events(
                        &format!("msg_{}", result.model_label),
                        &result.model_label,
                        &result.content,
                    ) {
                        if tx.send(f.into_bytes()).is_err() {
                            return;
                        }
                    }
                } else if streamed {
                    // The deltas already went out live; only the tail is left, and
                    // it must carry the SAME id and model that they did.
                    for f in openai::stream_tail_frames(&tail_id, &tail_model) {
                        if tx.send(f.into_bytes()).is_err() {
                            return;
                        }
                    }
                } else {
                    // A blocking backend (tmuxlet/cli, permanently — the child
                    // emits at exit) still emits the whole reply at once.
                    for f in openai::stream_frames(&id, &result.model_label, &result.content) {
                        if tx.send(f.into_bytes()).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(errors) => {
                stats.record_request(false);
                log_request_summary(
                    &reqid,
                    &model,
                    route_for_log.as_deref(),
                    elapsed_ms,
                    None,
                    first.as_deref(),
                );
                // §4.3/R5: the transport status is already committed to 200 — the
                // SSE headers went out before the chain ran — so an all-overflow
                // chain cannot answer 400 here. Take the helper's *body*, which
                // carries code: "context_length_exceeded", and drop its status.
                let (_status, body) = chain_error_response(&errors, redact);
                if anthropic_shape {
                    // Anthropic signals an in-band failure with an `error` event
                    // and has no `[DONE]` sentinel; emitting OpenAI's would leave
                    // the client waiting for a stop event that never arrives.
                    let _ = tx.send(
                        format!(
                            "event: error\ndata: {}\n\n",
                            anthropic::error_envelope("api_error", &chain_error_message(&errors))
                        )
                        .into_bytes(),
                    );
                } else {
                    let _ = tx.send(format!("data: {body}\n\n").into_bytes());
                    let _ = tx.send(b"data: [DONE]\n\n".to_vec());
                }
            }
        }
    });
    let mut headers = vec![
        header("Content-Type", "text/event-stream"),
        header("Cache-Control", "no-cache"),
    ];
    // §5.1: the streaming path can emit provenance for the *decision* but not for
    // the winner — these headers are written before the chain runs, and naming a
    // leg that might yet fall through would be a lie. `x-tmuxlet-backend` is
    // therefore non-streaming only; the winner is in the closing log line.
    headers.push(header("x-tmuxlet-route", &route_header));
    headers.push(header("x-tmuxlet-decision", decision.source.as_str()));
    if let Some(r) = decision.rationale_header(redact) {
        headers.push(header("x-tmuxlet-rationale", &r));
    }
    let resp = Response::new(
        StatusCode(200),
        headers,
        ChannelSseBody {
            rx,
            current: Vec::new(),
            pos: 0,
        },
        None,
        None,
    );
    let _ = req.respond(apply_cors(resp));
}

/// U-23's compact summary, on its own so the Anthropic envelope can carry the
/// same text without inheriting the OpenAI shape around it.
fn chain_error_message(errors: &[BackendError]) -> String {
    let summary = errors
        .iter()
        .map(|e| format!("{} ({})", e.backend_name(), e.class()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("all backends failed: {summary}")
}

fn build_503_body(errors: &[BackendError], redact: bool) -> String {
    // U-23: compact summary in `message`; full per-leg strings in details unless
    // redacted (S-6).
    let summary = errors
        .iter()
        .map(|e| format!("{} ({})", e.backend_name(), e.class()))
        .collect::<Vec<_>>()
        .join(", ");
    let details: Vec<String> = if redact {
        errors
            .iter()
            .map(|e| format!("{} ({})", e.backend_name(), e.class()))
            .collect()
    } else {
        errors.iter().map(|e| e.to_string()).collect()
    };
    openai::ErrorEnvelope::with_details(
        format!("all backends failed: {summary}"),
        "server_error",
        Some("all_backends_failed"),
        details,
    )
    .to_json()
}

/// §4.3: every leg reporting a context overflow means the client sent a request
/// that nothing in the chain can serve — a 400, not the generic 503 that invites
/// a pointless retry.
///
/// The rule is deliberately strict (plan O1). `run_chain` mixes real dispatch
/// errors with synthetic skip markers (`"skipped: cooling Ns"`, `"skipped:
/// budget"`) and `Busy` into one error vec, so a chain where one leg overflowed
/// while another was merely benched still returns 503 — a retry in 60s would in
/// fact work there, and a 400 would wrongly tell the client not to bother.
fn all_overflow(errors: &[BackendError]) -> bool {
    !errors.is_empty()
        && errors
            .iter()
            .all(|e| matches!(e, BackendError::Overflow(..)))
}

/// The status and body for a chain that produced no winner.
fn chain_error_response(errors: &[BackendError], redact: bool) -> (u16, String) {
    if all_overflow(errors) {
        (400, build_overflow_body(errors, redact))
    } else {
        (503, build_503_body(errors, redact))
    }
}

fn build_overflow_body(errors: &[BackendError], redact: bool) -> String {
    // Same redaction discipline as build_503_body (S-6): full per-leg strings
    // normally, name+class only when bound non-loopback.
    let summary = errors
        .iter()
        .map(|e| format!("{} ({})", e.backend_name(), e.class()))
        .collect::<Vec<_>>()
        .join(", ");
    let details: Vec<String> = if redact {
        errors
            .iter()
            .map(|e| format!("{} ({})", e.backend_name(), e.class()))
            .collect()
    } else {
        errors.iter().map(|e| e.to_string()).collect()
    };
    openai::ErrorEnvelope::with_details(
        format!("prompt exceeds the context window of every backend in the chain: {summary}"),
        "invalid_request_error",
        Some("context_length_exceeded"),
        details,
    )
    .to_json()
}

/// F-3: run one request handler, recovering from a panic so the worker loop
/// survives (a panic that escaped would silently shrink capacity). Returns true
/// if the handler panicked and was recovered, false on clean completion.
fn handle_recovering<F: FnOnce()>(f: F) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(()) => false,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            log::error(&format!("worker recovered from panic: {msg}"));
            true
        }
    }
}

/// U-22: the live `State`, swappable under SIGHUP.
///
/// Workers read the current `Arc<State>` once per request and then let go of the
/// lock, so a reload never blocks a dispatch and **in-flight requests finish on
/// the config they started with**. The read is a pointer clone against a turn
/// that runs for minutes; it does not show up.
pub type StateHolder = Arc<std::sync::RwLock<Arc<State>>>;

pub fn serve(server: Arc<Server>, holder: StateHolder, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let holder = Arc::clone(&holder);
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                // Snapshot the current config for this request, then release the
                // lock immediately — a reload mid-turn must not stall a worker.
                let st = match holder.read() {
                    Ok(g) => Arc::clone(&g),
                    // A poisoned holder means a reload panicked mid-swap. Routing
                    // must not stop; the value itself is a whole Arc<State>, so
                    // there is no torn state to observe.
                    Err(e) => Arc::clone(&e.into_inner()),
                };
                // F-3: a panic while handling one request must not tear down the
                // worker (which would silently shrink capacity).
                handle_recovering(|| handle(req, &st));
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_routes() {
        assert!(matches!(classify("GET", "/health"), Route::Health));
        assert!(matches!(classify("GET", "/v1/models"), Route::Models));
        assert!(matches!(classify("GET", "/v1/backends"), Route::Backends));
        assert!(matches!(
            classify("POST", "/v1/chat/completions"),
            Route::Chat
        ));
        // §5.2 / §5.3: the observability surface.
        assert!(matches!(classify("GET", "/v1/stats"), Route::Stats));
        assert!(matches!(
            classify("POST", "/v1/stats/reset"),
            Route::StatsReset
        ));
        assert!(matches!(classify("GET", "/metrics"), Route::Metrics));
        assert!(matches!(classify("GET", "/ui/index.html"), Route::Reserved));
        assert!(matches!(classify("GET", "/"), Route::Reserved));
        assert!(matches!(classify("GET", "/nope"), Route::NotFound));
        // U-17: HEAD mirrors GET routing.
        assert!(matches!(classify("HEAD", "/health"), Route::Health));
        assert!(matches!(classify("HEAD", "/v1/models"), Route::Models));
        // U-17: known path, wrong method → 405 with the right Allow value.
        assert!(matches!(
            classify("DELETE", "/v1/models"),
            Route::MethodNotAllowed("GET, HEAD")
        ));
        assert!(matches!(
            classify("GET", "/v1/chat/completions"),
            Route::MethodNotAllowed("POST")
        ));
        assert!(matches!(
            classify("GET", "/v1/stats/reset"),
            Route::MethodNotAllowed("POST")
        ));
        assert!(matches!(
            classify("POST", "/metrics"),
            Route::MethodNotAllowed("GET, HEAD")
        ));
    }

    #[test]
    fn channel_sse_body_streams_then_eof_on_drop() {
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        tx.send(b": keepalive\n\n".to_vec()).unwrap();
        tx.send(b"data: x\n\n".to_vec()).unwrap();
        drop(tx); // sender drop -> EOF
        let mut body = ChannelSseBody {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        let mut s = String::new();
        std::io::Read::read_to_string(&mut body, &mut s).unwrap();
        assert_eq!(s, ": keepalive\n\ndata: x\n\n");
    }

    #[test]
    fn dispatch_panic_becomes_in_band_503() {
        // A3: a panicking dispatch maps to an all_backends_failed body (which the
        // streaming coordinator then wraps with data: [DONE]) — never a silent
        // clean-EOF stream.
        let r = dispatch_or_panic_error::<(), _>("test", || panic!("boom"));
        let errs = r.expect_err("panic must become an error result");
        let body = build_503_body(&errs, false);
        assert!(
            body.contains("all_backends_failed"),
            "panic body must render as all_backends_failed: {body}"
        );
    }

    #[test]
    fn build_503_body_redacts_upstream_detail() {
        // S-6: redact=true drops the per-leg upstream detail but keeps the
        // name+class summary and the all_backends_failed code.
        let errs = vec![
            BackendError::Http("ollama".into(), 500, "secret upstream trace".into()),
            BackendError::Timeout("claude".into()),
        ];
        let redacted = build_503_body(&errs, true);
        assert!(redacted.contains("all_backends_failed"), "{redacted}");
        assert!(
            redacted.contains("ollama (http error)"),
            "redacted keeps name+class: {redacted}"
        );
        assert!(
            !redacted.contains("secret upstream trace"),
            "redacted must not leak upstream text: {redacted}"
        );
        let full = build_503_body(&errs, false);
        assert!(
            full.contains("secret upstream trace"),
            "unredacted keeps the detail: {full}"
        );
    }

    #[test]
    fn chain_error_response_is_400_only_when_every_leg_overflowed() {
        // The O1 rule in one place: strict all().
        let all = vec![
            BackendError::Overflow("a".into(), "maximum context length".into()),
            BackendError::Overflow("b".into(), "prompt is too long".into()),
        ];
        let (status, body) = chain_error_response(&all, false);
        assert_eq!(status, 400);
        assert!(body.contains("context_length_exceeded"), "{body}");
        assert!(body.contains("invalid_request_error"), "{body}");

        // One overflow plus a synthetic skip marker is still 503: a retry in 60s
        // would work, and a 400 would wrongly tell the client not to bother.
        let mixed = vec![
            BackendError::Overflow("a".into(), "maximum context length".into()),
            BackendError::Backend("b".into(), "skipped: cooling 42s".into()),
        ];
        assert_eq!(chain_error_response(&mixed, false).0, 503);
        assert_eq!(chain_error_response(&[], false).0, 503, "empty is not 400");
    }

    #[test]
    fn overflow_body_redacts_upstream_detail() {
        // S-6 discipline applies to the new 400 body exactly as to the 503 one.
        let errs = vec![BackendError::Overflow(
            "ollama".into(),
            "maximum context length is 8192, requested 41002".into(),
        )];
        let redacted = build_overflow_body(&errs, true);
        assert!(redacted.contains("ollama (context overflow)"), "{redacted}");
        assert!(
            !redacted.contains("41002"),
            "redacted must not leak: {redacted}"
        );
        assert!(build_overflow_body(&errs, false).contains("41002"));
    }

    #[test]
    fn worker_recovers_from_a_panicking_handler() {
        // F-3: a panicking handler is caught so the worker loop survives (an
        // escaped panic would permanently shrink capacity).
        assert!(
            handle_recovering(|| panic!("handler boom")),
            "a panic must be reported as recovered"
        );
        let mut ran = false;
        assert!(
            !handle_recovering(|| ran = true),
            "a clean handler reports no panic"
        );
        assert!(ran, "the clean handler must have executed");
    }

    #[test]
    fn a_router_model_gets_a_classifier_policy_and_everything_else_static() {
        // §3.2: policy selection is a startup decision, not a per-request branch.
        const C: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]

[routers.smart]
classifier = "echo"
fallback_class = "general"

[routers.smart.routes]
general = "default"
"#;
        let state = State::for_test(crate::config::parse(C).unwrap());
        assert!(state.policies.contains_key("smart"));
        assert!(!state.policies.contains_key("default"));
        // The fallback is shared, so an unknown model costs no allocation.
        assert!(std::ptr::eq(
            state.policy_for("anything-else") as *const dyn Policy as *const u8,
            &state.static_policy as *const StaticPolicy as *const u8
        ));
    }

    #[test]
    fn backend_costs_defaults_follow_the_backend_type() {
        // §7.3: cost is a property of the backend, and the defaults match how the
        // three types are actually used, so most configs need no edit at all.
        const C: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "local"
env_source = "process"

[backends.local]
type = "cli"
bin = "/bin/echo"

[backends.cloud]
type = "api"
base_url = "http://127.0.0.1:1/v1"
model = "m"

[backends.sub]
type = "tmuxlet"
target = "claude"
"#;
        let state = State::for_test(crate::config::parse(C).unwrap());
        let costs = state.backend_costs();
        assert_eq!(costs["local"], "local");
        assert_eq!(costs["cloud"], "metered");
        assert_eq!(costs["sub"], "subscription");
    }
}
