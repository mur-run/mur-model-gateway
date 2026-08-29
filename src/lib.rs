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

pub mod auth_probe;
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
///
/// No `#[derive(Debug)]`: `Static` carries a raw token, and a derived impl
/// would put it in any `{:?}`, tracing capture, or panic message. "Test
/// injection point" is a doc comment, not an invariant the type system
/// enforces — nothing stops production code from constructing this variant
/// — so it gets the same hand-written, redacting `Debug` the crate already
/// gives `CodexAuth`/`CodexCredential` (see `src/codex.rs`).
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
    /// Read `~/.codex/auth.json` and dispatch on `auth_mode`. Re-read on
    /// every request. Yields the OAuth access token in `chatgpt` mode;
    /// yields nothing in `apikey` mode, since this generic resolver feeds
    /// the Anthropic disguise path and an OpenAI API key must never be
    /// offered there.
    Codex(std::path::PathBuf),
    /// Always return this token. Used by integration tests.
    Static(Arc<String>),
    /// Never disguise; act as a pure passthrough proxy.
    Disabled,
}

impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenSource::Keychain => write!(f, "Keychain"),
            TokenSource::EnvVar(name) => f.debug_tuple("EnvVar").field(name).finish(),
            TokenSource::CredentialsFile(path) => {
                f.debug_tuple("CredentialsFile").field(path).finish()
            }
            TokenSource::Codex(path) => f.debug_tuple("Codex").field(path).finish(),
            // The one variant that carries a raw secret.
            TokenSource::Static(_) => f.debug_tuple("Static").field(&"<redacted>").finish(),
            TokenSource::Disabled => write!(f, "Disabled"),
        }
    }
}

impl TokenSource {
    /// Resolve this source's underlying OAuth credential — access token plus
    /// its expiry, when the source can supply one. The single shared
    /// fallback path: `resolve()` below, `anthropic_credential_expiry`, and
    /// `auth_probe::refresh_via_owner` all go through this rather than each
    /// re-deriving the Keychain→credentials-file fallback in its own way.
    ///
    /// Fix round 1, CRITICAL 1: before this method existed, that fallback
    /// was implemented once here (inline in `resolve()`) and independently
    /// reimplemented — Keychain-only, no fallback — at two other call sites.
    /// `TokenSource::Keychain` is the *production default*, so on every
    /// default Linux/Windows install (no OS keychain backend, credential
    /// lives in `~/.claude/.credentials.json`) both of those call sites
    /// silently saw `None` and the delegated-refresh probe never fired — on
    /// exactly the platform pairing the feature exists to fix. Routing every
    /// caller through one method is what stops a fourth instance of the same
    /// bug: there is now nowhere else to reimplement it.
    ///
    /// `Ok(Some)` for `Keychain`/`CredentialsFile` when a credential blob is
    /// found; `Ok(None)` for every other variant — they have no stored
    /// expiry to report (a raw token/key has none).
    pub fn resolve_credential(
        &self,
    ) -> Result<Option<keychain::OauthCredential>, keychain::KeychainError> {
        match self {
            TokenSource::Keychain => keychain_fallback(
                cfg!(target_os = "macos"),
                keychain::read_claude_code_credential(),
                keychain::default_credentials_path(),
            ),
            TokenSource::CredentialsFile(path) => keychain::read_credentials_file_credential(path),
            TokenSource::Codex(_) | TokenSource::EnvVar(_) | TokenSource::Static(_) => Ok(None),
            TokenSource::Disabled => Ok(None),
        }
    }

    /// `Ok(Some)` → token available, disguise applies.
    /// `Ok(None)` → no token, passthrough.
    /// `Err` → backend error, passthrough with a warning logged by the caller.
    pub fn resolve(&self) -> Result<Option<String>, keychain::KeychainError> {
        match self {
            TokenSource::Keychain | TokenSource::CredentialsFile(_) => {
                Ok(self.resolve_credential()?.map(|c| c.access_token))
            }
            TokenSource::Codex(path) => Ok(codex::read_credential(path).and_then(|c| match c {
                // This generic resolver also feeds the Anthropic disguise
                // path (global `MUR_MODEL_GATEWAY_TOKEN_SOURCE=codex`); an
                // OpenAI API key must never be offered as an Anthropic
                // credential, so ApiKey mode resolves to no token here.
                codex::CodexCredential::OAuth { access_token, .. } => Some(access_token),
                codex::CodexCredential::ApiKey { .. } => None,
            })),
            TokenSource::EnvVar(name) => Ok(std::env::var(name).ok()),
            TokenSource::Static(s) => Ok(Some(s.as_ref().clone())),
            TokenSource::Disabled => Ok(None),
        }
    }
}

