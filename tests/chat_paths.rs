//! Chat-handler paths with no prior coverage: the auto-router's classifier
//! (answer, failure, garbage, prompt template, tail truncation), P-9 cooldown
//! skips, the P-6 chain budget, U-6 empty output, the streaming all-failed
//! path, typed-shape and non-UTF-8 bodies, and HEAD under auth.
//!
//! Backends are small shell scripts in a per-test temp dir. Nothing here runs
//! tmuxlet or touches `~/.tmuxlet`.
#![cfg(unix)]
mod common;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

/// Write an executable `#!/bin/sh` script into `dir`; returns its path.
fn script(dir: &Path, name: &str, body: &str) -> String {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p.display().to_string()
}

/// `[server]` header shared by every config here.
fn server_block(port: u16, extra: &str) -> String {
    format!(
        "[server]\nlisten = \"127.0.0.1:{port}\"\ndefault_chain = \"default\"\nenv_source = \"process\"\n{extra}\n"
    )
}

fn chat_url(server: &common::Server) -> String {
    format!("{}/v1/chat/completions", server.base)
}

fn user_msg(model: &str, content: &str) -> String {
    serde_json::json!({"model": model, "messages": [{"role": "user", "content": content}]})
        .to_string()
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not JSON ({e}): {body}"))
}

/// The `/v1/backends` entry for `name`.
fn backend_entry(server: &common::Server, name: &str) -> serde_json::Value {
    let (status, body) = common::get(&format!("{}/v1/backends", server.base));
    assert_eq!(status, 200, "body: {body}");
    json(&body)
        .as_array()
        .expect("/v1/backends is an array")
        .iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("no entry for {name}: {body}"))
        .clone()
}

/// How many times a counting script ran (one line per call).
fn calls(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("calls"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

// ---- §5 auto-router: classify_task ----

fn router_config(port: u16, dir: &Path) -> String {
    let research = script(dir, "pick-research", "printf research");
    let garbage = script(dir, "pick-garbage", "printf banana");
    // Records the exact classifier prompt it receives, then answers.
    let recorder = script(
        dir,
        "recorder",
        "printf '%s' \"$1\" > \"$(dirname \"$0\")/seen\"; printf research",
    );
    format!(
        r#"{server}
[backends.pick_research]
type = "cli"
bin = "{research}"

[backends.pick_garbage]
type = "cli"
bin = "{garbage}"

[backends.pick_fail]
type = "cli"
bin = "/usr/bin/false"

[backends.recorder]
type = "cli"
bin = "{recorder}"
args = ["{{prompt}}"]

[backends.deep]
type = "cli"
bin = "/bin/echo"

[backends.fast]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["fast"]

[chains.deepc]
order = ["deep"]

[chains.fastc]
order = ["fast"]

[routers.r_ok]
classifier = "pick_research"
fallback_class = "quick"
routes = {{ research = "deepc", quick = "fastc" }}

[routers.r_garbage]
classifier = "pick_garbage"
fallback_class = "quick"
routes = {{ research = "deepc", quick = "fastc" }}

[routers.r_fail]
classifier = "pick_fail"
fallback_class = "quick"
routes = {{ research = "deepc", quick = "fastc" }}

[routers.r_tpl]
classifier = "recorder"
fallback_class = "quick"
classifier_prompt = "C={{classes}} I={{input}}"
classifier_max_chars = 5
routes = {{ research = "deepc", quick = "fastc" }}
"#,
        server = server_block(port, "")
    )
}

fn route_of(server: &common::Server, model: &str) -> String {
    let (status, headers, body) = common::post_headers(&chat_url(server), &user_msg(model, "hi"));
    assert_eq!(status, 200, "{model}: {body}");
    headers
        .get("x-tmuxlet-route")
        .unwrap_or_else(|| panic!("{model}: no x-tmuxlet-route header"))
        .clone()
}

#[test]
fn classifier_answer_selects_its_route() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&router_config(common::free_port(), dir.path()));
    assert_eq!(route_of(&server, "r_ok"), "research/deepc/deep");
}

#[test]
fn classifier_garbage_or_failure_uses_the_fallback_class() {
    // §5: routing never fails the request. A reply naming no class and a
    // classifier that exits non-zero both route via fallback_class.
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&router_config(common::free_port(), dir.path()));
    assert_eq!(route_of(&server, "r_garbage"), "quick/fastc/fast");
    assert_eq!(route_of(&server, "r_fail"), "quick/fastc/fast");
}

