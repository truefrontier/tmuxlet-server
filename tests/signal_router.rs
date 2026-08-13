//! §7 the signal router and the §7.3 cost model, observed from outside the
//! process.
//!
//! The scorer and the signal extractor have thorough unit tests; what those
//! cannot see is the wiring. Quota pressure is a fact about `Stats`, the
//! agentic constraint is a fact about `Config`, the chosen rung shows up only in
//! a response header, and "leg 0 is out of quota" must reach `/v1/backends`
//! without looking like ill-health. Every one of those claims spans three
//! modules, so it is asserted here against a real server.
//!
//! **Pressure is driven with `quota_calls`, never with a clock.** A backend's
//! pressure is `calls_in_window / quota_calls`, and every dispatch increments
//! the numerator, so `quota_calls = 2` makes turn 2 sit on exactly 0.50 and
//! turn 3 on exactly 1.00. Nothing here sleeps or races.
mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

use serde_json::Value;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// What a chat request reveals about the routing decision behind it.
#[derive(Debug)]
struct Routed {
    /// `x-tmuxlet-decision` — which rung of the §7.7 cascade fired.
    decision: String,
    /// `x-tmuxlet-backend` — the leg that actually answered.
    backend: String,
    /// `x-tmuxlet-rationale` — the human-readable why. Present because these
    /// servers bind loopback, so S-6 redaction is off.
    rationale: String,
}

