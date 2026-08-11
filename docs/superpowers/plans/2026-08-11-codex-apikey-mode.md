# Codex API-Key Mode (Stage 3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the Codex route serve a plain OpenAI API key: when `~/.codex/auth.json` says `auth_mode = "apikey"`, the gateway attaches `Authorization: Bearer <OPENAI_API_KEY>` and sends the request to `https://api.openai.com/v1/responses` — on both the raw `/v1/responses` path and the translated `/codex/v1/chat/completions` path — instead of the ChatGPT OAuth backend.

**Architecture:** A `CodexCredential` enum in `src/codex.rs` (`OAuth { access_token, account_id }` vs `ApiKey { key }`) replaces the `Option<(String, Option<String>)>` tuple that `forward()` threads today. The enum is resolved before the upstream URL is built, because the mode decides four coupled things: the upstream host, the path prefix, the header shape, and the 401 behaviour. API-key mode never retries on 401 (no refresh token to redeem); OAuth keeps its refresh-once-and-retry.

**Tech Stack:** Rust, axum 0.8, reqwest 0.12, serde_json 1, tokio 1, httpmock 0.7, tempfile. No new dependencies.

## Global Constraints

- **The API-key upstream is hard-coded `https://api.openai.com`** — a const `DEFAULT_UPSTREAM_CODEX_APIKEY` beside `DEFAULT_UPSTREAM_CODEX`. It is **not** configurable via `MUR_MODEL_GATEWAY_UPSTREAM_CODEX` or any env var. Tests override it only through a test seam on `AppState` (see the deviation note below).
- **Detection boundary: only `TokenSource::Codex` (reads `~/.codex/auth.json`) dispatches on `auth_mode`.** EnvVar/Static/CredentialsFile/Disabled sources keep today's behaviour — they resolve to the OAuth-shaped credential (test injection points).
- **The OAuth path must remain byte-for-byte identical:** same upstream (`chatgpt.com/backend-api/codex`), same `/v1` strip, same `apply_codex_headers`, same refresh-on-401. The seven `tests/codex_translate.rs` tests and the `src/codex.rs` unit tests stay green.
- **API-key 401 is never retried.** There is no refresh token to redeem; resending the same key cannot succeed. The upstream 401 is returned unchanged.
- **API-key headers are `Authorization: Bearer <key>`, set in tracked code only** (reqwest's `.bearer_auth()`). The gitignored `src/codex/codex_impl.rs` hook is OAuth-only and must not be touched; the load-bearing `#[rustfmt::skip]` on its `mod codex_impl;` declaration stays.
- **API-key mode keeps `/v1` on the upstream path** (`/v1/responses`); OAuth strips it (`/responses`). The translated route is `/v1/responses` in API-key mode, `/responses` in OAuth.
- **`/codex/v1/chat/completions` still translates in API-key mode** — streaming and aggregation are credential-agnostic and apply unchanged.
- **Tests never touch the network** — httpmock stands in for the upstream, exactly as in the existing `tests/codex_translate.rs`.
- **`cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` pass before every commit.**
- **Never print live credential file contents** — test fixtures use obviously-fake values (`sk-fake`, `sk-test-key`).
- **Never edit `tests/fixtures/`.** Never commit `.serena/project.yml`.
- Spec: `docs/superpowers/specs/2026-08-11-codex-apikey-mode-design.md`.

### Deviation from the spec, recorded

1. **`read_auth` is kept alongside `read_credential`.** The spec says "Replace `read_auth` with `read_credential`". `refreshed_access_token` (the OAuth 401 retry) needs the OAuth-only `refresh_token`, which only `read_auth`/`CodexAuth` carries. `read_credential` becomes the mode-dispatched read used by `forward()` and `TokenSource::resolve()`; `read_auth` stays as the OAuth-shaped helper. Same dispatch intent, one retained helper.
2. **`AppState` gains a test-only `upstream_codex_apikey` field.** The spec's testing plan says a request "reaches a mock at host `api.openai.com`", but httpmock binds `127.0.0.1:<random>` and cannot impersonate a real hostname, and the production upstream is hard-coded. The field defaults to `DEFAULT_UPSTREAM_CODEX_APIKEY` and is overridable via `with_upstream_codex_apikey(url)` for tests only — it is a test seam, not user-facing configuration, and no env var is wired to it. A unit test asserts the default equals the const.

---

### Task 1: `CodexCredential` enum and `read_credential` in `src/codex.rs`

**Files:**
- Modify: `src/codex.rs` (add enum + impls after the `CodexAuth` Debug impl, ~line 97; add `read_credential` after `read_auth`, ~line 129; update tests)

**Interfaces:**
- Consumes: nothing — `parse_auth`, `CodexAuth`, `read_auth` stay exactly as they are (OAuth arm).
- Produces:
  - `pub enum CodexCredential { OAuth { access_token: String, account_id: Option<String> }, ApiKey { key: String } }` with a redacting `Debug` impl.
  - `impl CodexCredential { pub fn bearer(&self) -> &str; pub fn account_id(&self) -> Option<&str> }`
  - `pub fn read_credential(path: &Path) -> Option<CodexCredential>`
  - Tests: `parses_api_key_mode` (replaces `rejects_api_key_mode`), `read_credential_dispatches_on_auth_mode`, `codex_credential_debug_redacts_secrets`.

- [ ] **Step 1: Write the failing tests**

Replace the `rejects_api_key_mode` test (currently `src/codex.rs:378-384`) with:

```rust
#[test]
fn parses_api_key_mode() {
    let dir = std::env::temp_dir().join("mmg-codex-apikey-mode");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("auth.json");
    std::fs::write(
        &p,
        r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#,
    )
    .unwrap();
    match read_credential(&p).expect("apikey mode parses") {
        CodexCredential::ApiKey { key } => assert_eq!(key, "sk-fake"),
        other => panic!("expected ApiKey, got {other:?}"),
    }
    // The OAuth-only parser still rejects API-key mode — that is the
    // dispatch boundary read_credential sits on.
    assert!(parse_auth(r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake"}"#).is_none());
    std::fs::remove_file(&p).ok();
}
```

Add a second test after it:

```rust
#[test]
fn read_credential_dispatches_on_auth_mode() {
    let dir = std::env::temp_dir().join("mmg-codex-read-credential");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("auth.json");

    // auth_mode = "chatgpt" → OAuth.
    std::fs::write(
        &p,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"a","refresh_token":"r","account_id":"acct"}}"#,
    )
    .unwrap();
    match read_credential(&p).expect("chatgpt parses") {
        CodexCredential::OAuth { access_token, account_id } => {
            assert_eq!(access_token, "a");
            assert_eq!(account_id.as_deref(), Some("acct"));
        }
        other => panic!("expected OAuth, got {other:?}"),
    }

    // auth_mode = "apikey" but the key is missing or empty → None (caller
    // falls through to passthrough, warning logged upstream).
    std::fs::write(&p, r#"{"auth_mode":"apikey","OPENAI_API_KEY":null}"#).unwrap();
    assert!(read_credential(&p).is_none());
    std::fs::write(&p, r#"{"auth_mode":"apikey","OPENAI_API_KEY":""}"#).unwrap();
    assert!(read_credential(&p).is_none());

    // Unrecognised mode → None.
    std::fs::write(&p, r#"{"auth_mode":"spaceship","OPENAI_API_KEY":"sk-x"}"#).unwrap();
    assert!(read_credential(&p).is_none());

    // Malformed JSON → None.
    std::fs::write(&p, "{not json").unwrap();
    assert!(read_credential(&p).is_none());

    std::fs::remove_file(&p).ok();
}
```

Add a redaction test:

```rust
#[test]
fn codex_credential_debug_redacts_secrets() {
    let oauth = CodexCredential::OAuth {
        access_token: "oa-tok".into(),
        account_id: Some("acct".into()),
    };
    let apikey = CodexCredential::ApiKey { key: "sk-fake".into() };

    let od = format!("{oauth:?}");
    assert!(!od.contains("oa-tok"), "OAuth access token must be redacted");
    assert!(od.contains("acct"), "account_id is not a secret");

    let ad = format!("{apikey:?}");
    assert!(!ad.contains("sk-fake"), "API key must be redacted");
    assert!(ad.contains("<redacted>"));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib codex`
Expected: FAIL — `read_credential` and `CodexCredential` are undefined (compile error).

- [ ] **Step 3: Implement the enum and `read_credential`**

Add after the `CodexAuth` Debug impl (`src/codex.rs:97`), before `default_auth_path`:

```rust
/// Credentials dispatched on `auth_mode` from `~/.codex/auth.json`.
///
/// `auth_mode = "chatgpt"` is an OAuth token pair plus an account id, sent
/// to ChatGPT's Codex backend. `auth_mode = "apikey"` is a plain OpenAI API
/// key, sent to `api.openai.com`. The two differ in upstream, path prefix,
/// header shape, and 401 behaviour — which is exactly why they are a tagged
/// union and not a tuple: a `CodexCredential` forces `forward()` to handle
/// each mode's coupled choices explicitly.
#[derive(Clone)]
pub enum CodexCredential {
    /// ChatGPT subscription OAuth.
    OAuth { access_token: String, account_id: Option<String> },
    /// Pay-per-use OpenAI API key.
    ApiKey { key: String },
}

/// Redacting Debug: a derived impl would dump the token/key into any `{:?}`
/// capture, exactly what `CodexAuth`'s manual impl exists to prevent.
impl std::fmt::Debug for CodexCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexCredential::OAuth { access_token, account_id } => f
                .debug_struct("CodexCredential::OAuth")
                .field("access_token", &"<redacted>")
                .field("account_id", &account_id)
                .finish(),
            CodexCredential::ApiKey { .. } => f
                .debug_struct("CodexCredential::ApiKey")
                .field("key", &"<redacted>")
                .finish(),
        }
    }
}

impl CodexCredential {
    /// The secret to send upstream: the OAuth access token or the API key.
    /// Used by `TokenSource::resolve()`.
    pub fn bearer(&self) -> &str {
        match self {
            CodexCredential::OAuth { access_token, .. } => access_token,
            CodexCredential::ApiKey { key } => key,
        }
    }

    /// `Some(account_id)` for OAuth, `None` for API-key mode (which sends no
    /// ChatGPT client headers).
    pub fn account_id(&self) -> Option<&str> {
        match self {
            CodexCredential::OAuth { account_id, .. } => account_id.as_deref(),
            CodexCredential::ApiKey { .. } => None,
        }
    }
}
```

Add `read_credential` after `read_auth` (`src/codex.rs:129`):

```rust
/// Read and parse the auth file into a [`CodexCredential`], dispatching on
/// `auth_mode`. `None` if absent, malformed, missing its key/token, or an
/// unrecognised mode — the caller falls through to passthrough, warning
/// logged. `parse_auth` stays the OAuth arm; API-key mode is handled here.
pub fn read_credential(path: &Path) -> Option<CodexCredential> {
    let raw = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    match v.get("auth_mode").and_then(|m| m.as_str()) {
        Some("chatgpt") => parse_auth(&raw).map(|a| CodexCredential::OAuth {
            access_token: a.access_token,
            account_id: a.account_id,
        }),
        Some("apikey") => v
            .get("OPENAI_API_KEY")
            .and_then(|k| k.as_str())
            .filter(|k| !k.is_empty())
            .map(|k| CodexCredential::ApiKey { key: k.to_string() }),
        _ => None,
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib codex`
Expected: PASS — all `src/codex.rs` tests green, including the two retained OAuth tests (`parses_chatgpt_mode_auth`, `persist_rotation_*`). The rest of the crate still compiles because nothing consumes `CodexCredential` yet.

- [ ] **Step 5: Format, lint, full test, commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/codex.rs
git commit -m "feat(codex): CodexCredential enum and read_credential dispatch"
```

---

### Task 2: Thread `CodexCredential` through `forward()` and activate API-key mode

**Files:**
- Modify: `src/lib.rs`
  - Const: after `DEFAULT_UPSTREAM_CODEX` (line 38): `pub const DEFAULT_UPSTREAM_CODEX_APIKEY: &str = "https://api.openai.com";`
  - `TokenSource::resolve()` Codex arm (line 163).
  - `AppState`: add `upstream_codex_apikey: String` field; wire its default in `with_version` (lines 220-235); add `with_upstream_codex_apikey` builder after `with_upstream_codex` (line 257).
  - `forward()`: move the Codex credential resolution above the `target_url` build (currently lines 311-315); change its type to `Option<codex::CodexCredential>`; make `target_path`/upstream mode-aware; replace the header attach (lines 495-497); update the 401 retry gate (line 512) and account-id extraction (line 521).
  - `codex_retry_eligible` (lines 703-709): take `Option<&codex::CodexCredential>` instead of `codex_cred_present: bool`.
- Test: `src/lib.rs` unit tests (`codex_token_source_resolves_access_token` at 822, `codex_retry_requires_gateway_supplied_credential` at 847, new default/override test).

**Interfaces:**
- Consumes: `codex::CodexCredential`, `codex::read_credential`, `CodexCredential::bearer()`, `CodexCredential::account_id()` from Task 1.
- Produces:
  - `pub const DEFAULT_UPSTREAM_CODEX_APIKEY: &str = "https://api.openai.com"`
  - `AppState::with_upstream_codex_apikey(self, url: impl Into<String>) -> Self`
  - `fn codex_retry_eligible(provider: Provider, status: reqwest::StatusCode, codex_cred: Option<&codex::CodexCredential>) -> bool`
  - Mode-aware upstream/path selection inside `forward()`.

- [ ] **Step 1: Add the constant, the `AppState` field, and the builder**

Add the constant after `DEFAULT_UPSTREAM_CODEX` (`src/lib.rs:38`):

```rust
/// API-key mode targets the public Responses API. Deliberately the same
/// value as `DEFAULT_UPSTREAM_OPENAI` but named for its role — Codex in
/// API-key mode is a different credential mode, not the OpenAI route. Not
/// env-configurable (spec: stage 3).
pub const DEFAULT_UPSTREAM_CODEX_APIKEY: &str = "https://api.openai.com";
```

Add the field to the `AppState` struct, after `upstream_codex` (line 176):

```rust
    /// Upstream for Codex in API-key mode. Defaults to
    /// `DEFAULT_UPSTREAM_CODEX_APIKEY`; a test-only seam (`with_upstream_codex_apikey`)
    /// lets integration tests point it at httpmock. Deliberately NOT wired to
    /// any env var — the production host is hard-coded by design.
    pub upstream_codex_apikey: String,
```

Wire the default in `with_version`'s struct literal (after `upstream_codex`, line 224):

```rust
            upstream_codex_apikey: DEFAULT_UPSTREAM_CODEX_APIKEY.to_string(),
```

Add the builder after `with_upstream_codex` (after line 257):

```rust
    /// Override the API-key-mode upstream. Test-only seam: the production
    /// default is the hard-coded `DEFAULT_UPSTREAM_CODEX_APIKEY`.
    pub fn with_upstream_codex_apikey(mut self, url: impl Into<String>) -> Self {
        self.upstream_codex_apikey = url.into();
        self
    }
```

Add a unit test asserting the default and the override:

```rust
#[test]
fn apikey_upstream_defaults_to_const_and_is_overridable() {
    let s = AppState::new("a", "o", "g", TokenSource::Disabled).unwrap();
    assert_eq!(s.upstream_codex_apikey, DEFAULT_UPSTREAM_CODEX_APIKEY);
    let s2 = s.with_upstream_codex_apikey("http://127.0.0.1:9");
    assert_eq!(s2.upstream_codex_apikey, "http://127.0.0.1:9");
}
```

Run: `cargo test --lib` → PASS.

- [ ] **Step 2: Point `TokenSource::resolve()` at the mode-dispatched read**

Replace the Codex arm (`src/lib.rs:163`):

```rust
            TokenSource::Codex(path) => Ok(codex::read_credential(path).map(|c| c.bearer().to_string())),
```

Extend the existing `codex_token_source_resolves_access_token` test (`src/lib.rs:822`) so it also covers API-key mode. After the existing chatgpt assertion, add:

```rust
        // API-key mode resolves to the key, not an OAuth access token.
        std::fs::write(
            &p,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#,
        )
        .unwrap();
        assert_eq!(ts.resolve().unwrap().as_deref(), Some("sk-fake"));
```

Run: `cargo test --lib` → PASS. (`ts` holds the path; `resolve()` re-reads the file on every call.)

- [ ] **Step 3: Refactor `forward()` and `codex_retry_eligible`**

**(a) Move the Codex credential resolution above the URL build.** Delete the block at `src/lib.rs:384-401` and re-insert it right after `let provider = detect_provider(path_only);` (line 310), typed as the enum:

```rust
    let translating = codex::should_translate(path_only);

    // Codex: callers with no real credential of their own get the stored
    // Codex credential plus the client headers; callers that already carry
    // one pass through untouched. Same INTENT as the Anthropic path's mode
    // 3 (don't second-guess a client that authenticates itself) but not the
    // same rule: unlike Anthropic's plain `contains_key` check,
    // `has_client_credential` treats a present-but-empty auth header as "no
    // credential" — see that function's doc for why.
    //
    // Resolved before the upstream URL is built: the credential's mode
    // decides the upstream host, path prefix, and headers.
    let codex_cred: Option<codex::CodexCredential> =
        if provider == Provider::Codex && !has_client_credential(&parts.headers) {
            match state.token_source_for(Provider::Codex) {
                TokenSource::Codex(path) => codex::read_credential(path),
                other => match other.resolve() {
                    Ok(Some(tok)) => Some(codex::CodexCredential::OAuth {
                        access_token: tok,
                        account_id: None,
                    }),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, "codex token source failed, passing through");
                        None
                    }
                },
            }
        } else {
            None
        };
