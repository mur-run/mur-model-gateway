# mur-model-gateway installation guide

`mur-model-gateway install` writes the platform-appropriate service definition and bakes the
configuration into environment variables. The runtime reads environment variables only
(`MUR_MODEL_GATEWAY_TOKEN_SOURCE*` / `MUR_MODEL_GATEWAY_BIND` / `MUR_MODEL_GATEWAY_UPSTREAM*` /
`MUR_MODEL_GATEWAY_COMPRESS`) — there is no config file.

## Quick start

```bash
./scripts/auto.sh             # fully automatic: detects GLIBC (→musl), headless (→--system) and token source, installs and starts
./scripts/setup.sh            # build → install to ~/.local/bin → register user service → start
./scripts/setup.sh --no-service   # install the binary only
./scripts/setup.sh --uninstall    # remove the service (the binary stays)
```

Arguments after `--` are passed through to `mur-model-gateway install` verbatim:

```bash
./scripts/setup.sh -- --token-source file --bind 127.0.0.1:9099
```

## install flags

| Flag | Effect |
|------|--------|
| `--token-source <spec>` | Bakes in `MUR_MODEL_GATEWAY_TOKEN_SOURCE` (see below) |
| `--token-source-codex <spec>` | Bakes in `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` — credential source for the `/v1/responses` route (see below) |
| `--bind <addr>` | Bakes in `MUR_MODEL_GATEWAY_BIND` (default `127.0.0.1:8088`) |
| `--upstream <url>` | Bakes in `MUR_MODEL_GATEWAY_UPSTREAM` |
| `--compress` / `--no-compress` | Toggles `MUR_MODEL_GATEWAY_COMPRESS=1` (with neither flag, the environment is sniffed) |
| `--system` | Linux only: system-level unit (see below) |

Values may not contain whitespace or `<>"&` — they are spliced straight into the plist/unit/cmd,
so `install` rejects them up front.

## Token sources (`--token-source`)

| spec | Behaviour |
|------|-----------|
| `keychain` (default) | Reads `Claude Code-credentials` from the OS keychain. **Off macOS it falls back to `~/.claude/.credentials.json` automatically** (Claude Code on Linux/Windows writes that file rather than the keychain), so any machine already logged into Claude Code works with zero configuration |
| `file` | Reads `~/.claude/.credentials.json` |
| `file:/path/to/credentials.json` | Reads the given JSON (same `claudeAiOauth.accessToken` shape) |
| `env:VAR` | Reads the token from environment variable `VAR` on every request (use this on headless hosts with no Claude Code) |
| `off` / `disabled` | Pure passthrough, no disguise |

The token is always re-read per request, so a background refresh by Claude Code takes effect
automatically.

## Codex token source (`--token-source-codex`)

| spec | Behaviour |
|------|-----------|
| `codex` (default) | Reads `~/.codex/auth.json`, the file `codex login` writes |
| `env:VAR` | Reads the token from environment variable `VAR` on every request |
| `off` / `disabled` | Pure passthrough on `/v1/responses`, no disguise |

This governs only the `/v1/responses` route — `--token-source` above still governs
Anthropic/OpenAI/Gemini. `codex` is already the runtime default with no flag needed; a rotated
access token is written back to `~/.codex/auth.json` automatically when the upstream refreshes it.

**No MUR agent can reach `/v1/responses` yet.** MUR's OpenAI client only speaks Chat Completions,
not the Responses API, so this route is reachable today only by a client that already speaks the
Responses API directly (`curl`, the Codex CLI itself).

### A request that works

Verified against the live backend on 2026-08-11. Note `input` is a **list**, not a string, and the
model must be one your ChatGPT account may use — `codex` lists them under `/model`. As of
codex-cli 0.147.0 those are the `gpt-5.6-*`, `gpt-5.5` and `gpt-5.4*` family; `gpt-5`,
`gpt-5-codex`, `codex-mini-latest` and `o4-mini` are all rejected.

```bash
curl -sS -X POST http://127.0.0.1:8088/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5.6-sol",
       "input":[{"type":"message","role":"user",
                 "content":[{"type":"input_text","text":"say ok"}]}],
       "store":false,"stream":true}'
```

Expect an SSE stream beginning `event: response.created`. Omitting the `content-type` header sends
form-encoded data and earns `Unsupported content type` from the backend.

