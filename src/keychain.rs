//! macOS Keychain reader for the OAuth token Claude Code stores.
//!
//! Claude Code writes a JSON document under generic-password service
//! `Claude Code-credentials`; the access token lives at
//! `claudeAiOauth.accessToken`. We read it on every request so the
//! proxy automatically picks up rotated tokens — Claude Code
//! refreshes it in the background, the proxy never caches.
//!
//! Iter 1 is macOS-only. Iter 3 will replace this with the `keyring`
//! crate for cross-platform support (Linux libsecret / Windows
//! Credential Manager).

use serde_json::Value;

const SERVICE: &str = "Claude Code-credentials";

#[derive(Debug, thiserror::Error)]
pub enum KeychainError {
    #[error("keychain backend error: {0}")]
    Backend(String),
    #[error("keychain entry malformed: {0}")]
    Malformed(String),
    #[error("platform not yet supported: {0}")]
    Unsupported(&'static str),
}

/// Read the current Claude Code OAuth access token from the OS keychain.
///
/// `Ok(Some(token))` — entry found and parsed.
/// `Ok(None)` — no entry exists (Claude Code never logged in on this machine).
/// `Err(_)` — backend / parse error; callers should pass through to upstream
/// rather than mask the failure.
#[cfg(target_os = "macos")]
pub fn read_claude_code_oauth() -> Result<Option<String>, KeychainError> {
    use std::process::Command;

    let output = Command::new("security")
        .args(["find-generic-password", "-s", SERVICE, "-w"])
        .output()
        .map_err(|e| KeychainError::Backend(format!("spawn `security`: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // exit 44 / "could not be found" → no entry, not an error.
        if output.status.code() == Some(44) || stderr.contains("could not be found") {
            return Ok(None);
        }
        return Err(KeychainError::Backend(stderr.into_owned()));
    }

    let raw = String::from_utf8(output.stdout)
        .map_err(|e| KeychainError::Malformed(format!("non-utf8 stdout: {e}")))?;
    let creds: Value = serde_json::from_str(raw.trim())
        .map_err(|e| KeychainError::Malformed(format!("not JSON: {e}")))?;
    let token = creds
        .get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| KeychainError::Malformed("missing claudeAiOauth.accessToken".into()))?;
    Ok(Some(token.to_string()))
}

#[cfg(not(target_os = "macos"))]
pub fn read_claude_code_oauth() -> Result<Option<String>, KeychainError> {
    Err(KeychainError::Unsupported(
        "non-macOS keychain support arrives in Iter 3",
    ))
}
