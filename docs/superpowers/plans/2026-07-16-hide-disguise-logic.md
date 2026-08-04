# Hide Disguise Logic Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move all disguise logic (OAUTH_BETAS, merge_betas, billing_prefix, inject_billing_prefix, header injection) into a gitignored file activated via build.rs `cfg(has_beta_hook)` so the public repo contains only a no-op stub.

**Architecture:** build.rs detects `src/disguise_impl.rs` at compile time → emits `cfg(has_beta_hook)`. disguise.rs keeps only `should_disguise()` plus cfg-gated stubs that are re-exported. lib.rs replaces inline header injection with a single `apply_disguise_headers()` call resolved by cfg.

**Tech Stack:** Rust 2024 edition, axum, reqwest, serde_json, anyhow

## Global Constraints

- `src/disguise_impl.rs` must be gitignored — never committed
- `build.rs` path check must be relative to project root: `src/disguise_impl.rs`
- cfg name `has_beta_hook` must be generic (no anthropic/disguise/beta references)
- `should_disguise()` stays public and unchanged
- Existing tests must pass both with and without `disguise_impl.rs`

---

### Task 1: Create build.rs and update .gitignore

**Files:**
- Create: `build.rs`
- Modify: `.gitignore`

**Interfaces:**
- Produces: `cfg(has_beta_hook)` flag for rustc when `src/disguise_impl.rs` exists

- [ ] **Step 1: Write build.rs**

```rust
fn main() {
    if std::path::Path::new("src/disguise_impl.rs").exists() {
        println!("cargo:rustc-cfg=has_beta_hook");
    }
}
```

- [ ] **Step 2: Add disguise_impl.rs to .gitignore**

Append this line to `.gitignore`:

```
src/disguise_impl.rs
```

- [ ] **Step 3: Verify build.rs compiles (cargo check)**

Run: `cargo check 2>&1`
Expected: compiles successfully (no disguise_impl.rs exists yet, so cfg is not set — stubs aren't written yet, so expect errors about missing functions in disguise.rs)

**Note:** Full build will fail until Tasks 2-3 are done. This task only ensures the files are in place.

- [ ] **Step 4: Commit**

```bash
git add build.rs .gitignore
git commit -m "chore: add build.rs for conditional beta hook + gitignore disguise_impl.rs

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 2: Refactor disguise.rs — keep should_disguise, add cfg-gated stubs

**Files:**
- Modify: `src/disguise.rs` (rewrite entirely)

**Interfaces:**
- Consumes: `cfg(has_beta_hook)` from Task 1 build.rs
- Produces:
  - `pub fn should_disguise(path: &str) -> bool` — unchanged
  - `pub fn inject_billing_prefix(body: &[u8], cc_version: &str) -> anyhow::Result<Vec<u8>>` — re-exported from disguise_impl (stub or real)
  - `pub fn apply_disguise_headers(upstream_req: reqwest::RequestBuilder, token: &str, client_betas: &[String], has_anthropic_version: bool) -> anyhow::Result<reqwest::RequestBuilder>` — re-exported from disguise_impl (stub or real)

- [ ] **Step 1: Write the new disguise.rs**

Replace the entire file:

```rust
//! Disguise layer: rewrites requests bound for `/v1/messages*`.
//!
//! In public builds, only `should_disguise()` is active — all other
//! functions are no-op stubs. The real implementation lives in a
//! gitignored file activated via build.rs (`cfg(has_beta_hook)`).

/// True if `path` is one of the Anthropic Messages endpoints we disguise on.
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
    pub fn inject_billing_prefix(body: &[u8], _cc_version: &str) -> anyhow::Result<Vec<u8>> {
        Ok(body.to_vec())
    }

    pub fn apply_disguise_headers(
        upstream_req: reqwest::RequestBuilder,
        _token: &str,
        _client_betas: &[String],
        _has_anthropic_version: bool,
    ) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(upstream_req)
    }
}

pub(crate) use disguise_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_disguise_paths() {
        assert!(should_disguise("/v1/messages"));
        assert!(should_disguise("/v1/messages/count_tokens"));
        assert!(should_disguise("/v1/messages?beta=true"));
        assert!(should_disguise("/v1/messages/count_tokens?x=1"));
        assert!(!should_disguise("/v1/messages/batches"));
        assert!(!should_disguise("/v1/files"));
        assert!(!should_disguise("/"));
    }
}
```

- [ ] **Step 2: Run cargo check (without disguise_impl.rs — stubs path)**

Run: `cargo check 2>&1`
Expected: compiles. If it fails, Task 3 (lib.rs changes) also needed. Just verify disguise.rs itself has no syntax errors.

- [ ] **Step 3: Commit**

```bash
git add src/disguise.rs
git commit -m "refactor(disguise): cfg-gate sensitive logic behind has_beta_hook

