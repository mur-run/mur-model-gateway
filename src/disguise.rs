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
