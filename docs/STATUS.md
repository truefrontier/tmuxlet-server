# tmuxlet-server — Status & Next Steps

_Last updated: 2026-07-10_

## TL;DR

**V1 plus the optimization spec is complete, verified, and hardened by a
second (post-implementation) review.** Phases 1–3 and Phase 4's opt-in items
(U-10 / U-14 / U-17 / U-19 / U-24) are implemented end-to-end on top of v0.1.1;
Phase 4's V2 items (true streaming, P-7, U-22, P-4) remain deferred. A second
adversarial review (2026-07-10) then found and fixed two behavioral gaps plus a
test/documentation debt — see "Post-implementation review" below. CI gates are green.

- **Repo:** https://github.com/CodefiLabs/tmuxlet-server
- **Branch:** `feat/optimization-spec` (based on `origin/main` = v0.1.1)
- **Tests:** 128 passing (81 unit + 47 integration across 10 binaries)
- **Gates:** `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, full suite — all clean
- **Spec:** [`docs/specs/2026-07-09-optimization-spec.md`](specs/2026-07-09-optimization-spec.md)

## What it does

A standalone synchronous Rust binary (edition 2024, no async) that exposes an
OpenAI-compatible `/v1/chat/completions` HTTP service backed by a TOML-configured
fallback chain over three backend types: `tmuxlet`, `api` (OpenAI-compatible HTTP
upstream), and `cli` (exec a binary, optionally in a PTY). Endpoints: `/health`,
`/v1/models`, `/v1/backends`, streaming + non-streaming `/v1/chat/completions`,
plus a TOML-driven auto-router (`§5`) that classifies a request and routes it to a
chain/backend.

## Optimization spec — implemented

**Phase 1 (correctness):** F-1 timeout→error (no silent truncation), F-4 PATH
resolution to an absolute path (executable-bit checked), F-5 Content-Length-aware
body read, F-8 PTY write-error propagation, U-6 empty-output-as-failure with an
`allow_empty` escape hatch, U-15 multi-part content join.

**Phase 2 (security):** S-1 bearer auth (constant-time compare; token from env or
a 0600 `~/.tmuxlet/token`), S-2 loopback-bind policy (refuse non-loopback without
auth), S-3 env allowlist (glob, base vars always passed), S-4 stdin-prompt for
CLI/tmuxlet, S-5 response-byte cap, S-7 `env -0` NUL parsing, S-9 env-capture
watchdog.

**Phase 3 (headline UX/perf):** P-1 worker sizing (`max(16, cores)`, clamped ≥1),
P-6 chain time budget, P-9 failure-aware cooldown, U-2 SSE keepalive, U-3 strict
models, U-8 leveled logging + request ids, U-13 `/v1/backends`, U-20 per-backend
concurrency cap (enforced on chain legs **and** the router classifier), U-4/U-5
config linting (unknown-key + did-you-mean), U-7/U-23 error detail, and the
**auto-router** (`§5`).

**Phase 4 (opt-in correctness/features):** U-17 (HEAD mirrors GET; wrong method →
405 + `Allow`), U-19 (bracketed IPv6 `base_url`), U-14 (per-backend
`prompt_mode = transcript | last_user`; unified `System:` label), U-10 (optional
CORS via `[server] cors_origins`), U-24 (per-backend `ca_file` custom-CA TLS, no
blanket insecure flag).

All new keys are opt-in — an existing pre-spec config still loads with zero new
required keys (UX invariant 1). `--validate` is strict; `serve` is forgiving
(config smells warn and continue). Every refusal names its one-line fix.

## Verification (done)

A 5-dimension adversarial subagent fan-out (security, concurrency/perf,
correctness/dispatch, protocol/HTTP, config/validation) reviewed the full
Phase 1–4 diff; each candidate finding was independently refuted before being
accepted. **5 confirmed findings — all fixed with regression tests:**

- **S-1:** `open(2)`'s `.mode(0600)` is ignored for a pre-existing token file, so
  a stale 0644 `~/.tmuxlet/token` left the generated secret world-readable — now
  forced to 0600 via `set_permissions`.
- **U-20:** the router classifier dispatch bypassed `max_concurrent` — now gated
  with the same acquire/release as chain legs (degrades to `fallback_class` at
  capacity; routing never fails the request).
- **S-5:** a hostile `Content-Length` near `usize::MAX` overflowed the body-framing
  offset (debug panic / release corruption) — now `checked_add`.
- **P-1:** `workers = 0` bound the socket, printed "listening", then exited 0 —
  now clamped to ≥1 with a lint warning.
- **F-4:** `resolve_program` accepted the first `is_file()` PATH match, so a
  non-executable shadow masked the real binary — now requires the exec bit and
  keeps scanning (execvp semantics).

The earlier 6-lens V1 review (18 issues, all fixed) remains in git history.

## Post-implementation review & remediation (2026-07-10)

A second, larger adversarial review (6 finder dimensions → severity-scaled refuter
panels) swept the full `feat/optimization-spec` diff, including the fix commit the
first pass never re-reviewed. 43 findings confirmed, 1 refuted; all actioned
findings are fixed with guard tests. Plan:
[`docs/plans/2026-07-09-review-remediation-plan.md`](plans/2026-07-09-review-remediation-plan.md).

**Behavioral / security fixes (Phase A):**

- **S-1 (reuse path):** the 0600 repair ran only when a token was *written*, so a
  pre-existing world-readable token file *with content* was reused as the live
  bearer secret and never re-secured. Now re-secured to 0600 on reuse; `write_token`
  also creates a fresh inode instead of truncating in place (no fd-reuse leak).
- **U-20 (RAII slots):** `run_chain` and the classifier gate leaked `state.active`
  slots on a dispatch panic (the server survives panics via F-3), permanently
  bricking a `max_concurrent = 1` backend until restart. Replaced both manual
  acquire/release pairs with an `ActiveSlot` guard that releases on Drop; the
  `Instant` deadline arithmetic is clamped so an absurd timeout can't panic at all.
- **Streaming panic recovery:** a panic on the SSE dispatch thread ended the
  already-committed 200 stream at clean EOF (no error, no `[DONE]`); now mapped to
  the same in-band 503 + `[DONE]` the non-streaming path returns.
- **Logging:** `log::line` no longer panics on EPIPE and no longer runs while the
  health lock is held (either would brick every later request); control characters
  are sanitized at the sink (CWE-117 log forging).

**Spec-compliance (Phase B):** `x-tmuxlet-route` now carries the winning backend
(`class/chain/backend`); the router log line uses the real router name and a
per-request summary line (`status=ok|fallback|fail`, emitted even on total
failure) was added; the `strict_models` 404 and the `cors_origins` / `log_level`
lints now name their one-line fix.

**Spec amendments (recorded in the spec):**

- **U-20 / P-9:** `Busy` is deliberately excluded from cooldown — a saturated
  backend recovers the instant a slot frees and a rejection costs microseconds, so
  cooling it would only delay recovery. Saturation is surfaced as `state: "busy"`
  on `/v1/backends` instead.
- **U-13:** the countdown field is `cooling_secs` (relative seconds; `Instant` has
  no wall-clock form) rather than the originally-specced `cooling_until`.

**Test & doc debt (Phases C–D):** ~14 new tests closed the gaps where whole
features (the auth gate, U-20 caps, `strict_models`, `/v1/backends`, S-6
redaction, the U-24 TLS happy path, F-3 panic survival) could be reverted with CI
staying green; README, AGENTS.md, and `examples/server.toml` (now a commented
reference block for every optional key, guarded by a lint-clean test) were brought
up to date.

**Optional polish (Phase E):** the discretionary items also landed — `Vary: Origin`
on every response when CORS is enabled (cache correctness behind a shared proxy),
de-duplicated the backend type-label and exec-bit helpers, typed the over-cap
response as `ErrorKind::FileTooLarge` instead of matching English text, and
dropped a double prompt-substitution in the CLI path. E5 (allocation micro-churn)
and E7 (HEAD `Content-Length` / `WWW-Authenticate` RFC niceties) were left as the
plan advised.

## V2 — the routing architecture (all seven phases)

Implements [`docs/specs/2026-08-11-v2-routing-architecture.md`](specs/2026-08-11-v2-routing-architecture.md).
Test count 129 → 317. Still zero new runtime dependencies: the only Cargo change
is enabling `nix`'s `signal` feature, on a crate already in the tree.

- **Phase 0 — correctness.** `parse_class` was nondeterministic under ambiguity
  (two passes over a `HashMap`'s keys, so a reply naming two classes picked a
  winner that varied between runs). The classifier was shown the latest user
  message alone, which on turn 12 of an agent run is "yes, continue" — it now
  sees the opening task too (60/40, opt out with `classifier_include_opening`).
  A context overflow no longer cools a healthy backend, and an all-overflow chain
  returns 400 `context_length_exceeded` instead of a 503 that invites a retry.
- **Phase 1 — the decision layer.** `src/http.rs` split into `proto/`, `route/`,
  `exec/` and `stats.rs`. Routing produces a `Decision` via a `Policy` chosen at
  startup, and every response carries `x-tmuxlet-route` / `-backend` /
  `-decision` / `-rationale`. New `/v1/stats`, `/v1/stats/reset` and (opt-in)
  `/metrics`.
- **Phase 2 — sessions.** Affinity off four recognized session headers replays
  the first turn's decision and skips the classifier entirely; overflow
  exclusions accumulate per conversation. The store is bounded both ways
  (`session_max`, `session_ttl_secs`) because a client-supplied header keying an
  unbounded map is a memory-exhaustion vector.
- **Phase 3 — the signal router.** `type = "signals"` routes on tool history with
  no LLM call. Cost is declared per backend (`subscription`/`local`/`free`/
  `metered`) because our flagship backends are flat-rate: the scarce resource is
  quota, not dollars, so the scorer is a ranker and `reserve_threshold` decides
  how much it gets to rank. A tool-running turn is never shed to a non-agentic
  leg. `signals.rs`/`scorer.rs` are derived from NVIDIA-NeMo/Switchyard — see
  [`NOTICE`](../NOTICE).
- **Phase 4 — acceptance gates.** `[chains.x.gate] type = "judge"` rates the
  completed turn; N consecutive escalations advance the chain and latch the
  session. A judge outage fails open and *holds* the streak, so an outage can
  never manufacture a latch.
- **Phase 5 — protocol surface.** Real token streaming for `api` backends, with
  first-token success as the commit point (a failure before it still falls
  through the chain). Text-only `/v1/messages`; a request carrying `tools` is
  refused with a 400 that names the limitation rather than returning prose where
  the caller expected a tool call.
- **Phase 6 — operations.** `schema_version` (absent means 1, forever),
  `--dry-run`, `--explain <model>`, `--replay <corpus.jsonl>` (§12's calibration
  harness), SIGHUP reload that preserves counters and sessions and refuses a bad
  config rather than bricking a running service, and `Retry-After` honored on 429.

### Known limits, stated rather than hidden

- **Overflow detection is `api`-only.** It keys on an HTTP 400, and only `api`
  backends produce a status; a `tmuxlet` or `cli` backend that overflows still
  cools as before.
- **Streaming is `api`-only, permanently.** `tmuxlet`/`cli` are structurally
  blocking — the child emits at exit, and a PTY's intermediate output is TUI
  redraw, not tokens. The Anthropic surface always buffers, because its event
  sequence cannot be re-shaped from a relayed OpenAI stream.
- **A signals router needs real tool traffic.** A plain chat client scores 0.0 and
  lands on leg 0 every time. `turn_depth` is a message-count proxy and is
  wire-format dependent.
- **Rationing is only as good as the quota estimate.** With no configured or
  learned limit the router is purely reactive: it finds the wall by hitting it.
  Still strictly better than V1, which hit the same wall *and* cooled a healthy
  backend for `cooldown_secs`.
- **Quota and session state are process-local** and die on restart.
- **The §12 replay harness reports the routing split, not the four quadrants.**
  Populating RESCUE/LOSS/SAFE/HARD needs conversations actually run both ways;
  see [`docs/replay.md`](replay.md).

## Deferred (by spec, not bugs)

- **P-7 client-disconnect cancel propagation** to the backend child. A dead
  socket is now detected on the streaming path (the send fails and the request is
  counted), but the child is not killed.
- **P-4 HTTP connection reuse.**

## Next steps (suggested)

1. **Open the PR** from `feat/optimization-spec` into `main` and let CI run the
   gates on Linux + macOS.
2. **Live-smoke** a real `api` upstream (Ollama / OpenRouter) and, if reachable, a
   `tmuxlet` backend end-to-end.
3. **Exercise the new opt-in paths** manually: auth on (`token` file), CORS from a
   browser client, a `last_user` prompt backend, and a self-signed `ca_file`
   upstream.
4. **Tag a release** once the PR merges.
