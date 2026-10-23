use super::{BackendError, CliBackend, DispatchResult};
use crate::env::Env;
use crate::pty;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Placeholder token in a cli backend's `args`. When present, the prompt is
/// substituted in place (e.g. `args = ["-p", "{prompt}"]` -> `-p <prompt>`)
/// instead of being appended / written to stdin. Required for tools like `agy`
/// whose `--print` flag takes the prompt as its value, not via stdin.
pub const PROMPT_PLACEHOLDER: &str = "{prompt}";

/// Substitute `{prompt}` in each arg. Returns the rewritten args and whether any
/// substitution happened, so the caller knows not to also pass the prompt by the
/// default channel (trailing positional for plain, stdin for PTY).
fn substitute_prompt(args: &[String], prompt: &str) -> (Vec<String>, bool) {
    let mut substituted = false;
    let out = args
        .iter()
        .map(|a| {
            if a.contains(PROMPT_PLACEHOLDER) {
                substituted = true;
                a.replace(PROMPT_PLACEHOLDER, prompt)
            } else {
                a.clone()
            }
        })
        .collect();
    (out, substituted)
}

/// Build the plain (non-stdin) argv from a `substitute_prompt` result: keep the
/// substituted args as-is, or append the prompt as the final positional argument
/// when no `{prompt}` placeholder consumed it. Taking the precomputed result lets
/// `dispatch` avoid substituting a second time.
fn plain_argv(mut args: Vec<String>, substituted: bool, prompt: &str) -> Vec<String> {
    if !substituted {
        args.push(prompt.into());
    }
    args
}