## Per platform

### macOS (launchd)

```bash
mur-model-gateway install [flags]
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/run.mur-model-gateway.plist
launchctl enable gui/$(id -u)/run.mur-model-gateway
```

Log: `~/Library/Logs/mur-model-gateway/proxy.log`.

### Linux — user unit (default)

```bash
mur-model-gateway install [flags]
systemctl --user daemon-reload
systemctl --user enable --now mur-model-gateway.service
```

⚠️ A user unit only runs while you are logged in. For headless hosts or start-at-boot, either
`loginctl enable-linger $USER` or use `--system`.

### Linux — system unit (`--system`, headless servers)

```bash
sudo mur-model-gateway install --system --token-source env:MUR_MODEL_GATEWAY_OAUTH_TOKEN
# then add the token to the env file yourself (keep it out of shell history and tool output):
#   sudoedit /etc/mur-model-gateway.env   → add a line MUR_MODEL_GATEWAY_OAUTH_TOKEN=sk-ant-oat01-…
sudo systemctl daemon-reload
sudo systemctl enable --now mur-model-gateway.service
journalctl -u mur-model-gateway.service -f
```

What it produces:
- `/etc/systemd/system/mur-model-gateway.service` — `User=<whoever ran install>`,
  `EnvironmentFile=/etc/mur-model-gateway.env`, `WantedBy=multi-user.target` (starts at boot, no login needed)
- `/etc/mur-model-gateway.env` — root-owned, mode 600; every environment variable (secrets included) lives here

`setup.sh --system -- <flags>` runs all of the above under sudo for you.

### Windows (Task Scheduler)

```powershell
mur-model-gateway install [flags]
# install prints a ready-to-paste command (run it from an elevated prompt):
schtasks /Create /F /SC ONLOGON /TN mur-model-gateway /TR "\"C:\Users\you\AppData\Local\mur-model-gateway\mur-model-gateway.cmd\""
schtasks /Run /TN mur-model-gateway
```

The `.cmd` contains every `set` line for the environment variables and redirects output to
`%LOCALAPPDATA%\mur-model-gateway\logs\proxy.log`. `/F` lets a repeat install overwrite in place.

## Hosts with an old GLIBC (e.g. Ubuntu 20.04 / GLIBC 2.31)

The dynamically linked release build needs GLIBC ≥2.34 and simply will not start on older systems
(`version 'GLIBC_2.34' not found`). Use the static musl build instead:

```bash
./scripts/setup.sh --musl [--system] [-- <install flags>]
```

With no musl toolchain on the machine, build in Docker as the script's own hint suggests:

```bash
docker run --rm -v "$PWD":/src -w /src rust:1.91-bookworm bash -c \
  'rustup target add x86_64-unknown-linux-musl && apt-get update && \
   apt-get install -y musl-tools && cargo build --release --target x86_64-unknown-linux-musl'
```

Note: mur-model-gateway is edition 2024 and needs Rust ≥1.85 (a rust:1.91-or-newer image is recommended).

## Uninstall / status

```bash
mur-model-gateway status      # lists the binary, the user/system service files and whether the env file exists
mur-model-gateway uninstall   # removes the service/env files on both the user and system side (/etc needs sudo; it prints a hint)
```

## Troubleshooting

- **systemd restart storm, `Address in use (os error 98)`** — a leftover mur-model-gateway
  process is holding the port: find it with `ss -ltnp | grep 8088`, kill it, then start again.
- **Upstream returns 404 `not_found_error: model: …`** — that reply comes from Anthropic itself,
  which means **authentication succeeded** and only the model id is stale. It is not a proxy
  routing problem.
- **Codex route returns `model is not supported…` or `Input must be a list`** — same story: these
  come from ChatGPT's backend, so **authentication succeeded** and only the request body is wrong.
  An unauthenticated request gets 401/403 instead, and one carrying a bad account id never reaches
  an entitlement decision. See the Codex section above for a request shape that works.
- **Pointing an app at it** — set `ANTHROPIC_BASE_URL=http://127.0.0.1:8088` in the app, and that
  is all. Requests carrying an OAuth-shaped key, or no auth at all, go through the disguise;
  requests with a normal `sk-ant-api03-*` key are passed through untouched.
