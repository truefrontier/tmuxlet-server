//! §8 acceptance gates, observed from outside the process.
//!
//! A gate is only meaningful end to end: "the cheap leg answered but the chain
//! advanced anyway" is a claim about the dispatch loop, the session store, and
//! the judge backend agreeing. A unit test on `JudgeGate` can check the verdict
//! arithmetic — it cannot show which leg's text reached the client.
//!
//! So every assertion here is on the **served answer**. Each leg echoes a fixed
//! string, which makes the response body the one honest witness to which backend
//! actually ran. The judge is a `/bin/sh` one-liner, so no mock HTTP is needed
//! and no verdict is timing-dependent.
mod common;

use serde_json::Value;

/// A conversation id, so the streak has somewhere to live. Without one §8 says
/// `confirmations` degrades to 1.
const SESSION: &[(&str, &str)] = &[("x-tmuxlet-session-id", "conv-gate")];

/// The cheap-then-strong chain every runtime test here routes over.
const ESCALATING: &str = r#"["cheap", "strong"]"#;

const GATE_1: &str = r#"
[chains.escalating.gate]
type = "judge"
classifier = "judge"
confirmations = 1
"#;

const GATE_2: &str = r#"
[chains.escalating.gate]
type = "judge"
classifier = "judge"
confirmations = 2
"#;

/// A judge backend that returns `reply` as its whole verdict.
///
/// `stdin_prompt = true` keeps the prompt out of argv. That matters: the default
/// judge prompt contains both "ACCEPT" and "ESCALATE", and a judge that echoed
/// its own argv would parse as unusable for reasons the test never intended.
fn judge_saying(reply: &str) -> String {
    format!(
        r#"type = "cli"
bin = "/bin/sh"
args = ["-c", "echo {reply}"]
stdin_prompt = true"#
    )
}

/// Two legs that name themselves, plus a judge spelled by the caller. `order`
/// and `gate` are injected so one template covers "no gate", "gate", and the
/// empty-first-leg case.
fn config(port: u16, judge: &str, order: &str, gate: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "escalating"
env_source = "process"
# P-9 off. A gate rejection is recorded as a leg failure, so with cooldown on
# the cheap leg would be skipped on later turns for reasons that have nothing to
# do with §8 — and a latch test cannot tell a latch from a cooldown.
cooldown = false

[backends.cheap]
type = "cli"
bin = "/bin/sh"
args = ["-c", "echo cheap-answer"]
stdin_prompt = true

[backends.strong]
type = "cli"
bin = "/bin/sh"
args = ["-c", "echo strong-answer"]
stdin_prompt = true

[backends.empty]
type = "cli"
bin = "/bin/sh"
args = ["-c", "true"]
stdin_prompt = true

[backends.judge]
{judge}

[chains.escalating]
order = {order}
{gate}
"#
    )
}

/// POST one non-streaming turn and return the served answer. Panics unless the
/// turn succeeded — a 503 here is never the property under test.
fn turn(server: &common::Server, headers: &[(&str, &str)]) -> String {
    let (status, _h, body) = common::send_body(
        "POST",
        &format!("{}/v1/chat/completions", server.base),
        headers,
        r#"{"model":"escalating","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "chat must succeed: {body}");
    let v: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| panic!("response is not JSON ({e}): {body}"));
    v["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_else(|| panic!("no message content: {body}"))
        .trim()
        .to_string()
}

fn stats(server: &common::Server) -> Value {
    let (status, body) = common::get(&format!("{}/v1/stats", server.base));
    assert_eq!(status, 200, "/v1/stats must be 200: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("/v1/stats is not JSON ({e}): {body}"))
}

/// The judge shares the classifier counter — it is the same round-trip, and §8's
/// cost argument is measured on it.
fn judge_calls(server: &common::Server) -> u64 {
    stats(server)["classifier"]["calls"].as_u64().unwrap_or(0)
}

fn judge_fail_open(server: &common::Server) -> u64 {
    stats(server)["classifier"]["fail_open"]
        .as_u64()
        .unwrap_or(0)
}

/// Run `--validate` against `body`; returns (exited zero, stderr).
fn validate(body: &str) -> (bool, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::write(&path, body).unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tmuxlet-server"))
        .arg("--validate")
        .arg("--config")
        .arg(&path)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A validate-only config: the gate sub-table is the variable, everything else
/// is already known-good.
fn validate_config(order: &str, gate: &str) -> String {
    format!(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "escalating"

[backends.cheap]
type = "cli"
bin = "/bin/echo"

[backends.strong]
type = "cli"
bin = "/bin/echo"

[backends.judge]
type = "cli"
bin = "/bin/echo"

[chains.escalating]
order = {order}
{gate}
"#
    )
}

#[test]
fn a_chain_without_a_gate_serves_the_first_leg() {
    // V1's behavior, which §8 must not have changed: leg 0 answered, leg 0 wins.
    // The judge backend exists in this config and is still never asked.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        "",
    ));

    assert_eq!(turn(&server, SESSION), "cheap-answer");
    assert_eq!(
        judge_calls(&server),
        0,
        "no gate configured must mean no judge round-trip"
    );
}

#[test]
fn a_judge_that_accepts_leaves_the_chain_where_it_was() {
    // The accept path has to be proved separately from the no-gate path: both
    // serve leg 0, and only the call counter tells them apart. A gate that
    // silently never ran would pass the first assertion alone.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ACCEPT"),
        ESCALATING,
        GATE_2,
    ));

    assert_eq!(turn(&server, SESSION), "cheap-answer");
    assert_eq!(judge_calls(&server), 1, "the judge must actually have run");
    assert_eq!(
        turn(&server, SESSION),
        "cheap-answer",
        "a decline must not accumulate toward an escalation"
    );
}

