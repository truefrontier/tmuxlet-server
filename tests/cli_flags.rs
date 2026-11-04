//! CLI flag handling: --version, --help, --validate (ok + fail), bad args.
use std::io::Write;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_tmuxlet-server"))
}

fn write_config(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.toml");
    std::fs::File::create(&path)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    (dir, path)
}

const GOOD: &str = r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.d]
order = ["echo"]
"#;

#[test]
fn version_flag_prints_and_exits_zero() {
    let out = bin().arg("--version").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("tmuxlet-server"));
}

#[test]
fn help_flag_exits_zero() {
    let out = bin().arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Usage"));
}

#[test]
fn validate_ok_on_good_config() {
    let (_dir, cfg) = write_config(GOOD);
    let out = bin()
        .arg("--validate")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("OK"));
}

#[test]
fn validate_fails_on_chain_referencing_unknown_backend() {
    let (_dir, cfg) = write_config(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"

[backends.echo]
type = "cli"
bin = "/bin/echo"

[chains.d]
order = ["nope"]
"#,
    );
    let out = bin()
        .arg("--validate")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected validation failure");
}

#[test]
fn unknown_arg_exits_nonzero() {
    let out = bin().arg("--bogus").output().unwrap();
    assert!(!out.status.success());
}

/// Run `--check-backends` against `body`; returns (success, stdout).
/// Configs must set `env_source = "process"` so the probe never runs the
/// developer's interactive shell.
fn check_backends(body: &str) -> (bool, String) {
    let (_dir, cfg) = write_config(body);
    let out = bin()
        .arg("--check-backends")
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn check_backends_resolves_a_bare_cli_bin_on_path() {
    // `bin = "sh"` runs fine at dispatch (the child's PATH finds it), and
    // examples/server-router-framework.toml ships `bin = "agy"` with a comment
    // saying it is resolved on PATH. --check-backends must resolve it the same
    // way instead of testing a cwd-relative path and reporting a false FAIL.
    let (ok, stdout) = check_backends(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"
env_source = "process"

[backends.shell]
type = "cli"
bin = "sh"

[chains.d]
order = ["shell"]
"#,
    );
    assert!(ok, "a bare bin on PATH must PASS: {stdout}");
    assert!(stdout.contains("PASS shell (cli)"), "stdout: {stdout}");
}

#[test]
fn check_backends_fails_a_missing_cli_bin_and_exits_nonzero() {
    let (ok, stdout) = check_backends(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"
env_source = "process"

[backends.ghost]
type = "cli"
bin = "/nonexistent/tmuxlet-test-ghost"

[chains.d]
order = ["ghost"]
"#,
    );
    assert!(!ok, "a missing bin must fail the probe: {stdout}");
    assert!(
        stdout.contains("FAIL ghost (cli)") && stdout.contains("not found or not executable"),
        "stdout: {stdout}"
    );
}

#[test]
fn check_backends_probes_api_reachability() {
    // One upstream is listening, one is a port nobody holds: the probe is a
    // bare TCP connect, so no HTTP request is ever sent.
    let up = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let up_port = up.local_addr().unwrap().port();
    let down_port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }; // listener dropped: nothing accepts on down_port
    let (ok, stdout) = check_backends(&format!(
        r#"
[server]
listen = "127.0.0.1:1"
default_chain = "d"
env_source = "process"

[backends.up]
type = "api"
base_url = "http://127.0.0.1:{up_port}/v1"
model = "m"

[backends.down]
type = "api"
base_url = "http://127.0.0.1:{down_port}/v1"
model = "m"

[chains.d]
order = ["up", "down"]
"#
    ));
    assert!(
        !ok,
        "one unreachable backend fails the whole probe: {stdout}"
    );
    assert!(
        stdout.contains(&format!("PASS up (api) — connected 127.0.0.1:{up_port}")),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "FAIL down (api) — cannot connect 127.0.0.1:{down_port}"
        )),
        "stdout: {stdout}"
    );
    drop(up);
}