/// Applies the Keychain→credentials-file fallback policy to an
/// already-attempted keychain read. Non-macOS Claude Code installs usually
/// skip the OS keychain and write `~/.claude/.credentials.json` instead;
/// on macOS the keychain result is authoritative, full stop.
///
/// Takes `is_macos` as a parameter rather than reading `cfg!(target_os =
/// "macos")` internally so the non-macOS branch is unit-testable from any
/// dev machine. `cfg!(...)` (unlike `#[cfg(...)]`) is not conditional
/// compilation — it bakes a fixed bool into the binary for whichever
/// platform it's built on, but both branches of code around it are always
/// compiled in, on every platform. Without this parameter, only the host
/// platform's branch could ever execute under `cargo test` here.
fn keychain_fallback(
    is_macos: bool,
    from_keychain: Result<Option<keychain::OauthCredential>, keychain::KeychainError>,
    fallback_path: Option<std::path::PathBuf>,
) -> Result<Option<keychain::OauthCredential>, keychain::KeychainError> {
    if is_macos {
        return from_keychain;
    }
    match from_keychain {
        Ok(Some(c)) => Ok(Some(c)),
        keychain_miss => match fallback_path {
            Some(p) if p.exists() => keychain::read_credentials_file_credential(&p),
            _ => keychain_miss,
        },
    }
}

/// Opt-out for the delegated-refresh probe. Set to `1` to stop the gateway
/// from arming the probe at startup, so an Anthropic 401 never spawns
/// `claude auth status` and is never retried.
///
/// This does NOT change what the client receives on that 401: the
/// actionable error body (naming the credential store and how to fix it) is
/// swapped in either way, because that swap depends only on the request
/// being Anthropic + 401 + claude-owned — never on `auth_probe`. With the
/// kill switch set, the client still gets the actionable body, just without
/// a retry ever having been attempted first.
pub const PROBE_KILL_SWITCH_ENV: &str = "MUR_MODEL_GATEWAY_NO_AUTH_PROBE";

/// How the gateway asks the credential's owner to refresh it.
///
/// `Disabled` by default in every constructor — same discipline as
/// `AppState::token_source_codex`. Enabling spawns a real binary that can
/// rewrite the user's Claude Code credential, so a test-side `AppState` must
/// be structurally unable to reach it rather than merely unlikely to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthProbe {
    Disabled,
    /// Absolute path to the `claude` binary, resolved once at startup. Stored
    /// resolved (not looked up per call) so a later PATH change cannot swap
    /// which binary a long-running gateway executes.
    Command(std::path::PathBuf),
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
    /// See [`AuthProbe`]. `Disabled` by default in every constructor; only
    /// `with_default_auth_probe` (called from `main.rs`) can arm it.
    pub auth_probe: AuthProbe,
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
            // Structurally safe by default — see the AuthProbe doc and
            // with_default_auth_probe (the only enabling path, main.rs-only).
            auth_probe: AuthProbe::Disabled,
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

    /// Point the auth probe at the `claude` binary on PATH. Call from
    /// `main.rs` only — see the `AuthProbe` doc. A no-op when the kill switch
    /// is set or `claude` is not installed (a transplanted credential is a
    /// supported setup: a working token with no owner CLI).
    pub fn with_default_auth_probe(mut self) -> Self {
        if std::env::var(PROBE_KILL_SWITCH_ENV).is_ok_and(|v| v == "1") {
            return self;
        }
        match which_claude() {
            Some(p) => self.auth_probe = AuthProbe::Command(p),
            // Say so. A probe that cannot be armed makes the whole delegated
            // refresh inert — every expired-token 401 goes back to the caller
            // exactly as it did before this feature existed — and the only
            // other clue is `cc_version`'s unrelated warning about the same
            // missing binary. Observed in production: a launchd service whose
            // PATH was `/usr/bin:/bin:/usr/sbin:/sbin` could not see `claude`
            // in `~/.local/bin`, so the feature shipped and did nothing.
            None => tracing::warn!(
                path = %std::env::var("PATH").unwrap_or_default(),
                "`claude` not found on PATH — delegated refresh is disabled; \
                 an expired Anthropic token will return its 401 unchanged"
            ),
        }
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

/// Resolve `claude` on PATH to an absolute path. Deliberately not `which`:
/// one small lookup does not earn a dependency.
///
/// I2: checks more than `.is_file()` — see [`is_executable_file`]. A stale
/// or permission-stripped `claude` left on PATH would otherwise arm the
/// probe with a binary that can never run, turning every Anthropic 401 into
/// one guaranteed-failing spawn before the real error body.
fn which_claude() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    // Windows resolves a bare command name through PATHEXT; unix does not.
    let pathext = if cfg!(windows) {
        Some(std::env::var("PATHEXT").unwrap_or_else(|_| DEFAULT_PATHEXT.to_string()))
    } else {
        None
    };
    let names = claude_names(pathext.as_deref());
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|c| is_executable_file(c))
}