#[test]
fn two_confirmations_serve_the_cheap_leg_once_before_escalating() {
    // §8's headline: one judge opinion is noise, two consecutive ones are a
    // verdict. Turn 1 is served from the cheap leg *despite* the escalate —
    // the answer is already bought and paid for.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        GATE_2,
    ));

    assert_eq!(
        turn(&server, SESSION),
        "cheap-answer",
        "escalate 1/2 must still serve the buffered reply"
    );
    assert_eq!(
        turn(&server, SESSION),
        "strong-answer",
        "escalate 2/2 must reject the leg and advance the chain"
    );
}

#[test]
fn a_confirmed_escalation_latches_the_session_and_stops_paying_the_judge() {
    // The whole point of the latch is cost. Once this conversation has reached
    // the same verdict twice, re-running the cheap leg and the judge on every
    // later turn buys nothing — so the counter must not move.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        GATE_2,
    ));

    assert_eq!(turn(&server, SESSION), "cheap-answer");
    assert_eq!(turn(&server, SESSION), "strong-answer");
    let after_confirm = judge_calls(&server);
    assert_eq!(after_confirm, 2, "one judge call per gated turn");

    assert_eq!(
        turn(&server, SESSION),
        "strong-answer",
        "a latched session must skip the cheap leg outright"
    );
    assert_eq!(
        judge_calls(&server),
        after_confirm,
        "a latched session must not re-pay for a verdict it already reached"
    );
}

#[test]
fn without_a_session_confirmations_degrade_to_one() {
    // §8: `confirmations > 1` needs somewhere to keep the streak. With nowhere,
    // the gate escalates on the first verdict rather than silently never
    // escalating — which would look exactly like the gate working.
    //
    // The same server answers both halves, so the only difference between them
    // is the session header, not the config.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        GATE_2,
    ));

    assert_eq!(
        turn(&server, &[]),
        "strong-answer",
        "no session: confirmations = 2 must degrade to 1 and escalate at once"
    );
    assert_eq!(
        turn(&server, SESSION),
        "cheap-answer",
        "with a session the same config must wait for a second confirmation"
    );
}

#[test]
fn one_confirmation_escalates_on_the_first_turn() {
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        GATE_1,
    ));

    assert_eq!(turn(&server, SESSION), "strong-answer");
}

#[test]
fn an_unusable_verdict_fails_open_and_never_escalates() {
    // A reply with neither verdict word is not a decline and not an escalate —
    // it is nothing. §8 says fail open, forever if need be: an outage in the
    // judge must never manufacture an escalation the judge never asked for.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("unsure"),
        ESCALATING,
        GATE_2,
    ));

    for turn_no in 1..=3 {
        assert_eq!(
            turn(&server, SESSION),
            "cheap-answer",
            "turn {turn_no}: an unusable verdict must not escalate"
        );
    }
    assert_eq!(
        judge_fail_open(&server),
        3,
        "every unusable reply must be counted as a fail-open"
    );
}

#[test]
fn an_unusable_verdict_holds_the_streak_instead_of_clearing_it() {
    // The sharp end of "fail open". Failing open must not *also* discard
    // evidence: an outage between two genuine escalate verdicts would otherwise
    // reset the count and quietly buy the cheap leg another turn.
    //
    // This judge says ESCALATE, then something unusable, then ESCALATE. With
    // confirmations = 2 the third turn must confirm — which is only true if the
    // streak survived the middle turn.
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("verdicts");
    std::fs::write(&counter, "0\n").unwrap();
    let c = counter.display();
    let judge = format!(
        r#"type = "cli"
bin = "/bin/sh"
args = ["-c", "read n < {c}; n=$((n+1)); echo $n > {c}; if [ $n = 2 ]; then echo unsure; else echo ESCALATE; fi"]
stdin_prompt = true"#
    );
    let server = common::start(&config(common::free_port(), &judge, ESCALATING, GATE_2));

    assert_eq!(turn(&server, SESSION), "cheap-answer", "escalate 1/2");
    assert_eq!(
        turn(&server, SESSION),
        "cheap-answer",
        "unusable: fails open, still 1/2"
    );
    assert_eq!(
        turn(&server, SESSION),
        "strong-answer",
        "the second genuine escalate must confirm — the streak was held, not cleared"
    );
    assert_eq!(judge_fail_open(&server), 1, "exactly one unusable reply");
}

