//! mur-model-gateway — multi-provider LLM API reverse proxy (Anthropic, OpenAI, Gemini).
//!
//! Path-based routing: `/v1/messages*` → Anthropic, `/v1/chat/completions*` → OpenAI,
//! `/v1beta/models/*` → Gemini. A disguise layer applies to Anthropic traffic only:
//! when the inbound request carries no auth, the proxy resolves an OAuth token from
//! the configured [`TokenSource`], adds `Authorization: Bearer …`, the claude-code-*
//! `anthropic-beta` header, and prepends a billing-header text block to the request's
//! `system` field. Wire-level CCR compression (opt-in via `MUR_MODEL_GATEWAY_COMPRESS=1`)
//! applies to tool_result blocks for all three providers.

pub mod cc_version;
pub mod compress;
pub mod disguise;
pub mod install;
pub mod keychain;

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BIND: &str = "127.0.0.1:8088";
pub const DEFAULT_UPSTREAM_ANTHROPIC: &str = "https://api.anthropic.com";
pub const DEFAULT_UPSTREAM_OPENAI: &str = "https://api.openai.com";
pub const DEFAULT_UPSTREAM_GEMINI: &str = "https://generativelanguage.googleapis.com";
/// Backward-compatible alias — points to Anthropic.
pub const DEFAULT_UPSTREAM: &str = DEFAULT_UPSTREAM_ANTHROPIC;
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

/// Which LLM API provider a request targets, derived from its path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum Provider {
    Anthropic,
    OpenAI,
    Gemini,
}

/// Map a request path to its provider. Falls back to Anthropic for unrecognised paths.
// N.B. `/v1/models` is deliberately NOT in the OpenAI list — both Anthropic and OpenAI
// expose that endpoint, so we let it fall through to the Anthropic default.
pub fn detect_provider(path: &str) -> Provider {
    if path == "/v1/messages"
        || path.starts_with("/v1/messages/")
        || path.starts_with("/v1/messages?")
    {
        return Provider::Anthropic;
    }
    if (path.starts_with("/v1/chat/completions/")
        || path == "/v1/chat/completions"
        || path.starts_with("/v1/chat/completions?"))
        || (path.starts_with("/v1/embeddings/")
            || path == "/v1/embeddings"
            || path.starts_with("/v1/embeddings?"))
        || (path.starts_with("/v1/images/")
            || path == "/v1/images"
            || path.starts_with("/v1/images?"))
        || (path.starts_with("/v1/files/") || path == "/v1/files" || path.starts_with("/v1/files?"))
        || (path.starts_with("/v1/threads/")
            || path == "/v1/threads"
            || path.starts_with("/v1/threads?"))
        || (path.starts_with("/v1/assistants/")
            || path == "/v1/assistants"
            || path.starts_with("/v1/assistants?"))
    {
        return Provider::OpenAI;
    }
    if path.starts_with("/v1beta/models/")
        || path == "/v1beta/models"
        || path.starts_with("/v1beta/models?")
    {
        return Provider::Gemini;
    }
    tracing::debug!(path = %path, "unrecognised path, falling back to Anthropic");
    Provider::Anthropic
}

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
    /// Read `claudeAiOauth.accessToken` from a Claude Code credentials JSON
    /// file (Linux/Windows installs write `~/.claude/.credentials.json`
    /// instead of the OS keychain). Re-read on every request.
    CredentialsFile(std::path::PathBuf),
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
            TokenSource::Keychain => {
                let from_keychain = keychain::read_claude_code_oauth();
                // Non-macOS Claude Code installs usually skip the OS keychain
                // and write ~/.claude/.credentials.json instead; fall back so
                // the zero-config default works there too.
                if cfg!(target_os = "macos") {
                    return from_keychain;
                }
                match from_keychain {
                    Ok(Some(t)) => Ok(Some(t)),
                    keychain_miss => match keychain::default_credentials_path() {
                        Some(p) if p.exists() => keychain::read_credentials_file(&p),
                        _ => keychain_miss,
                    },
                }
            }
            TokenSource::CredentialsFile(path) => keychain::read_credentials_file(path),
            TokenSource::EnvVar(name) => Ok(std::env::var(name).ok()),
            TokenSource::Static(s) => Ok(Some(s.as_ref().clone())),
            TokenSource::Disabled => Ok(None),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub upstream_anthropic: String,
    pub upstream_openai: String,
    pub upstream_gemini: String,
    pub client: reqwest::Client,
    pub token_source: TokenSource,
    pub version_cache: Arc<cc_version::VersionCache>,
    /// Wire-level tool_result compression (spec: docs/specs/2026-07-03).
    /// Env-gated: MUR_MODEL_GATEWAY_COMPRESS=1. Tests flip the field directly.
    pub compress: bool,
}

