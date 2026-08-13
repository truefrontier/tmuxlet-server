//! §6 per-conversation state, observed from outside the process: routing
//! affinity, its bounds, and per-session overflow exclusions.
//!
//! Affinity is a *cost* feature, so most of these tests are only meaningful end
//! to end — "the classifier was not called" is a claim about the whole request
//! path, and a unit test on `SessionStore` cannot make it. The exclusion tests
//! are here for the same reason: the store, the chain loop, and the header
//! plumbing have to agree before a leg is actually skipped.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;

use serde_json::Value;

/// POST one non-streaming chat with extra request headers, returning
/// (status, lowercased response headers, body).
fn chat(
    server: &common::Server,
    model: &str,
    headers: &[(&str, &str)],
) -> (u16, std::collections::HashMap<String, String>, String) {
    chat_saying(server, model, headers, "hi")
}

/// Like `chat`, but with control over the opening user message — which is the
/// input `message_hash_fallback` keys on.
fn chat_saying(
    server: &common::Server,
    model: &str,
    headers: &[(&str, &str)],
    text: &str,
) -> (u16, std::collections::HashMap<String, String>, String) {
    common::send_body(
        "POST",
        &format!("{}/v1/chat/completions", server.base),
        headers,
        &format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"{text}"}}]}}"#),
    )
}

/// The `x-tmuxlet-decision` of a request that must have succeeded.
fn decision_of(
    (status, headers, body): (u16, std::collections::HashMap<String, String>, String),
) -> String {
    assert_eq!(status, 200, "chat must succeed: {body}");
    headers
        .get("x-tmuxlet-decision")
        .unwrap_or_else(|| panic!("no x-tmuxlet-decision header: {headers:?}"))
        .clone()
}

fn stats(server: &common::Server) -> Value {
    let (status, body) = common::get(&format!("{}/v1/stats", server.base));
    assert_eq!(status, 200, "/v1/stats must be 200: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("/v1/stats is not JSON ({e}): {body}"))
}

fn classifier_calls(server: &common::Server) -> u64 {
    stats(server)["classifier"]["calls"].as_u64().unwrap_or(0)
}

/// A one-class router over `/bin/echo`, with room for extra `[server]` and
/// `[routers.smart]` keys. The classifier echoes its own prompt, which lists the
/// class names — with exactly one class that is a single substring hit, so the
/// first decision is a deterministic `classifier`.
fn router_config(port: u16, server_extra: &str, router_extra: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
{server_extra}

[backends.echo]
type = "cli"
bin = "/bin/echo"

[routers.smart]
classifier = "echo"
fallback_class = "only"
routes = {{ only = "default" }}
{router_extra}

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn the_same_session_id_reuses_the_first_turns_decision() {
    // §6.3, the headline behavior: turn 1 pays for a decision, turn 2 replays it
    // and says so. The route header must match — affinity that quietly routed
    // somewhere else would be worse than no affinity at all.
    let server = common::start(&router_config(common::free_port(), "", ""));

    let (s1, h1, b1) = chat(&server, "smart", &[("x-tmuxlet-session-id", "conv-1")]);
    assert_eq!(s1, 200, "body: {b1}");
    let first = h1["x-tmuxlet-decision"].clone();
    assert!(
        first == "classifier" || first == "fall_open",
        "turn 1 must actually decide, not replay: {first}"
    );

    let (s2, h2, b2) = chat(&server, "smart", &[("x-tmuxlet-session-id", "conv-1")]);
    assert_eq!(s2, 200, "body: {b2}");
    assert_eq!(h2["x-tmuxlet-decision"], "affinity", "turn 2 must replay");
    assert_eq!(
        h1["x-tmuxlet-route"], h2["x-tmuxlet-route"],
        "affinity must replay the SAME route, not merely skip the classifier"
    );
}

#[test]
fn an_affinity_hit_does_not_call_the_classifier_again() {
    // This is the entire cost argument for §6. On turn 12 of an agent run the
    // classifier would otherwise be asked to label "yes, continue" — a
    // round-trip that buys nothing. If this counter moves, the feature is
    // decorative.
    let server = common::start(&router_config(common::free_port(), "", ""));

    assert_eq!(classifier_calls(&server), 0, "fresh server");
    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "c")])),
        "classifier"
    );
    let after_first = classifier_calls(&server);
    assert_eq!(after_first, 1, "turn 1 pays for exactly one classification");

    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "c")])),
        "affinity"
    );
    assert_eq!(
        classifier_calls(&server),
        after_first,
        "an affinity hit must not reach the classifier at all"
    );
}

