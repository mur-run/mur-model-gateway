# Codex OAuth Token Support (Stage 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An auth-less caller hitting `/v1/responses` on the gateway reaches ChatGPT's Codex backend with the user's Codex OAuth credentials attached, refreshed automatically when expired.

**Architecture:** A fourth `Provider::Codex` routed by path, with its own upstream and its own token source. Codex client headers and OAuth constants live in a gitignored module activated by `build.rs`, mirroring `src/disguise.rs`. Expired access tokens are refreshed in memory on a 401 and retried exactly once; `~/.codex/auth.json` is never written.

**Tech Stack:** Rust edition 2024, axum + reqwest, httpmock for integration tests, `directories` for home resolution.

**Spec:** `docs/superpowers/specs/2026-08-11-codex-oauth-design.md`

## Global Constraints

- Rust edition 2024, toolchain ≥ 1.85.
- CI gate on ubuntu-latest, macos-14, windows-latest: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`. All three must pass before any task is done.
- **Every `#[cfg(...)]`-gated `mod x;` declaration MUST carry `#[rustfmt::skip]`.** rustfmt resolves `mod` declarations syntactically and ignores `cfg`, so without it `cargo fmt --check` fails on any clean checkout. This broke CI for a month; fixed in `53c3b08`.
- Codex OAuth client id, token endpoint, and client header values NEVER appear in tracked source, in commit messages, or in test fixtures. They live only in gitignored `src/codex/codex_impl.rs`.
- **Writes to `~/.codex/auth.json` go through a temp file in the same directory plus `rename(2)`.** Never a partial in-place write — Codex CLI reads that file concurrently. Preserve all original JSON fields; replace only `tokens.access_token`, `tokens.refresh_token`, `last_refresh`.
- Compression stays off for `Provider::Codex` in this stage.
- Test fixtures use obviously fake tokens (`fake-access-token`, `fake-refresh-token`).

## Deviation from the spec

The spec has `TokenSource::resolve()` return `Credential { token, account_id }`. This plan keeps
`resolve()` returning `Option<String>` and reads `account_id` separately through
`codex::read_auth()`, which is cached anyway. Reason: changing the return type churns every existing
call site and test for one provider's benefit, and `account_id` is meaningless for the other three.
Same outcome, smaller diff. If review prefers the spec's shape, it is a contained change in Task 5.

---

### Task 1: Hidden module scaffolding

Sets up the cfg-gated module so Task 2 has somewhere to put the real constants. Ships stubs only — no Codex knowledge yet.

**Files:**
- Create: `src/codex.rs`
- Modify: `build.rs`, `.gitignore`, `src/lib.rs` (add `pub mod codex;` beside `pub mod disguise;` at line 13)

**Interfaces:**
- Consumes: nothing
- Produces: `codex::should_route(path: &str) -> bool`; `codex::apply_codex_headers(req: reqwest::RequestBuilder, token: &str, account_id: Option<&str>) -> reqwest::RequestBuilder`; `codex::refresh_access_token(refresh_token: &str) -> anyhow::Result<String>`

- [ ] **Step 1: Write the failing test**

