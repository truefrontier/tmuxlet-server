//! Route classification: /v1/models listing, 404 envelope, reserved 501.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
fn config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn models_lists_chains_and_backends() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/v1/models", server.base));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""object":"list""#), "body: {body}");
    assert!(
        body.contains(r#""id":"echo""#),
        "backend name missing: {body}"
    );
    assert!(
        body.contains(r#""id":"default""#),
        "chain name missing: {body}"
    );
}

#[test]
fn unknown_route_is_404_with_error_envelope() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/nope", server.base));
    assert_eq!(status, 404, "body: {body}");
    assert!(body.contains(r#""error""#), "no error envelope: {body}");
    assert!(body.contains("unknown_route"), "body: {body}");
}

#[test]
fn reserved_ui_route_is_501() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/ui/index.html", server.base));
    assert_eq!(status, 501, "body: {body}");
    assert!(body.contains("not_implemented"), "body: {body}");
}

#[test]
fn malformed_json_body_is_400() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        "{not valid json",
    );
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("parse_error"), "body: {body}");
}

#[test]
fn empty_messages_is_400() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[]}"#,
    );
    assert_eq!(status, 400, "body: {body}");
    assert!(body.contains("missing_messages"), "body: {body}");
}

#[test]
fn reserved_api_and_root_are_501() {
    let server = common::start(&config(common::free_port()));
    let (api_status, _) = common::get(&format!("{}/api/sessions/1", server.base));
    assert_eq!(api_status, 501, "/api/* should be reserved");
    let (root_status, _) = common::get(&format!("{}/", server.base));
    assert_eq!(root_status, 501, "/ should be reserved");
}

#[test]
fn head_on_known_get_route_is_200_with_empty_body() {
    // U-17: HEAD mirrors GET but carries no body.
    let server = common::start(&config(common::free_port()));
    let (status, _allow, body) = common::request("HEAD", &format!("{}/v1/models", server.base));
    assert_eq!(status, 200, "HEAD /v1/models should be 200");
    assert!(body.is_empty(), "HEAD must not return a body: {body:?}");
}

#[test]
fn wrong_method_on_known_path_is_405_with_allow_header() {
    // U-17: a known path reached with the wrong method → 405 naming the fix.
    let server = common::start(&config(common::free_port()));
    let (status, allow, body) = common::request("DELETE", &format!("{}/v1/models", server.base));
    assert_eq!(status, 405, "DELETE /v1/models should be 405: {body}");
    assert_eq!(
        allow.as_deref(),
        Some("GET, HEAD"),
        "Allow header missing/wrong"
    );
    assert!(body.contains("method_not_allowed"), "body: {body}");

    let (chat_status, chat_allow, _) =
        common::request("GET", &format!("{}/v1/chat/completions", server.base));
    assert_eq!(chat_status, 405, "GET /v1/chat/completions should be 405");
    assert_eq!(chat_allow.as_deref(), Some("POST"), "Allow should be POST");
}

fn echo_config(port: u16, prompt_mode: &str) -> String {
    // /bin/echo echoes the shaped prompt as argv, so the completion content is
    // exactly what the backend received — lets us assert U-14 shaping end-to-end.
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"
prompt_mode = "{prompt_mode}"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn prompt_mode_last_user_sends_verbatim_without_role_labels() {
    // U-14: last_user shaping drops "System:"/"User:" labels and the assistant turn.
    let server = common::start(&echo_config(common::free_port(), "last_user"));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"system","content":"be terse"},{"role":"user","content":"hi"},{"role":"assistant","content":"prev"},{"role":"user","content":"final ask"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("final ask"), "missing user text: {body}");
    assert!(body.contains("be terse"), "missing system text: {body}");
    assert!(
        !body.contains("User:"),
        "last_user must not label roles: {body}"
    );
    assert!(
        !body.contains("Assistant:"),
        "last_user drops the assistant turn: {body}"
    );
}

#[test]
fn prompt_mode_transcript_is_the_default_shaping() {
    // U-14: default (transcript) keeps role labels, unified to "User:"/"System:".
    let server = common::start(&echo_config(common::free_port(), "transcript"));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"system","content":"S"},{"role":"user","content":"U"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("System: S"), "body: {body}");
    assert!(body.contains("User: U"), "body: {body}");
}