#[test]
fn a_fall_open_decision_is_stored_so_a_flaky_classifier_is_not_re_hammered() {
    // Two classes over `/bin/echo`: the echoed prompt names both, so parse_class
    // sees an ambiguous reply and falls open — deterministically. §6.3 stores
    // that too, on the grounds that re-asking a classifier that just failed,
    // once per turn, is the opposite of what affinity is for.
    let server = common::start(&format!(
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
fallback_class = "alpha"
routes = {{ alpha = "default", beta = "default" }}

[chains.default]
order = ["echo"]
"#,
        port = common::free_port()
    ));

    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "c")])),
        "fall_open",
        "two classes both appear in the echoed prompt, so the reply is unusable"
    );
    let after_first = classifier_calls(&server);
    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "c")])),
        "affinity",
        "a stored fall_open must be replayed like any other decision"
    );
    assert_eq!(
        classifier_calls(&server),
        after_first,
        "the flaky classifier must not be called again"
    );
}

#[test]
fn every_recognized_session_header_grants_affinity_and_an_unknown_one_does_not() {
    // §6.1: a client that already tracks a conversation id should not have to
    // learn ours. Each harness-native header gets its own id, so what this
    // guards is a header silently ignored rather than headers merged.
    let server = common::start(&router_config(common::free_port(), "", ""));
    for name in [
        "x-tmuxlet-session-id",
        "x-claude-code-session-id",
        "x-session-id",
        "x-conversation-id",
    ] {
        let id = format!("id-for-{name}");
        let hdrs = [(name, id.as_str())];
        assert_ne!(
            decision_of(chat(&server, "smart", &hdrs)),
            "affinity",
            "{name}: the first turn of a new session cannot be a replay"
        );
        assert_eq!(
            decision_of(chat(&server, "smart", &hdrs)),
            "affinity",
            "{name} must be recognized as a session id"
        );
    }

    // Anything else is just a header. Recognizing arbitrary `*-id` headers would
    // make affinity depend on whatever a proxy happened to add.
    let hdrs = [("x-random-id", "not-a-session")];
    assert_ne!(decision_of(chat(&server, "smart", &hdrs)), "affinity");
    assert_ne!(
        decision_of(chat(&server, "smart", &hdrs)),
        "affinity",
        "an unrecognized header must not create a session"
    );
}

#[test]
fn two_different_session_ids_do_not_share_a_decision() {
    let server = common::start(&router_config(common::free_port(), "", ""));
    assert_ne!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "a")])),
        "affinity"
    );
    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "b")])),
        "classifier",
        "session b is a different conversation and must be classified on its own"
    );
    assert_eq!(
        decision_of(chat(&server, "smart", &[("x-tmuxlet-session-id", "a")])),
        "affinity",
        "and session a is still there"
    );
}

#[test]
fn a_blank_session_header_is_not_a_session() {
    // Treating "" as an id would merge every client that sends an empty header
    // into a single conversation — a correctness bug wearing a performance
    // win's clothes.
    //
    // ureq trims the value, so what reaches the server is a present-but-empty
    // `x-session-id:`. That is the branch worth guarding from out here; the
    // whitespace-only trim itself is a unit test in src/route/session.rs.
    let server = common::start(&router_config(common::free_port(), "", ""));
    let hdrs = [("x-session-id", "   ")];
    assert_ne!(decision_of(chat(&server, "smart", &hdrs)), "affinity");
    assert_ne!(
        decision_of(chat(&server, "smart", &hdrs)),
        "affinity",
        "a blank value must not key a session"
    );
    assert_eq!(
        stats(&server)["sessions"]["entries"],
        0,
        "and nothing must be stored for it"
    );
}

