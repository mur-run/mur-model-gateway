# Codex API-Key Mode (Stage 3)

**Date**: 2026-08-11
**Status**: draft

## Context

Stages 1 and 2 shipped a working Codex route against a ChatGPT subscription. `/v1/responses*` reaches
ChatGPT's Codex backend with the user's `~/.codex/auth.json` OAuth credentials attached and refreshed
on 401 (`2026-08-11-codex-oauth-design.md`); `/codex/v1/chat/completions` translates Chat Completions
in and Responses out, so a MUR agent — whose OpenAI client only speaks Chat Completions — can ride
the subscription (`2026-08-11-codex-chat-translation-design.md`).

That route is bound to a single credential shape. `~/.codex/auth.json` carries `auth_mode`; when it is
`"chatgpt"`, the credential is an OAuth token pair plus an account id, which authenticates against
`chatgpt.com/backend-api/codex` — not `api.openai.com`. When it is `"apikey"`, the file instead
carries an `OPENAI_API_KEY` value, which authenticates against `api.openai.com/v1/responses`. Stage 1
deliberately resolved API-key mode to `None` → passthrough, recording it as out of scope.

No MUR agent can use the Codex route with an API key today. A user whose ChatGPT subscription is
unavailable — or who prefers pay-per-use — has an `OPENAI_API_KEY` but no way to point the gateway at
`api.openai.com`. Stage 3 closes that gap.

Outcome: when `~/.codex/auth.json` says `auth_mode = "apikey"`, the gateway attaches the stored API
key to Codex-route requests and sends them to `api.openai.com/v1/responses`, on both the raw
`/v1/responses` path and the translated `/codex/v1/chat/completions` path. The OAuth path is
untouched.

## Decisions

| Question | Decision |
|---|---|
| Where the key comes from | `~/.codex/auth.json`, `auth_mode = "apikey"`, `OPENAI_API_KEY` field — matching Codex CLI |
| Which upstream serves API-key mode | Hard-coded `https://api.openai.com`, not `MUR_MODEL_GATEWAY_UPSTREAM_CODEX` |
| Detection boundary | Only `TokenSource::Codex` (reads auth.json) looks at `auth_mode`; EnvVar/Static sources keep today's behavior |
| How the two modes coexist | A `CodexCredential` enum in `src/codex.rs` — `OAuth { .. }` vs `ApiKey { .. }` — threaded through `forward()` |
| Path prefix | API-key mode keeps `/v1` on the upstream path (`/v1/responses`); OAuth strips it (`/responses`) |
| API-key 401 | No retry — there is no refresh token to redeem. The upstream 401 is returned unchanged |
| API-key headers | Plain `Authorization: Bearer <key>`, set in tracked code; the gitignored `codex_impl.rs` hook is OAuth-only and untouched |
| Translation | `/codex/v1/chat/completions` still translates in API-key mode; streaming and aggregation are credential-agnostic |
| Compression | Unchanged — runs before translation, reusing the OpenAI rewriter |

### Why the enum

`forward()` currently resolves `codex_cred: Option<(String, Option<String>)>` — a token and an
optional account id. API-key mode differs from OAuth in four coupled ways: the upstream, the path
prefix, the header shape, and the 401 behaviour. A tuple cannot express that coupling; a
`CodexCredential` enum forces the two modes apart at compile time, so API-key requests can never
accidentally get the OAuth upstream, the OAuth headers, or an OAuth refresh-on-401.

### Why hard-coded `api.openai.com`

Codex CLI in API-key mode targets the public Responses endpoint. There is no reason to make it
configurable before a caller needs it, and `MUR_MODEL_GATEWAY_UPSTREAM_CODEX` describes the ChatGPT
backend — reusing it for a different host would conflate two upstreams behind one variable. A
`DEFAULT_UPSTREAM_CODEX_APIKEY = "https://api.openai.com"` constant beside the existing
`DEFAULT_UPSTREAM_CODEX` keeps the two visibly separate.

### Why only `TokenSource::Codex`

Production Codex credentials come from `~/.codex/auth.json`, and only `TokenSource::Codex` reads that
file. The other sources — `EnvVar`, `Static`, `Disabled` — are test injection points and the fallback
path for callers who bring their own credential. Making them mode-aware would change existing test
behaviour for no production benefit. API-key mode is a property of the auth.json file, so it is
detected where that file is read.

## Ground truth (verified, 2026-08-11)

- `~/.codex/auth.json` stores `auth_mode` plus either `tokens` (chatgpt) or `OPENAI_API_KEY`
  (apikey). Codex CLI switches on this field.
- In API-key mode, Codex targets `https://api.openai.com/v1/responses` — the public Responses API —
  authenticating with `Authorization: Bearer <sk-…>` and no ChatGPT-specific client headers.
- ChatGPT's Codex backend (`chatgpt.com/backend-api/codex`) authenticates OAuth tokens, not API keys.
  Sending a `sk-` key there would 401. The two modes therefore have different upstreams.

## Approach

### Data model

Add to `src/codex.rs`:

```rust
pub enum CodexCredential {
    /// auth_mode = "chatgpt": OAuth token pair + account id → chatgpt.com backend.
    OAuth { access_token: String, account_id: Option<String> },
    /// auth_mode = "apikey": a plain OpenAI API key → api.openai.com.
    ApiKey { key: String },
}
```

