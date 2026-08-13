//! §7.1 Signal extraction: what the tool history says about how a turn is going.
//!
//! Pure, deterministic, zero-I/O. [`extract`] walks the verbatim request JSON and
//! returns a [`ToolSignals`]; [`crate::route::scorer`] turns that into a number.
//! Nothing here touches `State`, `Config`, or a clock, so it is testable without
//! a server around it.
//!
//! **Why `&Value` and not `&[ChatMessage]`.** The typed view in `proto::openai`
//! drops `tool_calls` and the `role:"tool"` results — exactly the fields the
//! signal lives in. Rather than widen the typed model for one consumer, this
//! reads the request JSON as it arrived. The cost is that every field is
//! optional and may be the wrong type, so every access here is defensive: a
//! malformed shape yields a default signal, never a panic.
//!
//! # Attribution (§14)
//!
//! Derived from NVIDIA-NeMo/Switchyard <https://github.com/NVIDIA-NeMo/Switchyard>,
//! `crates/libsy/src/algorithms/util/tool_signals.rs`, read at branch `main` on
//! 2026-08-11 (no upstream commit hash was recorded). Copyright (c) NVIDIA
//! CORPORATION & AFFILIATES. Licensed under the Apache License, Version 2.0
//! <http://www.apache.org/licenses/LICENSE-2.0>; distributed on an "AS IS" BASIS,
//! WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND. What is derived — the severity /
//! tool-name / test-phrase table *design* and two trace-mined matching rules
//! (`file does not exist`, and the digit-before-`failed` guard) — is spelled out
//! in the top-level `NOTICE`. The rest of this file is original and MIT.

use serde_json::Value;

/// §7.6 `recent_turn_window`. Three tool results is enough for an error to
/// survive a recovery turn or two without an ancient failure haunting a run that
/// has since recovered.
pub const DEFAULT_RECENT_WINDOW: usize = 3;

/// What the tool history says. Every field is a plain value with no lifetime, so
/// a policy can compute it once and hand it to the scorer, the stats layer, and
/// (Phase 2) the session store without re-walking the request.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolSignals {
    /// Windowed **max** over the recent tool results: `0.0` | `0.3` soft |
    /// `0.7` hard | `1.0` critical. Max rather than "most recent" so an error
    /// persists through the recovery turns instead of clearing the instant one
    /// clean result lands.
    pub severity: f32,
    /// Counts over the last `recent_window` **tool calls** — the same window
    /// `severity` uses, which in a well-formed transcript pairs one-to-one with
    /// the last `recent_window` tool results.
    pub recent_write_count: u32,
    pub recent_edit_count: u32,
    pub recent_read_count: u32,
    pub recent_todowrite_count: u32,
    /// A recent result looks like a passing test run and no recent result
    /// reports a counted failure. Half of the §7.2 "settled run" override.
    pub tests_passed: bool,
    /// §7.8: a **message-count proxy**, not a count of conversational turns, and
    /// wire-format dependent — Anthropic batches tool results into far fewer
    /// messages than OpenAI-chat does, so `STALL_MIN_TURN_DEPTH = 8` means
    /// something different per client. Gate on it loosely; do not tune it before
    /// Phase 5 lands and both surfaces can be measured.
    pub turn_depth: u32,
    /// A context-compaction marker was found in the prompt prefix. §7.2: this is
    /// meant to be **self-latching** — once an agent's context has been
    /// summarized the task belongs on the capable tier, and it must not snap back
    /// the moment the summary wipes the accumulated signals. The latch itself
    /// lives in the Phase 2 session store; all this field reports is the marker.
    pub compacted: bool,
    /// Whether this conversation contains **any** tool activity at all, not just
    /// inside the recent window.
    ///
    /// §7.8 is explicit that a pure chat client "has no tool history, scores 0.0,
    /// and lands on leg 0 every time". Without this flag a long *conversation*
    /// with no tools looks identical to an agent that has stopped calling them —
    /// both are deep with zero recent ops — and `spinning` fires on it. The
    /// scorer gates the stall signals on this so a 20-turn chat is not mistaken
    /// for a stuck agent.
    pub has_tool_history: bool,
}

/// Walk an OpenAI-shaped `messages` array and extract the routing signal.
///
/// `messages` is the `messages` member of the verbatim request body. Anything
/// that is not an array of objects yields [`ToolSignals::default`] — a plain chat
/// client has no tool history, scores 0.0, and lands on leg 0 (§7.8).
impl ToolSignals {
    /// §7.2's `recent_ops`, and §7.6's "a turn with a non-zero tool signal": the
    /// total tool activity in the recent window. Zero means a plain chat client
    /// with no tool history, which §7.8 says must land on leg 0 every time.
    pub fn recent_ops(&self) -> u32 {
        self.recent_write_count
            .saturating_add(self.recent_edit_count)
            .saturating_add(self.recent_read_count)
            .saturating_add(self.recent_todowrite_count)
    }
}

