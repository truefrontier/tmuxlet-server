use crate::backend::{Backend, BackendError, DispatchResult};
use crate::config::Config;
use crate::env::Env;
use crate::{log, openai, router};
use serde::Deserialize;
use std::collections::HashMap;
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
        ("POST", "/v1/chat/completions") => Route::Chat,
        // U-17: known path, wrong method → 405 with an Allow header.
        (_, "/health") | (_, "/v1/models") | (_, "/v1/backends") => {
            Route::MethodNotAllowed("GET, HEAD")
        }
        (_, "/v1/chat/completions") => Route::MethodNotAllowed("POST"),
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

/// P-9 / U-13: per-backend health, updated after each dispatch.
#[derive(Default, Clone)]
pub struct BackendHealth {
    pub consecutive_failures: u32,
    pub cooling_until: Option<Instant>,
    pub last_error: Option<String>,
    pub last_latency_ms: Option<u64>,
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
}

/// U-20: RAII guard for a `state.active` concurrency slot. `try_acquire`
/// increments the in-flight count under the backend's cap (returning `None` when
/// already at the cap); `Drop` decrements it. Because the server is built to
/// survive dispatch panics (F-3 `catch_unwind` in `serve`), this guard is what
/// keeps a panic between acquire and release from leaking the slot forever — one
/// leak would brick a `max_concurrent = 1` backend until restart.
struct ActiveSlot<'a> {
    active: &'a Mutex<HashMap<String, usize>>,
    name: String,
}

impl<'a> ActiveSlot<'a> {
    /// Reserve one slot for `name` under `max`. `None` if already at capacity.
    fn try_acquire(
        active: &'a Mutex<HashMap<String, usize>>,
        name: &str,
        max: usize,
    ) -> Option<Self> {
        let mut counts = active.lock().unwrap();
        let count = counts.entry(name.to_string()).or_insert(0);
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(Self {
            active,
            name: name.to_string(),
        })
    }
}

impl Drop for ActiveSlot<'_> {
    fn drop(&mut self) {
        if let Ok(mut counts) = self.active.lock()
            && let Some(c) = counts.get_mut(&self.name)
        {
            *c = c.saturating_sub(1);
        }
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
    let resp = Response::from_string(body)
        .with_header(header("Content-Type", "application/json"))
        .with_status_code(status);
    let _ = req.respond(apply_cors(resp));
}

