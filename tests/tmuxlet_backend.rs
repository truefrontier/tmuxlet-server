//! Integration coverage for the tmuxlet backend via a fake `tmuxlet` on PATH
//! (real tmuxlet isn't available in CI). Exercises spawn -> drain -> parse_output.
#![cfg(unix)]
mod common;

use std::os::unix::fs::PermissionsExt;

#[test]
fn tmuxlet_backend_parses_completed_json() {
    // Fake `tmuxlet` that ignores its args and emits a completed-status JSON.
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tmuxlet");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s' '{\"id\":\"r1\",\"target\":\"claude\",\"status\":\"completed\",\"output\":\"tmuxlet-marker-77\",\"cwd\":\"/tmp\",\"tmuxSession\":\"s\",\"completionSource\":\"hook\",\"elapsedMs\":1}'\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "tmux"
env_source = "process"

[backends.claude]
type = "tmuxlet"
target = "claude"

[chains.tmux]
order = ["claude"]
"#
    );
    let server = common::start_with_env(&cfg, &[("PATH", &path)]);
    let (status, body) = common::post_json(
        &format!("{}/v1/chat/completions", server.base),
        r#"{"model":"tmux","messages":[{"role":"user","content":"hi"}]}"#,
    );
    assert_eq!(status, 200, "body: {body}");
    assert!(
        "tmuxlet output missing: {body}"
    );
}

#[test]
fn tmuxlet_tools_protocol_holds_and_continues_via_send() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tmuxlet");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        &fake,
        r#"#!/bin/sh
if [ "$1" = "send" ]; then
  id="$2"
  base="${TMUXLET_HOME:-$HOME/.tmuxlet}"
  mkdir -p "$base/runs/$id"
  printf '%s' '{"kind":"text","content":"after-send"}' > "$base/runs/$id/turn.json"
  exit 0
fi
if [ "$1" = "stop" ]; then
  exit 0
fi
printf '%s' '{"id":"run1","target":"claude","status":"completed","output":"","cwd":"/tmp","tmuxSession":"s","completionSource":"hook","elapsedMs":1,"sessionHeld":true,"turnKind":"tool_calls","toolCalls":[{"id":"c1","name":"bash","arguments":"{\"command\":\"ls\"}"}]}'
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let port = common::free_port();
    let home_s = home.display().to_string();
    let tmuxlet_home = home.join(".tmuxlet");
    let tmuxlet_home_s = tmuxlet_home.display().to_string();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "tmux"
env_source = "process"

[backends.claude]
type = "tmuxlet"
target = "claude"
tools_protocol = true

[chains.tmux]
order = ["claude"]
"#
    );
    let server = common::start_with_env(
        &cfg,
        &[(
            "PATH",
            path.as_str(),
        ), ("HOME", home_s.as_str()), ("TMUXLET_HOME", tmuxlet_home_s.as_str())],
    );
    let url = format!("{}/v1/chat/completions", server.base);
    let (status, body) = {
        let (_s, _h, b) = common::send_body(
            "POST",
            &url,
            &[
                ("Content-Type", "application/json"),
                ("x-tmuxlet-session-id", "dsh-test-1"),
            ],
            r#"{"model":"tmux","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"bash","parameters":{}}}]}"#,
        );
        (_s, b)
    };
    assert_eq!(status, 200, "first turn body: {body}");
    assert!(
        body.contains("tool_calls") && body.contains("bash"),
        "expected tool_calls: {body}"
    );

    let (status2, body2) = {
        let (s, _h, b) = common::send_body(
            "POST",
            &url,
            &[
                ("Content-Type", "application/json"),
                ("x-tmuxlet-session-id", "dsh-test-1"),
            ],
            r#"{"model":"tmux","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":null},{"role":"tool","content":"ok"}]}"#,
        );
        (s, b)
    };
    assert_eq!(status2, 200, "continue body: {body2}");
    assert!(
        body2.contains("after-send"),
        "expected send continuation: {body2}"
    );
}

#[test]
fn tools_request_against_non_protocol_tmuxlet_is_400() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tmuxlet");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s' '{\"id\":\"r1\",\"target\":\"claude\",\"status\":\"completed\",\"output\":\"nope\",\"cwd\":\"/tmp\",\"tmuxSession\":\"s\",\"completionSource\":\"hook\",\"elapsedMs\":1}'\n",
    )
    .unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let port = common::free_port();
    let cfg = format!(
        r#"
[server]
listen = "127.0.0.1:{port}"
default_chain = "tmux"
env_source = "process"

[backends.claude]
type = "tmuxlet"
target = "claude"

[chains.tmux]
order = ["claude"]
"#
    );
    let server = common::start_with_env(&cfg, &[("PATH", &path)]);
    let url = format!("{}/v1/chat/completions", server.base);
    let (status, _h, body) = common::send_body(
        "POST",
        &url,
        &[("Content-Type", "application/json")],
        r#"{"model":"tmux","messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"bash"}}]}"#,
    );
    assert_eq!(status, 400, "body: {body}");
    assert!(
        body.contains("tools_protocol") || body.contains("tool_calls"),
        "400 should name the limitation: {body}"
    );
}