pub fn extract(messages: &Value, recent_window: usize) -> ToolSignals {
    let Some(msgs) = messages.as_array() else {
        return ToolSignals::default();
    };
    // A zero window would silence every signal; treat it as "one result".
    let window = recent_window.max(1);

    // `None` = a call we could not categorize (a `Task`, an MCP tool, a build
    // command). It still occupies a slot in the window, so an agent that spent
    // its last three calls on unclassifiable work reads as "no recent ops"
    // rather than inheriting counts from further back.
    let mut calls: Vec<Option<Op>> = Vec::new();
    let mut results: Vec<String> = Vec::new();

    for m in msgs {
        if let Some(tool_calls) = m.get("tool_calls").and_then(Value::as_array) {
            calls.extend(tool_calls.iter().map(classify));
        } else if let Some(fc) = m.get("function_call") {
            // Legacy single-function shape. Still emitted by older clients and
            // by some proxies; the name and arguments sit directly on it.
            calls.push(classify(fc));
        }
        if matches!(
            m.get("role").and_then(Value::as_str),
            Some("tool" | "function")
        ) {
            let mut text = String::new();
            push_text(m.get("content"), &mut text);
            text.make_ascii_lowercase();
            results.push(text);
        }
    }

    let recent_calls = &calls[calls.len().saturating_sub(window)..];
    let recent_results = &results[results.len().saturating_sub(window)..];

    let mut s = ToolSignals {
        turn_depth: u32::try_from(msgs.len()).unwrap_or(u32::MAX),
        compacted: compaction_marked(msgs),
        // Deliberately over the WHOLE conversation, not the recent window: an
        // agent that made tool calls twenty turns ago and none since is exactly
        // the stall `spinning` exists to catch.
        has_tool_history: !calls.is_empty() || !results.is_empty(),
        severity: recent_results
            .iter()
            .map(|r| severity_of(r))
            .fold(0.0, f32::max),
        ..ToolSignals::default()
    };
    for op in recent_calls.iter().flatten() {
        let counter = match op {
            Op::Write => &mut s.recent_write_count,
            Op::Edit => &mut s.recent_edit_count,
            Op::Read => &mut s.recent_read_count,
            Op::TodoWrite => &mut s.recent_todowrite_count,
        };
        *counter += 1;
    }
    // A pass anywhere in the window only counts if nothing in the window failed:
    // "5 passed" two results after "2 failed" is a partial recovery, not a
    // settled run.
    s.tests_passed = recent_results.iter().any(|r| looks_like_a_pass(r))
        && !recent_results.iter().any(|r| counted_failure(r));
    s
}

// ---------------------------------------------------------------------------
// Tool categorization
// ---------------------------------------------------------------------------

/// The four production/investigation axes §7.2 scores over. Anything else is
/// deliberately uncategorized rather than forced into a bucket.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Write,
    Edit,
    Read,
    TodoWrite,
}

/// Whole-file production. `Write` is claude-code; `create_file` / `write_file`
/// are the codex and OpenAI-function harnesses; `fs_write` / `file_write` /
/// `save_file` cover the hermes-style and local-agent families.
const WRITE_TOOLS: &[&str] = &[
    "write",
    "write_file",
    "create_file",
    "createfile",
    "save_file",
    "fs_write",
    "file_write",
    "new_file",
];

/// In-place modification. `edit` / `multiedit` / `notebook_edit` are claude-code;
/// `apply_patch` is codex; the rest are the diff- and replace-shaped tools the
/// other harnesses ship.
const EDIT_TOOLS: &[&str] = &[
    "edit",
    "multiedit",
    "multi_edit",
    "notebookedit",
    "notebook_edit",
    "apply_patch",
    "apply_diff",
    "edit_file",
    "patch_file",
    "update_file",
    "modify_file",
    "replace_in_file",
    "search_replace",
    "str_replace",
];

/// Investigation. Includes the search tools (`glob`, `grep`, `codebase_search`)
/// and the web tools: reading documentation is the same kind of act as reading a
/// file, and §7.2 only asks whether the agent is investigating rather than
/// producing.
const READ_TOOLS: &[&str] = &[
    "read",
    "read_file",
    "view_file",
    "open_file",
    "cat_file",
    "notebookread",
    "notebook_read",
    "glob",
    "grep",
    "ripgrep",
    "rg",
    "find",
    "ls",
    "list_files",
    "list_dir",
    "list_directory",
    "tree",
    "search",
    "search_files",
    "file_search",
    "grep_search",
    "codebase_search",
    "view",
    "todo_read",
    "todoread",
    "webfetch",
    "web_fetch",
    "websearch",
    "web_search",
    "fetch",
];

/// Plan maintenance. `todowrite` is claude-code, `update_plan` is codex,
/// `manage_todo_list` is the VS Code agent surface.
const TODO_TOOLS: &[&str] = &[
    "todowrite",
    "todo_write",
    "write_todos",
    "update_plan",
    "set_plan",
    "manage_todo_list",
];

/// Multiplexed editors that carry the verb in an argument rather than the name,
/// so they cannot be categorized from the name alone.
const EDITOR_TOOLS: &[&str] = &[
    "str_replace_editor",
    "str_replace_based_edit_tool",
    "text_editor",
    "file_editor",
    "editor",
];