In `src/codex.rs`, at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_responses_paths_only() {
        assert!(should_route("/v1/responses"));
        assert!(should_route("/v1/responses/abc"));
        assert!(should_route("/v1/responses?stream=true"));
        assert!(!should_route("/v1/messages"));
        assert!(!should_route("/v1/chat/completions"));
        assert!(!should_route("/v1/responsesX"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib codex::tests::routes_responses_paths_only`
Expected: FAIL — `src/codex.rs` does not exist / `should_route` not found.

- [ ] **Step 3: Write minimal implementation**

`src/codex.rs`, above the test module:

```rust
//! Codex route: forwards `/v1/responses*` to ChatGPT's Codex backend with
//! Codex credentials attached.
//!
//! In public builds only `should_route()` is active — the header and OAuth
//! details are no-op stubs. The real implementation lives in a gitignored
//! file activated via build.rs (`cfg(has_codex_hook)`).

/// True if `path` is a Codex Responses endpoint we route to ChatGPT.
pub fn should_route(path: &str) -> bool {
    path == "/v1/responses"
        || path.starts_with("/v1/responses/")
        || path.starts_with("/v1/responses?")
}

// ── cfg-gated: real impl or stub ──

// The #[rustfmt::skip] is load-bearing: rustfmt resolves `mod` declarations
// syntactically and ignores cfg, so a clean checkout without the gitignored
// file fails `cargo fmt --check` without it. Same fix as src/disguise.rs.
#[rustfmt::skip]
#[cfg(has_codex_hook)]
mod codex_impl;

#[cfg(not(has_codex_hook))]
mod codex_impl {
    /// Stub: forwards without Codex client headers.
    pub fn apply_codex_headers(
        req: reqwest::RequestBuilder,
        _token: &str,
        _account_id: Option<&str>,
    ) -> reqwest::RequestBuilder {
        req
    }

    /// Stub: no OAuth constants in the public build.
    pub fn refresh_access_token(_refresh_token: &str) -> anyhow::Result<String> {
        anyhow::bail!("codex refresh unavailable in this build")
    }
}

pub use codex_impl::*;
```

In `src/lib.rs`, beside `pub mod disguise;` (line 13):

```rust
pub mod codex;
```

- [ ] **Step 4: Extend build.rs**

Replace the body of `fn main()` in `build.rs`:

```rust
fn main() {
    println!("cargo::rustc-check-cfg=cfg(has_beta_hook)");
    println!("cargo::rustc-check-cfg=cfg(has_codex_hook)");
    // Re-run when a gitignored impl appears/disappears, else Cargo's cached
    // "file absent" result sticks and a freshly-restored impl stays stubbed.
    println!("cargo::rerun-if-changed=src/disguise/disguise_impl.rs");
    println!("cargo::rerun-if-changed=src/codex/codex_impl.rs");
    if std::path::Path::new("src/disguise/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
    if std::path::Path::new("src/codex/codex_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_codex_hook");
    }
}
```

Append to `.gitignore`:

```
src/codex/codex_impl.rs
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib codex::`
Expected: PASS.

- [ ] **Step 6: Prove the public build still works**

This is the step that catches the rustfmt trap. Run:

```bash
R=$(mktemp -d) && git archive HEAD | tar -x -C "$R" && cd "$R" \
  && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: all pass. A `failed to resolve mod codex_impl` error means `#[rustfmt::skip]` is missing.

- [ ] **Step 7: Commit**

```bash
git add src/codex.rs src/lib.rs build.rs .gitignore
git commit -m "feat(codex): add cfg-gated codex module and route predicate"
```

---

### Task 2: Extract and verify the Codex OAuth constants

**The riskiest task, deliberately second.** If the constants can't be recovered, stage 1 cannot work, and every later task would be built on mocks that only confirm our own assumptions. Nothing here is committed — the output file is gitignored.

**Files:**
- Create: `src/codex/codex_impl.rs` (gitignored — never committed)

**Interfaces:**
- Consumes: the stub signatures from Task 1
- Produces: real `apply_codex_headers` and `refresh_access_token` with the same signatures

- [ ] **Step 1: Locate the CLI bundle**

```bash
readlink -f "$(command -v codex)"
ls "$(dirname "$(readlink -f "$(command -v codex)")")/.."
```

Codex CLI 0.139.0 is a Node bundle installed at `~/.npm-global/bin/codex`.

- [ ] **Step 2: Recover the constants**

```bash
BUNDLE=$(readlink -f "$(command -v codex)")
grep -oE 'https://auth[a-z0-9.]*openai\.com[a-zA-Z0-9/_.-]*' "$BUNDLE" | sort -u
grep -oE 'client_id["'\'':= ]+[A-Za-z0-9_-]{16,}' "$BUNDLE" | sort -u
grep -oiE '(chatgpt-account-id|originator|session_id|openai-beta)' "$BUNDLE" | sort -u
grep -oE 'backend-api/codex[a-zA-Z0-9/_-]*' "$BUNDLE" | sort -u
```

If the bundle is minified, widen with `node -e` to print the module, or read `~/.codex/log/` for a request trace. **Do not paste recovered values into chat or commit messages.**

- [ ] **Step 3: Write the real impl**

Create `src/codex/codex_impl.rs` with the recovered values. Structure (values redacted here on purpose — fill from Step 2):

```rust
//! Real Codex implementation — gitignored, activated by build.rs when this
//! file exists (`cfg(has_codex_hook)`).

const CLIENT_ID: &str = "<recovered in Step 2>";
const TOKEN_ENDPOINT: &str = "<recovered in Step 2>";

/// Attach the headers Codex CLI sends, so the backend accepts the request.
pub fn apply_codex_headers(
    req: reqwest::RequestBuilder,
    token: &str,
    account_id: Option<&str>,
) -> reqwest::RequestBuilder {
    let req = req.bearer_auth(token);
    // ... remaining headers recovered in Step 2
    match account_id {
        Some(id) => req.header("chatgpt-account-id", id),
        None => req,
    }
}

/// Exchange a refresh token for a fresh access token. Blocking on purpose:
/// called from the 401 retry path, which is already awaiting a round trip.
pub fn refresh_access_token(refresh_token: &str) -> anyhow::Result<String> {
    // Overridable so integration tests can point at httpmock without the real
    // endpoint appearing in tracked source.
    let endpoint = std::env::var("MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT")
        .unwrap_or_else(|_| TOKEN_ENDPOINT.to_string());
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh_token,
    });
    let resp: serde_json::Value = reqwest::blocking::Client::new()
        .post(&endpoint)
        .json(&body)
        .send()?
        .error_for_status()?
        .json()?;
    resp.get("access_token")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("no access_token in refresh response"))
}
```

The public stub in `src/codex.rs` must gain the same env override, or `tests/codex.rs` cannot run in a build without the hidden file. Since the stub has no `CLIENT_ID`, it stays as `anyhow::bail!` — which is why `tests/codex.rs` carries `#![cfg(has_codex_hook)]`.

- [ ] **Step 4: Verify the refresh grant against the live endpoint**

This is the gate. Extract the refresh token to a shell variable (never a file, never chat):

```bash
RT=$(python3 -c "import json;print(json.load(open('$HOME/.codex/auth.json'))['tokens']['refresh_token'])")
curl -s -X POST "<TOKEN_ENDPOINT>" -H 'content-type: application/json' \
  -d "{\"client_id\":\"<CLIENT_ID>\",\"grant_type\":\"refresh_token\",\"refresh_token\":\"$RT\"}" \
  | python3 -c "import json,sys;d=json.load(sys.stdin);print('got access_token:',bool(d.get('access_token')))"
```

Expected: `got access_token: True`.

If it prints False or errors, **stop and report** rather than proceeding. Likely causes: wrong client_id, the login has expired entirely (needs `codex login`), or the grant requires PKCE. The remaining tasks are worthless if this gate fails.

- [ ] **Step 5: Confirm the cfg flips**

```bash
cargo build 2>&1 | tail -3
cargo test --lib codex::
```

Expected: builds with `has_codex_hook` active, tests pass. Nothing to commit — the file is gitignored. Confirm with `git status --short` that `src/codex/codex_impl.rs` does not appear.

---

### Task 3: Route `/v1/responses` to the Codex upstream

**Files:**
- Modify: `src/lib.rs` — `Provider` enum (line 41), `detect_provider` (line 50), upstream constants (lines 30-32), `AppState` (line 141), `upstream_for` (line 191), `target_url` construction (line 226)

**Interfaces:**
- Consumes: `codex::should_route` from Task 1
- Produces: `Provider::Codex`; `AppState.upstream_codex: String`; `AppState::with_upstream_codex(self, url: impl Into<String>) -> Self`; `codex_target_path(path_and_query: &str) -> String`

- [ ] **Step 1: Write the failing tests**

Add to the existing `mod tests` in `src/lib.rs`:

```rust
#[test]
fn detects_codex_provider() {
    assert_eq!(detect_provider("/v1/responses"), Provider::Codex);
    assert_eq!(detect_provider("/v1/responses?stream=true"), Provider::Codex);
    assert_eq!(detect_provider("/v1/messages"), Provider::Anthropic);
    // Unrecognised paths still fall back to Anthropic.
    assert_eq!(detect_provider("/v1/responsesX"), Provider::Anthropic);
}

#[test]
fn codex_target_path_strips_v1() {
    assert_eq!(codex_target_path("/v1/responses"), "/responses");
    assert_eq!(
        codex_target_path("/v1/responses?stream=true"),
        "/responses?stream=true"
    );
    // Defensive: a path that somehow lacks the prefix is passed through.
    assert_eq!(codex_target_path("/responses"), "/responses");
}

#[test]
fn upstream_for_resolves_codex() {
    let s = AppState::new("https://a.test", "https://o.test", "https://g.test", TokenSource::Disabled)
        .unwrap();
    assert_eq!(s.upstream_for(Provider::Codex), DEFAULT_UPSTREAM_CODEX);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib detects_codex_provider codex_target_path_strips_v1 upstream_for_resolves_codex`
Expected: FAIL — no `Provider::Codex` variant.

- [ ] **Step 3: Implement**

Add beside the other upstream constants (`src/lib.rs:30-32`):

```rust
pub const DEFAULT_UPSTREAM_CODEX: &str = "https://chatgpt.com/backend-api/codex";
```

Add the variant to `Provider` (line 41):

```rust
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
    Codex,
}
```

In `detect_provider`, before the Anthropic fallback:

```rust
    if codex::should_route(path) {
        return Provider::Codex;
    }
```

Add the field to `AppState` (after `upstream_gemini`, line 144):

```rust
    pub upstream_codex: String,
```

`AppState::new` keeps its three-upstream signature — existing callers and tests are untouched. Inside `new`, initialise the new field and add a builder for tests:

```rust
            upstream_codex: DEFAULT_UPSTREAM_CODEX.to_string(),
```

```rust
    /// Override the Codex upstream. Used by tests to point at httpmock.
    pub fn with_upstream_codex(mut self, url: impl Into<String>) -> Self {
        self.upstream_codex = url.into();
        self
    }
```

Extend `upstream_for` (line 191):

```rust
            Provider::Codex => &self.upstream_codex,
```

Add the path rewrite near `detect_provider`:

```rust
/// ChatGPT's Codex backend is rooted at `<base>/responses`, but callers use the
/// OpenAI-style `/v1/responses`. Strip the `/v1` so the two line up. Every other
/// provider concatenates the incoming path unchanged.
pub fn codex_target_path(path_and_query: &str) -> String {
    match path_and_query.strip_prefix("/v1") {
        Some(rest) => rest.to_string(),
        None => path_and_query.to_string(),
    }
}
```

Use it at the `target_url` construction (line 226):

```rust
    let target_path = match provider {
        Provider::Codex => codex_target_path(path_and_query),
        _ => path_and_query.to_string(),
    };
    let target_url = format!("{}{}", state.upstream_for(provider), target_path);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including the pre-existing `upstream_for_resolves_correctly` and the `detect_provider` fallback assertions at lines 497-509.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "feat(codex): route /v1/responses to the ChatGPT Codex upstream"
```

---

### Task 4: Read and cache `~/.codex/auth.json`

**Files:**
- Modify: `src/codex.rs`
- Reuse: the TTL cache helper in `src/keychain.rs` (around line 60) and `keychain::CACHE_TTL`

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces: `pub struct CodexAuth { pub access_token: String, pub refresh_token: Option<String>, pub account_id: Option<String> }`; `codex::default_auth_path() -> Option<PathBuf>`; `codex::parse_auth(raw: &str) -> Option<CodexAuth>`; `codex::read_auth(path: &Path) -> Option<CodexAuth>`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/codex.rs`:

```rust
    #[test]
    fn parses_chatgpt_mode_auth() {
        let raw = r#"{
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "fake-id-token",
                "access_token": "fake-access-token",
                "refresh_token": "fake-refresh-token",
                "account_id": "acct-fake"
            },
            "last_refresh": "2026-07-10T00:20:57.310171Z"
        }"#;
        let a = parse_auth(raw).expect("should parse");
        assert_eq!(a.access_token, "fake-access-token");
        assert_eq!(a.refresh_token.as_deref(), Some("fake-refresh-token"));
        assert_eq!(a.account_id.as_deref(), Some("acct-fake"));
    }

    #[test]
    fn rejects_api_key_mode() {
        // Stage 1 handles OAuth only; API-key mode resolves to None so the
        // caller falls through to passthrough rather than sending a bad token.
        let raw = r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#;
        assert!(parse_auth(raw).is_none());
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_auth("{not json").is_none());
        assert!(parse_auth("{}").is_none());
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib codex::tests`
Expected: FAIL — `parse_auth` not found.

- [ ] **Step 3: Implement**

Add to `src/codex.rs`, above the test module:

```rust
use std::path::{Path, PathBuf};

/// Credentials as Codex CLI stores them in `~/.codex/auth.json`.
#[derive(Clone, Debug)]
pub struct CodexAuth {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub account_id: Option<String>,
}

/// `~/.codex/auth.json`.
pub fn default_auth_path() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".codex/auth.json"))
}