```

Note `let translating = ...` now lives here; **delete** the duplicate `let translating = codex::should_translate(path_only);` that is currently at `src/lib.rs:344`. The `(client_model, client_wants_stream)` block and the translation `if translating` block below it keep working against the earlier binding.

**(b) Make `target_path` and the upstream mode-aware.** Replace `src/lib.rs:311-315` — keep the `let _: Uri = target_url.parse().context("target uri parse")?;` validation line (316) in place:

```rust
    let codex_apikey = matches!(codex_cred, Some(codex::CodexCredential::ApiKey { .. }));
    let (target_path, upstream) = match provider {
        // API-key mode keeps /v1 on the public Responses endpoint.
        Provider::Codex if codex_apikey && translating => {
            ("/v1/responses".to_string(), state.upstream_codex_apikey.as_str())
        }
        Provider::Codex if codex_apikey => {
            (path_and_query.to_string(), state.upstream_codex_apikey.as_str())
        }
        // OAuth (and passthrough) keep the stage-1/2 behaviour: strip /v1,
        // chatgpt.com upstream.
        Provider::Codex => (codex_target_path(path_and_query), state.upstream_for(provider)),
        _ => (path_and_query.to_string(), state.upstream_for(provider)),
    };
    let target_url = format!("{upstream}{target_path}");