impl AppState {
    pub fn new(
        upstream_anthropic: impl Into<String>,
        upstream_openai: impl Into<String>,
        upstream_gemini: impl Into<String>,
        token_source: TokenSource,
    ) -> anyhow::Result<Self> {
        Self::with_version(
            upstream_anthropic,
            upstream_openai,
            upstream_gemini,
            token_source,
            Arc::new(cc_version::VersionCache::detect_or_fallback()),
        )
    }

    pub fn with_version(
        upstream_anthropic: impl Into<String>,
        upstream_openai: impl Into<String>,
        upstream_gemini: impl Into<String>,
        token_source: TokenSource,
        version_cache: Arc<cc_version::VersionCache>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            upstream_anthropic: upstream_anthropic.into().trim_end_matches('/').to_string(),
            upstream_openai: upstream_openai.into().trim_end_matches('/').to_string(),
            upstream_gemini: upstream_gemini.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .context("reqwest client")?,
            token_source,
            version_cache,
            compress: std::env::var("MUR_MODEL_GATEWAY_COMPRESS").is_ok_and(|v| v == "1"),
        })
    }

    /// Return the upstream URL for the given provider.
    pub fn upstream_for(&self, provider: Provider) -> &str {
        match provider {
            Provider::Anthropic => &self.upstream_anthropic,
            Provider::OpenAI => &self.upstream_openai,
            Provider::Gemini => &self.upstream_gemini,
        }
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
            (StatusCode::BAD_GATEWAY, format!("mur-model-gateway: {err}")).into_response()
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
    let provider = detect_provider(path_only);
    let target_url = format!("{}{}", state.upstream_for(provider), path_and_query);
    let _: Uri = target_url.parse().context("target uri parse")?;

    let body_bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .context("read incoming body")?;

    // Wire-level compression (opt-in): rewrite fat tool_result blocks
    // before disguise. Fail-open — None means forward the original.
    let body_bytes = if state.compress && compress::should_compress(path_only, provider) {
        match compress::rewrite_request_body(&body_bytes, provider) {
            Some(rewritten) => {
                tracing::debug!(
                    before = body_bytes.len(),
                    after = rewritten.len(),
                    "compressed tool_result blocks"
                );
                axum::body::Bytes::from(rewritten)
            }
            None => body_bytes,
        }
    } else {
        body_bytes
    };

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

    let override_token: Option<String> =
        if !disguise_enabled || !on_messages_path || provider != Provider::Anthropic {
            None
        } else if extract_oauth_shape_token(&parts.headers).is_some() {
            // Mode 1: client signals OAuth intent (sk-ant-oat*) — use the fresh
            // Keychain token instead of the client's copy, which may be stale.
            match state.token_source.resolve() {
                Ok(token) => token,
                Err(e) => {
                    tracing::warn!(error = %e, "token source failed, passing through");
                    None
                }
            }
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

    let mut client_betas: Vec<String> = Vec::new();
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) || name == "host" || name == "content-length" {
            continue;
        }
        // When disguising we own auth headers — drop client copies — and
        // capture client-supplied `anthropic-beta` values so we can merge
        // them with OAUTH_BETAS rather than overwriting (Claude Code 4.7+
        // sends betas like `clear-thinking-2025-10-15` that the upstream
        // needs to accept the request body).
        if override_token.is_some() && (name == "authorization" || name == "x-api-key") {
            continue;
        }
        if override_token.is_some() && name == "anthropic-beta" {
            if let Ok(s) = value.to_str() {
                client_betas.push(s.to_string());
            }
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }

    if let Some(tok) = override_token.as_deref() {
        let has_anthropic_version = parts.headers.contains_key("anthropic-version");
        upstream_req = disguise::apply_disguise_headers(
            upstream_req,
            tok,
            &client_betas,
            has_anthropic_version,
        )?;
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
        provider = ?provider,
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
        let s = AppState::new(
            "https://api.anthropic.com/",
            "https://api.openai.com/",
            "https://generativelanguage.googleapis.com/",
            TokenSource::Disabled,
        )
        .unwrap();
        assert_eq!(s.upstream_anthropic, "https://api.anthropic.com");
        assert_eq!(s.upstream_openai, "https://api.openai.com");
        assert_eq!(
            s.upstream_gemini,
            "https://generativelanguage.googleapis.com"
        );
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

    #[test]
    fn detect_provider_anthropic() {
        assert_eq!(detect_provider("/v1/messages"), Provider::Anthropic);
        assert_eq!(
            detect_provider("/v1/messages?beta=true"),
            Provider::Anthropic
        );
        assert_eq!(
            detect_provider("/v1/messages/count_tokens"),
            Provider::Anthropic
        );
    }

    #[test]
    fn detect_provider_openai() {
        assert_eq!(detect_provider("/v1/chat/completions"), Provider::OpenAI);
        assert_eq!(
            detect_provider("/v1/chat/completions?stream=true"),
            Provider::OpenAI
        );
        assert_eq!(detect_provider("/v1/embeddings"), Provider::OpenAI);
        assert_eq!(detect_provider("/v1/images/generations"), Provider::OpenAI);
    }

    #[test]
    fn detect_provider_gemini() {
        assert_eq!(
            detect_provider("/v1beta/models/gemini-2.5-flash:generateContent"),
            Provider::Gemini
        );
        assert_eq!(
            detect_provider("/v1beta/models/gemini-2.5-flash:streamGenerateContent"),
            Provider::Gemini
        );
    }

    #[test]
    fn detect_provider_fallback() {
        assert_eq!(detect_provider("/"), Provider::Anthropic);
        assert_eq!(detect_provider("/v1/unknown"), Provider::Anthropic);
        // /v1/models is ambiguous (both Anthropic and OpenAI expose it);
        // deliberate fallback to Anthropic default.
        assert_eq!(detect_provider("/v1/models"), Provider::Anthropic);
    }

    #[test]
    fn detect_provider_boundary_prefixes() {
        // Paths that look like OpenAI prefixes but aren't must not match.
        assert_eq!(detect_provider("/v1/images-tools"), Provider::Anthropic);
        assert_eq!(detect_provider("/v1/files-legacy"), Provider::Anthropic);
        assert_eq!(detect_provider("/v1/threads-v2"), Provider::Anthropic);
        assert_eq!(detect_provider("/v1/assistants-old"), Provider::Anthropic);
        assert_eq!(
            detect_provider("/v1/embeddings-legacy"),
            Provider::Anthropic
        );
        // Gemini boundary
        assert_eq!(
            detect_provider("/v1beta/models-config"),
            Provider::Anthropic
        );
    }

    #[test]
    fn upstream_for_resolves_correctly() {
        let s = AppState::new(
            "https://api.anthropic.com",
            "https://api.openai.com",
            "https://generativelanguage.googleapis.com",
            TokenSource::Disabled,
        )
        .unwrap();
        assert_eq!(
            s.upstream_for(Provider::Anthropic),
            "https://api.anthropic.com"
        );
        assert_eq!(s.upstream_for(Provider::OpenAI), "https://api.openai.com");
        assert_eq!(
            s.upstream_for(Provider::Gemini),
            "https://generativelanguage.googleapis.com"
        );
    }
}