/// POST one non-streaming chat carrying a verbatim `messages` array.
///
/// The array is passed through as written rather than built from a typed
/// value: `signals::extract` reads the request body as it arrived, so the exact
/// `tool_calls` / `role:"tool"` shapes are the input under test.
fn route(server: &common::Server, model: &str, messages: &str) -> Routed {
    let (status, headers, body) = common::post_headers(
        &format!("{}/v1/chat/completions", server.base),
        &format!(r#"{{"model":"{model}","messages":{messages}}}"#),
    );
    assert_eq!(status, 200, "chat must succeed: {body}");
    let get = |name: &str| {
        headers
            .get(name)
            .unwrap_or_else(|| panic!("no {name} header: {headers:?}"))
            .clone()
    };
    Routed {
        decision: get("x-tmuxlet-decision"),
        backend: get("x-tmuxlet-backend"),
        rationale: headers
            .get("x-tmuxlet-rationale")
            .cloned()
            .unwrap_or_default(),
    }
}

fn stats(server: &common::Server) -> Value {
    let (status, body) = common::get(&format!("{}/v1/stats", server.base));
    assert_eq!(status, 200, "/v1/stats must be 200: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("/v1/stats is not JSON ({e}): {body}"))
}

/// §7.5 pressure for one backend, as `/v1/stats` reports it. Reading it back
/// out is what makes the threshold tests self-verifying: an assertion about
/// "exactly at the reserve" is worthless if the pressure silently drifted.
fn pressure(server: &common::Server, backend: &str) -> f64 {
    let s = stats(server);
    s["backends"][backend]["quota"]["pressure"]
        .as_f64()
        .unwrap_or_else(|| panic!("no numeric pressure for '{backend}': {s}"))
}

/// One backend's `/v1/backends` entry.
fn backend_health(server: &common::Server, name: &str) -> Value {
    let (status, body) = common::get(&format!("{}/v1/backends", server.base));
    assert_eq!(status, 200, "/v1/backends must be 200: {body}");
    let entries: Vec<Value> = serde_json::from_str(&body).expect("/v1/backends is JSON");
    entries
        .into_iter()
        .find(|e| e["name"] == name)
        .unwrap_or_else(|| panic!("no '{name}' in /v1/backends: {body}"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

/// A two-leg `signals` router over `/bin/echo`. `flat` is leg 0 — the flat-rate
/// backend rationing exists to protect — and `cheap` is the shed target.
///
/// `flat_extra` carries the quota keys that drive pressure, `cheap_extra` the
/// `agentic` declaration, and `router_extra` the thresholds under test.
fn two_leg(port: u16, flat_extra: &str, cheap_extra: &str, router_extra: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "pair"
env_source = "process"

[backends.flat]
type = "cli"
bin = "/bin/echo"
{flat_extra}

[backends.cheap]
type = "cli"
bin = "/bin/echo"
{cheap_extra}

[chains.pair]
order = ["flat", "cheap"]

[routers.auto]
type = "signals"
chain = "pair"
{router_extra}
"#
    )
}

// ---------------------------------------------------------------------------
// Request bodies
// ---------------------------------------------------------------------------
//
// Every assistant message that carries `tool_calls` also carries an explicit
// `"content": ""`. `proto::openai::ChatMessage::content` is not optional, so a
// message omitting it (or sending `null`, as OpenAI clients do) fails to
// deserialize with a 400 before routing is ever reached. That is worth knowing
// when reading these fixtures: they are not quite the shape a real OpenAI
// client emits.

/// §7.8: a plain chat client. No tool history, score 0.0, leg 0 every time.
const PLAIN: &str = r#"[{"role":"user","content":"hi"}]"#;

/// A compacted context. The marker is claude-code's auto-compact preamble,
/// `"this session is being continued from a previous conversation"` — the first
/// entry in `signals::COMPACTION_MARKERS`. It is only honored in the **prompt
/// prefix** (leading system/developer messages plus the first user turn), which
/// is why it sits in the system message here.
const COMPACTED: &str = r#"[
  {"role":"system","content":"This session is being continued from a previous conversation that ran out of context. The summary follows."},
  {"role":"user","content":"keep going"}
]"#;

/// A `role:"tool"` result carrying a critical error. `connection refused` is
/// severity 1.0 in the §7.1 table, which trips `should_escalate` outright — no
/// confidence threshold involved.
const CRITICAL_TOOL_RESULT: &str = r#"[
  {"role":"user","content":"start the worker"},
  {"role":"assistant","content":"","tool_calls":[{"id":"c1","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"psql -h localhost\"}"}}]},
  {"role":"tool","tool_call_id":"c1","content":"psql: error: connection to server at \"localhost\" failed: Connection refused"}
]"#;

/// Nine messages of an agent reading around and producing nothing: `turn_depth`
/// 9 clears the scorer's `STALL_MIN_TURN_DEPTH` of 8, the recent window holds
/// three reads and no writes or edits, and no result reports an error. That is
/// the §7.2 *exploring* shape — worth exactly one signal, `tanh(0.5) = +0.46`.
const EXPLORING: &str = r#"[
  {"role":"user","content":"figure out how routing works"},
  {"role":"assistant","content":"","tool_calls":[{"id":"r1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"src/route/mod.rs\"}"}}]},
  {"role":"tool","tool_call_id":"r1","content":"pub struct Decision { ... }"},
  {"role":"assistant","content":"","tool_calls":[{"id":"r2","type":"function","function":{"name":"Grep","arguments":"{\"pattern\":\"DecisionSource\"}"}}]},
  {"role":"tool","tool_call_id":"r2","content":"src/route/mod.rs:140 src/http.rs:602"},
  {"role":"assistant","content":"","tool_calls":[{"id":"r3","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"src/http.rs\"}"}}]},
  {"role":"tool","tool_call_id":"r3","content":"fn decision_headers(...) -> Vec<(&str, String)> { ... }"},
  {"role":"assistant","content":"Still mapping the call graph."},
  {"role":"user","content":"keep going"}
]"#;

/// An agent mid-edit. One `Edit` call and its result puts `recent_ops` above
/// zero, which is the precondition for §7.6's agentic constraint.
const EDITING: &str = r#"[
  {"role":"user","content":"rename the field"},
  {"role":"assistant","content":"","tool_calls":[{"id":"e1","type":"function","function":{"name":"Edit","arguments":"{\"file_path\":\"src/config.rs\"}"}}]},
  {"role":"tool","tool_call_id":"e1","content":"applied 1 edit to src/config.rs"},
  {"role":"user","content":"now the call sites"}
]"#;

