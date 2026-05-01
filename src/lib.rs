//! cc-proxy — local Anthropic API reverse proxy.
//!
//! Iter 0: pure byte-passthrough. Forwards every request unchanged
//! to the configured upstream and streams the response back.
//! No header rewriting, no auth handling. Validates that env-var
//! routing through a local proxy works end-to-end.

use anyhow::Context;
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderName, Response, StatusCode, Uri},
    response::IntoResponse,
    routing::any,
};
use std::time::Duration;

pub const DEFAULT_BIND: &str = "127.0.0.1:8088";
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";
pub const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct AppState {
    pub upstream: String,
    pub client: reqwest::Client,
}

impl AppState {
    pub fn new(upstream: impl Into<String>) -> anyhow::Result<Self> {
        Ok(Self {
            upstream: upstream.into().trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(UPSTREAM_TIMEOUT)
                .build()
                .context("reqwest client")?,
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
    let target_url = format!("{}{}", state.upstream, path_and_query);
    let _: Uri = target_url.parse().context("target uri parse")?;

    let body_bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .context("read incoming body")?;

    let mut upstream_req = state.client.request(parts.method.clone(), &target_url);
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) || name == "host" || name == "content-length" {
            continue;
        }
        upstream_req = upstream_req.header(name, value);
    }
    upstream_req = upstream_req.body(body_bytes);

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
        let s = AppState::new("https://api.anthropic.com/").unwrap();
        assert_eq!(s.upstream, "https://api.anthropic.com");
    }
}