#[test]
fn session_affinity_false_classifies_every_turn() {
    let server = common::start(&router_config(
        common::free_port(),
        "",
        "session_affinity = false",
    ));
    let hdrs = [("x-tmuxlet-session-id", "conv-1")];
    assert_eq!(decision_of(chat(&server, "smart", &hdrs)), "classifier");
    assert_eq!(
        decision_of(chat(&server, "smart", &hdrs)),
        "classifier",
        "the opt-out must hold even with a perfectly good session id"
    );
    assert_eq!(
        classifier_calls(&server),
        2,
        "both turns pay for a classification"
    );
}

#[test]
fn message_hash_fallback_keys_a_session_on_the_opening_message() {
    // §6.1: opt-in, because it collides across independent conversations that
    // open with an identical prompt. Both halves are asserted here — the flag
    // has to be what makes the difference, not the request shape.
    let opening = "Refactor the auth module";

    let on = common::start(&router_config(
        common::free_port(),
        "",
        "message_hash_fallback = true",
    ));
    assert_ne!(
        decision_of(chat_saying(&on, "smart", &[], opening)),
        "affinity"
    );
    assert_eq!(
        decision_of(chat_saying(&on, "smart", &[], opening)),
        "affinity",
        "with the fallback on, an identical opening is the same conversation"
    );

    let off = common::start(&router_config(common::free_port(), "", ""));
    assert_ne!(
        decision_of(chat_saying(&off, "smart", &[], opening)),
        "affinity"
    );
    assert_ne!(
        decision_of(chat_saying(&off, "smart", &[], opening)),
        "affinity",
        "default is off: no session header, no session"
    );
    assert_eq!(
        stats(&off)["sessions"]["entries"],
        0,
        "and no state is accumulated for it"
    );
}

#[test]
fn stats_reports_the_session_map_size_and_its_configured_bounds() {
    // §6.2: the map is bounded, and an operator should be able to watch the
    // bound hold rather than take it on faith. Non-default values on purpose —
    // echoing the defaults back would pass even with nothing wired up.
    let server = common::start(&router_config(
        common::free_port(),
        "session_max = 33\nsession_ttl_secs = 111",
        "",
    ));

    let s = stats(&server);
    assert_eq!(s["sessions"]["entries"], 0, "fresh server: {s}");
    assert_eq!(s["sessions"]["max"], 33, "{s}");
    assert_eq!(s["sessions"]["ttl_secs"], 111, "{s}");

    let (status, _h, body) = chat(&server, "smart", &[("x-tmuxlet-session-id", "conv-1")]);
    assert_eq!(status, 200, "body: {body}");
    let s = stats(&server);
    assert_eq!(
        s["sessions"]["entries"], 1,
        "one conversation, one entry: {s}"
    );
}

#[test]
fn the_configured_session_max_is_a_cap_not_a_default() {
    // The §11 Phase 2 gate ("memory bound proven under a synthetic key flood")
    // is carried by the 10k-key unit test in src/route/session.rs. What that
    // test cannot see is whether the *configured* number reaches the store, so
    // this drives a value no default would produce.
    let server = common::start(&router_config(common::free_port(), "session_max = 1", ""));

    for i in 0..3 {
        let id = format!("conv-{i}");
        let (status, _h, body) = chat(&server, "smart", &[("x-tmuxlet-session-id", &id)]);
        assert_eq!(status, 200, "body: {body}");
    }
    let s = stats(&server);
    assert_eq!(s["sessions"]["max"], 1, "{s}");
    assert!(
        s["sessions"]["entries"].as_u64().unwrap_or(u64::MAX) <= 1,
        "three conversations must not outgrow a cap of 1: {s}"
    );
}

// ---- §4.3 + §6.3: per-session overflow exclusion ----------------------------
//
// Exclusions are a property of the chain, not of a router, so these configs
// have no `[routers]` table at all.

/// Bind `port` and answer exactly `times` HTTP requests with `response`, one at
/// a time, then drop the listener. Same read-drain-respond shape as the helper
/// in tests/routes.rs; the count is what lets a test prove a leg was *not*
/// contacted again — the budget is spent, so a repeat contact cannot succeed.
fn mock_upstream(port: u16, response: String, times: usize) -> thread::JoinHandle<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock upstream");
    thread::spawn(move || {
        for _ in 0..times {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // Read headers, then drain the Content-Length body so the client's
            // write completes cleanly before we respond and close.
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
        }
    })
}

