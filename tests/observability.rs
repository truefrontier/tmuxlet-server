//! §5.2/§5.3 observability surface: `/v1/stats`, `POST /v1/stats/reset`, and the
//! opt-in `/metrics` exposition. These are integration tests on purpose — the
//! counters only mean anything once a real request has walked the decision layer
//! and a real chain, which is exactly the wiring unit tests cannot see.
mod common;

use std::collections::HashSet;

use serde_json::Value;

/// GET /v1/stats and parse it, asserting the 200 on the way through.
fn stats(server: &common::Server) -> Value {
    let (status, body) = common::get(&format!("{}/v1/stats", server.base));
    assert_eq!(status, 200, "/v1/stats must be 200: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("/v1/stats is not JSON ({e}): {body}"))
}

/// POST one non-streaming chat, returning (status, body).
fn chat(server: &common::Server, model: &str) -> (u16, String) {
    common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        &format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}]}}"#),
    )
}

/// Two cli backends in one chain. `echo` always answers, so `spare` is a
/// configured-but-never-dispatched backend — the case §5.2 says must still be
/// reported.
fn two_backend_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[backends.spare]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo", "spare"]
"#
    )
}

#[test]
fn a_fresh_server_reports_zero_traffic_and_every_configured_backend() {
    let server = common::start(&two_backend_config(common::free_port()));
    let s = stats(&server);

    assert!(
        s["uptime_secs"].is_u64(),
        "uptime_secs must be a number: {s}"
    );
    assert_eq!(s["requests"]["total"], 0, "{s}");
    assert_eq!(s["requests"]["ok"], 0, "{s}");
    assert_eq!(s["requests"]["failed"], 0, "{s}");
    assert_eq!(
        s["decisions"].as_object().map(serde_json::Map::len),
        Some(0),
        "decisions carries only sources actually seen, so a fresh server has none: {s}"
    );

    let backends = s["backends"]
        .as_object()
        .unwrap_or_else(|| panic!("backends must be an object: {s}"));
    assert_eq!(
        backends.keys().collect::<Vec<_>>(),
        vec!["echo", "spare"],
        "every configured backend is reported: {s}"
    );
    for (name, b) in backends {
        assert_eq!(b["requests"], 0, "{name}: {s}");
        assert_eq!(b["errors"], 0, "{name}: {s}");
        assert_eq!(b["overflow"], 0, "{name}: {s}");
        assert!(
            b["latency_ms"].is_null(),
            "{name}: no dispatch means no latency, not zero latency: {s}"
        );
        // Phase 1 counts quota; nothing configures or learns a limit yet, so the
        // two derived fields must read null rather than a made-up 0.
        assert_eq!(b["quota"]["rate_limited"], 0, "{name}: {s}");
        assert_eq!(b["quota"]["calls_in_window"], 0, "{name}: {s}");
        assert_eq!(b["quota"]["exhausted_secs_total"], 0, "{name}: {s}");
        assert!(b["quota"]["limit_hint"].is_null(), "{name}: {s}");
        assert!(b["quota"]["pressure"].is_null(), "{name}: {s}");
    }

    assert_eq!(s["classifier"]["calls"], 0, "{s}");
    assert_eq!(s["classifier"]["fail_open"], 0, "{s}");
}

#[test]
fn cost_defaults_from_the_backend_type() {
    // §7.3: the three types have three different marginal costs, and Phase 1
    // reports the default without any per-backend `cost` key existing yet. All
    // three sit in one chain so the config lints clean; `local` answers first,
    // so neither the api upstream nor the tmuxlet binary is ever needed.
    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.local]
type = "cli"
bin = "/bin/echo"

[backends.metered]
type = "api"
base_url = "http://127.0.0.1:1/v1"
model = "mock-model"

[backends.subscription]
type = "tmuxlet"
target = "claude"

[chains.default]
order = ["local", "metered", "subscription"]
"#
    );
    let server = common::start(&cfg);
    let s = stats(&server);
    assert_eq!(s["backends"]["local"]["cost"], "local", "cli: {s}");
    assert_eq!(s["backends"]["metered"]["cost"], "metered", "api: {s}");
    assert_eq!(
        s["backends"]["subscription"]["cost"], "subscription",
        "tmuxlet: {s}"
    );
}

