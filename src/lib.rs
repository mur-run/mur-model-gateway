//! cc-proxy — local Anthropic API reverse proxy.
//!
//! Iter 1: byte-passthrough plus an optional disguise layer for
//! `/v1/messages*`. When the inbound request carries no auth, the proxy
//! resolves an OAuth token from the configured [`TokenSource`], adds
//! `Authorization: Bearer …`, the claude-code-* `anthropic-beta` header,
//! and prepends a billing-header text block to the request's `system`
//! field. Requests that already authenticate themselves are passed
//! through untouched.

pub mod cc_version;
pub mod disguise;
pub mod install;
pub mod keychain;

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BIND: &str = "127.0.0.1:8088";
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Pluggable OAuth token source.
///
/// Production default is [`TokenSource::Keychain`] which reads via the
/// `keyring` crate (macOS Security framework / Linux libsecret / Windows
/// Credential Manager). [`TokenSource::EnvVar`] reads from a process env
/// var for platforms where Claude Code's keychain layout isn't supported.
/// [`TokenSource::Static`] is a test injection point.
#[derive(Clone)]
pub enum TokenSource {
    /// Read from the OS keychain (Claude Code's `Claude Code-credentials`).
    Keychain,
    /// Read from the named environment variable on every request. `Ok(None)`
    /// if unset.
    EnvVar(String),
    /// Always return this token. Used by integration tests.
    Static(Arc<String>),
    /// Never disguise; act as a pure passthrough proxy.
    Disabled,
}

impl TokenSource {
    /// `Ok(Some)` → token available, disguise applies.
    /// `Ok(None)` → no token, passthrough.
    /// `Err` → backend error, passthrough with a warning logged by the caller.
    pub fn resolve(&self) -> Result<Option<String>, keychain::KeychainError> {
        match self {
            TokenSource::Keychain => keychain::read_claude_code_oauth(),
            TokenSource::EnvVar(name) => Ok(std::env::var(name).ok()),
            TokenSource::Static(s) => Ok(Some(s.as_ref().clone())),
            TokenSource::Disabled => Ok(None),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub upstream: String,
    pub client: reqwest::Client,
    pub token_source: TokenSource,
    pub version_cache: Arc<cc_version::VersionCache>,
}

impl AppState {
    pub fn new(upstream: impl Into<String>, token_source: TokenSource) -> anyhow::Result<Self> {
        Self::with_version(
            upstream,
            token_source,
            Arc::new(cc_version::VersionCache::detect_or_fallback()),
        )
    }

    pub fn with_version(
        upstream: impl Into<String>,
        token_source: TokenSource,
        version_cache: Arc<cc_version::VersionCache>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            upstream: upstream.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .context("reqwest client")?,
            token_source,
            version_cache,
        })
    }
}

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", any(proxy))
        .route("/{*tail}", any(proxy))
        .with_state(state)
}

async fn proxy(State(state): State<AppState>, req: Request) -> Response<Body> {
    match forward(state, req).await {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(error = %err, "proxy error");
            (StatusCode::BAD_GATEWAY, format!("cc-proxy: {err}")).into_response()
        }
    }
}

