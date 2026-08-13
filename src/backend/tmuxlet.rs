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
    a.push(prompt.into());
    a
}

/// Parse tmuxlet's JSON output. Success iff `status == "completed"` (verified
/// against tmuxlet 0.3.0 `result_status_code`; deviation #1). Non-JSON stdout
/// is treated as raw content.
pub fn parse_output(name: &str, stdout: &str) -> Result<String, BackendError> {
    let trimmed = stdout.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(v) => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "completed" {
                Ok(v.get("output")
                    .and_then(|o| o.as_str())
                    .unwrap_or("")
                    .to_string())
            } else {
                Err(BackendError::Backend(
                    name.into(),
                    format!("tmuxlet status={status}"),
                ))
            }
        }
        Err(_) => Ok(stdout.to_string()),
    }
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
    let content = parse_output(&b.name, &stdout)?;
    Ok(DispatchResult {
        content,
        model_label: b.name.clone(),
    })
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
}