#[test]
fn a_successful_chat_records_the_request_decision_and_backend_latency() {
    let server = common::start(&two_backend_config(common::free_port()));
    let (status, body) = chat(&server, "default");
    assert_eq!(status, 200, "body: {body}");

    let s = stats(&server);
    assert_eq!(s["requests"]["total"], 1, "{s}");
    assert_eq!(s["requests"]["ok"], 1, "{s}");
    assert_eq!(s["requests"]["failed"], 0, "{s}");
    assert_eq!(
        s["decisions"]["static"], 1,
        "the model named a chain, so the decision is static: {s}"
    );

    let echo = &s["backends"]["echo"];
    assert_eq!(echo["requests"], 1, "{s}");
    assert_eq!(echo["errors"], 0, "{s}");
    for q in ["p50", "p95", "max"] {
        assert!(
            echo["latency_ms"][q].is_u64(),
            "latency_ms.{q} must be a number once a sample lands: {s}"
        );
    }
    // The leg that never ran stays null — the reservoir is per-backend.
    assert_eq!(s["backends"]["spare"]["requests"], 0, "{s}");
    assert!(s["backends"]["spare"]["latency_ms"].is_null(), "{s}");
}

#[test]
fn a_failing_chain_records_a_failed_request_and_a_backend_error() {
    // An api backend pointed at a port nothing is listening on: the dispatch
    // fails at connect, the chain has nowhere to fall through to, and both the
    // request-level and the backend-level error counters must move.
    let port = common::free_port();
    let dead = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.deadapi]
type = "api"
base_url = "http://127.0.0.1:{dead}/v1"
model = "mock-model"

[chains.default]
order = ["deadapi"]
"#
    );
    let server = common::start(&cfg);
    let (status, body) = chat(&server, "default");
    assert_eq!(status, 503, "a dead upstream must 503: {body}");

    let s = stats(&server);
    assert_eq!(s["requests"]["total"], 1, "{s}");
    assert_eq!(s["requests"]["ok"], 0, "{s}");
    assert_eq!(s["requests"]["failed"], 1, "{s}");
    let backend = &s["backends"]["deadapi"];
    assert_eq!(backend["requests"], 1, "{s}");
    assert_eq!(backend["errors"], 1, "{s}");
    assert_eq!(
        backend["overflow"], 0,
        "a connect failure is not an overflow: {s}"
    );
}

#[test]
fn an_unknown_model_records_a_default_decision_not_a_static_one() {
    // DecisionSource::Default is deliberately not a synonym for Static: the
    // client asked for something we do not know, and the stats have to say so.
    let server = common::start(&two_backend_config(common::free_port()));
    let (status, body) = chat(&server, "no-such-model");
    assert_eq!(
        status, 200,
        "strict_models is off, so this falls back: {body}"
    );

    let s = stats(&server);
    assert_eq!(s["decisions"]["default"], 1, "{s}");
    assert!(
        s["decisions"].get("static").is_none(),
        "a fallback must not be counted as a static decision: {s}"
    );
}

#[test]
fn a_router_request_records_a_classifier_decision_and_a_classifier_call() {
    // Single-class router over /bin/echo: whatever the classifier echoes,
    // parse_class either finds "only" or falls back to it, so the decision is
    // classifier or fall_open — both are router provenance, and either way the
    // classifier round-trip itself must be counted and timed.
    let port = common::free_port();
    let cfg = format!(
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
    );
    let server = common::start(&cfg);
    let (status, body) = chat(&server, "smart");
    assert_eq!(status, 200, "body: {body}");

    let s = stats(&server);
    let decisions = s["decisions"].as_object().unwrap();
    assert!(
        decisions.contains_key("classifier") || decisions.contains_key("fall_open"),
        "a router request must record router provenance: {s}"
    );
    assert!(
        s["classifier"]["calls"].as_u64().unwrap_or(0) >= 1,
        "the classifier round-trip must be counted: {s}"
    );
    assert!(
        s["classifier"]["mean_ms"].is_u64(),
        "mean_ms must be numeric once a call has landed: {s}"
    );
}