// ---------------------------------------------------------------------------
// The §7.7 cascade
// ---------------------------------------------------------------------------

#[test]
fn a_plain_chat_client_lands_on_leg_zero_every_turn() {
    // §7.8, and the reason the signal router is safe to switch on: with no tool
    // history the score is 0.0, and with no quota configured pressure is
    // unknown (0.0). The entire cascade is rung 4 — one comparison and a
    // return — and it must stay that way turn after turn.
    let server = common::start(&two_leg(common::free_port(), "", "", ""));
    for turn in 1..=3 {
        let r = route(&server, "auto", PLAIN);
        assert_eq!(r.decision, "static", "turn {turn}: {r:?}");
        assert_eq!(r.backend, "flat", "turn {turn}: {r:?}");
    }
}

#[test]
fn a_reserve_threshold_of_one_never_rations() {
    // §12's null hypothesis, spelled as config: `reserve_threshold = 1.0` means
    // "always leg 0, fall through only on a real 429" — today's behavior, and
    // per §12 quite possibly the correct one. `quota_calls = 100` makes each
    // dispatch worth exactly one hundredth, so 99 warmup turns park pressure at
    // 0.99: as close to the wall as it gets without being at it, since rung 2
    // owns 1.0 outright and would fire whatever the reserve says.
    let server = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 100",
        "",
        "reserve_threshold = 1.0",
    ));
    for turn in 1..=99 {
        let r = route(&server, "auto", PLAIN);
        assert_eq!(r.backend, "flat", "warmup turn {turn}: {r:?}");
    }
    let p = pressure(&server, "flat");
    assert!(
        close(p, 0.99),
        "warmup must leave pressure at 0.99, got {p}"
    );

    let r = route(&server, "auto", PLAIN);
    assert_eq!(r.decision, "static", "{r:?}");
    assert_eq!(r.backend, "flat", "{r:?}");
}

#[test]
fn pressure_exactly_at_the_reserve_threshold_starts_rationing() {
    // The comparison in rung 4 is `pressure < reserve`, so sitting *on* the
    // threshold rations. `quota_calls = 2` puts turn 2 on exactly 0.50, which
    // is the boundary an off-by-one here would quietly move by a whole leg.
    let server = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 2",
        "",
        "reserve_threshold = 0.5",
    ));
    let first = route(&server, "auto", PLAIN);
    assert_eq!(first.decision, "static", "turn 1 has headroom: {first:?}");

    let p = pressure(&server, "flat");
    assert!(close(p, 0.5), "pressure must be exactly 0.50, got {p}");

    // Nothing is decisive about a plain chat turn, so rationing lands on rung 8.
    let r = route(&server, "auto", PLAIN);
    assert_eq!(r.decision, "fall_open", "{r:?}");
    assert_eq!(r.backend, "cheap", "{r:?}");
    assert!(
        r.rationale.contains("pressure 0.50"),
        "the rationale must name the pressure that caused the shed: {r:?}"
    );
}

#[test]
fn quota_exhaustion_skips_leg_zero_without_marking_it_unhealthy() {
    // §4.3/§7.5, and the distinction the whole cost model rests on: a backend
    // that is out of quota is *healthy*. It is skipped, not cooled, and
    // `/v1/backends` must keep saying so — otherwise the next operator to look
    // sees a failing backend and starts debugging the wrong thing.
    let server = common::start(&two_leg(common::free_port(), "quota_calls = 1", "", ""));
    let first = route(&server, "auto", PLAIN);
    assert_eq!(
        first.backend, "flat",
        "turn 1 spends the only call: {first:?}"
    );

    let p = pressure(&server, "flat");
    assert!(close(p, 1.0), "pressure must be 1.00, got {p}");

    let r = route(&server, "auto", PLAIN);
    assert_eq!(r.decision, "quota", "{r:?}");
    assert_eq!(r.backend, "cheap", "leg 0 is skipped: {r:?}");

    let flat = backend_health(&server, "flat");
    assert_eq!(
        flat["state"], "ok",
        "quota exhaustion is not ill-health: {flat}"
    );
    assert_eq!(flat["consecutive_failures"], 0, "{flat}");
    assert_eq!(flat["last_error"], Value::Null, "{flat}");
    assert_eq!(
        flat["cooling_secs"],
        Value::Null,
        "skipped, not cooled: {flat}"
    );
}