#[test]
fn classifier_prompt_template_gets_the_tail_of_the_last_user_message() {
    // classifier_prompt substitutes {classes} and {input}; {input} is the LAST
    // user message cut to its final classifier_max_chars characters (chars,
    // not bytes: the multi-byte tail must survive intact).
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&router_config(common::free_port(), dir.path()));
    let body = serde_json::json!({"model": "r_tpl", "messages": [
        {"role": "user", "content": "an earlier ask"},
        {"role": "assistant", "content": "ok"},
        {"role": "user", "content": "xxxxxxxxxxtail✓"}
    ]})
    .to_string();
    let (status, headers, resp) = common::post_headers(&chat_url(&server), &body);
    assert_eq!(status, 200, "body: {resp}");
    assert_eq!(headers["x-tmuxlet-route"], "research/deepc/deep");

    let seen = std::fs::read_to_string(dir.path().join("seen")).unwrap();
    // Class order follows the routes map, so accept either order.
    assert!(
        seen == "C=research, quick I=tail✓" || seen == "C=quick, research I=tail✓",
        "classifier prompt: {seen:?}"
    );
}

#[test]
fn streaming_router_header_names_class_and_target_only() {
    // SSE headers are committed before any backend answers, so a streamed
    // router response carries class/target, without the winning backend.
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&router_config(common::free_port(), dir.path()));
    let body = r#"{"model":"r_ok","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, headers, resp) = common::post_headers(&chat_url(&server), body);
    assert_eq!(status, 200, "body: {resp}");
    assert_eq!(headers["x-tmuxlet-route"], "research/deepc");
    assert!(resp.trim_end().ends_with("data: [DONE]"), "body: {resp}");
}

// ---- P-9 cooldown ----

fn cooldown_config(port: u16, dir: &Path, cooldown: bool) -> String {
    // `flaky` counts its calls, then fails with no output (an Exit error).
    let flaky = script(
        dir,
        "flaky",
        "echo x >> \"$(dirname \"$0\")/calls\"; exit 1",
    );
    format!(
        r#"{server}
[backends.flaky]
type = "cli"
bin = "{flaky}"

[backends.good]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["flaky", "good"]

[chains.alone]
order = ["flaky"]
"#,
        server = server_block(port, &format!("cooldown = {cooldown}\ncooldown_secs = 300"))
    )
}

#[test]
fn a_failed_leg_cools_down_and_is_skipped_while_a_fallback_remains() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&cooldown_config(common::free_port(), dir.path(), true));
    for _ in 0..2 {
        let (status, body) = common::post_json(&chat_url(&server), &user_msg("default", "hi"));
        assert_eq!(status, 200, "good must answer: {body}");
        assert_eq!(json(&body)["model"], "good", "body: {body}");
    }
    assert_eq!(calls(dir.path()), 1, "the cooling leg must be skipped");

    let flaky = backend_entry(&server, "flaky");
    assert_eq!(flaky["state"], "cooling", "{flaky}");
    // A skip is not a failure: the count stays at the one real failure.
    assert_eq!(flaky["consecutive_failures"], 1, "{flaky}");
    assert!(
        flaky["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("exited with 1")),
        "{flaky}"
    );
    let secs = flaky["cooling_secs"].as_u64().expect("cooling_secs is set");
    assert!((250..=300).contains(&secs), "cooldown_secs = 300: {flaky}");
}

#[test]
fn a_cooling_leg_is_still_tried_when_it_is_the_last_option() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&cooldown_config(common::free_port(), dir.path(), true));
    for _ in 0..2 {
        let (status, body) = common::post_json(&chat_url(&server), &user_msg("alone", "hi"));
        assert_eq!(status, 503, "body: {body}");
        assert!(!body.contains("skipped: cooling"), "body: {body}");
    }
    assert_eq!(calls(dir.path()), 2, "the only leg runs even while cooling");
}

#[test]
fn cooldown_off_retries_the_failed_leg_every_time() {
    let dir = tempfile::tempdir().unwrap();
    let server = common::start(&cooldown_config(common::free_port(), dir.path(), false));
    for _ in 0..2 {
        let (status, body) = common::post_json(&chat_url(&server), &user_msg("default", "hi"));
        assert_eq!(status, 200, "body: {body}");
    }
    assert_eq!(calls(dir.path()), 2, "cooldown = false never skips");
    let flaky = backend_entry(&server, "flaky");
    assert_eq!(flaky["consecutive_failures"], 2, "{flaky}");
    assert!(flaky["cooling_secs"].is_null(), "{flaky}");
}

// ---- P-6 chain budget ----

#[test]
fn chain_budget_clamps_the_leg_and_skips_the_rest() {
    let dir = tempfile::tempdir().unwrap();
    let slow = script(dir.path(), "slow", "exec sleep 5");
    let cfg = format!(
        r#"{server}
[backends.slow]
type = "cli"
bin = "{slow}"

[backends.good]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["slow", "good"]
"#,
        server = server_block(common::free_port(), "chain_budget_secs = 1")
    );
    let server = common::start(&cfg);
    let t = Instant::now();
    let (status, body) = common::post_json(&chat_url(&server), &user_msg("default", "hi"));
    let elapsed = t.elapsed();
    assert_eq!(status, 503, "body: {body}");
    // The slow leg is cut at the 1s budget, not request_timeout_secs (1800).
    assert!(elapsed < Duration::from_secs(4), "took {elapsed:?}");
    let v = json(&body);
    assert_eq!(
        v["error"]["details"],
        serde_json::json!(["[slow] timed out", "[good] backend error: skipped: budget"]),
        "body: {body}"
    );
    assert_eq!(
        v["error"]["message"],
        "all backends failed: slow (timeout), good (backend error)"
    );
}