/// The PATHEXT Windows itself falls back to when the variable is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// Candidate file names for the CLI within one PATH directory, in search
/// order. `None` means "no extension search" (unix).
///
/// Without this the Windows lookup could never succeed: npm installs the CLI
/// as `claude.cmd`, a native build is `claude.exe`, and neither matches a
/// bare `claude` join. Delegated refresh was inert on every Windows install —
/// visible only as the "`claude` not found on PATH" warning.
///
/// Takes `pathext` as a parameter rather than reading the env internally,
/// mirroring [`anthropic_auth_error_body`]'s `is_macos`, so the Windows
/// branch is provable by a unit test on any dev machine and not only on CI's
/// Windows leg.
///
/// The extension-less name goes last: npm also drops a `claude` shell script
/// beside the shim, which `CreateProcess` cannot run, so it must not shadow
/// the `.cmd` in the same directory.
fn claude_names(pathext: Option<&str>) -> Vec<String> {
    let Some(pathext) = pathext else {
        return vec!["claude".to_string()];
    };
    pathext
        .split(';')
        .filter(|e| e.starts_with('.'))
        .map(|e| format!("claude{}", e.to_ascii_lowercase()))
        .chain(std::iter::once("claude".to_string()))
        .collect()
}

/// `.is_file()` plus, on unix, the executable bit (`mode & 0o111`). A file
/// that exists but isn't executable (`644`, or stripped by a broken
/// install) would fail every spawn attempt — plain `.is_file()` can't tell
/// that apart from a real, runnable binary.
#[cfg(unix)]
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable_file(path: &std::path::Path) -> bool {
    path.is_file()
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
                TokenSource::Codex(path) => {
                    let cred = codex::read_credential(path);
                    if cred.is_none() {
                        tracing::warn!(
                            path = %path.display(),
                            "codex credential file unreadable or invalid, passing through"
                        );
                    }
                    cred
                }
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

    // Anthropic: the owner CLI holds the refresh token, so ask it to refresh
    // rather than redeeming the token here. One retry only, same as Codex.
    //
    // `override_token.is_some()` is an extra guard beyond
    // `anthropic_retry_eligible`: the predicate only inspects the
    // *configured* token source, not whether this request actually used it.
    // Mode 3 (a client-supplied, non-OAuth-shape credential) leaves
    // `override_token` `None` even when a claude-owned TokenSource is
    // configured. Same principle as `codex_cred.as_ref()` being `Some`
    // above — eligibility requires proof *this gateway* attached the
    // credential that got rejected, so a third party's bad key can never
    // trigger a probe or spend the user's real refresh token.
    let anthropic_source = state.token_source_for(Provider::Anthropic);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        // Fail-safe, not a silently-swallowed bug: this only underflows if
        // the host clock reads before the Unix epoch, which no supported
        // deployment target allows. Falling back to 0 makes the eligibility
        // comparison in `anthropic_retry_eligible` fail closed (an
        // impossibly-old "now" can't be past any stored expiry, so the
        // credential reads as "not yet expired") instead of panicking the
        // request.
        .unwrap_or(0);
    // Fix round 1, IMPORTANT 4: `anthropic_credential_expiry` below can hit
    // the OS keychain or the filesystem, so — unlike `now_ms` above, a plain
    // clock read — it must not run on every proxied request. Cheap conjuncts
    // first: `override_token.is_some()` already implies `provider ==
    // Provider::Anthropic` (see the disguise-gate comment further up —
    // override_token is only ever set on that path), so it doubles as the
    // cheap provider check; `claude_owned` is a plain `matches!` on a
    // reference already in hand. Only once both hold — plus the status code,
    // also free — does `anthropic_expiry` run the actual credential-store
    // read.
    //
    // Task 5: hoisted out of the retry `if`'s own conjunct chain (it used to
    // be an inline `&& let anthropic_expiry = …` there) so the SAME read
    // backs both the retry-eligibility check below and the client-facing
    // error wording further down if the 401 survives the retry. A second,
    // independent read at the error site would repeat the exact mistake
    // `anthropic_credential_expiry`'s own doc comment describes — this plan
    // has already hardcoded or reimplemented that read wrongly three times.
    // `Option<Option<i64>>`: the outer `None` means the cheap conjuncts
    // below didn't hold (this 401 isn't for a credential this gateway
    // attached, so there's nothing here for the error path to explain
    // either); `Some` means they did, and the inner `Option<i64>` is the
    // stored expiry, or `None` if the blob doesn't carry one.
    let claude_owned = matches!(
        anthropic_source,
        TokenSource::Keychain | TokenSource::CredentialsFile(_)
    );
    let anthropic_expiry: Option<Option<i64>> = if override_token.is_some()
        && claude_owned
        && upstream_resp.status() == reqwest::StatusCode::UNAUTHORIZED
    {
        Some(anthropic_credential_expiry(anthropic_source))
    } else {
        None
    };
    if let Some(expiry) = anthropic_expiry
        && anthropic_retry_eligible(
            provider,
            upstream_resp.status(),
            anthropic_source,
            expiry,
            now_ms,
        )
        && auth_probe::refresh_via_owner(&state.auth_probe, anthropic_source, expiry).await
            == auth_probe::ProbeOutcome::Refreshed
        && let Ok(Some(fresh)) = anthropic_source.resolve()
    {
        let has_anthropic_version = parts.headers.contains_key("anthropic-version");
        let retry_body: Vec<u8> = if let Some(ver) = cc_version.as_deref() {
            disguise::inject_billing_prefix(&body_bytes, ver)?
        } else {
            body_bytes.to_vec()
        };
        let mut retry = state.client.request(parts.method.clone(), &target_url);
        // Mirrors the Codex retry arm immediately above: forward the same
        // client headers as the first attempt, minus hop-by-hop/host/
        // content-length and the auth headers we're about to replace.
        // `anthropic-beta` is skipped too, same reason it was
        // captured-and-skipped on the first attempt: `apply_disguise_headers`
        // below re-adds the merged value, and forwarding the client's raw
        // header as well would send the upstream two `anthropic-beta`
        // headers instead of one.
        for (name, value) in parts.headers.iter() {
            if is_hop_by_hop(name)
                || name == "host"
                || name == "content-length"
                || name == "authorization"
                || name == "x-api-key"
                || name == "anthropic-beta"
            {
                continue;
            }
            retry = retry.header(name, value);
        }
        retry =
            disguise::apply_disguise_headers(retry, &fresh, &client_betas, has_anthropic_version)?;
        upstream_resp = retry
            .body(retry_body)
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

    // Task 5: the retry arm above tried what it could. If the client is
    // still looking at a 401 for a credential *this gateway* attached
    // (`anthropic_expiry` is `Some` only when the cheap conjuncts held for
    // the first attempt — see the comment where it's computed above), swap
    // upstream's opaque "re-authenticate" body for one that names the
    // credential and where it lives, instead of leaving the reader to guess.
    //
    // Rechecking `status` here, rather than reusing `anthropic_retry_eligible`'s
    // verdict, is what makes this fire for the revoked-but-not-expired case
    // too: eligibility was false there and no retry was ever attempted, but
    // the client is still owed an explanation. It's also what makes this
    // skip cleanly once a retry actually repairs the credential: `status` is
    // the final response's status (the retry's, if one was sent), so it is
    // no longer 401 by this point.
    //
    // `expired` reads the expiry captured before the retry ran — the
    // question it answers is "did the original 401 look like an expiry
    // problem", which is what decided whether a refresh was even attempted,
    // not a fresh re-read (the probe above may just have rewritten it).
    if let Some(expiry) = anthropic_expiry
        && status == StatusCode::UNAUTHORIZED
    {
        let expired = expiry.is_none_or(|exp| exp <= now_ms);
        return Ok(anthropic_auth_error_response(anthropic_source, expired));
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
                            // Same splitter the non-streaming path uses: it
                            // tolerates CRLF, which a hand-rolled `find("\n\n")`
                            // does not.
                            for (event, data) in translate::split_sse_frames(&mut buf) {
                                for c in translator.push(&event, &data) {
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

/// Whether an Anthropic 401 is worth a delegated refresh.
///
/// A 401 with the stored expiry still in the future means the credential was
/// revoked upstream, not that it aged out — a refresh cannot fix that, so the
/// gateway must not spawn a probe for it. A blob with no expiry is allowed
/// through once; the probe's own cooldown bounds the cost if it is fruitless.
pub fn anthropic_retry_eligible(
    provider: Provider,
    status: reqwest::StatusCode,
    source: &TokenSource,
    expires_at_ms: Option<i64>,
    now_ms: i64,
) -> bool {
    // Only a store Claude Code owns can be repaired by asking Claude Code to
    // refresh. A raw key from the environment is rejected on its own merits.
    let claude_owned = matches!(
        source,
        TokenSource::Keychain | TokenSource::CredentialsFile(_)
    );
    provider == Provider::Anthropic
        && status == reqwest::StatusCode::UNAUTHORIZED
        && claude_owned
        && expires_at_ms.is_none_or(|exp| exp <= now_ms)
}

/// The stored expiry for Anthropic, read from **the same source the token came
/// from**, via [`TokenSource::resolve_credential`] — the single shared
/// fallback path (fix round 1, CRITICAL 1; see that method's doc for why a
/// second, independent implementation here was the bug). Reading the
/// keychain unconditionally, with no fallback, would be wrong on every Linux
/// and Windows install, where Claude Code writes
/// `~/.claude/.credentials.json` and there may be no keychain backend to
/// read — the expiry would come back unknown on every request and the probe
/// would fire on every 401.
fn anthropic_credential_expiry(source: &TokenSource) -> Option<i64> {
    source
        .resolve_credential()
        .ok()
        .flatten()
        .and_then(|c| c.expires_at_ms)
}

/// A 401 the caller can act on. The upstream body names no location, which is
/// what left users guessing which credential had gone stale.
///
/// The store is derived from the source the token actually came from — NOT a
/// hardcoded keychain string. On a Linux or Windows install the credential
/// lives in a file, and an error that confidently names the wrong store sends
/// the reader to a place that does not exist. This is the same mistake the
/// expiry read made in three separate places earlier in this plan; do not
/// reintroduce it here.
pub fn anthropic_auth_error_body(source: &TokenSource, expired: bool) -> String {
    let what = if expired {
        "Anthropic OAuth expired and an automatic refresh did not resolve it"
    } else {
        "Anthropic OAuth was revoked (the stored credential has not aged out)"
    };
    // I1: only read the keychain when `source` is actually `Keychain` — for
    // `CredentialsFile`/`Codex`/etc. this would be a pointless (and on some
    // Linux setups, dialog-prompting) read of a store the credential never
    // came from. Cache-backed (`keychain::CACHE_TTL`): `forward()` already
    // called `anthropic_credential_expiry` moments earlier on this same
    // request, so this is a cache hit, not a second live keychain round trip.
    let from_keychain = if matches!(source, TokenSource::Keychain) {
        keychain::read_claude_code_credential()
    } else {
        Ok(None)
    };
    format!(
        "{what} — credential: {}. \
         Fix: run `/login anthropic` in murmur, or `claude auth login`.",
        describe_credential_store(source, cfg!(target_os = "macos"), &from_keychain)
    )
}

/// Where a reader should go to look at the credential, in words. Never prints
/// the credential itself — `TokenSource`'s own `Debug` redacts, and this must
/// not become a way around that.
///
/// `is_macos` is a parameter rather than a `cfg!(target_os = "macos")` read
/// inside this function, same reason `keychain_fallback` above takes one (see
/// its doc comment): it makes the non-macOS branch unit-testable from a
/// macOS dev machine. Naming the keychain for an install that has none is
/// this plan's most-repeated mistake — the fourth occurrence, per the Task 5
/// brief — and without the parameter that branch could only ever be proven
/// by CI's Linux/Windows runners, never locally.
///
/// I1: `from_keychain` is the caller's own keychain read for this same
/// request (`Ok(None)` when `source` isn't `Keychain` at all). Before this
/// parameter existed, the `TokenSource::Keychain` branch named the file on
/// every non-macOS host unconditionally — but `keychain_fallback` (what
/// `resolve_credential` actually calls) prefers the keychain result whenever
/// it resolved to `Ok(Some(_))`, platform aside. So a non-macOS install with
/// a working keychain backend got a *wrong* answer: told to look at a file
/// that isn't where its token came from. This does not fully re-run
/// `keychain_fallback`'s policy (it still doesn't check whether the fallback
/// file exists before naming it) — just enough to fix the one case where the
/// two disagreed. A full provenance refactor is an explicitly out-of-scope
/// follow-up.
fn describe_credential_store(
    source: &TokenSource,
    is_macos: bool,
    from_keychain: &Result<Option<keychain::OauthCredential>, keychain::KeychainError>,
) -> String {
    match source {
        TokenSource::Keychain => {
            if is_macos || matches!(from_keychain, Ok(Some(_))) {
                "keychain \"Claude Code-credentials\"".to_string()
            } else {
                // The non-macOS fallback `resolve_credential` uses.
                keychain::default_credentials_path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "~/.claude/.credentials.json".to_string())
            }
        }
        TokenSource::CredentialsFile(p) => p.display().to_string(),
        other => format!("{other:?}"),
    }
}

/// The 401 returned to the client when the retry arm above did not repair an
/// Anthropic credential this gateway attached. A fresh response, not
/// upstream's headers with a swapped body — same shape as
/// [`openai_error_response`] below: the message is plain text, not
/// upstream's original content-type, and nothing else about the upstream
/// response is worth preserving (its body doesn't survive either).
fn anthropic_auth_error_response(source: &TokenSource, expired: bool) -> Response<Body> {
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Body::from(anthropic_auth_error_body(source, expired)))
        .expect("static error response builds")
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

    /// Fix round 1, IMPORTANT 3: `TokenSource::Static` carries a raw token —
    /// the hand-written `Debug` impl above must redact it, matching
    /// `CodexAuth`/`CodexCredential`'s existing redaction tests in
    /// `src/codex.rs` (`debug_redacts_tokens`,
    /// `codex_credential_debug_redacts_secrets`). Non-secret variants are
    /// asserted the other way: `Static`'s payload is the only thing this
    /// type must never leak, not a blanket "hide everything."
    #[test]
    fn token_source_debug_redacts_only_the_secret_variant() {
        let static_dbg = format!(
            "{:?}",
            TokenSource::Static(Arc::new("sk-ant-oat01-realsecret".to_string()))
        );
        assert!(
            !static_dbg.contains("sk-ant-oat01-realsecret"),
            "Static's raw token must never appear in Debug output: {static_dbg}"
        );
        assert!(static_dbg.contains("<redacted>"));

        // Non-secret variants keep showing their payload — an env-var name
        // is not a credential, and losing it from logs/panics would make
        // this type harder to debug for no security benefit.
        let env_dbg = format!("{:?}", TokenSource::EnvVar("ANTHROPIC_API_KEY".to_string()));
        assert!(env_dbg.contains("ANTHROPIC_API_KEY"));

        assert_eq!(format!("{:?}", TokenSource::Keychain), "Keychain");
        assert_eq!(format!("{:?}", TokenSource::Disabled), "Disabled");
    }

    /// Fix round 1, CRITICAL 1 (pure half — see
    /// `auth_probe::tests::keychain_default_falls_back_to_credentials_file_on_non_macos`
    /// for the `ProbeOutcome`-level regression this backs): pins
    /// `keychain_fallback`'s contract in isolation. On a non-macOS host, a
    /// keychain miss — `Ok(None)` (no entry) or `Err` (no backend at all)
    /// alike — falls back to the credentials-file path when one exists
    /// there, instead of propagating the miss the way the pre-fix,
    /// independently-reimplemented reads at the two call sites did.
    #[test]
    fn keychain_fallback_uses_the_credentials_file_when_keychain_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"from-file","expiresAt":42}}"#,
        )
        .unwrap();

        let err = keychain::KeychainError::Backend("no keychain backend available".into());
        let result = keychain_fallback(false, Err(err), Some(path)).unwrap();
        assert_eq!(
            result.map(|c| c.access_token),
            Some("from-file".to_string())
        );
    }

    /// The macOS-branch counterpart: the keychain result is authoritative
    /// and the fallback path is never consulted, even when a fixture file
    /// exists at it — a stale leftover file must not shadow a live macOS
    /// keychain answer of `Ok(None)` (Claude Code genuinely never logged in
    /// on this machine).
    #[test]
    fn keychain_fallback_does_not_consult_the_file_on_macos() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds.json");
        std::fs::write(
            &path,
            r#"{"claudeAiOauth":{"accessToken":"must-not-be-used","expiresAt":42}}"#,
        )
        .unwrap();

        let result = keychain_fallback(true, Ok(None), Some(path)).unwrap();
        assert_eq!(result, None);
    }

    /// Task 5, the plan's fourth brush with this exact bug (per the brief):
    /// an error that confidently names the keychain on an install that has
    /// none sends the reader somewhere that doesn't exist. `is_macos` is
    /// explicit (see `describe_credential_store`'s own doc comment)
    /// precisely so both branches are provable here, on whatever OS happens
    /// to run `cargo test`, rather than only by CI's Linux/Windows legs.
    #[test]
    fn describe_credential_store_names_the_keychain_only_on_macos() {
        let macos = describe_credential_store(&TokenSource::Keychain, true, &Ok(None));
        assert!(
            macos.contains("Claude Code-credentials"),
            "macOS install: {macos}"
        );

        // `Err`, not `Ok(None)`: a non-macOS host with no keychain backend at
        // all — the case this test is named for. `Ok(Some(_))` is covered
        // separately below (I1: that case must name the keychain even here).
        let no_backend = Err(keychain::KeychainError::Backend(
            "no keychain backend available".into(),
        ));
        let other = describe_credential_store(&TokenSource::Keychain, false, &no_backend);
        assert!(
            !other.contains("Claude Code-credentials"),
            "non-macOS install must not name a keychain it doesn't have: {other}"
        );
        assert!(
            other.contains(".claude") && other.contains("credentials"),
            "non-macOS install must name the credentials-file path instead: {other}"
        );
    }

    /// I1: `describe_credential_store` must mirror `keychain_fallback`'s
    /// actual decision, not just platform. `keychain_fallback` prefers the
    /// keychain result whenever it resolved to `Ok(Some(_))`, regardless of
    /// `is_macos` — so a non-macOS install with a *working* keychain backend
    /// must be told to look at the keychain, not unconditionally pointed at
    /// the fallback file it never used. Would fail against the old
    /// `is_macos`-only branch, which named the file here unconditionally.
    #[test]
    fn describe_credential_store_prefers_a_resolved_keychain_even_off_macos() {
        let from_keychain = Ok(Some(keychain::OauthCredential {
            access_token: "sk-ant-oat01-test".to_string(),
            expires_at_ms: Some(1_000),
        }));
        let d = describe_credential_store(&TokenSource::Keychain, false, &from_keychain);
        assert!(
            d.contains("Claude Code-credentials"),
            "a resolved non-macOS keychain read must be named, not the fallback file: {d}"
        );
    }

    #[test]
    fn describe_credential_store_names_the_configured_file_path() {
        let d = describe_credential_store(
            &TokenSource::CredentialsFile("/home/u/.claude/.credentials.json".into()),
            true,      // even a macOS host must defer to the configured file, not the keychain
            &Ok(None), // irrelevant: the CredentialsFile branch never reads it
        );
        assert_eq!(d, "/home/u/.claude/.credentials.json");
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
        // API-key mode must resolve to no token: this generic resolver also
        // feeds the Anthropic disguise path (global
        // MUR_MODEL_GATEWAY_TOKEN_SOURCE=codex), and an OpenAI API key must
        // never be sent to api.anthropic.com as a Claude credential.
        std::fs::write(
            &p,
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-fake","tokens":null}"#,
        )
        .unwrap();
        assert_eq!(ts.resolve().unwrap(), None);
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

    #[test]
    fn auth_probe_is_disabled_in_every_constructor() {
        // Mirrors token_source_codex: a test-side AppState must be unable to
        // spawn the real `claude`, not merely unlikely to. Both constructors
        // are checked — the safety property is that NO path out of this impl
        // block leaves the probe armed, so testing only `new` would let
        // `with_version` regress silently.
        let by_new = AppState::new(
            "https://a.test",
            "https://o.test",
            "https://g.test",
            TokenSource::Disabled,
        )
        .unwrap();
        assert_eq!(by_new.auth_probe, AuthProbe::Disabled, "AppState::new");

        let by_version = AppState::with_version(
            "https://a.test",
            "https://o.test",
            "https://g.test",
            TokenSource::Disabled,
            Arc::new(cc_version::VersionCache::detect_or_fallback()),
        )
        .unwrap();
        assert_eq!(
            by_version.auth_probe,
            AuthProbe::Disabled,
            "AppState::with_version"
        );
    }

    #[test]
    fn kill_switch_keeps_the_probe_disabled() {
        // with_default_auth_probe is the only enabling path, and it must
        // honour the opt-out even when `claude` genuinely resolves on PATH —
        // not just when this machine happens to lack a `claude` install
        // (which would make the assertion vacuous: which_claude() returns
        // None either way, kill switch or not). Fixture: a file named
        // `claude` in a temp dir, prepended onto PATH via
        // std::env::join_paths. Chmod +x on unix (I2: which_claude now checks
        // the executable bit there, not just is_file()) — without it this
        // fixture stops resolving at all and the control below would fail
        // for the wrong reason.
        let dir = tempfile::tempdir().unwrap();
        let fake_claude = dir.path().join("claude");
        std::fs::write(&fake_claude, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let ambient_path = std::env::var_os("PATH").unwrap_or_default();
        let path_with_fake = std::env::join_paths(
            std::iter::once(dir.path().to_path_buf()).chain(std::env::split_paths(&ambient_path)),
        )
        .unwrap();

        temp_env::with_var("PATH", Some(path_with_fake), || {
            temp_env::with_var(PROBE_KILL_SWITCH_ENV, Some("1"), || {
                let s = AppState::new(
                    "https://a.test",
                    "https://o.test",
                    "https://g.test",
                    TokenSource::Disabled,
                )
                .unwrap()
                .with_default_auth_probe();
                assert_eq!(
                    s.auth_probe,
                    AuthProbe::Disabled,
                    "kill switch: expected Disabled even with a resolvable `claude` on PATH"
                );
            });

            // Negative control: same PATH, kill switch unset. This is what
            // proves the fixture above actually works — if this assertion
            // fails, the fixture never put a resolvable `claude` on PATH and
            // the Disabled assertion above proved nothing.
            temp_env::with_var_unset(PROBE_KILL_SWITCH_ENV, || {
                let s = AppState::new(
                    "https://a.test",
                    "https://o.test",
                    "https://g.test",
                    TokenSource::Disabled,
                )
                .unwrap()
                .with_default_auth_probe();
                assert_eq!(
                    s.auth_probe,
                    AuthProbe::Command(fake_claude.clone()),
                    "control: fake `claude` on PATH did not arm the probe — PATH fixture is broken, so the Disabled assertion above proved nothing"
                );
            });
        });
    }

    /// I2: `which_claude` used to accept any `is_file()` match regardless of
    /// permissions — a stale or permission-stripped `claude` left earlier on
    /// PATH would shadow a perfectly good one later on PATH, silently arming
    /// the probe with a binary that fails every spawn. Two PATH entries, in
    /// order: a non-executable `claude` first, an executable one second —
    /// proving this is a skip-and-continue past the bad entry, not merely
    /// "reject a lone bad file" (which a stricter-but-still-wrong early
    /// return could also satisfy).
    #[cfg(unix)]
    #[test]
    fn which_claude_skips_a_non_executable_file_in_favor_of_an_executable_one() {
        use std::os::unix::fs::PermissionsExt;

        let dir_a = tempfile::tempdir().unwrap();
        let unusable = dir_a.path().join("claude");
        std::fs::write(&unusable, "").unwrap();
        std::fs::set_permissions(&unusable, std::fs::Permissions::from_mode(0o644)).unwrap();

        let dir_b = tempfile::tempdir().unwrap();
        let usable = dir_b.path().join("claude");
        std::fs::write(&usable, "").unwrap();
        std::fs::set_permissions(&usable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([dir_a.path(), dir_b.path()]).unwrap();
        temp_env::with_var("PATH", Some(path), || {
            assert_eq!(
                which_claude(),
                Some(usable.clone()),
                "a non-executable claude earlier on PATH must be skipped in \
                 favor of an executable one later on PATH — not returned, and \
                 not treated as though PATH had no claude at all"
            );
        });
    }

    /// The Windows lookup used to be impossible to satisfy: `which_claude`
    /// joined a bare `claude`, but npm installs the CLI as `claude.cmd` and a
    /// native build is `claude.exe`. Delegated refresh was inert on every
    /// Windows install. Runs on every platform because `claude_names` takes
    /// PATHEXT as an argument.
    #[test]
    fn claude_names_searches_pathext_on_windows_and_nothing_on_unix() {
        assert_eq!(
            claude_names(None),
            ["claude"],
            "unix resolves a bare command name with no extension search"
        );

        assert_eq!(
            claude_names(Some(DEFAULT_PATHEXT)),
            [
                "claude.com",
                "claude.exe",
                "claude.bat",
                "claude.cmd",
                "claude"
            ],
            "PATHEXT order is the resolution order, lowercased to match the \
             installed shim; the extension-less name goes LAST so npm's \
             unrunnable `claude` shell script cannot shadow `claude.cmd` in \
             the same directory"
        );

        assert_eq!(
            claude_names(Some(".EXE;;junk;.CMD")),
            ["claude.exe", "claude.cmd", "claude"],
            "empty and non-dot PATHEXT entries are skipped rather than \
             producing a `claudejunk` candidate"
        );
    }
}