Move OAUTH_BETAS, merge_betas, billing_prefix, inject_billing_prefix
into a conditional disguise_impl module. Public builds get no-op stubs.
Only should_disguise() and its test stay visible.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 3: Refactor lib.rs — replace inline header injection

**Files:**
- Modify: `src/lib.rs` (lines ~332–346)

**Interfaces:**
- Consumes: `apply_disguise_headers()` from disguise (Task 2)
- Produces: same proxy behavior via function call instead of inline headers

- [ ] **Step 1: Replace the header injection block in lib.rs**

**Find** (lines 332–346):
```rust
    if let Some(tok) = override_token.as_deref() {
        let merged_betas = disguise::merge_betas(disguise::OAUTH_BETAS, &client_betas);
        upstream_req = upstream_req
            .header(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {tok}")).context("invalid Bearer token")?,
            )
            .header(
                "anthropic-beta",
                HeaderValue::from_str(&merged_betas).context("invalid anthropic-beta")?,
            );
        if !parts.headers.contains_key("anthropic-version") {
            upstream_req = upstream_req.header("anthropic-version", "2023-06-01");
        }
    }
```

**Replace with**:
```rust
    if let Some(tok) = override_token.as_deref() {
        let has_anthropic_version = parts.headers.contains_key("anthropic-version");
        upstream_req = disguise::apply_disguise_headers(
            upstream_req,
            tok,
            &client_betas,
            has_anthropic_version,
        )?;
    }
```

- [ ] **Step 2: Check if HeaderValue import is still needed**

Run: `grep -n "HeaderValue" src/lib.rs`
If only the disguise block used it (now removed), delete the import line. Otherwise leave it.

- [ ] **Step 3: Run cargo check (without disguise_impl.rs)**

Run: `cargo check 2>&1`
Expected: compiles. All disguise functions resolve to stubs.

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs
git commit -m "refactor(proxy): replace inline disguise headers with apply_disguise_headers()

The Authorization, anthropic-beta, and anthropic-version header injection
moves into the cfg-gated disguise_impl module so the public repo shows
only a stub.

Co-Authored-By: Claude <noreply@anthropic.com>"
```

---

### Task 4: Verify without disguise_impl.rs (public build)

**Files:**
- None (verification only)

- [ ] **Step 1: cargo test (stubs)**

Run: `cargo test 2>&1`
Expected: all tests pass. Only `should_disguise_paths` runs from the disguise module. Stub functions are compiled but not called by tests (tests moved to disguise_impl.rs).

- [ ] **Step 2: cargo build --release and verify no sensitive strings**

Run:
```bash
cargo build --release 2>&1
strings target/release/mur-model-gateway | grep -i "anthropic-beta\|OAUTH_BETAS\|claude-code-2025\|billing-header" || echo "CLEAN: no sensitive strings found"
```
Expected: `CLEAN: no sensitive strings found`

---

### Task 5: Verify with disguise_impl.rs (private build)

**Files:**
- Create: `src/disguise_impl.rs` (gitignored — for verification only)

**Note:** This file is NOT committed. It exists only on the author's machine.

- [ ] **Step 1: Create src/disguise_impl.rs with the real implementation**

```rust
//! Real disguise implementation — kept out of the public repository.

use anyhow::Context;
use reqwest::header::HeaderValue;
use serde_json::{Value, json};

/// Beta capabilities Claude Code claims when authenticating via OAuth.
pub const OAUTH_BETAS: &str =
    "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,compact-2026-01-12";

/// Merge `OAUTH_BETAS` with whatever `anthropic-beta` values the client
/// already sent, preserving order (OAuth betas first) and deduping.
pub fn merge_betas(oauth: &str, client: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let client_iter = client.iter().flat_map(|s| s.split(','));
    for token in oauth.split(',').chain(client_iter) {
        let t = token.trim();
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.to_string()) {
            out.push(t.to_string());
        }
    }
    out.join(",")
}

/// Build the billing-header text block for a given Claude Code version.
pub fn billing_prefix(cc_version: &str) -> String {
    format!("x-anthropic-billing-header: cc_version={cc_version}; cc_entrypoint=sdk-cli;")
}

/// Inject the billing-header text block into the request body's `system` field.
pub fn inject_billing_prefix(body: &[u8], cc_version: &str) -> anyhow::Result<Vec<u8>> {
    let mut value: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Ok(body.to_vec()),
    };

    let Some(obj) = value.as_object_mut() else {
        return Ok(body.to_vec());
    };

    let prefix = billing_prefix(cc_version);

    match obj.get_mut("system") {
        Some(Value::String(s)) => {
            let original = std::mem::take(s);
            obj.insert(
                "system".into(),
                json!([
                    {"type": "text", "text": prefix},
                    {"type": "text", "text": original},
                ]),
            );
        }
        Some(Value::Array(arr)) => {
            arr.insert(0, json!({"type": "text", "text": prefix}));
        }
        Some(_) => {}
        None => {
            obj.insert("system".into(), json!(prefix));
        }
    }

    serde_json::to_vec(&value).context("serialize rewritten body")
}