async fn forward(state: AppState, req: Request) -> anyhow::Result<Response<Body>> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let path_only = parts.uri.path();
    let target_url = format!("{}{}", state.upstream, path_and_query);
    let _: Uri = target_url.parse().context("target uri parse")?;

    let body_bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .context("read incoming body")?;

    // Disguise gate. Three modes on Messages-shape paths (when the
    // configured TokenSource is NOT Disabled):
    //
    //   1. Client supplied an OAuth-shape token (sk-ant-oat*) via x-api-key
    //      or Bearer Authorization → upgrade THAT token to Bearer + disguise.
    //      This is the new-mur path: the public client doesn't know how to
    //      disguise, so it just forwards whatever key it has.
    //   2. Client supplied no auth at all → resolve a token from the
    //      configured TokenSource (Keychain / EnvVar / Static) and disguise.
    //   3. Client supplied a non-OAuth key → passthrough untouched. Don't
    //      second-guess clients that authenticate themselves.
    //
    // TokenSource::Disabled or non-Messages paths → pure passthrough.
    let disguise_enabled = !matches!(state.token_source, TokenSource::Disabled);
    let on_messages_path = disguise::should_disguise(path_only);

    let override_token: Option<String> = if !disguise_enabled || !on_messages_path {
        None
    } else if let Some(tok) = extract_oauth_shape_token(&parts.headers) {
        // Mode 1: client gave us an OAuth-shape token to use.
        Some(tok)
    } else if !parts.headers.contains_key("authorization")
        && !parts.headers.contains_key("x-api-key")
    {
        // Mode 2: no auth on inbound, resolve from TokenSource.
        match state.token_source.resolve() {
            Ok(token) => token,
            Err(e) => {
                tracing::warn!(error = %e, "token source failed, passing through");
                None
            }
        }
    } else {
        // Mode 3: pure passthrough.
        None
    };

    let cc_version = if override_token.is_some() {
        Some(state.version_cache.get())
    } else {
        None
    };
    let final_body: Vec<u8> = if let Some(ver) = cc_version.as_deref() {
        tracing::debug!(path = %path_only, cc_version = %ver, "applying disguise");
        disguise::inject_billing_prefix(&body_bytes, ver)?
    } else {
        body_bytes.to_vec()
    };

    let mut upstream_req = state.client.request(parts.method.clone(), &target_url);

    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) || name == "host" || name == "content-length" {
            continue;
        }
        // When disguising we own these headers — drop client copies.
        if override_token.is_some()
            && (name == "authorization" || name == "x-api-key" || name == "anthropic-beta")
        {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }

    if let Some(tok) = override_token.as_deref() {
        upstream_req = upstream_req
            .header(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {tok}")).context("invalid Bearer token")?,
            )
            .header("anthropic-beta", disguise::OAUTH_BETAS);
        if !parts.headers.contains_key("anthropic-version") {
            upstream_req = upstream_req.header("anthropic-version", "2023-06-01");
        }
    }
    upstream_req = upstream_req.body(final_body);

    let upstream_resp = upstream_req
        .send()
        .await
        .with_context(|| format!("upstream {target_url}"))?;

    let status = upstream_resp.status();
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        response_headers.insert(name.clone(), value.clone());
    }

    tracing::debug!(
        method = %parts.method,
        path = %path_and_query,
        status = %status,
        disguise = override_token.is_some(),
        "proxied"
    );

    let stream = upstream_resp.bytes_stream();
    let body = Body::from_stream(stream);

    let mut builder = Response::builder().status(status.as_u16());
    if let Some(h) = builder.headers_mut() {
        *h = response_headers;
    }
    builder.body(body).context("build response")
}

/// If the inbound request supplies an Anthropic OAuth subscription token
/// (sk-ant-oat*) via either `x-api-key` or `Authorization: Bearer`, return
/// it so the proxy can upgrade it to a fully-disguised Bearer request.
/// Returns None if the inbound auth is a regular API key (sk-ant-api03-*)
/// or absent entirely — pure passthrough in those cases.
fn extract_oauth_shape_token(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && v.contains("sk-ant-oat")
    {
        return Some(v.to_string());
    }
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(rest) = v
            .strip_prefix("Bearer ")
            .or_else(|| v.strip_prefix("bearer "))
        && rest.contains("sk-ant-oat")
    {
        return Some(rest.to_string());
    }
    None
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_filter_lowercase() {
        let n = HeaderName::from_static("connection");
        assert!(is_hop_by_hop(&n));
    }

    #[test]
    fn hop_by_hop_filter_passes_normal() {
        let n = HeaderName::from_static("authorization");
        assert!(!is_hop_by_hop(&n));
        let n = HeaderName::from_static("anthropic-beta");
        assert!(!is_hop_by_hop(&n));
    }

    #[test]
    fn appstate_strips_trailing_slash() {
        let s = AppState::new("https://api.anthropic.com/", TokenSource::Disabled).unwrap();
        assert_eq!(s.upstream, "https://api.anthropic.com");
    }

    #[test]
    fn token_source_static_resolves() {
        let ts = TokenSource::Static(Arc::new("abc".to_string()));
        assert_eq!(ts.resolve().unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn token_source_disabled_returns_none() {
        let ts = TokenSource::Disabled;
        assert_eq!(ts.resolve().unwrap(), None);
    }
}