#[test]
fn a_compacted_context_forces_the_flat_rate_leg() {
    // §7.7 rung 3, checked ahead of the headroom test: once an agent's context
    // has been summarized the task belongs on the capable tier, and rationing
    // does not get a vote. The `PLAIN` turn in the middle is the control — at
    // the *same* pressure it sheds, so what pins the compacted turn to leg 0 is
    // the marker and nothing else.
    let server = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 2",
        "",
        "reserve_threshold = 0.5",
    ));
    assert_eq!(route(&server, "auto", PLAIN).decision, "static");
    assert!(close(pressure(&server, "flat"), 0.5));

    let control = route(&server, "auto", PLAIN);
    assert_eq!(control.decision, "fall_open", "control: {control:?}");
    assert_eq!(control.backend, "cheap", "control: {control:?}");

    let r = route(&server, "auto", COMPACTED);
    assert_eq!(r.decision, "override", "{r:?}");
    assert_eq!(r.backend, "flat", "{r:?}");
    assert!(
        r.rationale.contains("context was compacted"),
        "the rationale must say which override fired: {r:?}"
    );
}

#[test]
fn a_critical_tool_error_forces_the_flat_rate_leg() {
    // §7.7 rung 3 by the other route: severity 1.0. A connection-refused result
    // means retrying the same backend will not help, so the turn goes to the
    // capable leg regardless of what rationing would have preferred.
    let server = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 2",
        "",
        "reserve_threshold = 0.5",
    ));
    assert_eq!(route(&server, "auto", PLAIN).decision, "static");
    assert!(close(pressure(&server, "flat"), 0.5));

    let r = route(&server, "auto", CRITICAL_TOOL_RESULT);
    assert_eq!(r.decision, "override", "{r:?}");
    assert_eq!(r.backend, "flat", "{r:?}");
    assert!(
        r.rationale.contains("critical tool error"),
        "the rationale must say which override fired: {r:?}"
    );
}

#[test]
fn a_deep_investigating_turn_is_routed_by_its_score() {
    // §7.7 rung 6. `EXPLORING` is deep, reading, and producing nothing: exactly
    // one maxed signal, which scores `tanh(0.5) = +0.46`. That is *below* the
    // 0.5 default on purpose — one noisy signal must not flip a route — so this
    // router lowers `confidence_threshold` to 0.4 to make the rung reachable
    // with a single signal, which is what keeps the fixture readable.
    let server = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 2",
        "",
        "reserve_threshold = 0.5\nconfidence_threshold = 0.4",
    ));
    assert_eq!(route(&server, "auto", PLAIN).decision, "static");
    assert!(close(pressure(&server, "flat"), 0.5));

    let r = route(&server, "auto", EXPLORING);
    assert_eq!(r.decision, "dimensions", "{r:?}");
    assert_eq!(
        r.backend, "flat",
        "a positive score means the turn is worth the quota: {r:?}"
    );
    assert!(
        r.rationale.contains("score +0.46") && r.rationale.contains("confidence 0.46"),
        "the rationale must carry the score that decided it: {r:?}"
    );
}

