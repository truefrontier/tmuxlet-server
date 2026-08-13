//! §9.1: `POST /v1/messages` — the text-only Anthropic Messages surface,
//! observed from outside the process.
//!
//! The point of these tests is the *seam*: an Anthropic-shaped request must
//! come out of the same routing, shaping, and chain machinery as an OpenAI one,
//! and go back in an Anthropic-shaped envelope. `/bin/echo` as the backend
//! makes the shaped prompt visible in the reply, so what the backend received
//! is assertable end-to-end.
mod common;

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
fn messages_returns_the_anthropic_envelope() {
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":1024,"messages":[{"role":"user","content":"anthropic-hello"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");

    let v: serde_json::Value = serde_json::from_str(&body).expect("response is JSON");
    assert_eq!(v["type"], "message", "body: {body}");
    assert_eq!(v["role"], "assistant", "body: {body}");
    assert_eq!(v["stop_reason"], "end_turn", "body: {body}");
    assert!(v["stop_sequence"].is_null(), "body: {body}");
    assert_eq!(v["content"][0]["type"], "text", "body: {body}");
    assert!(
        v["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("anthropic-hello"),
        "backend text missing: {body}"
    );
    assert!(
        v["id"].as_str().is_some_and(|s| !s.is_empty()),
        "id must be present: {body}"
    );
    // Zeroed deliberately — the backends report no token counts, and a
    // fabricated estimate would feed cost trackers plausible-but-wrong numbers.
    assert_eq!(v["usage"]["input_tokens"], 0, "body: {body}");
    assert_eq!(v["usage"]["output_tokens"], 0, "body: {body}");
    // No OpenAI leakage in the Anthropic envelope.
    assert!(v.get("choices").is_none(), "OpenAI shape leaked: {body}");
    assert!(v.get("object").is_none(), "OpenAI shape leaked: {body}");
}

#[test]
fn top_level_system_reaches_the_backend_as_a_system_message() {
    // The whole design rests on this: `system` becomes a leading `role:"system"`
    // message, so PromptMode shaping (here: the default transcript) treats an
    // Anthropic request exactly like an OpenAI one.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":16,"system":"be terse","messages":[{"role":"user","content":"hi there"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("System: be terse"), "system text: {body}");
    assert!(body.contains("User: hi there"), "user text: {body}");
}

#[test]
fn system_and_content_accept_block_arrays() {
    // Anthropic allows `system` and `content` as arrays of text blocks; non-text
    // blocks are dropped rather than failing the request.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":16,
            "system":[{"type":"text","text":"sys-block"}],
            "messages":[{"role":"user","content":[
                {"type":"text","text":"first-block"},
                {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}},
                {"type":"text","text":"second-block"}
            ]}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("sys-block"), "system block: {body}");
    assert!(body.contains("first-block"), "first text block: {body}");
    assert!(body.contains("second-block"), "second text block: {body}");
}

#[test]
fn unknown_top_level_fields_are_accepted() {
    // Anthropic clients send `temperature`, `metadata`, `thinking`, … — none of
    // which we act on. They must not make the request unparseable.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":16,"temperature":0.4,"metadata":{"user_id":"u1"},
            "thinking":{"type":"adaptive"},"messages":[{"role":"user","content":"extras-ok"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(body.contains("extras-ok"), "body: {body}");
}

#[test]
fn streaming_emits_anthropic_sse_events_in_order() {
    // Anthropic's SSE frames carry BOTH an `event:` and a `data:` line, and end
    // at `message_stop` — there is no OpenAI-style [DONE] sentinel.
    let server = common::start(&config(common::free_port()));
    let (status, content_type, body) = common::post_collect(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"streamtest"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        content_type.contains("text/event-stream"),
        "wrong SSE content-type: {content_type:?}"
    );

    let mut last = 0usize;
    for name in [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ] {
        let at = body
            .find(&format!("event: {name}"))
            .unwrap_or_else(|| panic!("missing `event: {name}` frame: {body}"));
        assert!(at >= last, "frames out of order at {name}: {body}");
        last = at;
    }
    assert!(body.contains("text_delta"), "no text delta: {body}");
    assert!(body.contains("streamtest"), "reply text missing: {body}");
    assert!(
        !body.contains("[DONE]"),
        "Anthropic streams end at message_stop, not [DONE]: {body}"
    );
}

#[test]
fn tools_are_refused_with_a_clear_400() {
    // The ceiling, enforced: a client expecting `tool_use` blocks back gets a
    // named refusal rather than prose where it wanted a tool call.
    let server = common::start(&config(common::free_port()));
    let (status, body) = common::post_json(
        &format!("{}/v1/messages", server.base),
        r#"{"model":"default","max_tokens":16,"messages":[{"role":"user","content":"weather?"}],
            "tools":[{"name":"get_weather","description":"weather","input_schema":{"type":"object"}}]}"#,
    );
    assert_eq!(status, 400, "tools must be refused: {body}");
    let v: serde_json::Value = serde_json::from_str(&body).expect("error body is JSON");
    assert_eq!(v["type"], "error", "body: {body}");
    assert_eq!(v["error"]["type"], "invalid_request_error", "body: {body}");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("tool"),
        "the message must name the limitation: {body}"
    );
}

#[test]
fn malformed_requests_get_the_anthropic_error_envelope() {
    let server = common::start(&config(common::free_port()));
    let url = format!("{}/v1/messages", server.base);

    for (label, payload) in [
        (
            "missing model",
            r#"{"messages":[{"role":"user","content":"hi"}]}"#,
        ),
        ("missing messages", r#"{"model":"default"}"#),
        ("not json", r#"{"model":"default","#),
    ] {
        let (status, body) = common::post_json(&url, payload);
        assert_eq!(status, 400, "{label} should be 400: {body}");
        let v: serde_json::Value =
            serde_json::from_str(&body).unwrap_or_else(|_| panic!("{label}: non-JSON body {body}"));
        assert_eq!(v["type"], "error", "{label}: {body}");
        assert_eq!(
            v["error"]["type"], "invalid_request_error",
            "{label}: {body}"
        );
        // Anthropic's envelope, not OpenAI's: no bare top-level `error.code`
        // wrapper without the `type` discriminator above it.
        assert!(
            v["error"]["message"].is_string(),
            "{label}: message must be a string: {body}"
        );
    }
}

#[test]
fn wrong_method_on_messages_is_405_with_allow_post() {
    // U-17 house convention: every known path names the fix in an Allow header.
    let server = common::start(&config(common::free_port()));
    let (status, allow, body) = common::request("GET", &format!("{}/v1/messages", server.base));
    assert_eq!(status, 405, "GET /v1/messages should be 405: {body}");
    assert_eq!(allow.as_deref(), Some("POST"), "Allow should be POST");
}
