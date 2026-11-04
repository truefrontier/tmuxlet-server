# tmuxlet-server

A single static Rust binary that exposes an **OpenAI-compatible
`/v1/chat/completions`** endpoint backed by a configurable fallback chain of
heterogeneous AI backends:

- **`tmuxlet`** — interactive coding CLIs (claude, codex, grok, …) run through
  [`tmuxlet`](https://github.com/truefrontier/tmuxlet).
- **`cli`** — arbitrary CLI tools, optionally in a PTY (for tools like `agy`
  that detect a non-TTY and redirect output). Use the `{prompt}` placeholder in
  `args` to inject the prompt as a flag value (e.g. `args = ["-p", "{prompt}"]`);
  without it the prompt is appended as the final positional argument.
- **`api`** — any OpenAI-compatible HTTP upstream (Ollama, OpenRouter, …).

> Companion project: [**truefrontier/tmuxlet**](https://github.com/truefrontier/tmuxlet)
> drives the interactive CLIs behind the `tmuxlet` backend.

Downstream clients (Continue, Cursor, Hermes, anything OpenAI-shaped) see one
endpoint; the routing happens server-side from a TOML config. A request's
`model` field selects a named chain, a single backend, or an auto-router (a
classifier backend picks the chain or backend for each request); the server
walks the chain in order until one succeeds. An unknown `model` runs `default_chain`, or gets a
`404` when `strict_models = true`.

## Install

```
cargo install --git https://github.com/truefrontier/tmuxlet-server --locked --force
mkdir -p ~/.tmuxlet
cp -n examples/server.toml ~/.tmuxlet/server.toml   # from a clone of this repo; then edit
tmuxlet-server --validate
tmuxlet-server --check-backends                   # optional: probe each backend
tmuxlet-server                                    # serve on 127.0.0.1:3456
```

`--locked` is required: it installs the dependency versions pinned in this
repo's `Cargo.lock`. Without it, Cargo resolves to the latest semver-compatible
releases, and `dispatch2 0.3.1` (pulled in transitively by `ctrlc` on macOS)
fails to compile with a `recursion limit reached` macro error.

For step-by-step install + service setup + verification, see
[`AGENTS.md`](./AGENTS.md). Example config: [`examples/server.toml`](./examples/server.toml).

## HTTP surface (V1)

| Method | Path | Behavior |
|---|---|---|
| `GET` | `/health` | `{"status":"ok","version":"…","backends":N,"chains":M}` |
| `GET` | `/v1/models` | OpenAI-compatible list of router, chain, and backend names |
| `GET` | `/v1/backends` | per-backend health: `state` (ok\|cooling\|busy\|unknown), consecutive_failures, last_error, last_latency_ms, cooling_secs |
| `POST` | `/v1/chat/completions` | OpenAI chat completions (JSON or SSE) |

Reserved namespaces (`/`, `/ui/*`, `/api/*`) return `501` for a future web UI. A
known path reached with the wrong method returns `405` with an `Allow` header
(not `404`); `HEAD` on a GET route mirrors GET with an empty body. Everything
else returns `404`. Errors use the OpenAI error envelope.

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
`cat ~/.tmuxlet/token` is the whole client setup.

## Streaming limitation (V1)

V1 backends are blocking: `cli`/`tmuxlet` emit output only after the child
exits, and `api` is a single non-streamed call. So with `stream: true` the
server sends the `200` status and SSE headers immediately, then emits
`: keepalive` comment frames every 15s while the backend runs; once the response
is ready the real frames (role-prime → content → final → `[DONE]`) are emitted
all at once. True incremental token streaming is V2. Because the headers go out
first, a streamed auto-router response's `x-tmuxlet-route` header is
`class/chain`; a non-streamed one adds the winning backend
(`class/chain/backend`).

## Dependency budget

Synchronous (no tokio). Direct deps: `tiny_http`, `ctrlc`, `serde`,
`serde_json`, `toml`, `rustls` (ring backend — no C toolchain), `webpki-roots`,
`portable-pty`, `nix` (already in-tree via `portable-pty`; used to disable PTY
echo). About 50 crates including transitive ones on macOS
(`cargo tree -e normal`).

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

## License

MIT © Codefi Foundation on Rural Innovation
