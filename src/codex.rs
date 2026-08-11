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