#[test]
fn a_tool_running_turn_is_never_shed_to_a_text_only_leg() {
    // §7.6, the constraint with the worst failure mode in the whole phase.
    //
    // `codex` writing files is not substitutable by a text-only model returning
    // prose *about* writing files: the request "succeeds" and hands back the
    // wrong kind of artifact entirely. Spending quota you meant to save is
    // recoverable next window; silently returning prose where the caller
    // expected an edit is not. So a turn with tool history stays on leg 0 when
    // there is no agentic leg deeper, even at full pressure.
    let pinned = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 1",
        "agentic = false",
        "",
    ));
    assert_eq!(route(&pinned, "auto", PLAIN).backend, "flat");
    assert!(close(pressure(&pinned, "flat"), 1.0));

    let held = route(&pinned, "auto", EDITING);
    assert_eq!(
        held.backend, "flat",
        "a turn with recent tool ops must not be shed to a text-only leg: {held:?}"
    );
    assert_eq!(held.decision, "quota", "{held:?}");
    assert!(
        held.rationale.contains("no agentic-safe leg is deeper"),
        "the rationale must name the constraint that held it: {held:?}"
    );

    // The control, on the same server at the same pressure: without tool
    // history there is nothing to protect, so the identical chain sheds. This
    // is what proves the pin above is the agentic rule and not the chain shape.
    let shed = route(&pinned, "auto", PLAIN);
    assert_eq!(shed.decision, "quota", "control: {shed:?}");
    assert_eq!(shed.backend, "cheap", "control: {shed:?}");

    // And with the deeper leg declared agentic, the very same tool-running turn
    // sheds — the constraint is about capability, not about tool history alone.
    let agentic = common::start(&two_leg(
        common::free_port(),
        "quota_calls = 1",
        "agentic = true",
        "",
    ));
    assert_eq!(route(&agentic, "auto", PLAIN).backend, "flat");
    assert!(close(pressure(&agentic, "flat"), 1.0));

    let r = route(&agentic, "auto", EDITING);
    assert_eq!(r.decision, "quota", "{r:?}");
    assert_eq!(
        r.backend, "cheap",
        "an agentic shed target makes the same turn sheddable: {r:?}"
    );
}

#[test]
fn a_one_leg_chain_has_nothing_to_shed_to_and_stays_put() {
    // The degenerate chain. Rung 2 still fires and still says `quota`, but
    // there is nowhere deeper to go, so the honest move is to try the exhausted
    // leg and let the upstream answer with a real 429.
    let port = common::free_port();
    let server = common::start(&format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "solo"
env_source = "process"

[backends.flat]
type = "cli"
bin = "/bin/echo"
quota_calls = 1

[chains.solo]
order = ["flat"]

[routers.auto]
type = "signals"
chain = "solo"
"#
    ));
    assert_eq!(route(&server, "auto", PLAIN).decision, "static");
    assert!(close(pressure(&server, "flat"), 1.0));

    let r = route(&server, "auto", PLAIN);
    assert_eq!(r.decision, "quota", "{r:?}");
    assert_eq!(r.backend, "flat", "{r:?}");
}

// ---------------------------------------------------------------------------
// §7.3 the cost model
// ---------------------------------------------------------------------------

#[test]
fn cost_classes_default_per_backend_type_and_can_be_overridden() {
    // §7.3/§7.4. The defaults encode how the three types are actually used —
    // `tmuxlet` fronts a subscription, `cli` is local compute, `api` is metered
    // dollars — so most existing configs need no edit at all. An explicit
    // `cost` overrides, which is how you tell the router that your "api"
    // backend is really a free local gateway.
    let port = common::free_port();
    let server = common::start(&format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "d"
env_source = "process"

[backends.sub]
type = "tmuxlet"
target = "claude"

[backends.local]
type = "cli"
bin = "/bin/echo"

[backends.metered]
type = "api"
base_url = "http://127.0.0.1:1/v1"
model = "gpt-x"

[backends.gifted]
type = "api"
base_url = "http://127.0.0.1:1/v1"
model = "gpt-x"
cost = "free"

[chains.d]
order = ["local"]
"#
    ));
    let s = stats(&server);
    let cost = |name: &str| s["backends"][name]["cost"].clone();
    assert_eq!(cost("sub"), "subscription", "{s}");
    assert_eq!(cost("local"), "local", "{s}");
    assert_eq!(cost("metered"), "metered", "{s}");
    assert_eq!(cost("gifted"), "free", "an explicit cost wins: {s}");
}

