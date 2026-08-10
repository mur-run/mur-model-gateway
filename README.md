# mur-model-gateway

Local LLM API gateway for the [MUR](https://github.com/mur-run/mur) agent platform.
It lets your MUR agents (and any other local tool) call **Anthropic, OpenAI, and
Gemini** through one local endpoint — also provide compress feature.

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
- **Cheap to leave running.** One static Rust binary, ~60–70 MB resident —
  no runtime, no Node process, no container.

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
are generated for you). **Compression is off unless you ask for it** — pass
`--compress` and it is baked into the service definition, so it survives
restarts and reinstalls:

```bash
mur-model-gateway install --compress   # writes + starts the service, compression on
mur-model-gateway status               # binary / service / env file overview
mur-model-gateway uninstall
```

From a source checkout, `./scripts/setup.sh -- --compress` does the build, the
install and the service registration in one shot.

Restoring a compressed `tool_result` needs the mur MCP server attached to your
client — that is what provides `mur_retrieve`. See
[docs/install.md](docs/install.md) for per-platform details and
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

## Resource usage and sizing

Measured on an Apple M4 (4P+6E, 16 GB), release build, against a mock upstream
on loopback so the numbers isolate the gateway rather than the network.
Concurrency 4, CPU sampled from the OS across each run.

| Mode | req/s | CPU per request | RSS |
|---|---|---|---|
| Passthrough or disguise, no compression | 5,000–22,000 | 0.05–0.2 ms | 9–45 MB |
| Compression on, 2 KB body (nothing eligible) | 104 | 27 ms | ~130 MB |
| Compression on, 32 KB `tool_result` | 97 | 38 ms | ~140 MB |
| Compression on, 128 KB `tool_result` | 59 | 66 ms | ~140 MB |

**Without compression the gateway is effectively free.** A real instance
serving Claude Code for 8 hours used 2m22s of CPU — 0.5% of one core — and
43 MB of RSS. Anything runs it: a Raspberry Pi, the smallest cloud VM, a spare
laptop.

**Compression is the entire cost.** The engine is constructed per request, so
turning it on adds a fixed ~27 ms of CPU and ~120 MB of RSS even to requests
with nothing large enough to compress, plus roughly 0.3 ms per KB of
`tool_result`. Budget about **35 compressed requests per second per core**.

For one person this is a non-question: 29 ms on a 5–30 second model call is
under 1% of the round trip, against a ~90% token saving. For a shared gateway,
size on requests rather than seats — a developer driving an agent hard issues
on the order of 20 requests/minute (0.33/s), so one core carries roughly 100
such users with compression on, and far more without it.

**Hardware is rarely the real limit.** Every request rides a single Claude Code
login, so the ceiling is that account's upstream rate limit, which you will hit
long before the CPU does. Size for the rate limit, not the box.

Two things grow with use: `~/.mur/compress` keeps every compressed original
(deduplicated by hash) so `mur_retrieve` can restore it, and each in-flight
request holds its body in memory — peak RSS tracks `concurrency × body size`
(50 concurrent 150 KB requests measured at 190 MB).

## Security notes

- Binds to loopback by default; it is a *local* outlet, not a shared server.
- Requests that already carry auth are forwarded untouched; credential injection
  only happens for auth-less local callers.
- Tokens are read from the OS keychain (macOS Keychain / Linux keyutils /
  Windows Credential Manager) and cached for 60 s; they are never written to disk.

## License

[MIT](LICENSE)