fn cors_config(port: u16, origin: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
cors_origins = ["{origin}"]

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn cors_preflight_204_and_echo_when_origin_allowed() {
    // U-10: OPTIONS preflight is answered; the allowed Origin is echoed on both
    // the preflight and the actual GET.
    let origin = "http://localhost:5173";
    let server = common::start(&cors_config(common::free_port(), origin));

    let (status, headers, _) = common::send(
        "OPTIONS",
        &format!("{}/v1/models", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(status, 204, "preflight should be 204");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some(origin)
    );
    assert!(
        headers
            .get("access-control-allow-headers")
            .is_some_and(|h| h.contains("authorization") && h.contains("content-type")),
        "preflight allow-headers: {headers:?}"
    );

    let (gs, gh, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(gs, 200);
    assert_eq!(
        gh.get("access-control-allow-origin").map(String::as_str),
        Some(origin),
        "actual response must echo the origin"
    );
}

#[test]
fn cors_absent_for_unlisted_origin_and_by_default() {
    // U-10 + E1: an unlisted Origin gets no ACAO, but a CORS-enabled server still
    // sends `Vary: Origin` on every response so a shared cache keys on Origin and
    // can't serve a browser from an allowed origin the cached no-ACAO body.
    let server = common::start(&cors_config(common::free_port(), "http://localhost:5173"));
    let (_s, gh, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", "http://evil.test")],
    );
    assert!(
        !gh.contains_key("access-control-allow-origin"),
        "unlisted origin must not be echoed: {gh:?}"
    );
    assert_eq!(
        gh.get("vary").map(String::as_str),
        Some("Origin"),
        "CORS-enabled server must Vary: Origin even for an unlisted origin: {gh:?}"
    );

    // And the default config (no cors_origins) never emits CORS headers.
    let plain = common::start(&config(common::free_port()));
    let (_s2, ph, _) = common::send(
        "GET",
        &format!("{}/v1/models", plain.base),
        &[("Origin", "http://localhost:5173")],
    );
    assert!(
        !ph.contains_key("access-control-allow-origin"),
        "CORS off by default: {ph:?}"
    );
    assert!(
        !ph.contains_key("vary"),
        "no CORS configured -> no Vary header: {ph:?}"
    );
}

#[test]
fn zero_workers_is_clamped_and_still_serves() {
    // P-1: workers = 0 must not exit the process; serve clamps to 1. If the clamp
    // regressed, the server would exit and common::start would panic on readiness.
    let cfg = config(common::free_port()).replace(
        "env_source = \"process\"",
        "env_source = \"process\"\nworkers = 0",
    );
    let server = common::start(&cfg);
    let (status, _) = common::get(&format!("{}/health", server.base));
    assert_eq!(status, 200, "clamped server must answer /health");
}

fn router_config(port: u16) -> String {
    // A single-class router over /bin/echo. Whatever the classifier echoes,
    // parse_class finds "only" (or falls back to it), so routing is deterministic:
    // class "only" -> chain "default" -> winning backend "echo".
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[routers.smart]
classifier = "echo"
fallback_class = "only"
routes = {{ only = "default" }}

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn route_header_names_the_model_class_target_and_winning_backend() {
    // B1 + §5.1: x-tmuxlet-route was `class/target/backend`; the decision layer
    // widens it to `model/class/target/backend` and emits it on every response,
    // not just router ones. The last segment still names the leg that actually
    // answered, which is unrecoverable from the body.
    let server = common::start(&router_config(common::free_port()));
    let (status, headers, body) = common::post_headers(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"smart","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    let route = headers
        .get("x-tmuxlet-route")
        .expect("x-tmuxlet-route must be present on a router request");
    assert_eq!(
        route, "smart/only/default/echo",
        "route must be model/class/target/backend"
    );
    // §5.1: the three companion headers ship on every response too.
    assert_eq!(
        headers.get("x-tmuxlet-backend").map(String::as_str),
        Some("echo")
    );
    assert!(
        matches!(
            headers.get("x-tmuxlet-decision").map(String::as_str),
            Some("classifier") | Some("fall_open")
        ),
        "decision header: {:?}",
        headers.get("x-tmuxlet-decision")
    );
    assert!(
        headers.contains_key("x-tmuxlet-rationale"),
        "a loopback bind is not redacted, so the rationale ships"
    );
}

#[test]
fn a_plain_chain_request_also_carries_provenance() {
    // §5.1: V1 emitted x-tmuxlet-route only on the router path, so a direct chain
    // request came back with no clue which backend had answered.
    let server = common::start(&config(common::free_port()));
    let (status, headers, body) = common::post_headers(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        headers.get("x-tmuxlet-route").map(String::as_str),
        Some("default/echo"),
        "a direct chain has no class/target, so it is model/backend"
    );
    assert_eq!(
        headers.get("x-tmuxlet-decision").map(String::as_str),
        Some("static"),
        "the model named a chain"
    );
}

#[test]
fn strict_models_off_falls_back_to_default_chain() {
    // U-3: default (strict_models unset) — an unknown model runs default_chain.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "unknown model should fall back, got: {body}");
    assert!(body.contains("chat.completion"), "body: {body}");
}

#[test]
fn strict_models_on_rejects_unknown_model_and_names_the_fix() {
    // U-3 + invariant 3: strict_models = true -> 404 that names its remedy.
    let cfg = config(common::free_port()).replace(
        "env_source = \"process\"",
        "env_source = \"process\"\nstrict_models = true",
    );
    let server = common::start(&cfg);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"nonexistent-model","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        status, 404,
        "strict_models must reject unknown model: {body}"
    );
    assert!(body.contains("model_not_found"), "body: {body}");
    assert!(
        body.contains("/v1/models") && body.contains("strict_models = false"),
        "404 must name both remedies: {body}"
    );
}

#[test]
fn backends_endpoint_shape_and_ok_transition() {
    // U-13/B5: /v1/backends reports each backend's health with cooling_secs (the
    // deliberate rename of cooling_until). A fresh backend is "unknown"; after a
    // successful dispatch it becomes "ok".
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::get(&format!("{}/v1/backends", server.base));
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains(r#""name":"echo""#), "body: {body}");
    assert!(body.contains(r#""type":"cli""#), "body: {body}");
    assert!(
        body.contains(r#""state":"unknown""#),
        "a fresh backend is unknown: {body}"
    );
    for field in [
        "consecutive_failures",
        "last_error",
        "last_latency_ms",
        "cooling_secs",
    ] {
        assert!(body.contains(field), "missing field {field}: {body}");
    }

    // Drive one successful request, then the backend transitions to "ok".
    let (cs, cb) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"default","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(cs, 200, "chat body: {cb}");
    let (_s, body2) = common::get(&format!("{}/v1/backends", server.base));
    assert!(
        body2.contains(r#""state":"ok""#),
        "backend should be ok after a success: {body2}"
    );
}

fn cors_multi_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
cors_origins = ["http://a.test", "http://b.test"]

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

fn cors_auth_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
auth = true
auth_token_env = "TEST_AUTH_TOKEN"
cors_origins = ["http://localhost:5173"]

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn cors_headers_present_on_error_responses() {
    // U-10: apply_cors runs on every response, so 404 and 405 also carry ACAO+Vary
    // for an allowed Origin (a browser needs it to read the error body).
    let origin = "http://localhost:5173";
    let server = common::start(&cors_config(common::free_port(), origin));

    let (s404, h404, _) = common::send(
        "GET",
        &format!("{}/nope", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(s404, 404);
    assert_eq!(
        h404.get("access-control-allow-origin").map(String::as_str),
        Some(origin),
        "ACAO on 404: {h404:?}"
    );
    assert_eq!(
        h404.get("vary").map(String::as_str),
        Some("Origin"),
        "Vary on 404: {h404:?}"
    );

    let (s405, h405, _) = common::send(
        "DELETE",
        &format!("{}/v1/models", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(s405, 405);
    assert_eq!(
        h405.get("access-control-allow-origin").map(String::as_str),
        Some(origin),
        "ACAO on 405: {h405:?}"
    );
}

#[test]
fn cors_header_present_on_the_sse_stream() {
    // U-10: a streaming (SSE) response also carries ACAO for an allowed Origin.
    let origin = "http://localhost:5173";
    let server = common::start(&cors_config(common::free_port(), origin));
    let (status, headers, _body) = common::send_body(
        "POST",
        &format!("{}/v1/chat/completions", server.base),
        &[("Origin", origin), ("Content-Type", "application/json")],
        r#"{"model":"default","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some(origin),
        "SSE must echo ACAO: {headers:?}"
    );
}

#[test]
fn cors_preflight_succeeds_with_auth_enabled_and_no_token() {
    // U-10 + S-1: the OPTIONS preflight is answered BEFORE the auth gate, so a
    // browser can preflight without a token (it can't attach one to a preflight).
    let origin = "http://localhost:5173";
    let server = common::start_with_env(
        &cors_auth_config(common::free_port()),
        &[("TEST_AUTH_TOKEN", "tok")],
    );
    let (status, headers, _) = common::send(
        "OPTIONS",
        &format!("{}/v1/chat/completions", server.base),
        &[("Origin", origin)],
    );
    assert_eq!(status, 204, "preflight must not require auth");
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some(origin)
    );
}

#[test]
fn cors_echoes_each_of_multiple_configured_origins() {
    let server = common::start(&cors_multi_config(common::free_port()));
    for origin in ["http://a.test", "http://b.test"] {
        let (_s, h, _) = common::send(
            "GET",
            &format!("{}/v1/models", server.base),
            &[("Origin", origin)],
        );
        assert_eq!(
            h.get("access-control-allow-origin").map(String::as_str),
            Some(origin),
            "must echo {origin}: {h:?}"
        );
    }
    let (_s, h, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", "http://c.test")],
    );
    assert!(
        !h.contains_key("access-control-allow-origin"),
        "an unlisted origin must not be echoed: {h:?}"
    );
}

#[test]
fn cors_star_is_not_a_wildcard() {
    // U-10 + B7: "*" is matched literally, not as a wildcard — a real Origin that
    // isn't the literal "*" gets no ACAO.
    let server = common::start(&cors_config(common::free_port(), "*"));
    let (_s, h, _) = common::send(
        "GET",
        &format!("{}/v1/models", server.base),
        &[("Origin", "http://anything.test")],
    );
    assert!(
        !h.contains_key("access-control-allow-origin"),
        "\"*\" must not match arbitrary origins: {h:?}"
    );
}

#[test]
fn body_cap_413_at_boundary() {
    // Body cap MAX_BODY = 16 MiB: MAX+1 bytes -> 413; a MAX-byte body is not
    // rejected by the cap (it fails later as parse_error / 400, not 413).
    const MAX: usize = 16 * 1024 * 1024;
    let server = common::start(&config(common::free_port()));
    let url = format!("{}/v1/chat/completions", server.base);

    let (s_over, b_over) = common::post_json(&url, &"a".repeat(MAX + 1));
    assert_eq!(s_over, 413, "over the cap must be 413: {b_over}");
    assert!(b_over.contains("payload_too_large"), "413 code: {b_over}");

    let (s_at, _b_at) = common::post_json(&url, &"a".repeat(MAX));
    assert_ne!(
        s_at, 413,
        "a body at exactly the cap must not be rejected by it"
    );
}

/// Bind `port` and, on a background thread, answer exactly one HTTP request with
/// the given raw `response` (same helper as tests/api_backend.rs). One listener
/// answers one request, so every leg of a chain needs its own.
fn mock_upstream(port: u16, response: String) -> thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock upstream");
    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
        // Read headers, then drain the Content-Length body so the client's write
        // completes cleanly before we respond and close.
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let mut header_end = None;
        while header_end.is_none() {
            match sock.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    header_end = buf.windows(4).position(|w| w == b"\r\n\r\n");
                }
                Err(_) => break,
            }
        }
        if let Some(he) = header_end {
            let head = String::from_utf8_lossy(&buf[..he]).to_ascii_lowercase();
            let clen = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut remaining = clen.saturating_sub(buf.len() - (he + 4));
            while remaining > 0 {
                match sock.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => remaining = remaining.saturating_sub(n),
                    Err(_) => break,
                }
            }
        }
        let _ = sock.write_all(response.as_bytes());
        let _ = sock.flush();
    })
}

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// The upstream shape `is_context_overflow` keys on: HTTP 400 whose body names
/// "maximum context length". Anything else (500, a 400 without a marker) is a
/// plain `Http` error and cools the leg as before.
fn overflow_response() -> String {
    http_response(
        400,
        "Bad Request",
        r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
    )
}

/// Two `api` legs in one chain, each pointed at its own mock upstream. Only
/// `api` backends can produce `Overflow` (the rule keys on an HTTP status).
fn two_leg_api_config(port: u16, leg1: u16, leg2: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "pair"
env_source = "process"

[backends.leg1]
type = "api"
base_url = "http://127.0.0.1:{leg1}/v1"
model = "leg1-model"

[backends.leg2]
type = "api"
base_url = "http://127.0.0.1:{leg2}/v1"
model = "leg2-model"

[chains.pair]
order = ["leg1", "leg2"]
"#
    )
}

#[test]
fn all_legs_overflow_returns_400_context_length_exceeded() {
    // §4.3: a chain where every leg reports "your prompt does not fit" is a client
    // problem, so it answers 400 with the OpenAI-shaped overflow code rather than
    // the 503 that invites a pointless retry.
    let (p1, p2) = (common::free_port(), common::free_port());
    let _m1 = mock_upstream(p1, overflow_response());
    let _m2 = mock_upstream(p2, overflow_response());
    let server = common::start(&two_leg_api_config(common::free_port(), p1, p2));

    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"pair","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 400, "an all-overflow chain must not 503: {body}");
    assert!(
        body.contains(r#""code":"context_length_exceeded""#),
        "overflow code: {body}"
    );
    assert!(
        body.contains(r#""type":"invalid_request_error""#),
        "overflow type: {body}"
    );

    // O4, observed from outside: an overflow says nothing about backend health, so
    // neither leg cooled and neither counted a failure. Two legs, two "ok" states.
    let (hs, hb) = common::get(&format!("{}/v1/backends", server.base));
    assert_eq!(hs, 200, "body: {hb}");
    assert_eq!(
        hb.matches(r#""state":"ok""#).count(),
        2,
        "an overflow must not bench a healthy backend: {hb}"
    );
    assert_eq!(
        hb.matches(r#""consecutive_failures":0"#).count(),
        2,
        "an overflow must not count as a failure: {hb}"
    );
}

#[test]
fn a_mixed_overflow_and_real_failure_stays_503() {
    // O1: the all-overflow rule is strict on purpose. Leg 2 failed for a reason a
    // retry could fix, so the chain keeps the retryable 503 — "any overflow -> 400"
    // would wrongly tell the client not to bother.
    let (p1, p2) = (common::free_port(), common::free_port());
    let _m1 = mock_upstream(p1, overflow_response());
    let _m2 = mock_upstream(
        p2,
        http_response(500, "Internal Server Error", r#"{"error":"boom"}"#),
    );
    let server = common::start(&two_leg_api_config(common::free_port(), p1, p2));

    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"pair","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        status, 503,
        "one real failure keeps the chain at 503: {body}"
    );
    assert!(body.contains("all_backends_failed"), "body: {body}");
}

#[test]
fn streaming_all_legs_overflow_carries_the_overflow_code_in_band() {
    // R5: the asymmetry with the non-streaming case is deliberate and documented in
    // stream_with_keepalive — the SSE headers are committed before the chain runs,
    // so the transport is already 200 and the overflow can only travel in-band.
    let (p1, p2) = (common::free_port(), common::free_port());
    let _m1 = mock_upstream(p1, overflow_response());
    let _m2 = mock_upstream(p2, overflow_response());
    let server = common::start(&two_leg_api_config(common::free_port(), p1, p2));

    let (status, content_type, body) = common::post_collect(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"pair","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(
        status, 200,
        "SSE transport status is committed early: {body}"
    );
    assert!(
        content_type.contains("text/event-stream"),
        "wrong SSE content-type: {content_type:?}"
    );
    let frame = body
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .find(|f| *f != "[DONE]")
        .expect("an in-band error frame before [DONE]");
    assert!(
        frame.contains(r#""code":"context_length_exceeded""#),
        "the overflow code must survive in the frame body: {frame}"
    );
    assert!(
        body.trim_end().ends_with("data: [DONE]"),
        "stream not terminated by [DONE]: {body}"
    );
}