#[test]
fn a_judge_outage_fails_open_without_latching() {
    // A judge that cannot even be spawned is the operational version of the
    // unusable reply. The chain must behave as if no gate were configured, and
    // must leave no latch behind for the turns that follow.
    let server = common::start(&config(
        common::free_port(),
        "type = \"cli\"\nbin = \"/nonexistent/tmuxlet-test-judge\"",
        ESCALATING,
        GATE_1,
    ));

    assert_eq!(turn(&server, SESSION), "cheap-answer");
    assert_eq!(
        turn(&server, SESSION),
        "cheap-answer",
        "a failed judge must not create a latch"
    );
    assert_eq!(
        judge_fail_open(&server),
        2,
        "both turns must have tried, and failed, to reach the judge"
    );
}

#[test]
fn the_last_leg_is_served_even_when_the_judge_would_escalate() {
    // There is nowhere to escalate to from the final leg, so the gate must not
    // run there. If it did, an eager judge would turn a perfectly good answer
    // into a 503 with every leg rejected.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ESCALATE"),
        ESCALATING,
        GATE_1,
    ));

    assert_eq!(turn(&server, &[]), "strong-answer");
    assert_eq!(
        judge_calls(&server),
        1,
        "the judge must be asked about leg 0 only"
    );
}

#[test]
fn an_empty_reply_is_rejected_even_with_a_judge_configured() {
    // `NonEmptyGate` is V1's hardcoded check and still runs unconditionally.
    // Configuring a judge must not replace it — and the judge must not be asked
    // to rate an answer that is not there.
    let server = common::start(&config(
        common::free_port(),
        &judge_saying("ACCEPT"),
        r#"["empty", "strong"]"#,
        r#"
[chains.escalating.gate]
type = "judge"
classifier = "judge"
"#,
    ));

    assert_eq!(turn(&server, SESSION), "strong-answer");
    assert_eq!(
        judge_calls(&server),
        0,
        "an empty leg is rejected before the judge is worth paying for"
    );
}

#[test]
fn validate_rejects_an_unknown_gate_type() {
    let (ok, stderr) = validate(&validate_config(
        ESCALATING,
        r#"
[chains.escalating.gate]
type = "vibes"
classifier = "judge"
"#,
    ));
    assert!(!ok, "expected validation failure, stderr: {stderr}");
    assert!(
        stderr.contains("gate type 'vibes' is unknown"),
        "message must name the bad type: {stderr}"
    );
}

#[test]
fn validate_rejects_a_gate_classifier_that_is_not_a_backend() {
    // The failure mode this catches is silent: an undefined classifier fails
    // open on every turn, so the gate looks configured and never gates.
    let (ok, stderr) = validate(&validate_config(
        ESCALATING,
        r#"
[chains.escalating.gate]
type = "judge"
classifier = "nope"
"#,
    ));
    assert!(!ok, "expected validation failure, stderr: {stderr}");
    assert!(
        stderr.contains("gate classifier 'nope' is not a defined backend"),
        "message must name the missing backend: {stderr}"
    );
}

#[test]
fn validate_rejects_a_gate_on_a_chain_with_nowhere_to_escalate() {
    let (ok, stderr) = validate(&validate_config(
        r#"["cheap"]"#,
        r#"
[chains.escalating.gate]
type = "judge"
classifier = "judge"
"#,
    ));
    assert!(!ok, "expected validation failure, stderr: {stderr}");
    assert!(
        stderr.contains("has a gate but only 1 leg(s)"),
        "message must say the gate has nowhere to advance to: {stderr}"
    );
}

#[test]
fn a_typoed_gate_key_is_named_by_the_unknown_key_lint() {
    // Serde ignores unknown keys, so `confirmation` would silently mean
    // "confirmations = 2, default" — a gate that escalates twice as slowly as
    // written, with nothing to show for it.
    let (ok, stderr) = validate(&validate_config(
        ESCALATING,
        r#"
[chains.escalating.gate]
type = "judge"
classifier = "judge"
confirmation = 1
"#,
    ));
    assert!(!ok, "expected validation failure, stderr: {stderr}");
    assert!(
        stderr.contains("unknown config key 'chains.escalating.gate.confirmation'"),
        "lint must name the full path to the typo: {stderr}"
    );
    assert!(
        stderr.contains("did you mean 'confirmations'?"),
        "lint must suggest the fix: {stderr}"
    );
}
