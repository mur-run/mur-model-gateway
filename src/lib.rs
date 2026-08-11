//! mur-model-gateway — multi-provider LLM API reverse proxy (Anthropic, OpenAI, Gemini, Codex).
//!
//! Path-based routing: `/v1/messages*` → Anthropic, `/v1/chat/completions*` → OpenAI,
//! `/v1beta/models/*` → Gemini, `/v1/responses*` → Codex (ChatGPT backend). A disguise
//! layer applies to Anthropic traffic only: when the inbound request carries no auth,
//! the proxy resolves an OAuth token from the configured [`TokenSource`], adds
//! `Authorization: Bearer …`, the claude-code-* `anthropic-beta` header, and prepends a
//! billing-header text block to the request's `system` field. The Codex route attaches
//! credentials under its own, simpler rule instead — see [`codex`] — and is
//! deliberately excluded from wire compression. Wire-level CCR compression (opt-in via
//! `MUR_MODEL_GATEWAY_COMPRESS=1`) applies to tool_result blocks for the other three
//! providers.

pub mod cc_version;
pub mod codex;
pub mod compress;
pub mod disguise;
pub mod install;
pub mod keychain;
pub mod translate;

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::any,
};
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_BIND: &str = "127.0.0.1:8088";
pub const DEFAULT_UPSTREAM_ANTHROPIC: &str = "https://api.anthropic.com";
pub const DEFAULT_UPSTREAM_OPENAI: &str = "https://api.openai.com";
pub const DEFAULT_UPSTREAM_GEMINI: &str = "https://generativelanguage.googleapis.com";
pub const DEFAULT_UPSTREAM_CODEX: &str = "https://chatgpt.com/backend-api/codex";
/// API-key mode targets the public Responses API. Deliberately the same
/// value as `DEFAULT_UPSTREAM_OPENAI` but named for its role — Codex in
/// API-key mode is a different credential mode, not the OpenAI route. Not
/// env-configurable (spec: stage 3).
pub const DEFAULT_UPSTREAM_CODEX_APIKEY: &str = "https://api.openai.com";
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
    Codex,
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
    if codex::should_route(path) {
        return Provider::Codex;
    }
    tracing::debug!(path = %path, "unrecognised path, falling back to Anthropic");
    Provider::Anthropic
}