/// Add Authorization, anthropic-beta, and anthropic-version headers to the
/// upstream request.
pub fn apply_disguise_headers(
    upstream_req: reqwest::RequestBuilder,
    token: &str,
    client_betas: &[String],
    has_anthropic_version: bool,
) -> anyhow::Result<reqwest::RequestBuilder> {
    let merged_betas = merge_betas(OAUTH_BETAS, client_betas);
    let mut req = upstream_req
        .header(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}")).context("invalid Bearer token")?,
        )
        .header(
            "anthropic-beta",
            HeaderValue::from_str(&merged_betas).context("invalid anthropic-beta")?,
        );
    if !has_anthropic_version {
        req = req.header("anthropic-version", "2023-06-01");
    }
    Ok(req)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_VER: &str = "9.9.9";

    fn expected_prefix() -> String {
        billing_prefix(TEST_VER)
    }

    #[test]
    fn injects_into_string_system() {
        let body = br#"{"model":"x","system":"hello","messages":[]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["system"].as_array().expect("array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], expected_prefix());
        assert_eq!(arr[1]["text"], "hello");
    }

    #[test]
    fn injects_into_array_system_preserving_cache_control() {
        let body =
            br#"{"system":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let arr = v["system"].as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], expected_prefix());
        assert_eq!(arr[1]["text"], "hi");
        assert_eq!(arr[1]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn injects_when_system_absent() {
        let body = br#"{"model":"x","messages":[]}"#;
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"], expected_prefix());
    }

    #[test]
    fn passes_through_non_json() {
        let body = b"not json at all";
        let out = inject_billing_prefix(body, TEST_VER).unwrap();
        assert_eq!(out, body);
    }

    #[test]
    fn billing_prefix_includes_version() {
        let p = billing_prefix("2.1.126");
        assert!(p.contains("cc_version=2.1.126"));
        assert!(p.contains("cc_entrypoint=sdk-cli"));
    }

    #[test]
    fn merge_betas_appends_client_values() {
        let merged = merge_betas("a,b", &["c".to_string(), "d".to_string()]);
        assert_eq!(merged, "a,b,c,d");
    }

    #[test]
    fn merge_betas_dedupes_and_preserves_oauth_order() {
        let merged = merge_betas(
            "claude-code-20250219,compact-2026-01-12",
            &["compact-2026-01-12,clear-thinking-2025-10-15".to_string()],
        );
        assert_eq!(
            merged,
            "claude-code-20250219,compact-2026-01-12,clear-thinking-2025-10-15"
        );
    }

    #[test]
    fn merge_betas_handles_empty_client() {
        assert_eq!(merge_betas(OAUTH_BETAS, &[]), OAUTH_BETAS);
    }

    #[test]
    fn merge_betas_trims_whitespace() {
        let merged = merge_betas("a,b", &[" c , d ".to_string()]);
        assert_eq!(merged, "a,b,c,d");
    }
}
```

- [ ] **Step 2: cargo test with disguise_impl.rs present**

Run: `cargo test 2>&1`
Expected: all tests pass — including the 8 tests in disguise_impl.rs plus should_disguise_paths from disguise.rs.

- [ ] **Step 3: Verify sensitive strings ARE present in this build**

Run:
```bash
strings target/release/mur-model-gateway | grep "anthropic-beta" || echo "NOT FOUND"
```
Expected: `anthropic-beta` IS found (it's compiled in from disguise_impl.rs).

- [ ] **Step 4: Remove disguise_impl.rs to restore clean state**

```bash
rm src/disguise_impl.rs
cargo test 2>&1
```
Expected: all remaining tests still pass (stub path).

- [ ] **Step 5: Do NOT commit disguise_impl.rs (it's gitignored)**

Run: `git status`
Expected: `src/disguise_impl.rs` does NOT appear (gitignored). Working tree clean except for any local-only changes.

---

### Task 6: Final verification — public build strings check

**Files:**
- None

- [ ] **Step 1: Rebuild release without disguise_impl.rs**

```bash
cargo build --release 2>&1
```

- [ ] **Step 2: Scan for any leaked sensitive strings**

```bash
strings target/release/mur-model-gateway | grep -iE "anthropic-beta|OAUTH_BETAS|claude-code-2025|billing-header|x-anthropic" || echo "CLEAN"
```
Expected: `CLEAN`

- [ ] **Step 3: Run full test suite one final time**

```bash
cargo test 2>&1
```
Expected: all tests pass.

- [ ] **Step 4: Commit any remaining changes**

```bash
git status
git add -A
git commit -m "chore: final verification — public build is clean

Co-Authored-By: Claude <noreply@anthropic.com>"
```
