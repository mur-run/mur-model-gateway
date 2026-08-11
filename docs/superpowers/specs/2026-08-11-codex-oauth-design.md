# Codex OAuth Token Support

**Date**: 2026-08-11
**Status**: draft

## Context

The gateway lets auth-less local callers ride the subscription you already pay for: a request
arriving without credentials gets Claude Code's OAuth token attached from the OS keychain and is
forwarded to Anthropic. That trick is Anthropic-only. `src/lib.rs:264-268` gates injection on
`provider == Provider::Anthropic` **and** a `/v1/messages*` path, so the OpenAI and Gemini routes
are pure passthrough — no credential path exists for them at all.

The same subletting should work for a ChatGPT subscription through Codex. Codex stores its
credentials at `~/.codex/auth.json`, and in `auth_mode = "chatgpt"` (the case here — `OPENAI_API_KEY`
is null) the credential is an OAuth token pair plus an account id, not an API key. A ChatGPT-mode
token authenticates against ChatGPT's Codex backend, **not** `api.openai.com`, so the existing
OpenAI route cannot carry it. This needs a distinct provider path.

Outcome: an auth-less caller hitting the gateway on the Codex route reaches ChatGPT's Codex backend
with the user's Codex credentials attached, refreshed automatically when they expire.

## Decisions

| Question | Decision |
|---|---|
| What the credentials enable | A new ChatGPT/Codex provider path, on top of per-provider token sources |
| Expired access tokens | Refresh in memory on 401; never write back to `auth.json` |
| Routing | Staged — `/v1/responses*` passthrough first; Chat Completions translation is a separate spec |
| Codex client headers | Hidden behind a build.rs cfg, mirroring the Anthropic disguise |

## Approach

### Provider and routing

Add `Provider::Codex` to the enum at `src/lib.rs:41`. `detect_provider` maps `/v1/responses*` to it;
that path currently falls through to the `Provider::Anthropic` default, so the assertions at
`src/lib.rs:497-509` gain a sibling case.

Add `DEFAULT_UPSTREAM_CODEX = "https://chatgpt.com/backend-api/codex"` beside the three constants at
`src/lib.rs:30-32`, overridable by `MUR_MODEL_GATEWAY_UPSTREAM_CODEX`, and wire it into
`upstream_for` (`src/lib.rs:191`).

The other three providers concatenate base URL and incoming path unchanged. Codex cannot: the client
sends `/v1/responses`, the backend expects `<base>/responses`. Codex therefore needs a path rewrite
that strips the `/v1` prefix. This is the only place the new provider is not symmetric with the
existing three, and it is the most likely source of a silent 404.

### Per-provider token sources

`AppState.token_source` becomes a per-provider lookup.

- `MUR_MODEL_GATEWAY_TOKEN_SOURCE` keeps its present meaning as the fallback for every provider, so
  existing installs behave identically after upgrade.
- `MUR_MODEL_GATEWAY_TOKEN_SOURCE_ANTHROPIC` and `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` override per
  provider.
- Defaults: Anthropic → `keychain` (unchanged), Codex → the new `codex` source.

`TokenSource::resolve()` returns `Option<String>` today (`src/lib.rs:114`). Codex needs both
`access_token` and `account_id`, so it returns `Credential { token: String, account_id:
Option<String> }` instead. The existing call sites are few.

`install.rs` bakes the new variables into the service descriptors through `env_pairs`
(`src/install.rs:100`), so a `--system` install carries them.

### Credential reading and refresh

New `src/codex.rs` reads `~/.codex/auth.json` → `tokens.{access_token, refresh_token, account_id}`,
reusing the TTL cache helper already at `src/keychain.rs:60` rather than adding a second caching
mechanism.

On a 401 from the ChatGPT backend: refresh once against the OAuth token endpoint using
`refresh_token`, retry the request exactly once, and keep the new access token in memory. One retry,
never a loop. `~/.codex/auth.json` is never written — Codex CLI owns that file, and two writers race.

### Hidden implementation

Mirrors `src/disguise.rs`. Tracked `src/codex.rs` exposes `should_route()` and a cfg-gated module:

```
build.rs ── src/codex/codex_impl.rs exists? ── yes ──> rustc --cfg has_codex_hook
                                             └─ no ──> no-op stubs, plain passthrough
```

`build.rs` gains `has_codex_hook` beside `has_beta_hook`, with matching `rustc-check-cfg` and
`rerun-if-changed`. `.gitignore` gains `src/codex/codex_impl.rs`.

**The `mod codex_impl;` declaration must carry `#[rustfmt::skip]`.** rustfmt resolves `mod`
declarations syntactically and ignores `#[cfg]`, so without it `cargo fmt --check` fails on any
clean checkout — exactly the CI break fixed in `53c3b08`.

Hidden in `codex_impl.rs`: the Codex client header set (`chatgpt-account-id`, `originator`,
`session_id`, User-Agent, `openai-beta`), the OAuth client id, and the token endpoint. These values
are read from the locally installed Codex CLI (`~/.npm-global/bin/codex`, codex-cli 0.139.0, a Node
bundle) at implementation time. They are never invented, and never committed to tracked source.

### Deliberately out of scope

- **Compression is off for Codex.** `compress::should_compress` returns false for the new provider.
  The Responses body is not the messages body, and guessing at its shape would corrupt requests.
- **Chat Completions translation is stage 2**, with its own spec. MUR's OpenAI client only speaks
  `POST $base_url/chat/completions` (`mur-agent-runtime/src/llm/openai.rs:296,373`) and has no
  Responses client, so until stage 2 lands no MUR agent can reach this path. Stage 1 is verified by
  calling `/v1/responses` directly.

## Error handling

| Condition | Behaviour |
|---|---|
| `~/.codex/auth.json` missing or unparseable | Resolve to `None` → passthrough, warning logged. Matches the existing keychain-failure path. |
| `auth_mode` is not `chatgpt` (API-key mode) | Resolve to `None` → passthrough. Stage 1 does not handle API-key mode. |
| Upstream 401 | Refresh once, retry once. A second 401 is returned to the caller unchanged. |
| Refresh itself fails | Return the original 401 to the caller, log the refresh error. Never a retry loop. |

## Verification

Unit tests in the modules they cover:
- `detect_provider("/v1/responses")` → `Provider::Codex`, and the existing default-to-Anthropic
  assertions still hold for unrelated paths.
- The `/v1` path rewrite produces `<base>/responses`.
- `auth.json` parsing against a fixture containing fake tokens — no real credential in the repo.

Integration test `tests/codex.rs`, following the shape of `tests/disguise.rs`, with httpmock standing
in for the ChatGPT backend:
- A request carrying no auth arrives upstream with a Bearer token and the account-id header.
- A 401 triggers exactly one refresh and one retry; assert the mock saw two requests, not three.

Full gate before claiming done: `cargo fmt --check && cargo clippy --all-targets -- -D warnings &&
cargo test`, plus one run from a clean `git archive HEAD` tree to prove the public build still
compiles with the stubs and that fmt does not trip over the gitignored module.

**End-to-end against the real backend requires a fresh `codex login`.** `last_refresh` on this
machine is 2026-07-10, so the stored access token is almost certainly expired — which exercises the
refresh path, but means a failure there is ambiguous until a known-good login exists. The httpmock
tests have no such dependency.