/// Generic shells. These say nothing on their own — an agent driving `agy`, a
/// build, or a bare pipeline all arrive as `Bash` — so they fall through to
/// [`classify_command`] on the command text. That fallback is how a harness with
/// exactly one tool gets classified at all.
const SHELL_TOOLS: &[&str] = &[
    "bash",
    "sh",
    "zsh",
    "shell",
    "shell_command",
    "run",
    "exec",
    "execute",
    "terminal",
    "run_command",
    "run_terminal_cmd",
    "run_in_terminal",
    "execute_command",
    "execute_bash",
    "exec_command",
    "local_shell",
    "container_exec",
];

fn classify(call: &Value) -> Option<Op> {
    let name = normalize(call_name(call)?);
    let n = name.as_str();
    if WRITE_TOOLS.contains(&n) {
        return Some(Op::Write);
    }
    if EDIT_TOOLS.contains(&n) {
        return Some(Op::Edit);
    }
    if READ_TOOLS.contains(&n) {
        return Some(Op::Read);
    }
    if TODO_TOOLS.contains(&n) {
        return Some(Op::TodoWrite);
    }
    if EDITOR_TOOLS.contains(&n) {
        return Some(editor_op(arguments(call).as_ref().unwrap_or(&Value::Null)));
    }
    if SHELL_TOOLS.contains(&n) {
        return classify_command(&shell_command(&arguments(call)?)?);
    }
    None
}

fn call_name(call: &Value) -> Option<&str> {
    call.get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        .or_else(|| call.get("name").and_then(Value::as_str))
}

/// Tool names arrive namespaced in the wild — `mcp__server__read_file`,
/// `functions.Read`, `Notebook-Edit`. Reduce to the bare verb before matching so
/// one table entry covers every wrapping.
fn normalize(name: &str) -> String {
    let bare = name.rsplit("__").next().unwrap_or(name);
    let bare = bare.rsplit('.').next().unwrap_or(bare);
    bare.trim().to_ascii_lowercase().replace('-', "_")
}

/// A call's arguments as a `Value`. OpenAI puts them in `function.arguments` as
/// a **JSON string**; Anthropic-shaped and several local harnesses put an object
/// in `input` or `arguments`. A string that does not parse is kept verbatim —
/// some harnesses pass the bare command.
fn arguments(call: &Value) -> Option<Value> {
    let raw = call
        .get("function")
        .and_then(|f| f.get("arguments"))
        .or_else(|| call.get("arguments"))
        .or_else(|| call.get("input"))?;
    match raw.as_str() {
        Some(s) => Some(serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_owned()))),
        None => Some(raw.clone()),
    }
}

fn editor_op(args: &Value) -> Op {
    match args.get("command").and_then(Value::as_str).unwrap_or("") {
        "create" => Op::Write,
        "view" => Op::Read,
        // `str_replace`, `insert`, `undo_edit`, and the missing-field case: the
        // dominant use of a multiplexed editor is in-place modification.
        _ => Op::Edit,
    }
}

