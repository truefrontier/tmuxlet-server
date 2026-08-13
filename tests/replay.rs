//! §12: the calibration replay harness, exercised as a black box.
//!
//! `--replay` scores a captured corpus offline — no model calls, no server, which
//! is why `route::signals` and `route::scorer` were kept pure. These tests run
//! the built binary against the fixtures in `tests/fixtures/replay/` and assert
//! the properties the calibration argument actually rests on. See
//! `docs/replay.md` for how to read the output and add a corpus.

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    // The integration-test binary lives in target/<profile>/deps/, so the server
    // binary is two levels up. Same trick tests/cli_flags.rs uses.
    let mut p = std::env::current_exe().expect("test exe path");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("tmuxlet-server")
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/replay")
        .join(name)
}

fn replay(name: &str) -> String {
    let out = Command::new(bin())
        .arg("--replay")
        .arg(fixture(name))
        .output()
        .expect("run --replay");
    assert!(
        out.status.success(),
        "--replay {name} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fixtures() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/replay");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no fixtures in {}", dir.display());
    names
}

/// `("conversation <id>", score, confidence)` for each line of a report.
fn conversations(report: &str) -> Vec<(String, f64, f64)> {
    report
        .lines()
        .filter(|l| l.starts_with("conversation "))
        .map(|l| {
            let field = |k: &str| -> f64 {
                l.split_whitespace()
                    .find_map(|t| t.strip_prefix(k))
                    .unwrap_or_else(|| panic!("no {k} in {l:?}"))
                    .parse()
                    .unwrap_or_else(|e| panic!("bad {k} in {l:?}: {e}"))
            };
            let id = l
                .trim_start_matches("conversation ")
                .split(':')
                .next()
                .unwrap()
                .to_string();
            (id, field("score="), field("confidence="))
        })
        .collect()
}

/// `(threshold, keep, shed)` per sweep line.
fn sweep(report: &str) -> Vec<(f64, u32, u32)> {
    report
        .lines()
        .filter(|l| l.starts_with("threshold "))
        .map(|l| {
            let mut tok = l.split_whitespace();
            tok.next(); // "threshold"
            let t: f64 = tok
                .next()
                .and_then(|v| v.trim_end_matches(':').parse().ok())
                .unwrap_or_else(|| panic!("no threshold value in {l:?}"));
            let n = |k: &str| -> u32 {
                l.split_whitespace()
                    .find_map(|x| x.strip_prefix(k))
                    .unwrap_or_else(|| panic!("no {k} in {l:?}"))
                    .parse()
                    .unwrap()
            };
            (t, n("keep="), n("shed="))
        })
        .collect()
}

#[test]
fn every_fixture_parses_and_reports_a_full_sweep() {
    for f in fixtures() {
        let report = replay(&f);
        assert!(
            !conversations(&report).is_empty(),
            "{f} produced no conversations"
        );
        let s = sweep(&report);
        assert_eq!(
            s.iter().map(|(t, _, _)| *t).collect::<Vec<_>>(),
            vec![0.0, 0.3, 0.5, 0.7, 0.9],
            "{f}: §12 sweeps a fixed threshold set"
        );
    }
}

#[test]
fn the_split_is_monotonic_in_the_threshold() {
    // A higher bar can only shed more turns, never fewer. If this ever fails the
    // sweep is not measuring what §12 says it measures, and no threshold read off
    // it means anything.
    for f in fixtures() {
        let s = sweep(&replay(&f));
        for w in s.windows(2) {
            assert!(
                w[1].2 >= w[0].2,
                "{f}: shed count fell from {} at {} to {} at {}",
                w[0].2,
                w[0].0,
                w[1].2,
                w[1].0
            );
            assert_eq!(
                w[0].1 + w[0].2,
                w[1].1 + w[1].2,
                "{f}: turns appeared/vanished"
            );
        }
    }
}

#[test]
fn a_conversation_with_no_tools_scores_exactly_zero() {
    // §7.8, and the honest limit of the whole approach: a plain chat client has
    // no tool history to read, so the scorer must say nothing rather than guess.
    // The `-deep` fixture is the one that matters — a long chat is otherwise
    // indistinguishable from an agent that stopped calling tools, and used to
    // score 0.462 as "spinning" until the fixtures caught it.
    for f in ["plain-chat.jsonl", "plain-chat-deep.jsonl"] {
        for (id, score, confidence) in conversations(&replay(f)) {
            assert_eq!(score, 0.0, "{f}/{id} must score 0.0");
            assert_eq!(confidence, 0.0, "{f}/{id} must have no confidence");
        }
    }
}

#[test]
fn the_published_calibration_holds_on_real_conversations() {
    // §7.2: one maxed signal → tanh(0.5) = 0.462; two corroborating → tanh(1.0) =
    // 0.762. That spacing is what makes `confidence_threshold = 0.5` mean "about
    // one-and-a-half signals", which is the property that keeps a single noisy
    // signal from flipping a route. Asserting it against fixtures rather than
    // synthetic structs proves the extractor and the scorer agree.
    let near = |a: f64, b: f64| (a - b).abs() < 1e-3;
    let spinning = conversations(&replay("spinning.jsonl"));
    assert!(
        spinning.iter().any(|(_, s, _)| near(*s, 0.462)),
        "a deep no-production run is one maxed signal: {spinning:?}"
    );
    let recovery = conversations(&replay("error-recovery.jsonl"));
    assert!(
        recovery.iter().any(|(_, s, _)| near(*s, 0.762)),
        "a hard error in a stalled run is two corroborating signals: {recovery:?}"
    );
}

#[test]
fn production_pulls_the_score_negative() {
    // The one axis that argues for shedding: a run that is producing edits is a
    // run that is going fine.
    for (id, score, _) in conversations(&replay("producing.jsonl")) {
        assert!(score < 0.0, "{id} is producing, so it must lean efficient");
    }
}

#[test]
fn an_unreadable_corpus_fails_loudly() {
    // A silent zero-conversation report would make a sweep look clean when it
    // measured nothing at all.
    let out = Command::new(bin())
        .arg("--replay")
        .arg(fixture("does-not-exist.jsonl"))
        .output()
        .expect("run --replay");
    assert!(!out.status.success(), "a missing corpus must be an error");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("cannot read"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