/// Parse the auth blob. `None` for malformed JSON, missing tokens, or
/// API-key mode — all of which mean "no OAuth credential available".
pub fn parse_auth(raw: &str) -> Option<CodexAuth> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("auth_mode").and_then(|m| m.as_str()) != Some("chatgpt") {
        return None;
    }
    let tokens = v.get("tokens")?;
    Some(CodexAuth {
        access_token: tokens.get("access_token")?.as_str()?.to_string(),
        refresh_token: tokens
            .get("refresh_token")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        account_id: tokens
            .get("account_id")
            .and_then(|t| t.as_str())
            .map(str::to_string),
    })
}

/// Read and parse the auth file. `None` if absent or unusable — the caller
/// falls through to passthrough.
pub fn read_auth(path: &Path) -> Option<CodexAuth> {
    parse_auth(&std::fs::read_to_string(path).ok()?)
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib codex::tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/codex.rs
git commit -m "feat(codex): read and parse ~/.codex/auth.json"
```

---

### Task 5: `TokenSource::Codex` and a per-provider lookup

**Files:**
- Modify: `src/lib.rs` — `TokenSource` (line 94), `TokenSource::resolve` (line 114), `AppState` (line 141)

**Interfaces:**
- Consumes: `codex::read_auth`, `codex::default_auth_path` from Task 4
- Produces: `TokenSource::Codex(PathBuf)`; `AppState.token_source_codex: TokenSource`; `AppState::token_source_for(&self, provider: Provider) -> &TokenSource`; `AppState::with_token_source_codex(self, ts: TokenSource) -> Self`

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn codex_token_source_resolves_access_token() {
    let dir = std::env::temp_dir().join("mmg-codex-test");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("auth.json");
    std::fs::write(
        &p,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"fake-access-token","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();
    let ts = TokenSource::Codex(p.clone());
    assert_eq!(ts.resolve().unwrap().as_deref(), Some("fake-access-token"));
    std::fs::remove_file(&p).ok();
}

#[test]
fn token_source_for_picks_per_provider() {
    let s = AppState::new("https://a.test", "https://o.test", "https://g.test", TokenSource::Disabled)
        .unwrap()
        .with_token_source_codex(TokenSource::Static(Arc::new("codex-tok".to_string())));
    // Anthropic keeps the global source; Codex gets its own.
    assert!(matches!(s.token_source_for(Provider::Anthropic), TokenSource::Disabled));
    assert_eq!(
        s.token_source_for(Provider::Codex).resolve().unwrap().as_deref(),
        Some("codex-tok")
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib codex_token_source_resolves_access_token token_source_for_picks_per_provider`
Expected: FAIL — no `TokenSource::Codex` variant.

- [ ] **Step 3: Implement**

Add the variant to `TokenSource` (line 94):

```rust
    /// Read `tokens.access_token` from a Codex credentials JSON file
    /// (`~/.codex/auth.json`). Re-read on every request.
    Codex(std::path::PathBuf),
```

Add the arm to `resolve` (line 114):

```rust
            TokenSource::Codex(path) => Ok(codex::read_auth(path).map(|a| a.access_token)),
```

Add the field to `AppState` (after `token_source`, line 146):

```rust
    /// Credential source for `Provider::Codex`. Defaults to `~/.codex/auth.json`;
    /// `token_source` stays the fallback for every other provider.
    pub token_source_codex: TokenSource,
```

Initialise it in `AppState::new`:

```rust
            token_source_codex: match codex::default_auth_path() {
                Some(p) => TokenSource::Codex(p),
                None => TokenSource::Disabled,
            },
```

Add the lookup and the test builder:

```rust
    /// Credential source for a provider. Only Codex has its own; everything
    /// else uses the global source, so existing installs are unaffected.
    pub fn token_source_for(&self, provider: Provider) -> &TokenSource {
        match provider {
            Provider::Codex => &self.token_source_codex,
            _ => &self.token_source,
        }
    }

    /// Override the Codex token source. Used by tests.
    pub fn with_token_source_codex(mut self, ts: TokenSource) -> Self {
        self.token_source_codex = ts;
        self
    }
```

- [ ] **Step 4: Wire the env override**

In `AppState::from_env` (near the `compress` read at line 186), after the state is built, apply:

```rust
        if let Ok(spec) = std::env::var("MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX") {
            state.token_source_codex = parse_token_source(&spec)?;
        }
```

Reuse whatever function `from_env` already uses to turn `MUR_MODEL_GATEWAY_TOKEN_SOURCE` into a `TokenSource`; if it is inline, extract it to `fn parse_token_source(spec: &str) -> anyhow::Result<TokenSource>` first and call it from both places. Extend that function with a `"codex"` spec mapping to `TokenSource::Codex(default_auth_path())`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, all pre-existing tests included.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs
git commit -m "feat(codex): per-provider token sources with a Codex source"
```

---

### Task 6: Attach Codex credentials on the request path

**Files:**
- Modify: `src/lib.rs` — the credential block at lines 264-308

**Interfaces:**
- Consumes: `Provider::Codex` (Task 3), `codex::read_auth` (Task 4), `token_source_for` (Task 5), `codex::apply_codex_headers` (Tasks 1/2)
- Produces: a Codex branch that leaves the Anthropic path byte-for-byte unchanged

- [ ] **Step 1: Write the failing test**

New file `tests/codex.rs`:

```rust
//! Stage 1 acceptance: the Codex route attaches OAuth credentials.
//!
//! Only compiled when codex_impl.rs is present (cfg(has_codex_hook)).
#![cfg(has_codex_hook)]

use httpmock::prelude::*;
use mur_model_gateway::cc_version::{VersionCache, VersionStrategy};
use mur_model_gateway::{AppState, TokenSource, build_router};
use std::sync::Arc;
use std::time::Duration;

fn pinned_version() -> Arc<VersionCache> {
    Arc::new(VersionCache::new(VersionStrategy::Static("9.9.9".to_string())))
}

/// Same shape as `spawn` in tests/disguise.rs:330, with the Codex upstream and
/// token source overridden. The other three upstreams point at .invalid so a
/// misrouted request fails loudly instead of escaping to the network.
async fn spawn_codex(upstream: String, codex_ts: TokenSource) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let state = AppState::with_version(
        "https://a.invalid",
        "https://o.invalid",
        "https://g.invalid",
        TokenSource::Disabled,
        pinned_version(),
    )
    .unwrap()
    .with_upstream_codex(upstream)
    .with_token_source_codex(codex_ts);
    let app = build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(20)).await;
    addr.to_string()
}

