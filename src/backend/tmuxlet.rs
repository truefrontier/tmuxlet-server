use super::{BackendError, DispatchResult, TmuxletBackend};
use crate::env::Env;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Assemble the argv for `tmuxlet -p --target T --output-format json -C cwd
/// --timeout S [--target-arg A ...] <prompt>`.
pub fn build_args(b: &TmuxletBackend, prompt: &str, timeout_secs: u64) -> Vec<String> {
    let mut a = vec![
        "-p".into(),
        "--target".into(),
        b.target.clone(),
        "--output-format".into(),
        "json".into(),
        "-C".into(),
        b.cwd.display().to_string(),
        "--timeout".into(),
        timeout_secs.to_string(),
    ];
    for ta in &b.target_args {
        a.push("--target-arg".into());
        a.push(ta.clone());
    }
    if b.tools_protocol {
        a.push("--tools-protocol".into());
    }
    a.push(prompt.into());
    a
}

fn parse_emitted_calls(v: Option<&serde_json::Value>) -> Option<Vec<crate::backend::EmittedToolCall>> {
    let arr = v.and_then(|x| x.as_array())?;
    if arr.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for c in arr {
        let id = c
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let name = c
            .get("name")
            .and_then(|x| x.as_str())
            .or_else(|| {
                c.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })?
            .to_string();
        if name.is_empty() {
            continue;
        }
        let arguments = match c
            .get("arguments")
            .or_else(|| c.get("function").and_then(|f| f.get("arguments")))
        {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => "{}".into(),
        };
        let id = if id.is_empty() {
            format!("call_{}", out.len())
        } else {
            id
        };
        out.push(crate::backend::EmittedToolCall {
            id,
            name,
            arguments,
        });
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Parse tmuxlet's JSON stdout into a `DispatchResult` (text and/or tool_calls).
pub fn parse_result(name: &str, stdout: &str) -> Result<DispatchResult, BackendError> {
    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status != "completed" {
                return Err(BackendError::Backend(
                    name.into(),
                    format!("tmuxlet status={status}"),
                ));
            }
            let content = v
                .get("output")
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string();
            let session_held = v
                .get("sessionHeld")
                .and_then(|x| x.as_bool())
                .unwrap_or(false);
            let run_id = v.get("id").and_then(|x| x.as_str()).map(str::to_string);
            let mut tool_calls = parse_emitted_calls(v.get("toolCalls"));
            if tool_calls.is_none() {
                tool_calls = parse_emitted_calls(v.get("tool_calls"));
            }
            if v.get("turnKind").and_then(|x| x.as_str()) != Some("tool_calls")
                && tool_calls
                    .as_ref()
                    .is_none_or(|c| c.is_empty())
            {
                tool_calls = None;
            }
            Ok(DispatchResult {
                content,
                model_label: name.to_string(),
                tool_calls,
                session_held,
                run_id,
            })
        }
        Err(_) => Ok(DispatchResult::text(stdout.to_string(), name.to_string())),
    }
}

/// Parse tmuxlet's JSON output. Success iff `status == "completed"` (verified
/// against tmuxlet 0.3.0 `result_status_code`; deviation #1). Non-JSON stdout
/// is treated as raw content.
pub fn parse_output(name: &str, stdout: &str) -> Result<String, BackendError> {
    parse_result(name, stdout).map(|r| r.content)
}

/// Parse a `turn.json` written by `tmuxlet bridge turn` (kind tool_calls | text).
pub fn parse_turn_file(name: &str, raw: &str) -> Result<DispatchResult, BackendError> {
    let v: serde_json::Value = serde_json::from_str(raw.trim()).map_err(|e| {
        BackendError::Parse(name.into(), format!("turn.json: {e}"))
    })?;
    match v.get("kind").and_then(|k| k.as_str()) {
        Some("tool_calls") => {
            let calls = parse_emitted_calls(v.get("calls")).ok_or_else(|| {
                BackendError::Parse(name.into(), "turn.json tool_calls missing calls".into())
            })?;
            Ok(DispatchResult {
                content: String::new(),
                model_label: name.to_string(),
                tool_calls: Some(calls),
                session_held: true,
                run_id: None,
            })
        }
        Some("text") => Ok(DispatchResult {
            content: v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string(),
            model_label: name.to_string(),
            tool_calls: None,
            session_held: true,
            run_id: None,
        }),
        other => Err(BackendError::Parse(
            name.into(),
            format!("turn.json unknown kind: {other:?}"),
        )),
    }
}