#[test]
fn stats_reset_zeroes_the_counters_and_still_lists_every_backend() {
    let server = common::start(&two_backend_config(common::free_port()));
    let (status, body) = chat(&server, "default");
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(stats(&server)["requests"]["total"], 1, "precondition");

    let (rs, rb) = common::post_json(&format!("{}/v1/stats/reset", server.base), "");
    assert_eq!(rs, 200, "reset body: {rb}");
    assert_eq!(rb.trim(), r#"{"reset":true}"#, "reset body: {rb}");

    let s = stats(&server);
    assert_eq!(s["requests"]["total"], 0, "{s}");
    assert_eq!(s["requests"]["ok"], 0, "{s}");
    assert_eq!(
        s["decisions"].as_object().map(serde_json::Map::len),
        Some(0),
        "{s}"
    );
    assert_eq!(
        s["backends"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["echo", "spare"],
        "reset clears traffic, not the config-derived backend list: {s}"
    );
    assert_eq!(s["backends"]["echo"]["requests"], 0, "{s}");
    assert!(s["backends"]["echo"]["latency_ms"].is_null(), "{s}");
}

#[test]
fn metrics_is_404_metrics_disabled_when_the_key_is_absent() {
    // Off by default, and a 404 rather than a 403 — a scraper pointed at the
    // wrong server should read "no such endpoint", which is the truth.
    let server = common::start(&two_backend_config(common::free_port()));
    let (status, body) = common::get(&format!("{}/metrics", server.base));
    assert_eq!(status, 404, "body: {body}");
    assert!(body.contains("metrics_disabled"), "body: {body}");
    assert!(
        body.contains("metrics = true"),
        "the 404 must name its own remedy: {body}"
    );
}

fn metrics_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
metrics = true

[backends.echo]
type = "cli"
bin = "/bin/echo"

[backends.spare]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo", "spare"]
"#
    )
}

/// The metric name a sample line belongs to: everything before the label block
/// or the value separator.
fn sample_name(line: &str) -> &str {
    line.split(['{', ' ']).next().unwrap_or("")
}

/// Every label key in a sample line's `{...}` block. Walks key="value" pairs and
/// skips over quoted values (which may legally contain `,`, `=`, `{`, or `}`),
/// so this stays honest even for a backend named something hostile.
fn label_keys(line: &str) -> Vec<String> {
    let (Some(open), Some(close)) = (line.find('{'), line.rfind('}')) else {
        return Vec::new();
    };
    let mut rest = &line[open + 1..close];
    let mut keys = Vec::new();
    while let Some(eq) = rest.find('=') {
        keys.push(
            rest[..eq]
                .trim_matches(|c: char| c == ',' || c.is_whitespace())
                .to_string(),
        );
        let after = &rest[eq + 1..];
        let Some(start) = after.find('"') else { break };
        let bytes = after.as_bytes();
        let mut i = start + 1;
        let mut end = None;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => i += 2,
                b'"' => {
                    end = Some(i);
                    break;
                }
                _ => i += 1,
            }
        }
        let Some(end) = end else { break };
        rest = &after[end + 1..];
    }
    keys
}

#[test]
fn metrics_emits_help_and_type_before_every_metric_it_exposes() {
    let server = common::start(&metrics_config(common::free_port()));
    let (cs, cb) = chat(&server, "default");
    assert_eq!(cs, 200, "chat body: {cb}");

    let (status, headers, body) = common::send("GET", &format!("{}/metrics", server.base), &[]);
    assert_eq!(status, 200, "body: {body}");
    assert_eq!(
        headers.get("content-type").map(String::as_str),
        Some("text/plain; version=0.0.4; charset=utf-8"),
        "exposition format 0.0.4 is a content-type contract, not a suggestion"
    );

    let mut helped: HashSet<&str> = HashSet::new();
    let mut typed: HashSet<&str> = HashSet::new();
    let mut samples: HashSet<&str> = HashSet::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("# HELP ") {
            let name = rest.split_whitespace().next().unwrap_or("");
            assert!(!name.is_empty(), "empty HELP name: {line}");
            helped.insert(name);
        } else if let Some(rest) = line.strip_prefix("# TYPE ") {
            let mut parts = rest.split_whitespace();
            let name = parts.next().unwrap_or("");
            let kind = parts.next().unwrap_or("");
            assert!(
                matches!(
                    kind,
                    "counter" | "gauge" | "histogram" | "summary" | "untyped"
                ),
                "unknown metric type on: {line}"
            );
            typed.insert(name);
        } else if !line.trim().is_empty() && !line.starts_with('#') {
            let name = sample_name(line);
            assert!(
                helped.contains(name),
                "sample for {name} with no preceding # HELP:\n{body}"
            );
            assert!(
                typed.contains(name),
                "sample for {name} with no preceding # TYPE:\n{body}"
            );
            samples.insert(name);
        }
    }
    // Guard against a vacuously-passing parse: the traffic above must have
    // produced the core families.
    for name in [
        "tmuxlet_uptime_seconds",
        "tmuxlet_requests_total",
        "tmuxlet_decisions_total",
        "tmuxlet_backend_requests_total",
        "tmuxlet_backend_latency_ms",
        "tmuxlet_classifier_calls_total",
    ] {
        assert!(samples.contains(name), "no {name} series emitted:\n{body}");
    }
}