fn shell_command(args: &Value) -> Option<String> {
    if let Some(s) = args.as_str() {
        return Some(s.to_owned());
    }
    for key in ["command", "cmd", "script", "input", "code", "commands"] {
        match args.get(key) {
            Some(Value::String(s)) => return Some(s.clone()),
            // codex's `local_shell` passes argv as an array.
            Some(Value::Array(parts)) => {
                let argv: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                if !argv.is_empty() {
                    return Some(argv.join(" "));
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Bash-command pattern matching
// ---------------------------------------------------------------------------

/// In-place rewrites. Checked first because precedence is edit > write > read:
/// `sed -i f > log` edits `f` whatever else it does with its stdout.
const EDIT_CMD_PATTERNS: &[&str] = &[
    "sed -i",
    "sed --in-place",
    "perl -i",
    "awk -i inplace",
    "git apply",
    "apply_patch",
    "patch ",
    "patch<",
    "patch -",
];

/// Command heads that produce files. `tee` is here as well as in the redirection
/// check because splitting on `|` turns `... | tee f` into its own segment.
const WRITE_CMD_HEADS: &[&str] = &["touch", "mkdir", "cp", "mv", "tee", "dd", "rsync"];

/// Command heads that only look. `sed` and `awk` are reads here because the
/// in-place forms were already claimed by [`EDIT_CMD_PATTERNS`].
const READ_CMD_HEADS: &[&str] = &[
    "cat", "ls", "ll", "rg", "grep", "egrep", "fgrep", "ag", "ack", "find", "fd", "head", "tail",
    "less", "more", "wc", "tree", "stat", "file", "diff", "pwd", "which", "nl", "bat", "jq", "yq",
    "xxd", "od", "strings", "du", "df", "readlink", "realpath", "awk", "sed", "cut", "sort",
    "uniq", "column",
];

/// `git` subcommands that only report. The rest of `git` is left uncategorized:
/// `git commit` produces history, not the working-tree edit this axis measures.
const READ_GIT_SUBCOMMANDS: &[&str] = &[
    "status", "diff", "log", "show", "blame", "grep", "ls-files", "branch", "remote", "describe",
];

fn classify_command(cmd: &str) -> Option<Op> {
    let lower = cmd.to_ascii_lowercase();
    if EDIT_CMD_PATTERNS.iter().any(|p| lower.contains(p)) {
        return Some(Op::Edit);
    }
    let segments: Vec<&str> = lower
        .split(['\n', ';', '|', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if redirects_to_a_file(&lower) || segments.iter().any(|s| head_is(s, WRITE_CMD_HEADS)) {
        return Some(Op::Write);
    }
    if segments
        .iter()
        .any(|s| head_is(s, READ_CMD_HEADS) || read_only_git(s))
    {
        return Some(Op::Read);
    }
    None
}

fn redirects_to_a_file(cmd: &str) -> bool {
    // Strip the redirections that create nothing: fd duplication (`2>&1`) and
    // the bit bucket. Without this every `cargo build 2>&1` in a transcript
    // reads as file production, which would swamp the write axis.
    let stripped = cmd
        .replace("2>&1", " ")
        .replace("1>&2", " ")
        .replace(">&2", " ")
        .replace("> /dev/null", " ")
        .replace(">/dev/null", " ");
    stripped.contains('>')
}

/// The first real word of a segment, with `sudo`, `env VAR=v`, and a leading
/// path stripped, so `/usr/bin/env FOO=1 sudo cat x` still reads as `cat`.
fn head_word(segment: &str) -> Option<&str> {
    segment
        .split_whitespace()
        .find(|t| !t.contains('=') && !matches!(*t, "sudo" | "time" | "env" | "nohup" | "\\"))
        .map(|t| {
            let t = t.trim_start_matches("./");
            t.rsplit('/').next().unwrap_or(t)
        })
}

fn head_is(segment: &str, heads: &[&str]) -> bool {
    head_word(segment).is_some_and(|h| heads.contains(&h))
}

fn read_only_git(segment: &str) -> bool {
    if head_word(segment) != Some("git") {
        return false;
    }
    segment
        .split_whitespace()
        .skip_while(|t| !t.ends_with("git"))
        .nth(1)
        .is_some_and(|sub| READ_GIT_SUBCOMMANDS.contains(&sub))
}

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

const CRITICAL: f32 = 1.0;
const HARD: f32 = 0.7;
const SOFT: f32 = 0.3;

/// The curated substring table, evaluated lowercased. Ordering does not matter —
/// [`severity_of`] takes the max over every hit.
///
/// Note what is *absent*: a bare `"oom"`, which fires on "room", "zoom", and
/// "boom". Every entry here has to be a phrase that cannot occur incidentally,
/// because severity 1.0 escalates a route unconditionally (§7.2).
const ERROR_PATTERNS: &[(f32, &str)] = &[
    // Critical — the process could not run at all, and retrying the same backend
    // will not help.
    (CRITICAL, "out of memory"),
    (CRITICAL, "outofmemory"),
    (CRITICAL, "oomkilled"),
    (CRITICAL, "oom-kill"),
    (CRITICAL, "memoryerror"),
    (CRITICAL, "cannot allocate memory"),
    (CRITICAL, "connection refused"),
    (CRITICAL, "econnrefused"),
    // Hard — a real failure the agent now has to recover from.
    (HARD, "traceback"),
    (HARD, "importerror"),
    (HARD, "import error"),
    (HARD, "modulenotfounderror"),
    (HARD, "cannot find module"),
    (HARD, "assertionerror"),
    (HARD, "assertion failed"),
    (HARD, "timeout"),
    (HARD, "timed out"),
    (HARD, "no such file or directory"),
    // Anchored as a FULL PHRASE, preserved verbatim from the reference
    // implementation. A bare "does not exist" fires on `ls` output ("ls: foo: No
    // such file"), on `git` hints, and on ordinary prose in a tool result — this
    // is trace-mined, not obvious, and shortening it is a regression.
    (HARD, "file does not exist"),
];

/// Test-failure keywords. See [`nonzero_count_before`] for why they are not in
/// [`ERROR_PATTERNS`].
const FAILURE_KEYWORDS: &[&str] = &["failed", "failures", "errors"];

/// Phrases that mean a test run came back clean, for the runners that report no
/// counts. Deliberately specific: a bare `passed` appears in prose and in a
/// *failing* run's summary line, so the counted form goes through
/// [`nonzero_count_before`] instead — see [`looks_like_a_pass`].
const TEST_PASS_PATTERNS: &[&str] = &[
    "test result: ok", // cargo test
    "tests passed",    // "all tests passed" and friends
    "--- pass",        // go test -v
    "0 failed",        // a runner that reports counts and counted none failing
    "all checks passed",
];

/// A recent result that reads as a passing test run.
///
/// The counted form is the mirror of the failure rule: `41 passed` counts,
/// `0 passed` does not, and "the deadline passed" has no integer in front of it
/// so it does not either. That one rule covers pytest's `5 passed in 0.12s`,
/// jest's `Tests: 41 passed, 41 total`, and cargo's `12 passed; 0 failed`
/// without a per-runner entry for each.
fn looks_like_a_pass(text: &str) -> bool {
    TEST_PASS_PATTERNS.iter().any(|p| text.contains(p)) || nonzero_count_before(text, "passed")
}

fn severity_of(text: &str) -> f32 {
    let mut worst = 0.0f32;
    for (sev, pattern) in ERROR_PATTERNS {
        if text.contains(pattern) {
            worst = worst.max(*sev);
        }
    }
    if counted_failure(text) {
        worst = worst.max(HARD);
    }
    if nonzero_exit(text) {
        worst = worst.max(SOFT);
    }
    worst
}

/// A test run that reported a non-zero count of failures.
fn counted_failure(text: &str) -> bool {
    FAILURE_KEYWORDS
        .iter()
        .any(|kw| nonzero_count_before(text, kw))
}

/// `failed` / `errors` only trip when a **non-zero integer immediately
/// precedes** them. Preserved verbatim from the reference implementation and
/// trace-mined rather than obvious: cargo ends a *clean* run with
/// `test result: ok. 12 passed; 0 failed;` and `go vet` ends one with
/// `0 errors`. A plain substring match reads both as failures and would escalate
/// every successful test run.
///
/// The corollary is deliberate: an uncounted bare `FAILED` does not trip this.
/// Real failures still arrive through [`ERROR_PATTERNS`] and
/// [`nonzero_exit`]; a keyword with no count attached is too weak to act on.
fn nonzero_count_before(text: &str, keyword: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(rel) = text[from..].find(keyword) {
        let at = from + rel;
        from = at + keyword.len();
        // Back over the separating whitespace...
        let mut i = at;
        while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
            i -= 1;
        }
        if i == at {
            continue; // nothing between the keyword and whatever precedes it
        }
        // ...then over the digits.
        let end = i;
        while i > 0 && bytes[i - 1].is_ascii_digit() {
            i -= 1;
        }
        if i < end && text[i..end].bytes().any(|b| b != b'0') {
            return true;
        }
    }
    false
}

/// A bare non-zero exit: the weakest evidence in the table, worth 0.3. On its
/// own it is a third of a signal — enough to tilt a score, never enough to move
/// a route (§7.2).
fn nonzero_exit(text: &str) -> bool {
    if text.contains("non-zero exit") || text.contains("nonzero exit") {
        return true;
    }
    for marker in [
        "exit code",
        "exit status",
        "exit_code",
        "exited with code",
        "exited with status",
        "returncode",
        "return code",
    ] {
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(marker) {
            from = from + rel + marker.len();
            let rest = text[from..].trim_start_matches([' ', ':', '=', '(', '\'', '"']);
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits > 0 && rest[..digits].bytes().any(|b| b != b'0') {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Markers a harness leaves when it has summarized a conversation to fit the
/// window. Matched lowercased.
const COMPACTION_MARKERS: &[&str] = &[
    // claude-code's /compact and auto-compact preamble, both phrasings.
    "this session is being continued from a previous conversation",
    "continued from a previous conversation that ran out of context",
    // Summary envelopes and explicit notices used by the other harnesses.
    "<conversation_summary>",
    "conversation has been compacted",
    "context was compacted",
    "[context compacted]",
    "summary of the conversation so far",
];

/// Only the **prompt prefix** is scanned — the leading system/developer messages
/// and the first user message. A compaction preamble lives there by construction,
/// and scanning the whole conversation would let an agent that merely *discussed*
/// compaction latch itself onto the capable tier.
fn compaction_marked(msgs: &[Value]) -> bool {
    let mut prefix = String::new();
    for m in msgs {
        match m.get("role").and_then(Value::as_str) {
            Some("system" | "developer") => push_text(m.get("content"), &mut prefix),
            Some("user") => {
                push_text(m.get("content"), &mut prefix);
                break; // the first user turn ends the prefix
            }
            _ => break,
        }
    }
    prefix.make_ascii_lowercase();
    COMPACTION_MARKERS.iter().any(|k| prefix.contains(k))
}

/// Message content is a string in the common case and an array of typed parts in
/// the multimodal variants. Anything else contributes nothing rather than erroring.
fn push_text(content: Option<&Value>, out: &mut String) {
    match content {
        Some(Value::String(s)) => out.push_str(s),
        Some(Value::Array(parts)) => {
            for p in parts {
                if let Some(t) = p.get("text").and_then(Value::as_str).or_else(|| p.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An assistant message carrying one tool call.
    fn call(name: &str, args: &str) -> Value {
        json!({"role": "assistant", "tool_calls": [
            {"type": "function", "function": {"name": name, "arguments": args}}
        ]})
    }

    fn result(text: &str) -> Value {
        json!({"role": "tool", "content": text})
    }

    fn sig(msgs: Vec<Value>) -> ToolSignals {
        extract(&Value::Array(msgs), DEFAULT_RECENT_WINDOW)
    }

    /// The classification of a single call, as the four counters see it.
    fn op_of(name: &str, args: &str) -> Option<Op> {
        classify(&json!({"function": {"name": name, "arguments": args}}))
    }

    #[test]
    fn empty_conversation_yields_defaults() {
        // §7.8: a plain chat client has no tool history. It must score 0.0 and
        // land on leg 0, so every field here has to be the zero value.
        assert_eq!(sig(vec![]), ToolSignals::default());
        let chat = sig(vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": "hello"}),
        ]);
        assert_eq!(chat.severity, 0.0);
        assert_eq!(chat.recent_read_count, 0);
        assert_eq!(chat.recent_write_count, 0);
        assert!(!chat.tests_passed);
        assert!(!chat.compacted);
        assert_eq!(chat.turn_depth, 2);
    }

    #[test]
    fn non_array_messages_yields_defaults() {
        for shape in [
            json!(null),
            json!("messages"),
            json!({"messages": []}),
            json!(7),
        ] {
            assert_eq!(extract(&shape, 3), ToolSignals::default(), "{shape}");
        }
    }

    #[test]
    fn malformed_shapes_do_not_panic() {
        // Every field is attacker-shaped: the request body is echoed straight in.
        let s = sig(vec![
            json!("a bare string, not an object"),
            json!({"role": "assistant", "tool_calls": "not an array"}),
            json!({"role": "assistant", "tool_calls": [null, 7, "x", {}]}),
            json!({"role": "assistant", "tool_calls": [{"function": {}}]}),
            json!({"role": "assistant", "tool_calls": [{"function": {"name": 42}}]}),
            json!({"role": "assistant", "tool_calls": [{"function": {"name": "Bash"}}]}),
            json!({"role": "assistant", "tool_calls": [{"function": {"name": "Bash", "arguments": "{{{"}}]}),
            json!({"role": "tool", "content": null}),
            json!({"role": "tool", "content": {"nested": true}}),
            json!({"role": "tool"}),
            json!({}),
        ]);
        assert_eq!(s.severity, 0.0);
        assert_eq!(s.recent_write_count + s.recent_read_count, 0);
        assert_eq!(s.turn_depth, 11);
    }

    #[test]
    fn claude_code_tool_names_classify() {
        assert_eq!(op_of("Write", "{}"), Some(Op::Write));
        assert_eq!(op_of("Edit", "{}"), Some(Op::Edit));
        assert_eq!(op_of("MultiEdit", "{}"), Some(Op::Edit));
        assert_eq!(op_of("NotebookEdit", "{}"), Some(Op::Edit));
        assert_eq!(op_of("Read", "{}"), Some(Op::Read));
        assert_eq!(op_of("Glob", "{}"), Some(Op::Read));
        assert_eq!(op_of("Grep", "{}"), Some(Op::Read));
        assert_eq!(op_of("WebFetch", "{}"), Some(Op::Read));
        assert_eq!(op_of("TodoWrite", "{}"), Some(Op::TodoWrite));
        // Not on any production axis, and deliberately so.
        assert_eq!(op_of("Task", "{}"), None);
    }

    #[test]
    fn codex_and_hermes_tool_names_classify() {
        assert_eq!(op_of("apply_patch", "{}"), Some(Op::Edit));
        assert_eq!(op_of("create_file", "{}"), Some(Op::Write));
        assert_eq!(op_of("write_file", "{}"), Some(Op::Write));
        assert_eq!(op_of("read_file", "{}"), Some(Op::Read));
        assert_eq!(op_of("list_dir", "{}"), Some(Op::Read));
        assert_eq!(op_of("codebase_search", "{}"), Some(Op::Read));
        assert_eq!(op_of("update_plan", "{}"), Some(Op::TodoWrite));
        assert_eq!(op_of("search_replace", "{}"), Some(Op::Edit));
    }

    #[test]
    fn str_replace_editor_dispatches_on_its_command_argument() {
        // The verb is in the arguments, not the name, so the name table alone
        // would put every one of these in the same bucket.
        assert_eq!(
            op_of("str_replace_editor", r#"{"command":"create","path":"a"}"#),
            Some(Op::Write)
        );
        assert_eq!(
            op_of("str_replace_editor", r#"{"command":"view","path":"a"}"#),
            Some(Op::Read)
        );
        assert_eq!(
            op_of("str_replace_editor", r#"{"command":"str_replace"}"#),
            Some(Op::Edit)
        );
        assert_eq!(op_of("text_editor", "{}"), Some(Op::Edit));
    }

    #[test]
    fn namespaced_and_mcp_names_normalize() {
        assert_eq!(op_of("mcp__filesystem__read_file", "{}"), Some(Op::Read));
        assert_eq!(op_of("functions.Write", "{}"), Some(Op::Write));
        assert_eq!(op_of("Notebook-Edit", "{}"), Some(Op::Edit));
        assert_eq!(op_of("  READ  ", "{}"), Some(Op::Read));
    }

    #[test]
    fn shell_reads_are_matched_on_the_command() {
        // How an agent driving `agy` or a bare shell gets classified at all.
        for cmd in [
            "cat src/main.rs",
            "ls -la /tmp",
            "rg 'fn main' src",
            "grep -rn TODO .",
            "find . -name '*.rs'",
            "head -50 Cargo.toml",
            "cd /repo && wc -l src/*.rs",
            "git status --short",
            "git log --oneline -20",
            "sudo cat /etc/hosts",
            "/usr/bin/tail -f log",
            "rg foo | head -5",
        ] {
            let args = json!({"command": cmd}).to_string();
            assert_eq!(op_of("Bash", &args), Some(Op::Read), "{cmd}");
        }
    }

    #[test]
    fn shell_writes_are_matched_on_the_command() {
        for cmd in [
            "echo hello > out.txt",
            "cat tpl >> dst",
            "touch newfile.rs",
            "mkdir -p src/route",
            "cp a.rs b.rs",
            "mv old.rs new.rs",
            "cargo test 2>&1 | tee test.log",
        ] {
            let args = json!({"command": cmd}).to_string();
            assert_eq!(op_of("shell", &args), Some(Op::Write), "{cmd}");
        }
    }

    #[test]
    fn shell_edits_are_matched_on_the_command() {
        for cmd in [
            "sed -i 's/a/b/' f.rs",
            "sed -i '' 's/a/b/' f.rs",
            "perl -i -pe 's/a/b/' f.rs",
            "git apply fix.patch",
            "patch -p1 < fix.diff",
            // Precedence: an in-place edit that also redirects is still an edit.
            "sed -i 's/a/b/' f.rs > log",
        ] {
            let args = json!({"command": cmd}).to_string();
            assert_eq!(op_of("run_command", &args), Some(Op::Edit), "{cmd}");
        }
    }

    #[test]
    fn fd_redirection_alone_is_not_a_write() {
        // Without this, every `... 2>&1` in a transcript reads as file
        // production and the write axis stops meaning anything.
        for cmd in [
            "cargo build 2>&1",
            "make test > /dev/null",
            "ls 2>/dev/null",
        ] {
            let args = json!({"command": cmd}).to_string();
            assert_ne!(op_of("Bash", &args), Some(Op::Write), "{cmd}");
        }
        // A build that neither reads nor writes files stays uncategorized.
        let args = json!({"command": "cargo build --release"}).to_string();
        assert_eq!(op_of("Bash", &args), None);
    }

    #[test]
    fn shell_argv_arrays_and_bare_strings_are_accepted() {
        // codex's local_shell sends argv; some proxies send the command itself
        // where a JSON object is expected.
        let argv = json!({"command": ["ls", "-la", "/tmp"]}).to_string();
        assert_eq!(op_of("local_shell", &argv), Some(Op::Read));
        assert_eq!(op_of("bash", "touch f"), Some(Op::Write));
    }

    #[test]
    fn severity_critical_patterns() {
        for text in [
            "fatal: out of memory",
            "java.lang.OutOfMemoryError",
            "the pod was OOMKilled",
            "MemoryError",
            "fork: Cannot allocate memory",
            "curl: (7) Failed to connect: Connection refused",
            "Error: connect ECONNREFUSED 127.0.0.1:5432",
        ] {
            let s = sig(vec![result(text)]);
            assert_eq!(s.severity, 1.0, "{text}");
        }
    }

    #[test]
    fn severity_hard_patterns() {
        for text in [
            "Traceback (most recent call last):",
            "ImportError: cannot import name 'x'",
            "import error while loading conftest",
            "ModuleNotFoundError: No module named 'foo'",
            "Error: Cannot find module 'react'",
            "AssertionError: 1 != 2",
            "assertion failed: left == right",
            "error: test timeout after 30s",
            "the request timed out",
            "cat: nope.txt: No such file or directory",
            "Error: file does not exist",
        ] {
            let s = sig(vec![result(text)]);
            assert_eq!(s.severity, 0.7, "{text}");
        }
    }

    #[test]
    fn severity_soft_is_a_bare_nonzero_exit() {
        for text in [
            "command exited with code 1",
            "process finished with exit code 2",
            "exit status: 127",
            "returncode=137",
            "the command returned a non-zero exit",
        ] {
            let s = sig(vec![result(text)]);
            assert_eq!(s.severity, 0.3, "{text}");
        }
        // Zero is not a failure, however it is spelled.
        for text in ["exit code 0", "exit status: 0", "returncode=0"] {
            assert_eq!(sig(vec![result(text)]).severity, 0.0, "{text}");
        }
    }

    #[test]
    fn zero_counts_do_not_read_as_failures() {
        // The anti-patterns. cargo and go end a *clean* run with these exact
        // lines; a plain substring match on "failed"/"errors" escalates every
        // successful test run in the corpus.
        let cargo = "test result: ok. 12 passed; 0 failed; 0 ignored";
        let go = "ok  \tgithub.com/x/y\t0.01s\n0 errors";
        assert_eq!(sig(vec![result(cargo)]).severity, 0.0);
        assert_eq!(sig(vec![result(go)]).severity, 0.0);
        // A non-zero count is a hard failure.
        assert_eq!(
            sig(vec![result("test result: FAILED. 9 passed; 3 failed")]).severity,
            0.7
        );
        assert_eq!(sig(vec![result("2 errors generated.")]).severity, 0.7);
    }

    #[test]
    fn ls_output_mentioning_a_missing_file_is_not_an_error() {
        // The reason "file does not exist" is anchored as a full phrase: this
        // line is routine output, and a bare "does not exist" match escalates it.
        let s = sig(vec![result(
            "note: the branch 'wip' does not exist on the remote yet",
        )]);
        assert_eq!(s.severity, 0.0);
        // The anchored phrase still fires when it is genuinely there.
        assert_eq!(
            sig(vec![result("Error: file does not exist")]).severity,
            0.7
        );
    }

    #[test]
    fn severity_is_a_windowed_max_over_recent_results() {
        // Two results back is still inside the default window of 3, so the error
        // persists through the recovery turns instead of clearing the instant one
        // clean result lands.
        let two_back = sig(vec![
            result("Traceback (most recent call last):"),
            result("ok"),
            result("ok"),
        ]);
        assert_eq!(two_back.severity, 0.7);
        // Four back is outside it.
        let four_back = sig(vec![
            result("Traceback (most recent call last):"),
            result("ok"),
            result("ok"),
            result("ok"),
            result("ok"),
        ]);
        assert_eq!(four_back.severity, 0.0);
    }

    #[test]
    fn recent_counts_use_the_same_window_as_severity() {
        let msgs = vec![
            call("Write", "{}"), // aged out
            call("Write", "{}"), // aged out
            call("Read", "{}"),
            call("Read", "{}"),
            call("Edit", "{}"),
        ];
        let s = sig(msgs);
        assert_eq!(s.recent_read_count, 2);
        assert_eq!(s.recent_edit_count, 1);
        assert_eq!(s.recent_write_count, 0, "older writes must age out");
        // A wider window brings them back.
        let all = extract(
            &json!([
                call("Write", "{}"),
                call("Write", "{}"),
                call("Read", "{}"),
                call("Read", "{}"),
                call("Edit", "{}"),
            ]),
            10,
        );
        assert_eq!(all.recent_write_count, 2);
    }

    #[test]
    fn tests_passed_detects_a_clean_run() {
        for text in [
            "test result: ok. 12 passed; 0 failed; 0 ignored", // cargo
            "Tests:       41 passed, 41 total",                // jest
            "5 passed in 0.12s",                               // pytest
            "--- PASS: TestFoo (0.00s)",                       // go -v
        ] {
            assert!(sig(vec![result(text)]).tests_passed, "{text}");
        }
        // The counted form is the mirror of the failure rule, so it inherits the
        // same two guards: a zero count is not a pass, and prose is not a count.
        for text in [
            "collected 0 items, 0 passed",
            "the deadline passed, so the job was skipped",
            "compiling",
        ] {
            assert!(!sig(vec![result(text)]).tests_passed, "{text}");
        }
    }

    #[test]
    fn tests_passed_is_false_when_the_window_holds_a_failure() {
        // A pass two results after a failure is a partial recovery, not a
        // settled run — should_deescalate must not fire on it.
        let s = sig(vec![
            result("test result: FAILED. 9 passed; 3 failed"),
            result("recompiling"),
            result("test result: ok. 12 passed; 0 failed"),
        ]);
        assert!(!s.tests_passed);
        assert_eq!(s.severity, 0.7);
        // Once the failure ages out, the run reads as settled.
        let recovered = sig(vec![
            result("test result: FAILED. 9 passed; 3 failed"),
            result("recompiling"),
            result("test result: ok. 12 passed; 0 failed"),
            result("done"),
        ]);
        assert!(recovered.tests_passed);
        assert_eq!(recovered.severity, 0.0);
    }

    #[test]
    fn turn_depth_counts_messages() {
        // §7.8: a message-count proxy, not a turn count, and wire-format
        // dependent. The assertion is on the literal message count on purpose.
        let msgs: Vec<Value> = (0..9)
            .map(|_| json!({"role": "user", "content": "x"}))
            .collect();
        assert_eq!(sig(msgs).turn_depth, 9);
    }

    #[test]
    fn compaction_is_detected_in_the_prompt_prefix() {
        let s = sig(vec![
            json!({"role": "system", "content":
                "This session is being continued from a previous conversation that ran out of context."}),
            json!({"role": "user", "content": "keep going"}),
        ]);
        assert!(s.compacted);
        // Also in the first user message, which is where several harnesses put it.
        let in_user = sig(vec![
            json!({"role": "user", "content": "<conversation_summary>...</conversation_summary>"}),
        ]);
        assert!(in_user.compacted);
    }

    #[test]
    fn compaction_mentioned_mid_conversation_does_not_latch() {
        // Scanning the whole transcript would let an agent that merely discussed
        // compaction pin itself to the capable tier for the rest of the run.
        let s = sig(vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content":
                "I could not read it because this session is being continued from a previous conversation"}),
        ]);
        assert!(!s.compacted);
    }

    #[test]
    fn content_part_arrays_are_flattened() {
        let s = sig(vec![
            json!({"role": "system", "content": [{"type": "text", "text": "context was compacted"}]}),
            json!({"role": "user", "content": "go"}),
            json!({"role": "tool", "content": [{"type": "text", "text": "Traceback (most recent call last):"}]}),
        ]);
        assert!(s.compacted);
        assert_eq!(s.severity, 0.7);
    }

    #[test]
    fn legacy_function_call_shape_is_counted() {
        let s = sig(vec![json!({
            "role": "assistant",
            "function_call": {"name": "Edit", "arguments": "{}"}
        })]);
        assert_eq!(s.recent_edit_count, 1);
    }

    #[test]
    fn a_zero_window_is_clamped_to_one() {
        let s = extract(
            &json!([
                result("Traceback (most recent call last):"),
                result("all good"),
            ]),
            0,
        );
        // Only the newest result is in scope, so the traceback is out of window
        // — but the window did not silence everything.
        assert_eq!(s.severity, 0.0);
        let s = extract(&json!([result("Traceback (most recent call last):")]), 0);
        assert_eq!(s.severity, 0.7);
    }
}
