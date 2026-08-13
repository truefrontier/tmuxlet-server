# tmuxlet-server

A single static Rust binary that exposes an **OpenAI-compatible
`/v1/chat/completions`** endpoint backed by a configurable fallback chain of
heterogeneous AI backends:

- **`tmuxlet`** — interactive coding CLIs (claude, codex, grok, …) run through
  [`tmuxlet`](https://github.com/CodefiLabs/tmuxlet).
- **`cli`** — arbitrary CLI tools, optionally in a PTY (for tools like `agy`
  that detect a non-TTY and redirect output). Use the `{prompt}` placeholder in
  `args` to inject the prompt as a flag value (e.g. `args = ["-p", "{prompt}"]`);
  without it the prompt is appended as the final positional argument.
- **`api`** — any OpenAI-compatible HTTP upstream (Ollama, OpenRouter, …).

> Companion project: [**CodefiLabs/tmuxlet**](https://github.com/CodefiLabs/tmuxlet)
> drives the interactive CLIs behind the `tmuxlet` backend.

Downstream clients (Continue, Cursor, Hermes, anything OpenAI-shaped) see one
endpoint; the routing happens server-side from a TOML config. A request's
`model` field selects a named chain (or a single backend); the server walks the
chain in order until one succeeds.

## Install

```
cargo install --git https://github.com/CodefiLabs/tmuxlet-server --locked --force
cp examples/server.toml ~/.tmuxlet/server.toml   # then edit
tmuxlet-server --validate
tmuxlet-server                                    # serve on 127.0.0.1:3456
```

`--locked` is required: it installs the dependency versions pinned in this
repo's `Cargo.lock`. Without it, Cargo resolves to the latest semver-compatible
releases, and `dispatch2 0.3.1` (pulled in transitively by `ctrlc` on macOS)
fails to compile with a `recursion limit reached` macro error.

For step-by-step install + service setup + verification, see
[`AGENTS.md`](./AGENTS.md). Example config: [`examples/server.toml`](./examples/server.toml).

## HTTP surface

| Method | Path | Behavior |
|---|---|---|
| `GET` | `/health` | `{"status":"ok","version":"…","backends":N,"chains":M}` |
| `GET` | `/v1/models` | OpenAI-compatible list of router + chain + backend names |
| `GET` | `/v1/backends` | per-backend health: `state` (ok\|cooling\|busy\|unknown), consecutive_failures, last_error, last_latency_ms, cooling_secs |
| `GET` | `/v1/stats` | traffic: requests, routing decisions by provenance, per-backend latency percentiles / errors / cost class / quota, classifier calls, session-map occupancy |
| `POST` | `/v1/stats/reset` | zero the counters (for benchmarking runs) |
| `GET` | `/metrics` | Prometheus text format 0.0.4 — **off by default**, enable with `[server] metrics = true` |
| `POST` | `/v1/chat/completions` | OpenAI chat completions (JSON or SSE) |
| `POST` | `/v1/messages` | Anthropic Messages, **text-only** — see the ceiling below |

Reserved namespaces (`/`, `/ui/*`, `/api/*`) return `501` for a future web UI. A
known path reached with the wrong method returns `405` with an `Allow` header
(not `404`); `HEAD` on a GET route mirrors GET with an empty body. Everything
else returns `404`. Errors use the OpenAI error envelope.

### Response provenance

Every chat response — router-driven or not, success or total failure — carries
where it went and why:

| Header | Example |
|---|---|
| `x-tmuxlet-route` | `do/code/do-code/agy` — `model[/class/target]/backend` |
| `x-tmuxlet-backend` | `agy` (the leg that actually answered; `-` if none did) |
| `x-tmuxlet-decision` | `static`, `default`, `affinity`, `quota`, `override`, `settled`, `dimensions`, `classifier`, `fall_open` |
| `x-tmuxlet-rationale` | `quota pressure 0.82 >= reserve 0.70; shedding a leg` |

`fall_open` is not a synonym for `default`: `default` means the client asked for
a model we do not know, `fall_open` means we tried to be smart and could not.
The rationale is bounded, ASCII-sanitized, and **suppressed entirely when bound
to a non-loopback address** — it names backends and can quote upstream error
text. On a streaming request the backend header is absent and the route header
carries no winner: SSE headers are committed before the chain runs, and naming a
leg that might yet fall through would be a lie.

## Routing

A request's `model` selects a chain, a single backend, or a `[routers.<name>]`
table. Two router types:

- **`type = "classifier"`** (default) — a cheap model labels the task, and the
  class picks a chain. The classifier is shown the **opening task plus the latest
  turn**, not just the latest: on turn 12 of an agent run the latest turn is
  "yes, continue", which carries no routing signal at all.
- **`type = "signals"`** — reads the conversation's tool history (writes, edits,
  reads, error severity, whether the context was compacted) and routes with no
  LLM call at all. The scorer is a **ranker, not a decider**: quota pressure sets
  *how many* turns get the flat-rate backend, and the score only chooses *which*
  ones, and only once headroom is actually scarce.

**Why quota and not dollars.** `claude-max` and `codex` run through tmuxlet
against an interactive subscription: the marginal cost of a call is zero until
the quota window is exhausted, then effectively infinite. That is a step
function, not a line, so "is this task hard enough to deserve the good model" is
the wrong question — the right one is "how much headroom is left, and is this
turn worth spending it on". Declare it per backend with
`cost = "subscription" | "local" | "free" | "metered"`; only `metered` costs
money at the margin, and rationing exists to protect you from it.

**A turn that ran tools is never shed to a text-only backend.** `codex` writing
files is not substitutable by a cloud model returning prose *about* writing
files — the request would "succeed" and return the wrong kind of artifact.
Spending quota you meant to save is recoverable next window; that is not.

A signals router needs real tool traffic. A plain chat client has no tool
history, scores 0.0, and lands on leg 0 every time.

## Sessions

If a client sends `x-tmuxlet-session-id`, `x-claude-code-session-id`,
`x-session-id`, or `x-conversation-id`, the first turn's routing decision is
reused for the rest of the conversation and the classifier call is skipped
entirely. A backend that reported a context overflow is excluded for the rest of
that session — after truncating or resetting context, send a **new** session id.
The session map is bounded by `session_max` (4096) and `session_ttl_secs`
(7200): a client-supplied header keying an unbounded map is a
memory-exhaustion vector.

The `usage` object in completions reports zeros
(`prompt_tokens`/`completion_tokens`/`total_tokens` = 0) deliberately: the
backends don't report token counts, and fabricated `len/4` estimates would feed
cost trackers plausible-but-wrong numbers, so zeros mean "no data".

## Auth (optional)

Auth is off by default. With `[server] auth = true`, the server generates a
32-byte token into `~/.tmuxlet/token` (mode `0600`) — unless `auth_token_env`
names an env var holding the token — and clients send
`Authorization: Bearer <token>` on `/v1/*` (missing/invalid → `401
unauthorized`). `/health` stays open (no token) for liveness probes.
`cat ~/.tmuxlet/token` is the whole client setup. `/v1/stats`, `/v1/stats/reset`
and `/metrics` are gated too — they name backends and expose traffic volume,
which is not something to hand an unauthenticated caller just because a scraper
finds it convenient.

## CLI

```
tmuxlet-server --validate            # strict config check (--dry-run is an alias)
tmuxlet-server --check-backends      # probe each backend's reachability
tmuxlet-server --explain do          # print the policy, cascade and leg order for a model
tmuxlet-server --replay corpus.jsonl # score a captured corpus offline and sweep thresholds
```

`--explain` answers "which backend will this model actually hit, and why"
without sending a request. `--replay` runs captured conversations through the
scorer with no model calls and no server, which is what makes the routing
thresholds measurable rather than guessed — see [`docs/replay.md`](./docs/replay.md).

## Streaming

**`api` backends stream for real.** With `stream: true` the upstream is called
with `stream: true` too and tokens are relayed as they arrive.

The design constraint that governs this: *streaming and chain fallback are
mutually exclusive after the first byte.* Once a `data:` frame has gone out
there is no un-sending it. So the commit point is **first-token success** —
connect failures, a non-2xx, an unparseable first frame all happen before
anything is written and still fall through the chain normally. A leg that fails
*after* relaying a token ends the response rather than splicing a second
backend's answer onto the first.

**`tmuxlet` and `cli` backends do not stream, permanently.** They are
structurally blocking: the child emits at exit, and a PTY's intermediate output
is TUI redraw, not tokens. For those the server sends the `200` and SSE headers
immediately, emits `: keepalive` comment frames every 15s while the backend
runs, then the real frames all at once. That is not a gap to be closed.

`/v1/messages` always buffers: Anthropic's event sequence cannot be re-shaped
from a relayed OpenAI chunk after the fact.

## The `/v1/messages` ceiling

Because our backends are text-in/text-out, the Anthropic surface is a shallow
translation — the request is normalized to the OpenAI shape at the door, so
routing, the classifier, signal extraction and prompt shaping all work
unchanged, and only the response envelope differs.

**A client that sends `tools` and expects `tool_use` blocks back will not
work.** That is most of Claude Code's real usage. The endpoint refuses such a
request with a `400` naming the limitation rather than returning prose where the
caller expected a tool call. It serves text-shaped Anthropic clients; it does
not make tmuxlet-server a drop-in Anthropic provider.

## Dependency budget

Synchronous (no tokio). Direct deps: `tiny_http`, `ctrlc`, `serde`,
`serde_json`, `toml`, `rustls` (ring backend — no C toolchain), `webpki-roots`,
`portable-pty`, `nix` (already in-tree via `portable-pty`; used to disable PTY
echo). ~25 crates including transitive.

## Status

**V1 complete**, plus the optimization spec (auto-router, per-backend
concurrency caps, failure-aware cooldown, CORS, optional auth, prompt shaping,
custom-CA TLS, HEAD/405) implemented on top. All three backend `dispatch`
functions (`src/backend/{tmuxlet,api,cli}.rs`) are implemented and tested;
`/health`, `/v1/models`, `/v1/backends`, and `/v1/chat/completions` (streaming
and non-streaming) all work, as does the fallback chain. CI (`fmt + clippy +
test`) is green on macOS and Linux. See [`docs/STATUS.md`](./docs/STATUS.md) for
the verification summary and
[`docs/superpowers/plans/2026-05-28-tmuxlet-server.md`](./docs/superpowers/plans/2026-05-28-tmuxlet-server.md)
for the design.

## Attribution

The tool-signal tables in `src/route/signals.rs` and the scorer constants and
formula in `src/route/scorer.rs` are derived from
[NVIDIA-NeMo/Switchyard](https://github.com/NVIDIA-NeMo/Switchyard)
(`crates/libsy/src/algorithms/util/{tool_signals,stage}.rs`), Apache-2.0, read at
`main` on 2026-08-11. See [`NOTICE`](./NOTICE). The trace-mining behind those
tables — anchoring `"file does not exist"` as a full phrase so it does not fire
on `ls` output, requiring a non-zero integer before `failed`/`errors` so cargo's
`0 failed` is not read as a failure — is real work by other people.

## License

MIT © Codefi Foundation on Rural Innovation
