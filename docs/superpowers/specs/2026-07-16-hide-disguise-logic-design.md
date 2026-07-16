# Hide Disguise Logic from Public Repository

**Date**: 2026-07-16
**Status**: draft

## Goal

The full disguise module logic (`billing_prefix`, `inject_billing_prefix`, `OAUTH_BETAS`,
`merge_betas`, and the `Authorization`/`anthropic-beta`/`anthropic-version` header injection
in the proxy handler) must not appear in the public repository. Only `should_disguise()`
(path matching) stays visible.

A developer who has a local `src/disguise_impl.rs` (gitignored) gets the real logic compiled
in. Everyone else gets no-op stubs — the proxy passes requests through without disguise.

## Approach

Conditional compilation via `build.rs` detecting a gitignored file:

```
build.rs ── detects src/disguise_impl.rs exists? ── yes ──> rustc --cfg has_beta_hook
                        │
                        └── no ──> cfg not set, stubs compiled
```

The cfg name `has_beta_hook` is deliberately generic — it reveals nothing about what the
hook does.

## File Layout

### New Files

| File | Committed? | Purpose |
|---|---|---|
| `build.rs` | Yes (public) | Detect `src/disguise_impl.rs`, emit `cargo:rustc-cfg=has_beta_hook` |
| `src/disguise_impl.rs` | **No** (gitignored) | Real disguise logic: constants, body rewriting, header injection |

### Changed Files

| File | What changes |
|---|---|
| `src/disguise.rs` | Keep `should_disguise()`. Everything else becomes cfg-gated stubs. |
| `src/lib.rs` | The header-injection block (lines 332–346) moves to a function in disguise_impl; called conditionally. |
| `.gitignore` | Add `src/disguise_impl.rs` |

## disguise.rs (Public)

```rust
//! Disguise layer: rewrites requests bound for `/v1/messages*`.
//!
//! In public builds, only `should_disguise()` is active — all other
//! functions are no-op stubs. The real implementation lives in a
//! gitignored file activated via build.rs (`cfg(has_beta_hook)`).

/// True if `path` is one of the Anthropic Messages endpoints.
pub fn should_disguise(path: &str) -> bool {
    matches!(path, "/v1/messages" | "/v1/messages/count_tokens")
        || path.starts_with("/v1/messages?")
        || path.starts_with("/v1/messages/count_tokens?")
}

// ── cfg-gated: real impl or stub ──

#[cfg(has_beta_hook)]
mod disguise_impl;

#[cfg(not(has_beta_hook))]
mod disguise_impl {
    use anyhow::Context;

    pub fn inject_billing_prefix(body: &[u8], _cc_version: &str) -> anyhow::Result<Vec<u8>> {
        Ok(body.to_vec())
    }

    pub fn apply_disguise_headers(
        upstream_req: reqwest::RequestBuilder,
        _token: &str,
        _client_betas: &[String],
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(upstream_req)
    }
}

// Re-export so lib.rs calls are unchanged
pub(crate) use disguise_impl::*;
```

## disguise_impl.rs (Private, gitignored)

Contains the real implementations:

```rust
//! Real disguise implementation — kept out of the public repository.

use anyhow::Context;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};

/// Beta capabilities Claude Code claims when authenticating via OAuth.
pub const OAUTH_BETAS: &str = "<real beta string>";

/// Merge OAuth betas with client-supplied betas (dedup, order-preserving).
pub fn merge_betas(oauth: &str, client: &[String]) -> String { /* ... */ }

/// Build the billing-header text block.
pub fn billing_prefix(cc_version: &str) -> String { /* ... */ }

/// Inject billing-header prefix into the request body's `system` field.
pub fn inject_billing_prefix(body: &[u8], cc_version: &str) -> anyhow::Result<Vec<u8>> { /* ... */ }

/// Add Authorization, anthropic-beta, and anthropic-version headers to the
/// upstream request.
pub fn apply_disguise_headers(
    upstream_req: reqwest::RequestBuilder,
    token: &str,
    client_betas: &[String],
) -> anyhow::Result<reqwest::RequestBuilder> { /* ... */ }
```

**Key design choice**: `apply_disguise_headers` encapsulates the three headers
(`Authorization: Bearer`, `anthropic-beta`, `anthropic-version`) that are
currently set inline in `lib.rs`. This moves ALL sensitive header logic into
the gitignored file.

## lib.rs Changes

Two call sites change:

### 1. Body rewrite (currently line 303)

Before:
```rust
disguise::inject_billing_prefix(&body_bytes, ver)?
```

After (no change in call syntax — the stub returns same bytes):
```rust
disguise::inject_billing_prefix(&body_bytes, ver)?
```

### 2. Header injection (currently lines 332–346)

Before (inline):
```rust
if let Some(tok) = override_token.as_deref() {
    let merged_betas = disguise::merge_betas(disguise::OAUTH_BETAS, &client_betas);
    upstream_req = upstream_req
        .header("Authorization", ...)
        .header("anthropic-beta", ...);
    if !parts.headers.contains_key("anthropic-version") {
        upstream_req = upstream_req.header("anthropic-version", "2023-06-01");
    }
}
```

After:
```rust
if let Some(tok) = override_token.as_deref() {
    upstream_req = disguise::apply_disguise_headers(upstream_req, tok, &client_betas)?;
}
```

In the no-op stub, `apply_disguise_headers` returns the builder unchanged — the
headers are simply not added.

## build.rs

```rust
fn main() {
    if std::path::Path::new("src/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
}
```

## .gitignore

Add one line:
```
src/disguise_impl.rs
```

## Runtime Behavior Matrix

| `has_beta_hook` | `TokenSource` | Result |
|---|---|---|
| No | Any | Passthrough — no body rewrite, no disguise headers |
| Yes | `Disabled` | Passthrough — `override_token` is `None` |
| Yes | Non-`Disabled` | Full disguise: body rewrite + headers |

No additional env var needed. The existing `TokenSource` mechanism controls
whether disguise activates at runtime; the cfg flag controls whether the
logic exists at all.

## Test Migration

Existing tests in `src/disguise.rs` split:

- `should_disguise_paths` → stays in `disguise.rs` (public)
- All other tests (`injects_into_string_system`, `injects_into_array_system_*`,
  `injects_when_system_absent`, `passes_through_non_json`, `billing_prefix_*`,
  `merge_betas_*`) → move into `src/disguise_impl.rs`

## Test Plan

1. `cargo test` passes without `disguise_impl.rs` (stubs compile, only
   `should_disguise` test runs)
2. `cargo test` passes with `disguise_impl.rs` present (all tests run)
3. Manual: build without the file, verify `strings` output contains no
   `anthropic-beta` or `OAUTH_BETAS` values
