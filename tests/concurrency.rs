//! U-20: per-backend concurrency caps — a saturated leg falls through, an
//! all-saturated chain 503s with the busy class, a slot is released after the
//! dispatch completes, /v1/backends reports "busy" under load, and a saturated
//! classifier degrades to the fallback class. All of this had zero coverage and
//! could be reverted with CI staying green.
mod common;

use std::thread;
use std::time::{Duration, Instant};

fn cap_config(port: u16) -> String {
    // `slow` sleeps then prints, holding its single slot long enough for a second
    // concurrent request to observe saturation. `echo` is the fast fallthrough.
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.slow]
type = "cli"
bin = "/bin/sh"
args = ["-c", "sleep 0.6; printf ok"]
max_concurrent = 1

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.default]
order = ["slow", "echo"]

[chains.solo]
order = ["slow"]
"#
    )
}

fn chat(base: &str, model: &str, content: &str) -> (u16, String) {
    common::post_json(
        &format!("{base}/v1/chat/completions"),
        &format!(r#"{{"model":"{model}","messages":[{{"role":"user","content":"{content}"}}]}}"#),
    )
}

#[test]
fn saturated_leg_falls_through_to_the_next_backend() {
    let server = common::start(&cap_config(common::free_port()));
    let base = server.base.clone();
    // A occupies slow's only slot for ~0.6s.
    let a = thread::spawn(move || chat(&base, "default", "AAA"));
    thread::sleep(Duration::from_millis(200));
    // B: slow is saturated → skip it (Busy) and get echo's answer (the prompt).
    let (bs, bb) = chat(&server.base, "default", "BBB-marker");
    assert_eq!(bs, 200, "B should fall through to echo: {bb}");
    assert!(
        bb.contains("BBB-marker"),
        "B must be answered by echo (which echoes the prompt): {bb}"
    );
    let (as_, _ab) = a.join().unwrap();
    assert_eq!(as_, 200, "A should complete via slow");
}

#[test]
fn all_saturated_chain_returns_503_busy() {
    let server = common::start(&cap_config(common::free_port()));
    let base = server.base.clone();
    let a = thread::spawn(move || chat(&base, "solo", "AAA"));
    thread::sleep(Duration::from_millis(200));
    // B: the only leg is saturated with no fallthrough → 503 all_backends_failed.
    let (bs, bb) = chat(&server.base, "solo", "BBB");
    assert_eq!(bs, 503, "all-saturated must 503: {bb}");
    assert!(bb.contains("busy"), "must name the busy class: {bb}");
    assert!(bb.contains("all_backends_failed"), "{bb}");
    let _ = a.join();
}

#[test]
fn slot_is_released_after_a_dispatch_completes() {
    // Sequential requests to the capped backend both succeed: if the slot leaked,
    // the second would be 503 busy (this is the RAII-guard decrement, end-to-end).
    let server = common::start(&cap_config(common::free_port()));
    let (s1, b1) = chat(&server.base, "solo", "one");
    assert_eq!(s1, 200, "first: {b1}");
    let (s2, b2) = chat(&server.base, "solo", "two");
    assert_eq!(s2, 200, "slot must be reusable after completion: {b2}");
}

#[test]
fn backends_endpoint_reports_busy_under_load() {
    let server = common::start(&cap_config(common::free_port()));
    let base = server.base.clone();
    let a = thread::spawn(move || chat(&base, "solo", "AAA"));
    thread::sleep(Duration::from_millis(200));
    let (_s, body) = common::get(&format!("{}/v1/backends", server.base));
    assert!(body.contains(r#""name":"slow""#), "{body}");
    assert!(
        body.contains(r#""state":"busy""#),
        "slow must report busy while its slot is held: {body}"
    );
    let _ = a.join();
}

fn router_cap_config(port: u16) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "default"
env_source = "process"

[backends.slowclass]
type = "cli"
bin = "/bin/sh"
args = ["-c", "sleep 0.6; printf research"]
max_concurrent = 1

[backends.echo]
type = "cli"
bin = "/bin/echo"

[routers.smart]
classifier = "slowclass"
fallback_class = "quick"
routes = {{ research = "echo", quick = "echo" }}

[chains.default]
order = ["echo"]
"#
    )
}

#[test]
fn classifier_at_capacity_degrades_to_the_fallback_class() {
    let server = common::start(&router_cap_config(common::free_port()));
    let base = server.base.clone();
    // A occupies the slow, capped classifier for ~0.6s.
    let a = thread::spawn(move || {
        common::post_headers(
            &format!("{base}/v1/chat/completions"),
            r#"{"model":"smart","messages":[{"role":"user","content":"A"}]}"#,
        )
    });
    thread::sleep(Duration::from_millis(200));
    // B: the classifier is saturated → degrade to fallback_class "quick" WITHOUT
    // dispatching it (so B doesn't wait ~0.6s), routing via the fallback class.
    let t = Instant::now();
    let (bs, bh, bb) = common::post_headers(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"smart","messages":[{"role":"user","content":"B"}]}"#,
    );
    let elapsed = t.elapsed();
    assert_eq!(bs, 200, "B must still succeed: {bb}");
    let route = bh
        .get("x-tmuxlet-route")
        .expect("router response must carry x-tmuxlet-route");
    assert!(
        route.starts_with("smart/quick/"),
        "B must route via the fallback class (§5.1: model/class/target/backend): {route}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "B must not block on the saturated classifier: {elapsed:?}"
    );
    let _ = a.join();
}