```

**(c) Replace the header attach.** Replace `src/lib.rs:495-497`:

```rust
    match codex_cred.as_ref() {
        Some(codex::CodexCredential::OAuth { access_token, account_id }) => {
            upstream_req = codex::apply_codex_headers(
                upstream_req,
                access_token,
                account_id.as_deref(),
            );
        }
        Some(codex::CodexCredential::ApiKey { key }) => {
            upstream_req = upstream_req.bearer_auth(key);
        }
        None => {}
    }
```

**(d) Update the 401 retry.** Change the gate (`src/lib.rs:512`) from `codex_cred.is_some()` to `codex_cred.as_ref()`, and the account-id extraction (`src/lib.rs:521`) to the enum accessor:

```rust
    if codex_retry_eligible(provider, upstream_resp.status(), codex_cred.as_ref())
        && let TokenSource::Codex(path) = state.token_source_for(Provider::Codex)
        && let Some(fresh) = codex::refreshed_access_token(path).await
    {
        let account_id = codex_cred
            .as_ref()
            .and_then(codex::CodexCredential::account_id)
            .map(str::to_string);
```

The rest of the retry body (`apply_codex_headers(retry, &fresh, account_id.as_deref())`, `body_bytes.clone()`) is unchanged — it can only be reached for OAuth now.

**(e) Make `codex_retry_eligible` mode-aware.** Replace `src/lib.rs:703-709`:

```rust
fn codex_retry_eligible(
    provider: Provider,
    status: reqwest::StatusCode,
    codex_cred: Option<&codex::CodexCredential>,
) -> bool {
    match codex_cred {
        Some(codex::CodexCredential::OAuth { .. }) => {
            provider == Provider::Codex && status == reqwest::StatusCode::UNAUTHORIZED
        }
        // API-key 401 means the key itself is rejected: there is no refresh
        // token to redeem, and resending the same key cannot succeed, so the
        // upstream 401 is returned unchanged. `None` (no gateway-supplied
        // credential) is likewise never eligible, as before.
        Some(codex::CodexCredential::ApiKey { .. }) | None => false,
    }
}
```

Update its unit test (`src/lib.rs:847`):

```rust
#[test]
fn codex_retry_requires_gateway_supplied_credential() {
    let oauth = codex::CodexCredential::OAuth {
        access_token: "t".into(),
        account_id: None,
    };
    let apikey = codex::CodexCredential::ApiKey { key: "sk".into() };
    assert!(!codex_retry_eligible(
        Provider::Codex,
        reqwest::StatusCode::UNAUTHORIZED,
        None
    ));
    assert!(codex_retry_eligible(
        Provider::Codex,
        reqwest::StatusCode::UNAUTHORIZED,
        Some(&oauth)
    ));
    // API-key 401 is a rejected key — no refresh token to redeem, never retried.
    assert!(!codex_retry_eligible(
        Provider::Codex,
        reqwest::StatusCode::UNAUTHORIZED,
        Some(&apikey)
    ));
    // Sanity: the other conjuncts still matter on their own.
    assert!(!codex_retry_eligible(
        Provider::Anthropic,
        reqwest::StatusCode::UNAUTHORIZED,
        Some(&oauth)
    ));
    assert!(!codex_retry_eligible(
        Provider::Codex,
        reqwest::StatusCode::OK,
        Some(&oauth)
    ));
}
```

- [ ] **Step 4: Run the full suite — OAuth must be byte-identical**

Run: `cargo test`
Expected: PASS — all 7 `tests/codex_translate.rs` tests (Static source → OAuth-shaped credential → still hits `/responses` on the mock), all `src/codex.rs` and `src/lib.rs` unit tests, and every other suite. If any Codex test fails, the OAuth path regressed — stop and fix before proceeding.

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add src/lib.rs
git commit -m "feat(codex): thread CodexCredential through forward, activate api-key mode"
```

---

### Task 3: Integration tests for API-key mode

**Files:**
- Modify: `tests/codex_translate.rs`

**Interfaces:**
- Consumes: `AppState::with_upstream_codex_apikey`, `TokenSource::Codex(path)` from Task 2; `CodexCredential::ApiKey` behavior from Task 1.
- Produces: `spawn_apikey_gateway(upstream: &str, auth_json: &str) -> (String, tempfile::TempDir)` and three httpmock tests.

- [ ] **Step 1: Add the apikey gateway helper**

Add after `spawn_gateway` (line 23). `tempfile` is already a dev-dependency (used at line 336):

```rust
/// Start a gateway whose Codex route runs in API-key mode: the credential
/// comes from a temp `auth.json` whose `auth_mode` is "apikey", and the
/// API-key upstream is pointed at the mock (the production default is the
/// hard-coded api.openai.com — see the plan's deviation note). Returns the
/// address and the TempDir; the TempDir must stay alive for the test body,
/// because the gateway re-reads auth.json on every request.
async fn spawn_apikey_gateway(upstream: &str, auth_json: &str) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("auth.json"), auth_json).unwrap();
    let path = dir.path().join("auth.json");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut state = AppState::new(upstream, upstream, upstream, TokenSource::Disabled)
        .unwrap()
        .with_upstream_codex_apikey(upstream)
        .with_token_source_codex(TokenSource::Codex(path));
    state.compress = false;
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr.to_string(), dir)
}
```

Add the fixture blob as a const at the top of the file, after `SSE_REPLY` (line 91):

```rust
const APICKEY_AUTH: &str = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test-key","tokens":null}"#;
```

- [ ] **Step 2: Raw `/v1/responses` reaches `/v1/responses` with `Bearer <key>` and no ChatGPT client headers**

First add an absence matcher after `is_untranslated_chat_body` (line 60):

```rust
/// True only if the request carries none of the ChatGPT client headers the
/// OAuth `apply_codex_headers` hook would set. API-key mode sends the key
/// and nothing else.
///
/// httpmock 0.7 exposes headers as `Option<Vec<(String, String)>>` — a list,
/// not a map — so this scans it. Absent headers (`None`) trivially satisfy
/// the absence check.
fn has_no_chatgpt_account_id(req: &HttpMockRequest) -> bool {
    req.headers.as_ref().is_none_or(|hs| {
        !hs.iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
    })
}
```

Then the test:

```rust
#[tokio::test]
async fn apikey_mode_keeps_v1_and_sends_bearer_key() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/responses")
                .header("authorization", "Bearer sk-test-key")
                .matches(has_no_chatgpt_account_id);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"ok":true}"#);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    let resp = post(&gw, "/v1/responses", json!({"model": "m", "input": "hi"})).await;

    assert_eq!(resp.status(), 200);
    // The mock matches ONLY `/v1/responses` carrying `Bearer sk-test-key`
    // and no `chatgpt-account-id` header. OAuth mode would hit `/responses`,
    // never this key, and would attach the account-id header — so a hit
    // proves all three API-key facts at once.
    m.assert_async().await;
}
```

Run: `cargo test --test codex_translate apikey_mode_keeps_v1_and_sends_bearer_key`
Expected: PASS.

- [ ] **Step 3: API-key 401 is never retried**

```rust
#[tokio::test]
async fn apikey_401_is_returned_unchanged_with_no_retry() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/v1/responses");
            then.status(401).body(r#"{"error":"invalid api key"}"#);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    let resp = post(&gw, "/v1/responses", json!({"model": "m", "input": "hi"})).await;

    assert_eq!(resp.status(), 401);
    // Exactly one upstream hit: API-key mode has no refresh token to redeem,
    // so a 401 must not trigger the OAuth refresh-and-retry path.
    m.assert_hits_async(1).await;
}
```

Run: `cargo test --test codex_translate apikey_401_is_returned_unchanged_with_no_retry`
Expected: PASS.

- [ ] **Step 4: The translated Chat Completions path works in API-key mode**

```rust
#[tokio::test]
async fn apikey_mode_translates_chat_completions_to_v1_responses() {
    let upstream = MockServer::start_async().await;
    let m = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/v1/responses")
                .matches(is_translated_responses_body);
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(SSE_REPLY);
        })
        .await;

    let (gw, _dir) = spawn_apikey_gateway(&upstream.base_url(), APICKEY_AUTH).await;
    // stream is absent, so the client wants a single JSON reply — the gateway
    // translates, asks the upstream to stream, and aggregates.
    let resp = post(
        &gw,
        "/codex/v1/chat/completions",
        json!({"model": "gpt-5.4", "messages": [{"role": "user", "content": "hi"}]}),
    )
    .await;

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()["content-type"],
        "application/json",
        "the upstream's text/event-stream must not leak to the client"
    );
    let out: Value = resp.json().await.unwrap();
    assert_eq!(out["object"], json!("chat.completion"));
    assert_eq!(out["choices"][0]["message"]["content"], json!("hi back"));
    // The upstream saw a translated Responses body at /v1/responses — the
    // OAuth translate route targets /responses instead, so this proves the
    // mode-aware path.
    m.assert_async().await;
}
```

Run: `cargo test --test codex_translate`
Expected: PASS — the three new tests plus the seven existing ones.

- [ ] **Step 5: Full gate and commit**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
git add tests/codex_translate.rs
git commit -m "test(codex): integration tests for api-key mode"
```

---

### Task 4: Full gate and clean-checkout verification

**Files:** none — verification only.

- [ ] **Step 1: Full gate**

Run:
```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
Expected: all green.

- [ ] **Step 2: Clean-checkout build with the stubs**

Prove the public build still compiles with the gitignored `codex_impl.rs` absent, and that fmt does not trip over the load-bearing `#[rustfmt::skip]`:

```bash
rm -rf /tmp/mmg-clean && mkdir -p /tmp/mmg-clean
git archive HEAD | tar -x -C /tmp/mmg-clean
cd /tmp/mmg-clean && cargo build --all-targets && cargo fmt --check
```

Expected: compiles and passes fmt.

- [ ] **Step 3: Report**

No commit — this task is verification. Report the full gate and clean-checkout results to the human partner.