fn turn_path(env: &Env, run_id: &str) -> std::path::PathBuf {
    let home = env
        .get("TMUXLET_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            env.get("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".tmuxlet"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".tmuxlet"));
    home.join("runs").join(run_id).join("turn.json")
}

fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && !run_id.contains('/')
        && !run_id.contains('\\')
        && !run_id.contains("..")
}

/// `tmuxlet send <id> <message>` then wait for the next `turn.json`.
pub fn continue_turn(
    b: &TmuxletBackend,
    run_id: &str,
    message: &str,
    env: &Env,
    timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    if !valid_run_id(run_id) {
        return Err(BackendError::Backend(
            b.name.clone(),
            "invalid held run id".into(),
        ));
    }
    let turn = turn_path(env, run_id);
    let _ = std::fs::remove_file(&turn);
    let mut child = Command::new(&b.bin)
        .args(["send", run_id, message])
        .env_clear()
        .envs(env.filtered_pairs(b.env_pass.as_deref()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
    let deadline = super::poll_deadline(timeout.saturating_add(Duration::from_secs(5)));
    loop {
        match child
            .try_wait()
            .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?
        {
            Some(s) => {
                if !s.success() {
                    return Err(BackendError::Backend(
                        b.name.clone(),
                        format!("tmuxlet send exited {}", s.code().unwrap_or(-1)),
                    ));
                }
                break;
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BackendError::Timeout(b.name.clone()));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
    loop {
        if let Ok(raw) = std::fs::read_to_string(&turn) {
            let mut result = parse_turn_file(&b.name, &raw)?;
            let _ = std::fs::remove_file(&turn);
            result.session_held = true;
            result.run_id = Some(run_id.to_string());
            result.model_label = b.name.clone();
            return Ok(result);
        }
        if Instant::now() >= deadline {
            return Err(BackendError::Timeout(b.name.clone()));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// Kill a held tmuxlet pane (`tmuxlet stop <id>`). Best-effort; eviction must not fail.
pub fn stop_run(bin: &std::path::Path, run_id: &str) {
    if !valid_run_id(run_id) {
        return;
    }
    let _ = Command::new(bin).args(["stop", run_id]).status();
}

/// Spawn `tmuxlet`, draining stdout/stderr on threads, with a watchdog backstop
/// (tmuxlet also self-limits via `--timeout`). Output is parsed by `parse_output`.
pub fn dispatch(
    b: &TmuxletBackend,
    prompt: &str,
    env: &Env,
    timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    let use_stdin = b.use_stdin;
    // S-4: when the installed tmuxlet supports it, pass the prompt via stdin
    // (`-p ... -`) instead of argv (which is ps-visible and E2BIG-prone for
    // large transcripts). Otherwise the prompt is the final positional.
    let positional = if use_stdin { "-" } else { prompt };
    let args = build_args(b, positional, timeout.as_secs());
    let mut child = Command::new(&b.bin)
        .args(&args)
        .env_clear()
        .envs(env.filtered_pairs(b.env_pass.as_deref()))
        .stdin(if use_stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?;
    if use_stdin && let Some(mut si) = child.stdin.take() {
        let payload = prompt.to_string();
        thread::spawn(move || {
            use std::io::Write;
            let _ = si.write_all(payload.as_bytes());
        });
    }

    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let (otx, orx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut out, &mut s);
        let _ = otx.send(s);
    });
    let (etx, erx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut s = Vec::new();
        let _ = std::io::Read::read_to_end(&mut err, &mut s);
        let _ = etx.send(s);
    });

    // Server-side watchdog backstop; tmuxlet also self-limits via --timeout.
    let deadline = super::poll_deadline(timeout.saturating_add(Duration::from_secs(5)));
    let status = loop {
        match child
            .try_wait()
            .map_err(|e| BackendError::Spawn(b.name.clone(), e.to_string()))?
        {
            Some(s) => break s,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BackendError::Timeout(b.name.clone()));
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    };

    // Bounded collection: the child has exited, but a grandchild that inherited
    // the pipe could keep it open. Don't block the worker forever on read_to_end;
    // give the drained output a short grace window, then move on.
    let grace = Duration::from_secs(5);
    let stdout = String::from_utf8_lossy(&orx.recv_timeout(grace).unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&erx.recv_timeout(grace).unwrap_or_default()).into_owned();
    if !status.success() && stdout.trim().is_empty() {
        return Err(BackendError::Exit(
            b.name.clone(),
            status.code().unwrap_or(-1),
            stderr.lines().last().unwrap_or("").to_string(),
        ));
    }
    parse_result(&b.name, &stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn b() -> TmuxletBackend {
        TmuxletBackend {
            name: "t".into(),
            bin: PathBuf::from("tmuxlet"),
            target: "claude".into(),
            target_args: vec!["--effort".into(), "max".into()],
            cwd: PathBuf::from("/tmp"),
            allow_empty: false,
            env_pass: None,
            use_stdin: false,
            max_concurrent: None,
            prompt_mode: crate::proto::PromptMode::Transcript,
            tools_protocol: false,
        }
    }

    #[test]
    fn assembles_args_with_target_args_and_timeout() {
        let a = build_args(&b(), "hello", 1800);
        assert_eq!(
            a,
            vec![
                "-p",
                "--target",
                "claude",
                "--output-format",
                "json",
                "-C",
                "/tmp",
                "--timeout",
                "1800",
                "--target-arg",
                "--effort",
                "--target-arg",
                "max",
                "hello"
            ]
        );
    }

    #[test]
    fn parses_completed_status() {
        let j = r#"{"id":"r1","target":"claude","status":"completed","output":"done","cwd":"/tmp","tmuxSession":"s","completionSource":"hook","elapsedMs":12}"#;
        assert_eq!(parse_output("t", j).unwrap(), "done");
    }

    #[test]
    fn non_completed_status_is_error() {
        let j = r#"{"id":"r1","target":"claude","status":"timeout","output":"partial","cwd":"/tmp","tmuxSession":"s","completionSource":"none","elapsedMs":9}"#;
        assert!(parse_output("t", j).is_err());
    }

    #[test]
    fn non_json_falls_back_to_raw() {
        assert_eq!(parse_output("t", "just text").unwrap(), "just text");
    }

    #[test]
    fn parse_result_reads_tool_calls_and_held_session() {
        let j = r#"{"id":"r9","status":"completed","output":"","sessionHeld":true,"turnKind":"tool_calls","toolCalls":[{"id":"c1","name":"bash","arguments":{"command":"ls"}}]}"#;
        let r = parse_result("t", j).unwrap();
        assert!(r.session_held);
        assert_eq!(r.run_id.as_deref(), Some("r9"));
        let calls = r.tool_calls.expect("tool_calls");
        assert_eq!(calls[0].name, "bash");
        assert!(calls[0].arguments.contains("ls"));
    }

    #[test]
    fn parse_turn_file_reads_text_kind() {
        let r = parse_turn_file("t", r#"{"kind":"text","content":"hi"}"#).unwrap();
        assert_eq!(r.content, "hi");
        assert!(r.session_held);
        assert!(r.tool_calls.is_none());
    }

    #[test]
    fn assembles_args_with_tools_protocol() {
        let mut b = b();
        b.tools_protocol = true;
        let a = build_args(&b, "hello", 30);
        assert!(a.iter().any(|x| x == "--tools-protocol"));
    }
}