#[test]
fn metrics_labels_are_only_backend_decision_outcome_and_quantile() {
    // §5.3 cardinality discipline: no per-request or per-user value ever becomes
    // a label. This test is the thing that fails when someone adds `model=` or
    // `request_id=` "just for debugging".
    const ALLOWED: [&str; 4] = ["backend", "decision", "outcome", "quantile"];

    let server = common::start(&metrics_config(common::free_port()));
    // Two different decisions and a real dispatch, so the labelled families are
    // actually populated rather than trivially empty.
    let (cs, cb) = chat(&server, "default");
    assert_eq!(cs, 200, "chat body: {cb}");
    let (us, ub) = chat(&server, "no-such-model");
    assert_eq!(us, 200, "fallback body: {ub}");

    let (status, body) = common::get(&format!("{}/metrics", server.base));
    assert_eq!(status, 200, "body: {body}");

    let mut seen: HashSet<String> = HashSet::new();
    for line in body.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        for key in label_keys(line) {
            assert!(
                ALLOWED.contains(&key.as_str()),
                "unbounded label '{key}' on line: {line}"
            );
            seen.insert(key);
        }
    }
    for key in ALLOWED {
        assert!(
            seen.contains(key),
            "expected at least one {key}= series, so this assertion is not vacuous:\n{body}"
        );
    }
}

const TOKEN: &str = "s3cr3t-observability-token";

#[test]
fn auth_gates_stats_and_metrics_but_leaves_health_open() {
    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"
metrics = true
auth = true
auth_token_env = "TEST_AUTH_TOKEN"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["echo"]
"#
    );
    let server = common::start_with_env(&cfg, &[("TEST_AUTH_TOKEN", TOKEN)]);
    let auth = format!("Bearer {TOKEN}");
    let stats_url = format!("{}/v1/stats", server.base);
    let reset_url = format!("{}/v1/stats/reset", server.base);
    let metrics_url = format!("{}/metrics", server.base);

    for url in [&stats_url, &metrics_url] {
        let (status, _h, body) = common::send("GET", url, &[]);
        assert_eq!(status, 401, "{url} must require auth: {body}");
        assert!(body.contains("unauthorized"), "{url}: {body}");
    }
    let (reset_401, _h, reset_body) = common::send_body("POST", &reset_url, &[], "");
    assert_eq!(reset_401, 401, "reset must require auth: {reset_body}");

    for url in [&stats_url, &metrics_url] {
        let (status, _h, body) = common::send("GET", url, &[("Authorization", &auth)]);
        assert_eq!(status, 200, "{url} with a valid token: {body}");
    }
    let (reset_200, _h, reset_ok) =
        common::send_body("POST", &reset_url, &[("Authorization", &auth)], "");
    assert_eq!(reset_200, 200, "reset with a valid token: {reset_ok}");
    assert_eq!(reset_ok.trim(), r#"{"reset":true}"#, "{reset_ok}");

    // Liveness probes must not need a token, or orchestrators cannot check health.
    let (health, hbody) = common::get(&format!("{}/health", server.base));
    assert_eq!(health, 200, "health must stay open: {hbody}");
}

#[test]
fn wrong_method_on_an_observability_route_is_405_with_an_allow_header() {
    let server = common::start(&metrics_config(common::free_port()));

    let (status, allow, body) = common::request("GET", &format!("{}/v1/stats/reset", server.base));
    assert_eq!(status, 405, "GET /v1/stats/reset should be 405: {body}");
    assert_eq!(allow.as_deref(), Some("POST"), "Allow should be POST");
    assert!(body.contains("method_not_allowed"), "body: {body}");

    let (mstatus, mallow, mbody) = common::request("POST", &format!("{}/metrics", server.base));
    assert_eq!(mstatus, 405, "POST /metrics should be 405: {mbody}");
    assert_eq!(
        mallow.as_deref(),
        Some("GET, HEAD"),
        "Allow should be GET, HEAD"
    );
}