/// ChatGPT's Codex backend is rooted at `<base>/responses`, but callers use the
/// OpenAI-style `/v1/responses`. Strip the `/v1` so the two line up. Every other
/// provider concatenates the incoming path unchanged.
pub fn codex_target_path(path_and_query: &str) -> String {
    // A translated Chat Completions request always becomes a Responses call.
    // Its inbound query string describes the chat request, not the Responses
    // one, so it is deliberately dropped — `stream` travels in the body.
    let path_only = path_and_query.split('?').next().unwrap_or(path_and_query);
    if codex::should_translate(path_only) {
        return "/responses".to_string();
    }
    match path_and_query.strip_prefix("/v1") {
        Some(rest) => rest.to_string(),
        None => path_and_query.to_string(),
    }
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
    /// Read `tokens.access_token` from a Codex credentials JSON file
    /// (`~/.codex/auth.json`). Re-read on every request.
    Codex(std::path::PathBuf),
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
            TokenSource::Codex(path) => {
                Ok(codex::read_credential(path).map(|c| c.bearer().to_string()))
            }
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
    pub upstream_codex: String,
    /// Upstream for Codex in API-key mode. Defaults to
    /// `DEFAULT_UPSTREAM_CODEX_APIKEY`; a test-only seam (`with_upstream_codex_apikey`)
    /// lets integration tests point it at httpmock. Deliberately NOT wired to
    /// any env var — the production host is hard-coded by design.
    pub upstream_codex_apikey: String,
    pub client: reqwest::Client,
    pub token_source: TokenSource,
    /// Credential source for `Provider::Codex`. `token_source` stays the
    /// fallback for every other provider.
    ///
    /// Fix round 1, finding 6: defaults to `Disabled`, deliberately — NOT
    /// the user's real `~/.codex/auth.json`. Every constructor here
    /// (`new`/`with_version`) leaves it `Disabled` unless
    /// [`AppState::with_default_codex_source`] is called explicitly; only
    /// `main.rs` calls it. This makes a test-side `AppState` that forgets
    /// to override this field structurally unable to resolve, read, or
    /// refresh-and-rewrite the real file — even if it accidentally
    /// exercises the Codex route — rather than merely unlikely to.
    pub token_source_codex: TokenSource,
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
            upstream_codex: DEFAULT_UPSTREAM_CODEX.to_string(),
            upstream_codex_apikey: DEFAULT_UPSTREAM_CODEX_APIKEY.to_string(),
            client: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .context("reqwest client")?,
            token_source,
            // Fix round 1, finding 6: safe by default. See the field doc on
            // `token_source_codex` and `with_default_codex_source` below.
            token_source_codex: TokenSource::Disabled,
            version_cache,
            compress: std::env::var("MUR_MODEL_GATEWAY_COMPRESS").is_ok_and(|v| v == "1"),
        })
    }

    /// Point the Codex token source at the user's real `~/.codex/auth.json`
    /// (or leave it `Disabled` if the home directory can't be resolved).
    /// This is what gives an unconfigured production gateway a working
    /// Codex credential — call it from `main.rs` only. Every test-side
    /// `AppState` should instead either leave the constructor default
    /// (`Disabled`) alone or call `with_token_source_codex` with a fixture
    /// path/mock token, never this method (fix round 1, finding 6).
    pub fn with_default_codex_source(mut self) -> Self {
        self.token_source_codex = match codex::default_auth_path() {
            Some(p) => TokenSource::Codex(p),
            None => TokenSource::Disabled,
        };
        self
    }

    /// Override the Codex upstream. Used by tests to point at httpmock.
    pub fn with_upstream_codex(mut self, url: impl Into<String>) -> Self {
        self.upstream_codex = url.into();
        self
    }

    /// Override the API-key-mode upstream. Test-only seam: the production
    /// default is the hard-coded `DEFAULT_UPSTREAM_CODEX_APIKEY`.
    pub fn with_upstream_codex_apikey(mut self, url: impl Into<String>) -> Self {
        self.upstream_codex_apikey = url.into();
        self
    }

    /// Override the Codex token source. Used by tests.
    pub fn with_token_source_codex(mut self, ts: TokenSource) -> Self {
        self.token_source_codex = ts;
        self
    }

    /// Return the upstream URL for the given provider.
    pub fn upstream_for(&self, provider: Provider) -> &str {
        match provider {
            Provider::Anthropic => &self.upstream_anthropic,
            Provider::OpenAI => &self.upstream_openai,
            Provider::Gemini => &self.upstream_gemini,
            Provider::Codex => &self.upstream_codex,
        }
    }

    /// Credential source for a provider. Only Codex has its own; everything
    /// else uses the global source, so existing installs are unaffected.
    pub fn token_source_for(&self, provider: Provider) -> &TokenSource {
        match provider {
            Provider::Codex => &self.token_source_codex,
            _ => &self.token_source,
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
    let codex_apikey = matches!(codex_cred, Some(codex::CodexCredential::ApiKey { .. }));
    let (target_path, upstream) = match provider {
        // API-key mode keeps /v1 on the public Responses endpoint.
        Provider::Codex if codex_apikey && translating => (
            "/v1/responses".to_string(),
            state.upstream_codex_apikey.as_str(),
        ),
        Provider::Codex if codex_apikey => (
            path_and_query.to_string(),
            state.upstream_codex_apikey.as_str(),
        ),
        // OAuth (and passthrough) keep the stage-1/2 behaviour: strip /v1,
        // chatgpt.com upstream.
        Provider::Codex => (
            codex_target_path(path_and_query),
            state.upstream_for(provider),
        ),
        _ => (path_and_query.to_string(), state.upstream_for(provider)),
    };
    let target_url = format!("{upstream}{target_path}");
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

    // Stage 2: the Codex chat path arrives as Chat Completions and must
    // leave as a Responses request. Assigning into `body_bytes` (not
    // `final_body`) is load-bearing: the 401 retry below re-sends
    // `body_bytes`, and an untranslated retry would be rejected upstream.
    let (client_model, client_wants_stream) = if translating {
        let chat = serde_json::from_slice::<serde_json::Value>(&body_bytes).unwrap_or_default();
        (
            chat.get("model")
                .and_then(|m| m.as_str().map(str::to_string)),
            chat.get("stream").and_then(serde_json::Value::as_bool) == Some(true),
        )
    } else {
        (None, false)
    };
    let body_bytes = if translating {
        let chat: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|_| anyhow::anyhow!("codex chat body is not JSON"))?;
        match translate::chat_to_responses(&chat) {
            Ok(v) => axum::body::Bytes::from(serde_json::to_vec(&v)?),
            Err(translate::TranslateError::Rejected { param }) => {
                return Ok(openai_error_response(
                    StatusCode::BAD_REQUEST,
                    &format!("`{param}` is not supported on the Codex route"),
                ));
            }
            Err(translate::TranslateError::NotAnObject) => {
                return Ok(openai_error_response(
                    StatusCode::BAD_REQUEST,
                    "request body must be a JSON object",
                ));
            }
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
        if (override_token.is_some() || codex_cred.is_some())
            && (name == "authorization" || name == "x-api-key")
        {
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
    match codex_cred.as_ref() {
        Some(codex::CodexCredential::OAuth {
            access_token,
            account_id,
        }) => {
            upstream_req =
                codex::apply_codex_headers(upstream_req, access_token, account_id.as_deref());
        }
        Some(codex::CodexCredential::ApiKey { key }) => {
            upstream_req = upstream_req.bearer_auth(key);
        }
        None => {}
    }
    upstream_req = upstream_req.body(final_body);

    let mut upstream_resp = upstream_req
        .send()
        .await
        .with_context(|| format!("upstream {target_url}"))?;

    // One retry only. A second 401 goes back to the caller unchanged.
    //
    // Known limitation (explicitly out of scope): ChatGPT may return 403,
    // not just 401, for some expired-token cases. That's unverified, and
    // refreshing on a genuine permission error would be worse than not
    // retrying at all, so only 401 is ever treated as "possibly an expired
    // token" here.
    if codex_retry_eligible(provider, upstream_resp.status(), codex_cred.as_ref())
        && let TokenSource::Codex(path) = state.token_source_for(Provider::Codex)
        && let Some(fresh) = codex::refreshed_access_token(path).await
    {
        // Fix round 3, finding I2: reuse the account id captured on the
        // first pass (`codex_cred`) instead of re-reading auth.json here.
        // A transient read failure on this second read would otherwise
        // silently drop the account header from the retried request, and a
        // real backend rejects requests that are missing it.
        let account_id = codex_cred
            .as_ref()
            .and_then(codex::CodexCredential::account_id)
            .map(str::to_string);
        let mut retry = state.client.request(parts.method.clone(), &target_url);
        // Fix round 1, finding 4: forward the same client headers as the
        // first attempt (content-type, accept — including
        // text/event-stream for streaming calls — plus anything else the
        // client sent), minus hop-by-hop/host/content-length and the auth
        // headers we're about to replace with the freshly refreshed token.
        // `apply_codex_headers` only ever sets Authorization, `originator`,
        // and the account-id header — never content-type or accept — so
        // without this a real upstream can reject the retry outright. Kept
        // as its own loop rather than reusing the one above so the
        // Anthropic disguise path stays untouched by this fix.
        for (name, value) in parts.headers.iter() {
            if is_hop_by_hop(name)
                || name == "host"
                || name == "content-length"
                || name == "authorization"
                || name == "x-api-key"
            {
                continue;
            }
            retry = retry.header(name, value);
        }
        let retry = codex::apply_codex_headers(retry, &fresh, account_id.as_deref());
        upstream_resp = retry
            .body(body_bytes.clone())
            .send()
            .await
            .with_context(|| format!("upstream retry {target_url}"))?;
    }

    let status = upstream_resp.status();
    let mut response_headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        response_headers.insert(name.clone(), value.clone());
    }

    if translating && client_wants_stream {
        use futures_util::StreamExt;

        let model = client_model.clone().unwrap_or_default();
        let state = (
            upstream_resp.bytes_stream(),
            String::new(), // partial-frame buffer
            translate::SseTranslator::new(model),
            false, // upstream exhausted?
        );

        let stream = futures_util::stream::unfold(
            state,
            |(mut src, mut buf, mut translator, drained)| async move {
                if drained {
                    return None;
                }
                loop {
                    match src.next().await {
                        Some(Ok(chunk)) => {
                            buf.push_str(&String::from_utf8_lossy(&chunk));
                            let mut out = Vec::new();
                            // SSE frames are separated by a blank line.
                            while let Some(split) = buf.find("\n\n") {
                                let frame: String = buf.drain(..split + 2).collect();
                                let (mut event, mut data) = ("", "");
                                for line in frame.lines() {
                                    if let Some(e) = line.strip_prefix("event:") {
                                        event = e.trim();
                                    }
                                    if let Some(d) = line.strip_prefix("data:") {
                                        data = d.trim();
                                    }
                                }
                                let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data)
                                else {
                                    continue;
                                };
                                for c in translator.push(event, &parsed) {
                                    out.push(format!("data: {c}\n\n"));
                                }
                            }
                            // A chunk can arrive mid-frame and yield nothing;
                            // keep pulling rather than emitting an empty item.
                            if !out.is_empty() {
                                let bytes = axum::body::Bytes::from(out.concat());
                                return Some((
                                    Ok::<_, std::io::Error>(bytes),
                                    (src, buf, translator, false),
                                ));
                            }
                        }
                        // End of stream, or a transport error: close the
                        // conversation cleanly. Headers are already sent, so
                        // the status can no longer change.
                        _ => {
                            let mut tail: Vec<String> = translator
                                .finish()
                                .iter()
                                .map(|c| format!("data: {c}\n\n"))
                                .collect();
                            tail.push("data: [DONE]\n\n".to_string());
                            let bytes = axum::body::Bytes::from(tail.concat());
                            return Some((Ok(bytes), (src, buf, translator, true)));
                        }
                    }
                }
            },
        );

        response_headers.remove("content-length");
        let mut builder = Response::builder().status(status);
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }
        return builder
            .body(Body::from_stream(stream))
            .context("build translated stream");
    }

    if translating && !client_wants_stream {
        let raw = upstream_resp.bytes().await.context("read upstream body")?;
        if !status.is_success() {
            // Pass the upstream's status through, but in a shape an OpenAI
            // client can parse.
            let msg = String::from_utf8_lossy(&raw).to_string();
            return Ok(openai_error_response(status, &msg));
        }
        let mut buf = String::from_utf8_lossy(&raw).into_owned();
        let mut agg = translate::ResponseAggregator::new();
        for (event, data) in translate::split_sse_frames(&mut buf) {
            agg.push(&event, &data);
        }
        let model = client_model.as_deref().unwrap_or("");
        let chat = translate::responses_to_chat(&agg.finish(), model);
        let out = serde_json::to_vec(&chat)?;
        // The upstream said text/event-stream; the client is getting JSON.
        response_headers.remove("content-length");
        response_headers.remove("content-type");
        let mut builder = Response::builder()
            .status(status)
            .header("content-type", "application/json");
        for (name, value) in response_headers.iter() {
            builder = builder.header(name, value);
        }
        return builder
            .body(Body::from(out))
            .context("build translated response");
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

/// Whether the Codex 401 fallback is allowed to attempt a refresh at all.
/// Split out of `forward`'s retry `if` (fix round 3, finding C1) so the
/// credential-ownership rule is unit-testable on its own, without the
/// network and file-system machinery — or the gitignored Codex OAuth
/// implementation — the rest of the retry path needs.
///
/// `codex_cred` must be `Some` of an OAuth credential: it reflects whether
/// *this gateway* attached the credential that got rejected, and only OAuth
/// carries a refresh token to redeem. Without the presence check, a client
/// posting its own bad key to the Codex route would get a 401 here, and the
/// gateway would redeem — and rotate — the user's real, stored refresh
/// token to retry a request that never used the user's credential in the
/// first place. A third party's bad key must never be able to spend or
/// rotate the user's credential. API-key credentials are never eligible
/// either way: there is no refresh token to redeem, so resending the same
/// key cannot succeed.
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

/// True if the inbound request already carries a non-empty client
/// credential via `Authorization` or `x-api-key`. Mirrors the Anthropic
/// path's mode-3 rule (any auth header present → passthrough, don't
/// second-guess a client that authenticates itself) rather than inventing a
/// new notion of "already authenticated" for Codex. Unlike `contains_key`,
/// an empty or whitespace-only header value does not count as a credential.
fn has_client_credential(headers: &HeaderMap) -> bool {
    fn non_empty(v: Option<&axum::http::HeaderValue>) -> bool {
        v.and_then(|v| v.to_str().ok())
            .is_some_and(|s| !s.trim().is_empty())
    }
    non_empty(headers.get(header::AUTHORIZATION)) || non_empty(headers.get("x-api-key"))
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

/// An OpenAI-shaped error body. The Codex route's clients are OpenAI
/// clients, so they parse this shape and nothing else.
fn openai_error_response(status: StatusCode, message: &str) -> Response<Body> {
    let body = serde_json::json!({
        "error": {"message": message, "type": "invalid_request_error"}
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response builds")
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
        // API-key mode resolves to the key, not an OAuth access token.
        std::fs::write(
            &p,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#,
        )
        .unwrap();
        assert_eq!(ts.resolve().unwrap().as_deref(), Some("sk-fake"));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn apikey_upstream_defaults_to_const_and_is_overridable() {
        let s = AppState::new("a", "o", "g", TokenSource::Disabled).unwrap();
        assert_eq!(s.upstream_codex_apikey, DEFAULT_UPSTREAM_CODEX_APIKEY);
        let s2 = s.with_upstream_codex_apikey("http://127.0.0.1:9");
        assert_eq!(s2.upstream_codex_apikey, "http://127.0.0.1:9");
    }

    /// Fix round 3, finding C1 (red against pre-fix code): a client's own
    /// credential getting a 401 on the Codex route must never be eligible
    /// for the refresh-and-retry path. Only a 401 on a credential *this
    /// gateway* attached may trigger it — otherwise a third party's bad key
    /// spends and rotates the user's stored refresh token. This is the pure
    /// guard logic only; the full end-to-end regression (asserting the
    /// token endpoint sees zero hits and auth.json is untouched) lives in
    /// tests/codex.rs, which needs the real Codex OAuth implementation to
    /// be meaningful and so does not run on CI — see that file's
    /// `client_credential_401_does_not_trigger_refresh`.
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

    #[test]
    fn detects_codex_provider() {
        assert_eq!(detect_provider("/v1/responses"), Provider::Codex);
        assert_eq!(
            detect_provider("/v1/responses?stream=true"),
            Provider::Codex
        );
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
    fn codex_translate_path_targets_responses() {
        // A translated request goes to /responses regardless of its inbound path.
        assert_eq!(
            codex_target_path("/codex/v1/chat/completions"),
            "/responses"
        );
        assert_eq!(
            codex_target_path("/codex/v1/chat/completions?stream=true"),
            "/responses"
        );
        // Stage 1's behaviour is unchanged.
        assert_eq!(codex_target_path("/v1/responses"), "/responses");
        assert_eq!(
            codex_target_path("/v1/responses?stream=true"),
            "/responses?stream=true"
        );
    }

    #[test]
    fn upstream_for_resolves_codex() {
        let s = AppState::new(
            "https://a.test",
            "https://o.test",
            "https://g.test",
            TokenSource::Disabled,
        )
        .unwrap();
        assert_eq!(s.upstream_for(Provider::Codex), DEFAULT_UPSTREAM_CODEX);
    }

    #[test]
    fn token_source_for_picks_per_provider() {
        let s = AppState::new(
            "https://a.test",
            "https://o.test",
            "https://g.test",
            TokenSource::Disabled,
        )
        .unwrap()
        .with_token_source_codex(TokenSource::Static(Arc::new("codex-tok".to_string())));
        // Anthropic keeps the global source; Codex gets its own.
        assert!(matches!(
            s.token_source_for(Provider::Anthropic),
            TokenSource::Disabled
        ));
        assert_eq!(
            s.token_source_for(Provider::Codex)
                .resolve()
                .unwrap()
                .as_deref(),
            Some("codex-tok")
        );
    }
}