/// Run the CLI backend and return cleaned output. `pty=true` runs the binary in
/// a PTY (for TUI tools like agy); `pty=false` uses a plain piped child guarded
/// by a try_wait watchdog. Either way the output is stripped of ANSI/control
/// bytes via `pty::clean_output`. Both modes run in `b.cwd` (U-11).
pub fn dispatch(
    b: &CliBackend,
    prompt: &str,
    env: &Env,
    timeout: Duration,
) -> Result<DispatchResult, BackendError> {
    let content = if b.pty {
        // If args carry `{prompt}`, the prompt goes into argv and stdin stays
        // empty (so the PTY doesn't echo the prompt back into the output).
        // Otherwise the prompt is written to the child's stdin, as before.
        let (pty_args, substituted) = substitute_prompt(&b.args, prompt);
        let stdin_payload = if substituted { "" } else { prompt };
        let (cols, rows) = b.pty_size;
        let (raw, status, timed_out) = pty::run_in_pty(
            &b.bin.display().to_string(),
            &pty_args,
            &env.filtered_pairs(b.env_pass.as_deref()),
            &b.cwd.display().to_string(),
            stdin_payload,
            cols,
            rows,
            timeout,
        )
        .map_err(|e| BackendError::Spawn(b.name.clone(), e))?;
        // F-1: a watchdog kill is a timeout, even if partial output was
        // captured — never ship a truncated answer as success; the chain advances.
        if timed_out {
            return Err(BackendError::Timeout(b.name.clone()));
        }
        let cleaned = pty::clean_output(&raw);
        // A non-zero exit with no usable output is a failure, so the fallback
        // chain advances to the next backend (mirrors the non-PTY branch).
        if !status.success() && cleaned.trim().is_empty() {
            return Err(BackendError::Exit(
                b.name.clone(),
                status.exit_code() as i32,
                String::new(),
            ));
        }
        cleaned
    } else {
        // S-4: with stdin_prompt the prompt goes to the child's stdin and stays
        // out of argv; otherwise plain_argv substitutes/appends it, reusing the
        // single substitution above (a `{prompt}` placeholder always wins and
        // forces the prompt into argv).
        let (subst_args, substituted) = substitute_prompt(&b.args, prompt);
        let use_stdin = b.stdin_prompt && !substituted;
        let argv = if use_stdin {
            subst_args
        } else {
            plain_argv(subst_args, substituted, prompt)
        };
        let mut child = Command::new(&b.bin)
            .args(&argv)
            .current_dir(&b.cwd)
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
        // Write the prompt to stdin on a thread so a large prompt can't deadlock
        // against a child that hasn't started draining it yet.
        if use_stdin && let Some(mut si) = child.stdin.take() {
            let payload = prompt.to_string();
            thread::spawn(move || {
                use std::io::Write;
                let _ = si.write_all(payload.as_bytes());
                // `si` drops here, closing stdin (EOF).
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
        let deadline = super::poll_deadline(timeout);
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
        // Bounded collection (see tmuxlet backend): a grandchild holding the
        // pipe must not block the worker forever after the child has exited.
        let grace = Duration::from_secs(5);
        let cleaned = pty::clean_output(&orx.recv_timeout(grace).unwrap_or_default());
        let stderr =
            String::from_utf8_lossy(&erx.recv_timeout(grace).unwrap_or_default()).into_owned();
        // A non-zero exit with no usable stdout is a failure, so the fallback
        // chain advances to the next backend (mirrors the tmuxlet backend).
        if !status.success() && cleaned.trim().is_empty() {
            return Err(BackendError::Exit(
                b.name.clone(),
                status.code().unwrap_or(-1),
                stderr.lines().last().unwrap_or("").to_string(),
            ));
        }
        cleaned
    };
    Ok(DispatchResult::text(content, b.name.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cli(name: &str, bin: &str, args: Vec<String>, pty: bool) -> CliBackend {
        CliBackend {
            name: name.into(),
            bin: PathBuf::from(bin),
            args,
            pty,
            cwd: PathBuf::from("/tmp"),
            pty_size: (200, 50),
            allow_empty: false,
            env_pass: None,
            stdin_prompt: false,
            max_concurrent: None,
            prompt_mode: crate::proto::PromptMode::Transcript,
        }
    }

    #[test]
    fn plain_argv_appends_prompt_when_no_placeholder() {
        let b = cli("x", "/bin/echo", vec!["-n".into()], false);
        let (a, s) = substitute_prompt(&b.args, "hello");
        assert_eq!(
            plain_argv(a, s, "hello"),
            vec!["-n".to_string(), "hello".to_string()]
        );
    }

    #[test]
    fn plain_argv_substitutes_placeholder_instead_of_appending() {
        // When `{prompt}` appears in args, the prompt is substituted there and
        // NOT also appended as a trailing positional (which would double it).
        let b = cli(
            "x",
            "/bin/echo",
            vec!["-p".into(), "{prompt}".into()],
            false,
        );
        let (a, s) = substitute_prompt(&b.args, "hello world");
        assert_eq!(
            plain_argv(a, s, "hello world"),
            vec!["-p".to_string(), "hello world".to_string()]
        );
    }

    #[test]
    fn pty_dispatch_substitutes_placeholder_into_argv() {
        // A PTY tool whose flag takes the prompt as its value (like agy's
        // `-p <prompt>`) must receive the prompt in argv, not on stdin.
        let b = cli("echoer", "/bin/echo", vec!["{prompt}".into()], true);
        let env = Env::capture("process", "", 5);
        let r = dispatch(&b, "READYTOKEN", &env, Duration::from_secs(10)).unwrap();
        assert_eq!(r.content.trim(), "READYTOKEN");
    }

    #[test]
    fn pty_dispatch_fails_on_nonzero_exit_with_no_output() {
        // The fallback chain advances only if a failed backend surfaces as an
        // error. Use the `{prompt}` placeholder so the prompt goes to argv and
        // stdin stays empty (no PTY echo); `exit 1` then produces no output.
        let b = cli(
            "failer",
            "/bin/sh",
            vec!["-c".into(), "exit 1".into(), "{prompt}".into()],
            true,
        );
        let env = Env::capture("process", "", 5);
        let err = dispatch(&b, "anything", &env, Duration::from_secs(10)).unwrap_err();
        assert!(
            matches!(err, BackendError::Exit(ref n, _, _) if n == "failer"),
            "expected Exit error naming the backend, got: {err:?}"
        );
    }
}