fn respond_err(req: Request, status: u16, msg: &str, etype: &str, code: Option<&str>) {
    respond_json(
        req,
        status,
        openai::ErrorEnvelope::new(msg, etype, code).to_json(),
    );
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
    let route = classify(method, &path);
    // S-1: gate protected routes; /health stays open for liveness probes.
    if let Some(token) = &state.auth_token
        && matches!(route, Route::Models | Route::Chat | Route::Backends)
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
    if method == "HEAD" && matches!(route, Route::Health | Route::Models | Route::Backends) {
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
        Route::Chat => {
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
            handle_chat(req, &raw, state);
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
fn handle_chat(req: Request, raw: &str, state: &Arc<State>) {
    let reqid = log::next_reqid();
    let req_start = Instant::now();
    // P-2: parse the body once to a Value, then deserialize the typed view.
    let raw_value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return respond_err(
                req,
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
            return respond_err(
                req,
                400,
                &format!("parse error: {e}"),
                "invalid_request_error",
                Some("parse_error"),
            );
        }
    };
    if parsed.messages.is_empty() {
        return respond_err(
            req,
            400,
            "messages must not be empty",
            "invalid_request_error",
            Some("missing_messages"),
        );
    }

    // ---- Routing (auto-router §5 / U-3 strict models) ----
    let model = parsed.model.clone();
    let (names_owned, route_label): (Vec<String>, Option<String>) = if let Some(router_cfg) =
        state.cfg.routers.get(&model)
    {
        let (class, source, classifier_ms) = classify_task(state, router_cfg, &parsed.messages);
        let target = router_cfg.routes[&class].clone();
        let names = match router::resolve(&target, &state.cfg) {
            Ok(n) => n.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            Err(e) => return respond_err(req, 500, &e, "server_error", Some("router_error")),
        };
        log::info(&format!(
            "{reqid} route {model} class={class} target={target} source={} classifier_ms={classifier_ms}",
            source.as_str()
        ));
        (names, Some(format!("{class}/{target}")))
    } else {
        let known =
            state.cfg.chains.contains_key(&model) || state.cfg.backends.contains_key(&model);
        if !known {
            if state.cfg.server.strict_models {
                return respond_err(
                    req,
                    404,
                    &format!(
                        "unknown model '{model}' — list valid ids at GET /v1/models, or set strict_models = false to fall back to default_chain"
                    ),
                    "invalid_request_error",
                    Some("model_not_found"),
                );
            }
            log::warn(&format!("{reqid} unknown model '{model}' → default chain"));
        }
        match router::resolve(&model, &state.cfg) {
            Ok(n) => (n.iter().map(|s| s.to_string()).collect(), None),
            Err(e) => return respond_err(req, 400, &e, "invalid_request_error", Some("bad_model")),
        }
    };

    let prompt = openai::flatten_to_prompt(&parsed.messages);
    let last_user = openai::last_user_prompt(&parsed.messages); // U-14
    let default_to = Duration::from_secs(state.cfg.server.request_timeout_secs);

    if parsed.stream {
        stream_with_keepalive(
            req,
            state,
            names_owned,
            prompt,
            last_user,
            raw_value,
            default_to,
            route_label,
            reqid,
            req_start,
            model,
        );
        return;
    }

    let names_ref: Vec<&str> = names_owned.iter().map(|s| s.as_str()).collect();
    let first = names_owned.first().map(|s| s.as_str());
    match run_chain(
        state, &names_ref, &prompt, &last_user, &raw_value, default_to, &reqid,
    ) {
        Ok((result, winner)) => {
            log_request_summary(
                &reqid,
                &model,
                route_label.as_deref(),
                req_start.elapsed().as_millis() as u64,
                Some(&winner),
                first,
            );
            let id = format!("chatcmpl-{}", result.model_label);
            let body = serde_json::to_string(&openai::build_completion(
                id,
                result.model_label,
                result.content,
            ))
            .unwrap();
            let mut resp = Response::from_string(body)
                .with_header(header("Content-Type", "application/json"))
                .with_status_code(StatusCode(200));
            if let Some(rl) = &route_label {
                // B1: append the winning backend — the full class/chain/backend.
                resp = resp.with_header(header("x-tmuxlet-route", &format!("{rl}/{winner}")));
            }
            let _ = req.respond(apply_cors(resp));
        }
        Err(errors) => {
            log_request_summary(
                &reqid,
                &model,
                route_label.as_deref(),
                req_start.elapsed().as_millis() as u64,
                None,
                first,
            );
            let (status, body) = chain_error_response(&errors, state.redact_errors);
            respond_json(req, status, body);
        }
    }
}

/// A3: run the streaming dispatch, converting a panic into an in-band chain
/// error. Without this, a panic on the dispatch thread drops `rtx` silently and
/// the coordinator ends the already-committed 200 SSE stream at clean EOF — no
/// error, no `data: [DONE]`, indistinguishable from an empty success. The server
/// is built to survive dispatch panics (F-3), so map them to the same 503 body
/// the non-streaming path returns.
fn dispatch_or_panic_error<F>(
    reqid: &str,
    f: F,
) -> Result<(DispatchResult, String), Vec<BackendError>>
where
    F: FnOnce() -> Result<(DispatchResult, String), Vec<BackendError>>,
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
    names: Vec<String>,
    prompt: String,
    last_user: String,
    raw_value: serde_json::Value,
    default_to: Duration,
    route_label: Option<String>,
    reqid: String,
    req_start: Instant,
    model: String,
) {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let (rtx, rrx) = mpsc::channel::<Result<(DispatchResult, String), Vec<BackendError>>>();
    let redact = state.redact_errors;
    let state_dispatch = Arc::clone(state);
    let reqid2 = reqid.clone();
    let first = names.first().cloned();
    std::thread::spawn(move || {
        let names_ref: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let r = dispatch_or_panic_error(&reqid2, || {
            run_chain(
                &state_dispatch,
                &names_ref,
                &prompt,
                &last_user,
                &raw_value,
                default_to,
                &reqid2,
            )
        });
        let _ = rtx.send(r);
    });
    let route_for_log = route_label.clone();
    std::thread::spawn(move || {
        let final_result = loop {
            match rrx.recv_timeout(Duration::from_secs(15)) {
                Ok(r) => break r,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if tx.send(b": keepalive\n\n".to_vec()).is_err() {
                        return; // client gone
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        let elapsed_ms = req_start.elapsed().as_millis() as u64;
        match final_result {
            Ok((result, winner)) => {
                log_request_summary(
                    &reqid,
                    &model,
                    route_for_log.as_deref(),
                    elapsed_ms,
                    Some(&winner),
                    first.as_deref(),
                );
                let id = format!("chatcmpl-{}", result.model_label);
                for f in openai::stream_frames(&id, &result.model_label, &result.content) {
                    if tx.send(f.into_bytes()).is_err() {
                        return;
                    }
                }
            }
            Err(errors) => {
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
                let _ = tx.send(format!("data: {body}\n\n").into_bytes());
                let _ = tx.send(b"data: [DONE]\n\n".to_vec());
            }
        }
    });
    let mut headers = vec![
        header("Content-Type", "text/event-stream"),
        header("Cache-Control", "no-cache"),
    ];
    if let Some(rl) = &route_label {
        headers.push(header("x-tmuxlet-route", rl));
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

/// Walk the chain with P-9 cooldown, P-6 budget, and U-20 concurrency. Returns
/// the winning result or every leg's error.
fn run_chain(
    state: &State,
    names: &[&str],
    prompt: &str,
    last_user: &str,
    raw_value: &serde_json::Value,
    default_to: Duration,
    reqid: &str,
) -> Result<(DispatchResult, String), Vec<BackendError>> {
    let cooldown_on = state.cfg.server.cooldown;
    let start = Instant::now();
    let budget_deadline = state
        .cfg
        .server
        .chain_budget_secs
        // checked_add: an absurd budget would overflow Instant; treat as unbounded.
        .and_then(|b| start.checked_add(Duration::from_secs(b)));

    // P-9: snapshot cooling status per candidate.
    let cooling: Vec<Option<Duration>> = names
        .iter()
        .map(|&n| {
            if !cooldown_on {
                return None;
            }
            let h = state.health.lock().unwrap();
            h.get(n)
                .and_then(|hh| hh.cooling_until)
                .and_then(|u| u.checked_duration_since(start))
        })
        .collect();

    let mut errors: Vec<BackendError> = Vec::new();
    for (i, &name) in names.iter().enumerate() {
        // P-6: budget exhausted — skip the rest.
        if let Some(dl) = budget_deadline
            && Instant::now() >= dl
        {
            errors.push(BackendError::Backend(
                name.to_string(),
                "skipped: budget".into(),
            ));
            log::debug(&format!("{reqid} skip backend={name}: budget exhausted"));
            continue;
        }
        // P-9: skip a cooling leg only if a non-cooling leg remains ahead.
        if let Some(remaining) = cooling[i] {
            let non_cooling_ahead = cooling[i + 1..].iter().any(|c| c.is_none());
            if non_cooling_ahead {
                let secs = remaining.as_secs();
                errors.push(BackendError::Backend(
                    name.to_string(),
                    format!("skipped: cooling {secs}s"),
                ));
                log::debug(&format!("{reqid} skip backend={name}: cooling {secs}s"));
                continue;
            }
        }
        let backend = &state.backends[name];
        // U-20: reserve a concurrency slot (RAII: released on Drop, even on a
        // dispatch panic). At the cap this leg is Busy and the chain advances.
        let _slot = match backend.max_concurrent() {
            Some(max) => match ActiveSlot::try_acquire(&state.active, name, max) {
                Some(slot) => Some(slot),
                None => {
                    errors.push(BackendError::Busy(name.to_string()));
                    continue;
                }
            },
            None => None,
        };
        // P-6: clamp the leg timeout to the remaining budget.
        let leg_to = backend.timeout_override().unwrap_or(default_to);
        let timeout = match budget_deadline {
            Some(dl) => dl.saturating_duration_since(Instant::now()).min(leg_to),
            None => leg_to,
        };
        let leg_start = Instant::now();
        let leg_prompt = match backend.prompt_mode() {
            openai::PromptMode::LastUser => last_user,
            openai::PromptMode::Transcript => prompt,
        };
        let result = backend.dispatch(leg_prompt, raw_value, &state.env, timeout);
        let elapsed_ms = leg_start.elapsed().as_millis() as u64;
        match result {
            Ok(res) => {
                if res.content.trim().is_empty() && !backend.allow_empty() {
                    let e = BackendError::Backend(name.to_string(), "empty output".into());
                    record_failure(state, name, &e, elapsed_ms);
                    log::warn(&format!("{reqid} skip backend={name}: empty output"));
                    errors.push(e);
                    continue;
                }
                record_success(state, name, elapsed_ms);
                log::info(&format!(
                    "{reqid} ok backend={name} elapsed_ms={elapsed_ms}"
                ));
                return Ok((res, name.to_string()));
            }
            Err(e) => {
                log::warn(&format!("{reqid} skip backend={name}: {e}"));
                record_failure(state, name, &e, elapsed_ms);
                errors.push(e);
            }
        }
    }
    Err(errors)
}

fn record_success(state: &State, name: &str, latency_ms: u64) {
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.consecutive_failures = 0;
    e.cooling_until = None;
    e.last_error = None;
    e.last_latency_ms = Some(latency_ms);
}

fn record_failure(state: &State, name: &str, err: &BackendError, latency_ms: u64) {
    // §4.3: a context overflow says nothing about the backend's health — the
    // backend is fine, the prompt does not fit. Keep it out of the cooldown
    // bookkeeping entirely, or one long conversation benches a 32k local model
    // for every short request that follows.
    if let BackendError::Overflow(..) = err {
        return record_overflow(state, name, err, latency_ms);
    }
    // A4: mutate health under the lock, then RELEASE it before logging. log::line
    // can block on a stalled stderr pipe; blocking (or, before the log.rs fix,
    // panicking on EPIPE) while holding state.health would stall or poison the
    // mutex and brick every later request.
    let cooldown_log = {
        let mut h = state.health.lock().unwrap();
        let e = h.entry(name.to_string()).or_default();
        e.consecutive_failures = e.consecutive_failures.saturating_add(1);
        e.last_error = Some(err.to_string());
        e.last_latency_ms = Some(latency_ms);
        if state.cfg.server.cooldown {
            let cd = cooldown_for(err, e.consecutive_failures, state.cfg.server.cooldown_secs);
            // checked_add: an absurd cooldown_secs would overflow Instant.
            e.cooling_until = Instant::now().checked_add(cd);
            Some((cd.as_secs(), err.class()))
        } else {
            None
        }
    };
    if let Some((secs, class)) = cooldown_log {
        log::info(&format!("cooldown {name} for {secs}s ({class})"));
    }
}

/// §4.3: record a context overflow without letting it read as ill-health.
///
/// `last_error`/`last_latency_ms` still move, so `/v1/backends` can explain why a
/// leg was skipped. `consecutive_failures` is neither incremented **nor reset**
/// (plan O4): an overflow is no more evidence of health than of ill-health, and
/// zeroing the counter would let a chatty client launder a genuinely failing
/// backend back into rotation. No cooldown is applied.
fn record_overflow(state: &State, name: &str, err: &BackendError, latency_ms: u64) {
    let mut h = state.health.lock().unwrap();
    let e = h.entry(name.to_string()).or_default();
    e.last_error = Some(err.to_string());
    e.last_latency_ms = Some(latency_ms);
}

/// P-9: cooldown scaled by what a wasted retry costs.
fn cooldown_for(err: &BackendError, failures: u32, cooldown_secs: u64) -> Duration {
    match err {
        // A retry burns the whole timeout — back off exponentially 30 -> 300s.
        BackendError::Timeout(_) => {
            let shift = failures.saturating_sub(1).min(4);
            Duration::from_secs((30u64 << shift).min(300))
        }
        // Connect-refused fails in milliseconds; the "just restarted Ollama"
        // case must recover fast.
        BackendError::Spawn(_, msg) if msg.to_ascii_lowercase().contains("refused") => {
            Duration::from_secs(5)
        }
        // 429 / other HTTP: base cooldown (Retry-After not yet threaded through).
        _ => Duration::from_secs(cooldown_secs),
    }
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

/// Provenance of a routing decision. Phase 0 needs only the two classifier
/// outcomes; §3.1 of the v2 spec widens this to nine variants and moves it to
/// `route/mod.rs` in Phase 1. Deliberately not pre-declared in full: CI runs
/// `clippy --all-targets -- -D warnings`, and seven unconstructed variants are a
/// hard failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DecisionSource {
    /// The classifier returned a class we could use.
    Classifier,
    /// We tried to be smart and could not — the classifier was missing, at its
    /// concurrency cap, errored, or answered ambiguously. Distinct from routing
    /// a model we simply do not know.
    FallOpen,
}

impl DecisionSource {
    fn as_str(self) -> &'static str {
        match self {
            DecisionSource::Classifier => "classifier",
            DecisionSource::FallOpen => "fall_open",
        }
    }
}

/// §5: classify the task via the router's classifier backend. Never fails the
/// request — any error/timeout/garbage falls back to `fallback_class`, and every
/// such path reports `FallOpen` so the fall-open rate is greppable in the logs.
fn classify_task(
    state: &State,
    router_cfg: &crate::config::Router,
    messages: &[openai::ChatMessage],
) -> (String, DecisionSource, u64) {
    let classes: Vec<&str> = router_cfg.routes.keys().map(|s| s.as_str()).collect();
    let truncated = classifier_input(
        messages,
        router_cfg.classifier_max_chars,
        state.cfg.server.classifier_include_opening,
    );
    let prompt = match &router_cfg.classifier_prompt {
        Some(tpl) => tpl
            .replace("{classes}", &classes.join(", "))
            .replace("{input}", &truncated),
        None => format!(
            "Classify the task into exactly one of these labels: {}.\n\nTask:\n{}\n\nAnswer with exactly one label.",
            classes.join(", "),
            truncated
        ),
    };
    let Some(classifier) = state.backends.get(&router_cfg.classifier) else {
        return (
            router_cfg.fallback_class.clone(),
            DecisionSource::FallOpen,
            0,
        );
    };
    // U-20: honor the classifier backend's concurrency cap. At capacity, degrade
    // to the fallback class rather than spawning an uncounted session (the same
    // cap run_chain enforces on chain legs). Routing never fails the request.
    let _slot = match classifier.max_concurrent() {
        Some(max) => match ActiveSlot::try_acquire(&state.active, &router_cfg.classifier, max) {
            Some(slot) => Some(slot),
            None => {
                return (
                    router_cfg.fallback_class.clone(),
                    DecisionSource::FallOpen,
                    0,
                );
            }
        },
        None => None,
    };
    let timeout = Duration::from_secs(router_cfg.classifier_timeout_secs);
    let raw = serde_json::json!({"messages": [{"role": "user", "content": prompt}]});
    let start = Instant::now();
    let result = classifier.dispatch(&prompt, &raw, &state.env, timeout);
    let ms = start.elapsed().as_millis() as u64;
    let (class, source) = match result {
        Ok(r) => parse_class(&r.content, &router_cfg.routes, &router_cfg.fallback_class),
        Err(_) => (router_cfg.fallback_class.clone(), DecisionSource::FallOpen),
    };
    (class, source, ms)
}

/// §4.1: map a classifier reply onto one of the router's classes.
///
/// Pass 1 is an exact match and is unambiguous by construction — map keys are
/// unique, so at most one can compare equal. Pass 2 is a substring match and only
/// commits when **exactly one** class matches: a reply like "this is a code task,
/// not a write task" hits both `code` and `write`, and which one won used to vary
/// between runs of the same binary because `HashMap`'s `RandomState` reseeds per
/// process. Ambiguity now falls open and says so.
fn parse_class(
    reply: &str,
    routes: &HashMap<String, String>,
    fallback: &str,
) -> (String, DecisionSource) {
    let r = reply.trim().to_ascii_lowercase();

    // Pass 1: exact match. Unambiguous by construction (keys are unique).
    if let Some(k) = routes.keys().find(|k| k.to_ascii_lowercase() == r) {
        return (k.clone(), DecisionSource::Classifier);
    }

    // Pass 2: substring — only when EXACTLY ONE class matches. Iterate a sorted
    // key list so a future change here can't reintroduce order-dependence.
    let mut keys: Vec<&String> = routes.keys().collect();
    keys.sort_unstable();
    let hits: Vec<&&String> = keys
        .iter()
        .filter(|k| r.contains(&k.to_ascii_lowercase()))
        .collect();
    match hits.as_slice() {
        [only] => ((**only).clone(), DecisionSource::Classifier),
        // 0 matches (nothing recognizable) or 2+ (ambiguous) both fall open.
        _ => (fallback.to_string(), DecisionSource::FallOpen),
    }
}

fn truncate_tail(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[chars.len() - max..].iter().collect()
}

/// Mirror image of `truncate_tail`: keep the **first** `max` chars. Char-based,
/// not byte-based, so it cannot split a UTF-8 sequence.
fn truncate_head(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    chars[..max].iter().collect()
}

/// §4.2: build what the classifier is actually shown.
///
/// v0.2.0 tail-truncated the *latest* user message, which on turn 12 of an agent
/// run hands the classifier "yes, continue" — zero routing signal — and then logs
/// a confident class. Show it both ends of the conversation instead: the opening
/// task head-truncated (intent is at the start) plus the latest turn
/// tail-truncated (the ask is at the end), joined by a marker so the classifier
/// can tell which is current.
///
/// The separator is charged to the budget, so the result is always at most `max`
/// chars — which is what `classifier_max_chars` promises whoever set it. A short
/// opening donates its unused remainder to the latest turn (plan O2).
fn classifier_input(messages: &[openai::ChatMessage], max: usize, include_opening: bool) -> String {
    let latest = openai::last_user_text(messages);
    // The opt-out, and the degenerate single-turn case: tail-truncate the latest
    // turn over the whole budget. Byte-for-byte v0.2.0 behavior.
    if !include_opening || openai::user_message_count(messages) < 2 {
        return truncate_tail(&latest, max);
    }
    const SEP: &str = "\n\n[latest message]\n";
    let body = max.saturating_sub(SEP.chars().count());
    if body == 0 {
        // No room for both halves plus the marker; the latest turn is the safer
        // half to keep, and the `<= max` guarantee holds.
        return truncate_tail(&latest, max);
    }
    let head = truncate_head(&openai::first_user_text(messages), body * 60 / 100);
    // O2: whatever the opening did not use goes to the latest turn.
    let tail = truncate_tail(&latest, body - head.chars().count());
    format!("{head}{SEP}{tail}")
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
pub fn serve(server: Arc<Server>, state: Arc<State>, workers: usize) {
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        handles.push(std::thread::spawn(move || {
            while let Ok(req) = server.recv() {
                // F-3: a panic while handling one request must not tear down the
                // worker (which would silently shrink capacity).
                let st = &state;
                handle_recovering(|| handle(req, st));
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
    fn cooldown_scales_with_error_class() {
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 1, 60).as_secs(),
            30
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 3, 60).as_secs(),
            120
        );
        assert_eq!(
            cooldown_for(&BackendError::Timeout("t".into()), 9, 60).as_secs(),
            300
        );
        assert_eq!(
            cooldown_for(
                &BackendError::Spawn("t".into(), "Connection refused".into()),
                1,
                60
            )
            .as_secs(),
            5
        );
        assert_eq!(
            cooldown_for(&BackendError::Http("t".into(), 429, String::new()), 1, 60).as_secs(),
            60
        );
    }

    /// Build the two-class route table T1-T5 share, fresh each call so every
    /// `HashMap` gets its own `RandomState` seed.
    fn routes(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn msg(role: &str, text: &str) -> openai::ChatMessage {
        openai::ChatMessage {
            role: role.to_string(),
            content: openai::MessageContent::Text(text.to_string()),
            name: None,
        }
    }

    #[test]
    fn parse_class_exact_match_wins_over_substring() {
        // T1: `code` and `code-review` both hit on the substring pass, so the
        // exact match must short-circuit before it ever runs.
        let r = routes(&[("code", "a"), ("code-review", "b")]);
        assert_eq!(
            parse_class("code", &r, "code-review"),
            ("code".to_string(), DecisionSource::Classifier)
        );
    }

    #[test]
    fn parse_class_single_substring_match_is_classifier() {
        // T2: the v0.2.0 test, retyped for the tuple. Exactly one class appears
        // in the reply, so the substring pass still commits.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("RESEARCH", &r, "execution"),
            ("research".to_string(), DecisionSource::Classifier)
        );
        assert_eq!(
            parse_class("I think this is execution work", &r, "research"),
            ("execution".to_string(), DecisionSource::Classifier)
        );
    }

    #[test]
    fn parse_class_ambiguous_reply_falls_open_across_100_maps() {
        // T3: the nondeterminism this fix exists for. `RandomState` reseeds per
        // HashMap *construction*, so one map is a coin flip that passes ~50% of
        // the time against the old code — build a fresh one every iteration.
        for i in 0..100 {
            let r = routes(&[("code", "a"), ("write", "b")]);
            assert_eq!(
                parse_class("this is a code task, not a write task", &r, "general"),
                ("general".to_string(), DecisionSource::FallOpen),
                "iteration {i}: two classes matched, so the reply is unusable"
            );
        }
    }

    #[test]
    fn parse_class_no_match_falls_open() {
        // T4: zero matches is FallOpen, not Classifier — §3.1 insists the two are
        // different things, and today both used to land in the same silent path.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("nonsense", &r, "execution"),
            ("execution".to_string(), DecisionSource::FallOpen)
        );
    }

    #[test]
    fn parse_class_is_case_and_whitespace_insensitive() {
        // T5: guards the trim().to_ascii_lowercase() the rewrite must preserve.
        let r = routes(&[("research", "best"), ("execution", "fast")]);
        assert_eq!(
            parse_class("  RESEARCH \n", &r, "execution"),
            ("research".to_string(), DecisionSource::Classifier)
        );
    }

    /// A 12-message agent run: a real opening task, then filler, ending on the
    /// content-free continuation that used to be all the classifier ever saw.
    fn agent_run() -> Vec<openai::ChatMessage> {
        let mut m = vec![msg("user", "Refactor the auth module to use JWTs")];
        for i in 0..5 {
            m.push(msg("assistant", &format!("step {i}")));
            m.push(msg("user", "ok"));
        }
        m.push(msg("user", "yes, continue"));
        m
    }

    #[test]
    fn classifier_input_combines_opening_and_latest() {
        // T6: the defect, stated as an assertion.
        let got = classifier_input(&agent_run(), 4000, true);
        assert!(
            got.contains("Refactor the auth module to use JWTs"),
            "opening task missing: {got}"
        );
        assert!(got.contains("yes, continue"), "latest turn missing: {got}");
    }

    #[test]
    fn classifier_input_respects_the_budget_and_the_60_40_split() {
        // T7: both halves far over budget. Asserts on the actual boundary chars,
        // not just lengths — a head/tail swap is the bug this fix exists to stop.
        let opening: String = "abcdefghij".repeat(20); // 200 chars
        let latest: String = "0123456789".repeat(20); // 200 chars
        let messages = vec![
            msg("user", &opening),
            msg("assistant", "..."),
            msg("user", &latest),
        ];
        const MAX: usize = 119; // 100 body + the 19-char separator
        let got = classifier_input(&messages, MAX, true);
        assert!(
            got.chars().count() <= MAX,
            "budget blown: {} > {MAX}",
            got.chars().count()
        );
        let (head, tail) = got.split_once("\n\n[latest message]\n").expect("separator");
        assert_eq!(head.chars().count(), 60, "head budget is 60% of the body");
        assert_eq!(tail.chars().count(), 40, "tail gets the remainder");
        assert!(
            head.starts_with("abcdefghij"),
            "opening must be HEAD-truncated: {head}"
        );
        assert!(
            tail.ends_with("0123456789"),
            "latest must be TAIL-truncated: {tail}"
        );
    }

    #[test]
    fn classifier_input_single_user_message_gets_the_whole_budget() {
        // T8: one user message → v0.2.0 behavior, no separator.
        let only = "x".repeat(500);
        let messages = vec![msg("system", "be brief"), msg("user", &only)];
        let got = classifier_input(&messages, 100, true);
        assert_eq!(got, truncate_tail(&only, 100));
        assert!(!got.contains("[latest message]"), "no separator: {got}");
    }

    #[test]
    fn classifier_include_opening_false_reproduces_v0_2_0() {
        // T9: the opt-out is byte-for-byte, which is the promise the key makes.
        let messages = agent_run();
        assert_eq!(
            classifier_input(&messages, 40, false),
            truncate_tail(&openai::last_user_text(&messages), 40)
        );
    }

    #[test]
    fn classifier_input_is_char_safe_on_multibyte_input() {
        // T10: truncate_head is new index arithmetic; a byte-slice mistake would
        // panic here, or split a 4-byte sequence into invalid UTF-8.
        let emoji = "\u{1F600}".repeat(100);
        let messages = vec![
            msg("user", &emoji),
            msg("assistant", "…"),
            msg("user", &emoji),
        ];
        let got = classifier_input(&messages, 60, true);
        assert!(got.chars().count() <= 60);
        let (head, tail) = got.split_once("\n\n[latest message]\n").expect("separator");
        // Every surviving char is a whole emoji — no sequence was split.
        assert!(head.chars().all(|c| c == '\u{1F600}'), "head: {head:?}");
        assert!(tail.chars().all(|c| c == '\u{1F600}'), "tail: {tail:?}");
        // The head path alone, on a budget that lands mid-sequence.
        assert_eq!(truncate_head(&emoji, 3).chars().count(), 3);
    }

    #[test]
    fn overflow_does_not_cool_or_count_against_health() {
        // T14: an overflow must move neither consecutive_failures nor
        // cooling_until — and the same call with a Timeout must move both, so a
        // short-circuit that swallowed every error class would fail here.
        const C: &str = r#"
[server]
listen = "127.0.0.1:3456"
default_chain = "echo"
env_source = "process"
cooldown = true

[backends.echo]
type = "cli"
bin = "/bin/echo"
"#;
        let state = State {
            cfg: crate::config::parse(C).unwrap(),
            env: crate::env::Env::capture("process", "/bin/sh", 1),
            backends: HashMap::new(),
            auth_token: None,
            redact_errors: false,
            health: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
        };
        let overflow = BackendError::Overflow("echo".into(), "maximum context length".into());
        record_failure(&state, "echo", &overflow, 12);
        {
            let h = state.health.lock().unwrap();
            let e = h.get("echo").expect("overflow still records last_error");
            assert_eq!(e.consecutive_failures, 0, "overflow is not a health signal");
            assert!(e.cooling_until.is_none(), "overflow must not cool a leg");
            assert_eq!(e.last_latency_ms, Some(12));
            assert!(
                e.last_error
                    .as_deref()
                    .unwrap()
                    .contains("context overflow"),
                "/v1/backends must still explain the skip: {:?}",
                e.last_error
            );
        }
        record_failure(&state, "echo", &BackendError::Timeout("echo".into()), 34);
        let h = state.health.lock().unwrap();
        let e = h.get("echo").unwrap();
        assert_eq!(e.consecutive_failures, 1, "a real failure still counts");
        assert!(e.cooling_until.is_some(), "a real failure still cools");
    }

    #[test]
    fn chain_error_response_is_400_only_when_every_leg_overflowed() {
        // T16's unit-level twin, and the O1 rule in one place: strict all().
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
    fn active_slot_caps_and_frees() {
        // U-20: the guard blocks a second acquire at cap and frees on Drop.
        let active = Mutex::new(HashMap::new());
        let s1 = ActiveSlot::try_acquire(&active, "b", 1);
        assert!(s1.is_some(), "first acquire under cap succeeds");
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 1);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_none(),
            "second acquire at cap is refused"
        );
        drop(s1);
        assert_eq!(*active.lock().unwrap().get("b").unwrap(), 0);
        assert!(
            ActiveSlot::try_acquire(&active, "b", 1).is_some(),
            "slot is reusable after Drop"
        );
    }

    #[test]
    fn active_slot_releases_on_unwind() {
        // A2: a panic between acquire and release must not leak the slot (the
        // server survives dispatch panics via F-3 catch_unwind).
        let active = Mutex::new(HashMap::new());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _slot = ActiveSlot::try_acquire(&active, "b", 1).unwrap();
            panic!("boom");
        }));
        assert!(r.is_err());
        assert_eq!(
            *active.lock().unwrap().get("b").unwrap(),
            0,
            "slot must be freed after an unwind"
        );
    }

    #[test]
    fn dispatch_panic_becomes_in_band_503() {
        // A3: a panicking dispatch maps to an all_backends_failed body (which the
        // streaming coordinator then wraps with data: [DONE]) — never a silent
        // clean-EOF stream.
        let r = dispatch_or_panic_error("test", || panic!("boom"));
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
}