// ---- U-6 empty output ----

#[test]
fn empty_output_falls_through_unless_allow_empty() {
    let dir = tempfile::tempdir().unwrap();
    let blank = script(dir.path(), "blank", "exit 0");
    let cfg = format!(
        r#"{server}
[backends.blank]
type = "cli"
bin = "{blank}"

[backends.blank_ok]
type = "cli"
bin = "{blank}"
allow_empty = true

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["blank", "echo"]
"#,
        server = server_block(common::free_port(), "")
    );
    let server = common::start(&cfg);

    let (status, body) = common::post_json(&chat_url(&server), &user_msg("default", "hi"));
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        json(&body)["model"],
        "echo",
        "empty output must fall through"
    );

    let (status, body) = common::post_json(&chat_url(&server), &user_msg("blank", "hi"));
    assert_eq!(status, 503, "body: {body}");
    assert!(
        body.contains("[blank] backend error: empty output"),
        "{body}"
    );

    let (status, body) = common::post_json(&chat_url(&server), &user_msg("blank_ok", "hi"));
    assert_eq!(status, 200, "allow_empty accepts it: {body}");
    let v = json(&body);
    assert_eq!(v["model"], "blank_ok");
    assert_eq!(v["choices"][0]["message"]["content"], "");
}

// ---- streaming failure ----

#[test]
fn streaming_total_failure_is_an_in_band_error_then_done() {
    // The 200 and SSE headers are already sent, so a total failure arrives as
    // an error frame followed by [DONE], never as a bare EOF.
    let cfg = format!(
        r#"{server}
[backends.broken]
type = "cli"
bin = "/usr/bin/false"

[chains.default]
order = ["broken"]
"#,
        server = server_block(common::free_port(), "")
    );
    let server = common::start(&cfg);
    let (status, content_type, body) = common::post_collect(
        &chat_url(&server),
        r#"{"model":"default","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(content_type.contains("text/event-stream"), "{content_type}");
    let frames: Vec<&str> = body.split("\n\n").filter(|f| !f.is_empty()).collect();
    assert_eq!(frames.len(), 2, "error frame + [DONE]: {body}");
    let err = json(frames[0].strip_prefix("data: ").expect("data frame"));
    assert_eq!(err["error"]["code"], "all_backends_failed", "{body}");
    assert!(
        err["error"]["details"][0]
            .as_str()
            .is_some_and(|d| d.starts_with("[broken] exited with 1")),
        "{body}"
    );
    assert_eq!(frames[1], "data: [DONE]");
}

// ---- request bodies ----

fn echo_config(port: u16) -> String {
    format!(
        "{}\n[backends.echo]\ntype = \"cli\"\nbin = \"/bin/echo\"\n\n[chains.default]\norder = [\"echo\"]\n",
        server_block(port, "")
    )
}

#[test]
fn valid_json_with_the_wrong_shape_is_400_parse_error() {
    let server = common::start(&echo_config(common::free_port()));
    for body in [
        r#"{"model":"default","messages":"hi"}"#,
        r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        r#"{"model":"default","messages":[{"role":"user"}]}"#,
    ] {
        let (status, resp) = common::post_json(&chat_url(&server), body);
        assert_eq!(status, 400, "{body} -> {resp}");
        assert_eq!(
            json(&resp)["error"]["code"],
            "parse_error",
            "{body} -> {resp}"
        );
    }
}

#[test]
fn a_non_utf8_body_is_400_read_error() {
    let server = common::start(&echo_config(common::free_port()));
    let addr = server.base.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        b"POST /v1/chat/completions HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n\xff\xfe",
    )
    .unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let resp = String::from_utf8_lossy(&raw);
    assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
    assert!(resp.contains(r#""code":"read_error""#), "{resp}");
}

// ---- HEAD under auth ----

#[test]
fn head_on_a_protected_route_needs_the_token_but_health_does_not() {
    let cfg = echo_config(common::free_port()).replace(
        "env_source = \"process\"",
        "env_source = \"process\"\nauth = true\nauth_token_env = \"TEST_AUTH_TOKEN\"",
    );
    let server = common::start_with_env(&cfg, &[("TEST_AUTH_TOKEN", "t0k")]);
    let models = format!("{}/v1/models", server.base);
    let (status, _, _) = common::request("HEAD", &models);
    assert_eq!(status, 401, "HEAD /v1/models without a token");
    let (status, _, body) = common::send("HEAD", &models, &[("Authorization", "Bearer t0k")]);
    assert_eq!(status, 200);
    assert!(body.is_empty(), "HEAD has no body: {body:?}");
    let (status, _, _) = common::request("HEAD", &format!("{}/health", server.base));
    assert_eq!(status, 200, "HEAD /health stays open");
}