Replace `read_auth` with `read_credential(path) -> Option<CodexCredential>` that dispatches on
`auth_mode`: `"chatgpt"` → `OAuth` (existing `parse_auth` logic), `"apikey"` → `ApiKey` when
`OPENAI_API_KEY` is a non-empty string, anything else → `None` (missing key, malformed JSON, or an
unrecognised mode → passthrough, warning logged). `CodexAuth` and `parse_auth` stay for the OAuth
arm; the debug-redaction impl is reused so no credential leaks through `{:?}`.

`TokenSource::Codex` resolution (`src/lib.rs:163`) returns the credential's access token / key for
the existing non-credential-bearing paths; `forward()` matches on the enum for the four coupled
behaviours.

### Upstream and path

`forward()` resolves the credential **before** building `target_url` (currently `src/lib.rs:315`).
The upstream and path-prefix decision moves beside that resolution:

| Mode | Upstream | `/v1/responses` | `/codex/v1/chat/completions` |
|---|---|---|---|
| OAuth | `state.upstream_for(Provider::Codex)` | `/responses` (strip `/v1`) | `/responses` |
| ApiKey | `DEFAULT_UPSTREAM_CODEX_APIKEY` | `/v1/responses` (keep `/v1`) | `/v1/responses` |

The upstream path is chosen by mode, and `forward()` selects the upstream by mode instead of
unconditionally calling `state.upstream_for(Provider::Codex)`:

- Raw route (`/v1/responses*`): OAuth strips `/v1` via `codex_target_path` (unchanged); ApiKey keeps
  the incoming path verbatim (`/v1/responses`) — the `/v1` string surgery is OAuth-only.
- Translated route (`/codex/v1/chat/completions*`): the fixed upstream path is mode-aware, as it
  already is for translate — OAuth → `/responses`; ApiKey → `/v1/responses`.

### Header attachment

At `src/lib.rs:495`:

```rust
match codex_cred.as_ref() {
    Some(CodexCredential::OAuth { access_token, account_id }) => {
        upstream_req = codex::apply_codex_headers(upstream_req, access_token, account_id.as_deref());
    }
    Some(CodexCredential::ApiKey { key }) => {
        upstream_req = upstream_req.bearer_auth(key);
    }
    None => {}
}
```

`bearer_auth` sets `Authorization: Bearer <key>` directly in tracked code. The gitignored
`codex_impl.rs` hook (`apply_codex_headers`, the OAuth client headers) is not touched.

### 401 behaviour

`codex_retry_eligible` (currently `src/lib.rs:703`) takes the credential's mode. Only `OAuth`
retries: refresh the access token once and resend. `ApiKey` never retries — a 401 with an API key
means the key itself is rejected; there is no refresh token to redeem, and resending the same key
cannot succeed. The upstream 401 is returned to the caller unchanged.

### Regression surface

The OAuth path must behave byte-for-byte as it does today: same upstream, same stripped `/v1`, same
gitignored headers, same refresh-on-401. The enum is the only structural change; the OAuth arm is a
direct transcription of the current tuple logic.

## Error handling

| Condition | Behaviour |
|---|---|
| `auth_mode: "apikey"` but `OPENAI_API_KEY` missing | `read_credential` → `None` → passthrough, warning logged (matches the existing missing-credential path) |
| `~/.codex/auth.json` missing or unparseable | `None` → passthrough, warning logged (unchanged) |
| API-key 401 | No retry; upstream 401 returned unchanged |
| API-key 403 | No retry; upstream 403 returned unchanged (never a token-expiry case) |
| Upstream non-2xx | Existing re-shaping unchanged (translated path wraps errors in the OpenAI envelope) |

## Testing

1. **Unit** (`src/codex.rs`): `read_credential` dispatches correctly — apikey → `ApiKey`, chatgpt →
   `OAuth`, missing `OPENAI_API_KEY` → `None`, malformed JSON → `None`. The existing
   `rejects_api_key_mode` test becomes `parses_api_key_mode`.
2. **Integration** (`tests/codex_translate.rs`): with `auth_mode = "apikey"` and
   `TokenSource::Codex`, a request to `/v1/responses` reaches a mock at host `api.openai.com` on path
   `/v1/responses` with `Authorization: Bearer <key>` and no ChatGPT client headers.
3. **Integration**: API-key 401 hits the upstream exactly once and the 401 reaches the client — no
   retry.
4. **Integration**: API-key mode on `/codex/v1/chat/completions` still translates — the upstream
   receives a Responses body (`input` present, `messages` absent) at `/v1/responses`, and a
   non-streaming client gets an aggregated `chat.completion`.
5. **Regression**: the existing seven `codex_translate` tests (which use `TokenSource::Static`) and
   the ten `codex.rs` tests stay green, proving the OAuth path is untouched.

## Deliberately out of scope

- `X-Upstream` header override.
- Responses-native compression on the raw `/v1/responses` path.
- API-key detection for EnvVar/Static token sources — auth.json only.
- Multiple keys or key rotation — one key is enough; YAGNI.
- Any API-key-specific streaming work — the existing SSE streaming/aggregation is
  credential-agnostic and applies unchanged.