#[test]
fn validate_rejects_an_unknown_cost_class() {
    // A typo here silently changes which backend rationing protects, which is
    // the kind of mistake that only shows up on the invoice.
    let (_dir, cfg) = write_config(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"
cost = "cheep"

[chains.d]
order = ["echo"]
"#,
    );
    let out = validate(&cfg);
    assert!(!out.status.success(), "expected validation failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cost 'cheep' is unknown"),
        "the error must name the bad value: {stderr}"
    );
    assert!(
        stderr.contains("subscription") && stderr.contains("metered"),
        "the error must list the valid classes: {stderr}"
    );
}

#[test]
fn validate_requires_a_chain_for_a_signals_router() {
    // §7.6: a signals router rations over one ordered chain, so the chain IS
    // the cost preference. Without one there is nothing to ration.
    const ROUTER: &str = r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.d]
order = ["echo"]

[routers.auto]
type = "signals"
"#;
    let (_dir, missing) = write_config(ROUTER);
    let out = validate(&missing);
    assert!(!out.status.success(), "expected validation failure");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("has no chain"),
        "the error must name the missing key: {stderr}"
    );

    // The same router with a chain is accepted — and needs no classifier,
    // routes, or fallback_class, which a `classifier` router would demand.
    let (_dir2, present) = write_config(&format!("{ROUTER}chain = \"d\"\n"));
    let out = validate(&present);
    assert!(
        out.status.success(),
        "a signals router with a chain must validate: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_learned_quota_limit_appears_in_stats_after_a_429() {
    // §7.5 tier 3, for the providers whose limit is opaque: the window that
    // just hit the wall IS the limit estimate. The dispatch is counted before
    // the failure is recorded, so the one call that earned the 429 becomes
    // `limit_hint = 1` — and pressure pins to 1.0 while the upstream's window
    // is still being enforced.
    let server_port = common::free_port();
    let mock_port = common::free_port();
    let _mock = mock_once(mock_port, 429, r#"{"error":{"message":"slow down"}}"#);

    let server = common::start(&format!(
        r#"
[server]
listen = "127.0.0.1:{server_port}"
default_chain = "pair"
env_source = "process"

[backends.upstream]
type = "api"
base_url = "http://127.0.0.1:{mock_port}/v1"
model = "mock-model"
quota_learn = true

[backends.cheap]
type = "cli"
bin = "/bin/echo"

[chains.pair]
order = ["upstream", "cheap"]

[routers.auto]
type = "signals"
chain = "pair"
"#
    ));
    // Leg 0 429s, the chain falls through to `cheap`, and the request succeeds.
    let r = route(&server, "auto", PLAIN);
    assert_eq!(r.decision, "static", "turn 1 has headroom: {r:?}");
    assert_eq!(r.backend, "cheap", "the 429 falls through: {r:?}");

    let s = stats(&server);
    let quota = &s["backends"]["upstream"]["quota"];
    assert_eq!(quota["rate_limited"], 1, "{s}");
    assert_eq!(
        quota["limit_hint"], 1,
        "learn_limit records the count that proved to be too many: {s}"
    );
    assert_eq!(
        quota["pressure"], 1.0,
        "an enforced 429 window is full pressure: {s}"
    );
}

// ---------------------------------------------------------------------------
// Local harness bits
// ---------------------------------------------------------------------------

/// Bind `port` and answer exactly one HTTP request with `status` and `body`.
/// Drains the request body first so the client's write completes cleanly.
fn mock_once(port: u16, status: u16, body: &str) -> thread::JoinHandle<()> {
    let response = format!(
        "HTTP/1.1 {status} Too Many Requests\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock upstream");
    thread::spawn(move || {
        let Ok((mut sock, _)) = listener.accept() else {
            return;
        };
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

fn write_config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::write(&path, body).unwrap();
    (dir, path)
}

fn validate(cfg: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tmuxlet-server"))
        .arg("--validate")
        .arg("--config")
        .arg(cfg)
        .output()
        .expect("spawn tmuxlet-server --validate")
}
