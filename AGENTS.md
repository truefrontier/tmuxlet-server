# AGENTS.md

## Audience

You are an AI coding agent installing **tmuxlet-server** on the user's machine.
Follow these steps in order. Each step has a precondition, a command, an
expected outcome, and a verification you must run before moving on. Do not ask
the user questions you can answer by running a command.

> **Status:** V1 complete. All three backend `dispatch` functions
> (`src/backend/{tmuxlet,api,cli}.rs`) are implemented and tested, and the
> optimization spec (auto-router, per-backend concurrency caps, cooldown,
> optional bearer auth, CORS, prompt shaping, custom-CA TLS, HEAD/405) is also
> implemented; `/health`, `/v1/models`, and `/v1/chat/completions` (streaming
> and non-streaming) all work. See the implementation plan at
> `docs/superpowers/plans/2026-05-28-tmuxlet-server.md` for design details.

## Step 1: Verify prerequisites

```
command -v cargo   # Rust toolchain (edition 2024 / Rust >= 1.92)
command -v tmux    # required only if you use a `tmuxlet` backend
```

For each configured backend type you also need its tool:
- `tmuxlet` backend → `command -v tmuxlet` and the target CLI (`command -v claude`).
- `cli` backend → the `bin` path must exist and be executable. **agy specifically
  is a symlink into `/Applications/Antigravity.app`** — verify with
  `test -e "$(readlink -f /Users/$USER/.antigravity/antigravity/bin/agy)" && echo OK || echo "BROKEN: install Antigravity.app"`.
- `api` backend → the upstream must be reachable (`curl -fsS http://127.0.0.1:11434/api/tags` for Ollama).

If a prerequisite is missing, STOP and tell the user which to install.

## Step 2: Install

```
cargo install --git https://github.com/CodefiLabs/tmuxlet-server --locked --force
tmuxlet-server --version
```
Expected: prints `tmuxlet-server <version>`. If not, the `cargo install` failed —
read its output. **`--locked` is required**: it pins the dependency versions in
the repo's `Cargo.lock`. Without it Cargo resolves to the latest releases, and
`dispatch2 0.3.1` (a transitive macOS dep of `ctrlc`) fails to build with a
`recursion limit reached` macro error.

## Step 3: Write config

Steps 3 and 5 (and the optional `./scripts/smoke.sh` in Step 6) use files from
this repo. `cargo install --git` leaves no checkout, so run them from a clone
(`git clone --depth 1 https://github.com/CodefiLabs/tmuxlet-server && cd tmuxlet-server`).

```
mkdir -p ~/.tmuxlet
cp -n examples/server.toml ~/.tmuxlet/server.toml   # then edit for the detected backends
```
`cp -n` never overwrites. If the user already has a config (or plist, or unit
file in Step 5), it exits 1 and changes nothing: edit the existing file instead.
Use only backends whose prerequisites passed Step 1. `api_key_env` holds the
NAME of an env var (e.g. `OPENROUTER_API_KEY`), never the key itself.

For `cli` backends, put `{prompt}` in `args` where the tool expects the prompt
(e.g. `args = ["-p", "{prompt}"]` for a tool whose `--print` flag takes the
prompt as its value, like `agy`). Without the placeholder the prompt is appended
as a trailing positional arg (non-PTY) or written to the child's stdin (PTY).

## Step 4: Validate config

```
tmuxlet-server --validate
```
Expected: `OK (N backends, M chains)` and exit 0. On error the message names the
offending key (e.g. `chain 'default' references undefined backend 'ghost'`).
`--validate` is strict: config smells such as an unknown key fail it, while the
running server only logs them as warnings.

```
tmuxlet-server --check-backends
```
Expected: one `PASS <name> (<type>) — <detail>` line per backend and exit 0. A
`FAIL` line names the binary it could not find or the address it could not
connect to. The probe runs `tmuxlet --version` for tmuxlet backends and only
opens a TCP connection for api backends; it sends no prompts.

## Step 5: Install as a service

### macOS (LaunchAgent)
```
cp -n examples/launchagent.plist ~/Library/LaunchAgents/com.codefilabs.tmuxlet-server.plist
# edit paths/username inside the plist, then:
launchctl load ~/Library/LaunchAgents/com.codefilabs.tmuxlet-server.plist
launchctl list | grep tmuxlet-server      # verification
```

### Linux (systemd user)
```
mkdir -p ~/.config/systemd/user
cp -n examples/tmuxlet-server.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now tmuxlet-server
systemctl --user status tmuxlet-server    # verification
```

> Startup note: with `env_source = "shell"` the server runs `$SHELL -ilc env`
> once at boot (~1s with a heavy interactive zsh) so PATH entries from
> `~/.zshrc` (e.g. agy) are visible to backends. Set `env_source = "process"` to
> skip that if your service environment already has the right PATH.

## Step 6: Smoke test

```
curl -fsS http://127.0.0.1:3456/health
```
Expected: `{"status":"ok","version":"…","backends":N,"chains":M}`.

```
curl -fsS -X POST http://127.0.0.1:3456/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"default","messages":[{"role":"user","content":"say READY"}]}'
```
Expected: a chat completion JSON with non-empty `choices[0].message.content`.
(Or run `./scripts/smoke.sh`.)

## Step 7: Wire downstream clients

Point any OpenAI-compatible client at `http://127.0.0.1:3456/v1`. Auth is OFF
by default, so any (or an empty) API key works; but if the config sets
`[server] auth = true` the client MUST send the bearer token — it lives in
`~/.tmuxlet/token` (run `cat ~/.tmuxlet/token`) or in the env var named by
`auth_token_env`. The `model` field selects a chain name (e.g. `default`,
`thinking`), a single backend name, or a `[routers.*]` name. `GET /v1/models`
lists every valid id.

## Troubleshooting

| Symptom | Fix |
|---|---|
| `cargo install` fails: `recursion limit reached` in `dispatch2` | you omitted `--locked`; re-run the Step 2 command with `--locked` so the pinned `dispatch2 0.3.0` is used instead of the broken `0.3.1` |
| `failed to bind 127.0.0.1:3456` | another process owns the port; change `[server] listen` |
| chat returns `503 all_backends_failed` | read the `error.details` array; each entry names the backend and its error (only the backend and error class when bound to a non-loopback address) |
| `agy` leg always fails / `spawn failed` | `agy` symlink is broken — install Antigravity.app |
| api backend `HTTP 401` | the `api_key_env` var is unset in the captured env; check Step 5 startup note |
| startup hangs ~seconds | interactive-shell env capture; set `env_source = "process"` |
| chat returns `401 unauthorized` | auth is enabled (`[server] auth = true`); send `Authorization: Bearer $(cat ~/.tmuxlet/token)` (or the token from your `auth_token_env` var). `/health` stays open without a token |