fn http_response(status: u16, reason: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// The upstream shape `is_context_overflow` keys on: HTTP 400 naming "maximum
/// context length".
fn overflow_response() -> String {
    http_response(
        400,
        "Bad Request",
        r#"{"error":{"message":"This model's maximum context length is 8192 tokens"}}"#,
    )
}

fn ok_response(reply: &str) -> String {
    http_response(
        200,
        "OK",
        &format!(
            r#"{{"id":"mock","object":"chat.completion","choices":[{{"index":0,"message":{{"role":"assistant","content":"{reply}"}},"finish_reason":"stop"}}]}}"#
        ),
    )
}

/// Two `api` legs in one chain. Only `api` backends can produce `Overflow` —
/// the rule keys on an HTTP status.
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
fn an_overflowed_leg_is_skipped_for_the_rest_of_the_session() {
    // §6.3: the prompt only grows, so a leg that could not fit turn 1 will not
    // fit turn 2 — paying for the 400 again is pure latency. leg1 gets exactly
    // one listener, so a second contact could not have succeeded; the stats
    // count is what proves it was never attempted.
    let (p1, p2) = (common::free_port(), common::free_port());
    let _m1 = mock_upstream(p1, overflow_response(), 1);
    let _m2 = mock_upstream(p2, ok_response("from-leg2"), 2);
    let server = common::start(&two_leg_api_config(common::free_port(), p1, p2));
    let hdrs = [("x-tmuxlet-session-id", "conv-1")];

    let (s1, h1, b1) = chat(&server, "pair", &hdrs);
    assert_eq!(s1, 200, "leg2 must carry turn 1: {b1}");
    assert_eq!(h1["x-tmuxlet-backend"], "leg2");

    let (s2, h2, b2) = chat(&server, "pair", &hdrs);
    assert_eq!(s2, 200, "leg2 must carry turn 2: {b2}");
    assert_eq!(h2["x-tmuxlet-backend"], "leg2");

    let s = stats(&server);
    assert_eq!(
        s["backends"]["leg1"]["requests"], 1,
        "leg1 must be dispatched exactly once across both turns: {s}"
    );
    assert_eq!(
        s["backends"]["leg1"]["overflow"], 1,
        "that one dispatch was the overflow: {s}"
    );
    assert_eq!(
        s["backends"]["leg2"]["requests"], 2,
        "leg2 answered both turns: {s}"
    );
}

#[test]
fn an_excluded_leg_reports_why_when_every_leg_is_excluded() {
    // The skip is visible to the client, not just to the log: turn 2 never
    // touches an upstream (both mocks are spent), so the only way the body can
    // name a reason is if the chain skipped both legs itself.
    let (p1, p2) = (common::free_port(), common::free_port());
    let _m1 = mock_upstream(p1, overflow_response(), 1);
    let _m2 = mock_upstream(p2, overflow_response(), 1);
    let server = common::start(&two_leg_api_config(common::free_port(), p1, p2));
    let hdrs = [("x-tmuxlet-session-id", "conv-1")];

    let (s1, _h1, b1) = chat(&server, "pair", &hdrs);
    assert_eq!(s1, 400, "an all-overflow chain is a client error: {b1}");
    assert!(
        b1.contains(r#""code":"context_length_exceeded""#),
        "overflow code: {b1}"
    );

    let (s2, _h2, b2) = chat(&server, "pair", &hdrs);
    assert_eq!(
        s2, 503,
        "a chain with nothing left to try is all_backends_failed: {b2}"
    );
    assert_eq!(
        b2.matches("skipped: context overflow earlier this session")
            .count(),
        2,
        "both legs must say why they were skipped: {b2}"
    );

    // A different conversation is unaffected: the exclusion is per-session, and
    // this one has no idea the prompt ever overflowed. Both mocks are spent, so
    // its legs are contacted and fail on transport rather than being skipped.
    let (_s3, _h3, b3) = chat(&server, "pair", &[("x-tmuxlet-session-id", "conv-2")]);
    assert!(
        !b3.contains("skipped: context overflow earlier this session"),
        "conv-2 inherited conv-1's exclusions: {b3}"
    );
}