#[tokio::test]
async fn codex_route_attaches_bearer_token() {
    let upstream = MockServer::start_async().await;
    // path("/responses") also asserts the /v1 strip from Task 3.
    let mock = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer codex-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let proxy = spawn_codex(
        upstream.base_url(),
        TokenSource::Static(Arc::new("codex-tok".to_string())),
    )
    .await;

    // Client sends no auth at all — the proxy fills it in.
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    mock.assert_async().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test codex`
Expected: FAIL — the mock is not hit, or hit without the Bearer header, because no Codex branch exists yet.

- [ ] **Step 3: Implement**

In the credential block, the existing `override_token` computation stays exactly as it is for Anthropic. Add a Codex branch before it and skip the Anthropic logic when the provider is Codex:

```rust
    // Codex: auth-less callers get the stored Codex credential plus the client
    // headers. Requests that already carry auth pass through untouched, same
    // rule as the Anthropic path.
    let codex_cred: Option<(String, Option<String>)> = if provider == Provider::Codex
        && parts.headers.get(header::AUTHORIZATION).is_none()
    {
        match state.token_source_for(Provider::Codex) {
            TokenSource::Codex(path) => {
                codex::read_auth(path).map(|a| (a.access_token, a.account_id))
            }
            other => match other.resolve() {
                Ok(Some(tok)) => Some((tok, None)),
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

Then, where `upstream_req` is built (line 308), after the existing disguise header application:

```rust
    if let Some((token, account_id)) = codex_cred.as_ref() {
        upstream_req = codex::apply_codex_headers(upstream_req, token, account_id.as_deref());
    }
```

Guard the Anthropic branch so Codex never enters it — the existing condition at line 268 already requires `provider != Provider::Anthropic` to be false, so no change is needed there. Verify by reading it rather than assuming.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test codex && cargo test`
Expected: PASS, and `tests/disguise.rs` and `tests/passthrough.rs` still green — the Anthropic path must be untouched.

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs tests/codex.rs
git commit -m "feat(codex): attach Codex credentials on the responses route"
```

---

### Task 7: Refresh on 401, persist rotation, retry once

**Revised 2026-08-11 after the Task 2 gate.** The gate proved refresh tokens rotate: the grant
returns a new `refresh_token` differing from the stored one. In-memory-only refresh therefore works
exactly once and then strands both the gateway and Codex CLI on a dead credential. The rotated pair
must be persisted. See the spec's "Revision 2026-08-11" section.

**Files:**
- Modify: `src/lib.rs` (response handling after the upstream call, around line 346), `src/codex.rs`

**Interfaces:**
- Consumes: `codex::refresh_access_token` (Tasks 1/2), `codex::read_auth` (Task 4)
- Produces: `codex::refreshed_access_token(path: &Path) -> Option<String>` — returns a refreshed token, memoised for `keychain::CACHE_TTL`; `codex::reset_refresh_cache()` (test-only)

- [ ] **Step 1: Write the failing test**

Add to `tests/codex.rs`. The OAuth token endpoint is pointed at the same mock server, so nothing leaves the machine:

```rust
#[tokio::test]
async fn expired_token_triggers_one_refresh_and_retry() {
    let upstream = MockServer::start_async().await;

    // The stored token is rejected...
    let stale = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer stale-tok");
            then.status(401).body(r#"{"error":"expired"}"#);
        })
        .await;

    // ...the refreshed one is accepted.
    let fresh = upstream
        .mock_async(|when, then| {
            when.method(POST)
                .path("/responses")
                .header("authorization", "Bearer fresh-tok");
            then.status(200).body(r#"{"ok":true}"#);
        })
        .await;

    let token_ep = upstream
        .mock_async(|when, then| {
            when.method(POST).path("/oauth/token");
            then.status(200).body(r#"{"access_token":"fresh-tok"}"#);
        })
        .await;

    let dir = std::env::temp_dir().join("mmg-codex-refresh");
    std::fs::create_dir_all(&dir).unwrap();
    let auth = dir.join("auth.json");
    std::fs::write(
        &auth,
        r#"{"auth_mode":"chatgpt","tokens":{"access_token":"stale-tok","refresh_token":"fake-refresh-token","account_id":"acct-fake"}}"#,
    )
    .unwrap();

    // SAFETY: edition 2024 makes set_var unsafe; this test owns the process env.
    unsafe {
        std::env::set_var(
            "MUR_MODEL_GATEWAY_CODEX_TOKEN_ENDPOINT",
            format!("{}/oauth/token", upstream.base_url()),
        );
    }
    mur_model_gateway::codex::reset_refresh_cache();

    let proxy = spawn_codex(upstream.base_url(), TokenSource::Codex(auth.clone())).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{proxy}/v1/responses"))
        .header("content-type", "application/json")
        .body(r#"{"model":"gpt-5-codex","input":"say ok"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(stale.hits_async().await, 1, "exactly one 401 — never a retry loop");
    assert_eq!(token_ep.hits_async().await, 1, "exactly one refresh");
    assert_eq!(fresh.hits_async().await, 1, "exactly one retry");
    std::fs::remove_file(&auth).ok();
}
```

**Cache bleed warning:** the refresh cache is process-global, so a token cached by one test is visible to the next. That is why `reset_refresh_cache()` exists and why this test calls it first. Add it to `src/codex.rs` as:

```rust
/// Clear the in-memory refreshed token. Test-only: the cache is process-global
/// and would otherwise leak between integration tests.
pub fn reset_refresh_cache() {
    if let Some(cell) = REFRESHED.get() {
        *cell.lock().unwrap() = None;
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --test codex expired_token_triggers_one_refresh_and_retry`
Expected: FAIL — the 401 is returned to the caller, no retry.

- [ ] **Step 3: Implement the refresh with persistence**

The hidden `refresh_access_token` must now return the rotated pair, not just the access token.
Change its return type in **both** places — the stub in `src/codex.rs` (committed in Task 1) and the
real impl in the gitignored `src/codex/codex_impl.rs` (Task 2) — and add the shared struct to
tracked `src/codex.rs`:

```rust
/// What an OAuth refresh grant returns. `refresh_token` is `Some` when the
/// provider rotates it — ChatGPT does, so it must be persisted or the next
/// refresh fails.
#[derive(Clone, Debug)]
pub struct RefreshedTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}
```

Stub becomes `pub fn refresh_access_token(_rt: &str) -> anyhow::Result<RefreshedTokens>` and still
bails. The real impl parses both fields out of the grant response.

Then in `src/codex.rs`:

```rust
use anyhow::Context;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Most recent refresh, memoised so a burst of 401s triggers one grant.
static REFRESHED: OnceLock<Mutex<Option<(Instant, String)>>> = OnceLock::new();

/// A usable access token, refreshing when the stored one was rejected. The
/// grant rotates the refresh token, so the new pair is persisted — discarding
/// it strands both this gateway and Codex CLI on a dead credential.
/// Memoised for `keychain::CACHE_TTL`.
pub fn refreshed_access_token(path: &Path) -> Option<String> {
    let cell = REFRESHED.get_or_init(|| Mutex::new(None));
    let mut slot = cell.lock().unwrap();
    if let Some((at, tok)) = slot.as_ref()
        && at.elapsed() < crate::keychain::CACHE_TTL
    {
        return Some(tok.clone());
    }
    let rt = read_auth(path)?.refresh_token?;
    match refresh_access_token(&rt) {
        Ok(new) => {
            if let Err(e) = persist_rotation(path, &new) {
                // The access token still serves this request, but a lost
                // rotation means the next refresh fails. Warn loudly.
                tracing::warn!(error = %e, "codex token rotation not persisted");
            }
            *slot = Some((Instant::now(), new.access_token.clone()));
            Some(new.access_token)
        }
        Err(e) => {
            tracing::warn!(error = %e, "codex token refresh failed");
            None
        }
    }
}

/// Replace only the rotated fields, atomically. Codex CLI reads keys this
/// gateway does not model, so the rest of the document is preserved verbatim.
/// `last_refresh` is deliberately left alone — it is Codex CLI's bookkeeping,
/// and updating it would need a date dependency this crate does not have.
fn persist_rotation(path: &Path, new: &RefreshedTokens) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let mut doc: serde_json::Value = serde_json::from_str(&raw)?;
    let tokens = doc
        .get_mut("tokens")
        .context("auth.json has no tokens object")?;
    tokens["access_token"] = serde_json::Value::String(new.access_token.clone());
    if let Some(rt) = &new.refresh_token {
        tokens["refresh_token"] = serde_json::Value::String(rt.clone());
    }

    // Temp file in the SAME directory, so the rename stays on one filesystem —
    // that is what makes it atomic. A concurrent reader sees the old file or
    // the new one, never a torn one.
    let dir = path.parent().context("auth.json has no parent dir")?;
    let tmp = dir.join(".auth.json.mmg-tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&doc)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
```

Add a unit test proving persistence keeps unmodelled fields:

```rust
    #[test]
    fn persist_rotation_preserves_unmodelled_fields() {
        let dir = std::env::temp_dir().join("mmg-codex-persist");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("auth.json");
        std::fs::write(
            &p,
            r#"{"auth_mode":"chatgpt","OPENAI_API_KEY":null,"some_future_key":42,
                "tokens":{"id_token":"fake-id","access_token":"old-a","refresh_token":"old-r","account_id":"acct"}}"#,
        )
        .unwrap();

        persist_rotation(
            &p,
            &RefreshedTokens {
                access_token: "new-a".into(),
                refresh_token: Some("new-r".into()),
            },
        )
        .unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["tokens"]["access_token"], "new-a");
        assert_eq!(v["tokens"]["refresh_token"], "new-r");
        // Untouched fields survive — Codex CLI depends on them.
        assert_eq!(v["tokens"]["id_token"], "fake-id");
        assert_eq!(v["tokens"]["account_id"], "acct");
        assert_eq!(v["some_future_key"], 42);
        assert_eq!(v["auth_mode"], "chatgpt");
        std::fs::remove_file(&p).ok();
    }
```

- [ ] **Step 4: Implement the retry**

After the upstream response is received, before it is turned into the client response:

```rust
    // One retry only. A second 401 goes back to the caller unchanged.
    if provider == Provider::Codex
        && resp.status() == reqwest::StatusCode::UNAUTHORIZED
        && let TokenSource::Codex(path) = state.token_source_for(Provider::Codex)
        && let Some(fresh) = codex::refreshed_access_token(path)
    {
        let account_id = codex::read_auth(path).and_then(|a| a.account_id);
        let retry = state.client.request(parts.method.clone(), &target_url);
        let retry = codex::apply_codex_headers(retry, &fresh, account_id.as_deref());
        resp = retry.body(body_bytes.clone()).send().await
            .with_context(|| format!("upstream retry {target_url}"))?;
    }
```

`body_bytes` is already buffered earlier in the handler for the compression path, so the retry can reuse it. Confirm it is still in scope at this point; if it was moved into the first request, clone it before the first send.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/lib.rs src/codex.rs
git commit -m "feat(codex): refresh the access token on 401 and retry once"
```

---

### Task 8: Keep compression off for Codex

**Files:**
- Modify: `src/compress.rs` — `should_compress` (line 432)

**Interfaces:**
- Consumes: `Provider::Codex` (Task 3)
- Produces: nothing new

- [ ] **Step 1: Write the failing test**

Add to the existing `should_compress_per_provider` test in `src/compress.rs`:

```rust
    // The Responses body is not the messages body; compressing it would
    // corrupt requests. Deliberately out of scope for stage 1.
    assert!(!should_compress("/v1/responses", Provider::Codex));
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib should_compress_per_provider`
Expected: FAIL if `should_compress` matches on path alone and returns true; PASS immediately if the provider match already excludes Codex — in which case record that and move to Step 5.

- [ ] **Step 3: Implement**

Add an explicit arm to `should_compress` so the exclusion is stated rather than incidental:

```rust
        Provider::Codex => false,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/compress.rs
git commit -m "feat(codex): keep wire compression off for the Codex route"
```

---

### Task 9: Install-time configuration and documentation

**Files:**
- Modify: `src/install.rs` — `env_pairs` (line 100), `src/main.rs` — install flags (around line 26)
- Modify: `README.md`, `README-tw.md`, `docs/install.md`, `docs/install-tw.md`

**Interfaces:**
- Consumes: the env var names from Task 5
- Produces: nothing consumed by later tasks

- [ ] **Step 1: Write the failing test**

In `src/install.rs` tests, alongside `compress_env_passes_through_when_opted_in` (line 505):

```rust
    #[test]
    fn codex_token_source_is_baked_into_the_descriptor() {
        let mut opts = InstallOpts::default();
        opts.token_source_codex = Some("codex".to_string());
        let env = env_pairs(&opts, false).unwrap();
        assert!(env.iter().any(|(k, v)| k == "MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX" && v == "codex"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib codex_token_source_is_baked_into_the_descriptor`
Expected: FAIL — no `token_source_codex` field on `InstallOpts`.

- [ ] **Step 3: Implement**

Add `pub token_source_codex: Option<String>` to `InstallOpts` beside `compress` (line 28), emit the pair in `env_pairs`, and add the `--token-source-codex <spec>` clap flag in `src/main.rs` next to `--token-source`. Apply the same value validation the other install values get — whitespace and `<>"&` are rejected because values are spliced into plist/unit/cmd files.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib`
Expected: PASS.

- [ ] **Step 5: Update the documentation**

Add to the Configuration table in `README.md` and `README-tw.md`:

```markdown
| `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` | `codex` | Credential source for `/v1/responses` — `codex`, `off`, `env:<VAR>` |
| `MUR_MODEL_GATEWAY_UPSTREAM_CODEX` | `https://chatgpt.com/backend-api/codex` | Codex upstream |
```

Add the Codex route to the "One outlet" bullet in both READMEs: `/v1/responses*` → Codex.

**Unpark the CodeX wording.** `README.md` and `README-tw.md` are currently held uncommitted with the "Sublet your subscriptions" bullet changed to say CodeX, plus a `[中文說明](README-tw.md)` language link, and `README-tw.md` is untracked. This task is where they land. Reword that bullet to name both, since the gateway now attaches Claude Code credentials on `/v1/messages` and Codex credentials on `/v1/responses` — naming only one is inaccurate either way.

State plainly in both READMEs that MUR agents cannot use the Codex route yet: MUR's OpenAI client only speaks `/chat/completions`, so this is reachable only by a client that speaks the Responses API until stage 2 ships.

- [ ] **Step 6: Commit**

```bash
git add src/install.rs src/main.rs README.md README-tw.md docs/install.md docs/install-tw.md
git commit -m "feat(codex): install flag, env vars, and documentation"
```

- [ ] **Step 7: Full gate, after the commit**

Order matters: `git archive HEAD` only sees committed content, so running it
before the commit verifies stale HEAD and passes for the wrong reason. Task 1
hit exactly this.

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
R=$(mktemp -d) && git archive HEAD | tar -x -C "$R" && cd "$R" \
  && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: both pass. The second proves the public build compiles with the Codex stubs and that fmt does not trip over the gitignored module. Confirm the clean-tree run reports the same test count as the working-tree run — a lower count means it verified an older commit.

---

## Manual verification

Automated tests use mocks, so they confirm the wiring, not that ChatGPT accepts our requests. After Task 9:

1. `codex login` if `last_refresh` in `~/.codex/auth.json` is stale — on this machine it reads `2026-07-10`, so the stored token is almost certainly dead.
2. Start the gateway: `MUR_MODEL_GATEWAY_BIND=127.0.0.1:8099 cargo run --release`
3. Send an auth-less request:

```bash
curl -sS -X POST http://127.0.0.1:8099/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"gpt-5-codex","input":"say ok"}' | head -20
```

Expected: a Responses-API reply, not a 401. A 401 after Task 7 means the refresh grant is failing — recheck the Task 2 Step 4 gate before touching anything else.

4. Confirm the Anthropic path is unaffected: `curl -sS -X POST http://127.0.0.1:8099/v1/messages -H 'content-type: application/json' -d '{"model":"claude-sonnet-5","max_tokens":16,"messages":[{"role":"user","content":"say ok"}]}'`

## Out of scope

Stage 2, each needing its own spec:

- **Chat Completions translation** — request into the Responses shape and the reply, including SSE streaming and tool calls, back out. Until this lands no MUR agent can reach the Codex route.
- **Compression for the Responses body shape.**
- **API-key mode** (`auth_mode: "apikey"`) — currently resolves to passthrough.
