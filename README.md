# mur-model-gateway

Local LLM API gateway for the [MUR](https://github.com/mur-run/mur) agent platform.
It lets your MUR agents (and any other local tool) call **Anthropic, OpenAI, and
Gemini** through one local endpoint — reusing the credentials you already have,
including your Claude Code subscription login, instead of a separate per-token API key.

```
agents / tools ──► http://127.0.0.1:8088 ──► api.anthropic.com
                                         ──► api.openai.com
                                         ──► generativelanguage.googleapis.com
```

## Why

- **Sublet your subscriptions.** Requests that arrive without auth get the right
  credentials attached from the OS keychain (e.g. the OAuth token Claude Code
  stores), so agents share the plan you already pay for.
- **One outlet.** Point every tool at `127.0.0.1:8088`; the gateway routes by
  path — `/v1/messages*` → Anthropic, `/v1/chat/completions*` / `/v1/embeddings*`
  → OpenAI, `/v1beta/models/*` → Gemini.
- **Optional wire compression.** `MUR_MODEL_GATEWAY_COMPRESS=1` applies MUR's
  CCR compression to `tool_result` blocks on all three providers.

**Up to 90.4% token savings** — 3,026 compressions turned 5.89M input tokens into
568K, saving 5.32M (mur-compress v2.61.0, single day):

![Token compression stats: 3,026 compressions, 5,891,050 input tokens, 568,301 output tokens, 5,322,749 tokens saved, 90.4% savings](compress-ratio.png)

## Install

Grab a binary from [Releases](https://github.com/mur-run/mur-model-gateway/releases)
(macOS universal, Developer-ID signed and notarized; Linux x86_64; Windows x86_64),
or build from source (Rust ≥ 1.85):

```bash
cargo build --release
```

Run it as a background service (launchd / systemd / Task Scheduler descriptors
are generated for you):

```bash
mur-model-gateway install          # writes + starts the service for your platform
mur-model-gateway status           # binary / service / env file overview
mur-model-gateway uninstall
```

See [docs/install.md](docs/install.md) for per-platform details and
[docs/compress-setup.md](docs/compress-setup.md) for compression.

## Use with MUR

Point a model registry entry at the gateway:

```yaml
# ~/.mur/models.yaml
models:
  - alias: sonnet
    provider: anthropic
    model: claude-sonnet-5
    base_url: http://127.0.0.1:8088
```

Agents using that alias now ride your Claude Code login. Because the released
binary is signed with a stable Developer ID, macOS asks for keychain access
**once** — "Always Allow" survives every update.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `MUR_MODEL_GATEWAY_BIND` | `127.0.0.1:8088` | Listen address |
| `MUR_MODEL_GATEWAY_TOKEN_SOURCE` | `keychain` | `keychain`, `off`, `env:<VAR>`, `file`, or `file:<path>` |
| `MUR_MODEL_GATEWAY_UPSTREAM_ANTHROPIC` | `https://api.anthropic.com` | Anthropic upstream |
| `MUR_MODEL_GATEWAY_UPSTREAM_OPENAI` | `https://api.openai.com` | OpenAI upstream |
| `MUR_MODEL_GATEWAY_UPSTREAM_GEMINI` | `https://generativelanguage.googleapis.com` | Gemini upstream |
| `MUR_MODEL_GATEWAY_COMPRESS` | off | `1` enables tool_result compression |

## Security notes

- Binds to loopback by default; it is a *local* outlet, not a shared server.
- Requests that already carry auth are forwarded untouched; credential injection
  only happens for auth-less local callers.
- Tokens are read from the OS keychain (macOS Keychain / Linux keyutils /
  Windows Credential Manager) and cached for 60 s; they are never written to disk.

## License

[MIT](LICENSE)
