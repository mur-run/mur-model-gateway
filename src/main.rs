use anyhow::Context;
use cc_proxy::{AppState, DEFAULT_BIND, DEFAULT_UPSTREAM, TokenSource, build_router};
use std::net::SocketAddr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,cc_proxy=debug")),
        )
        .init();

    let bind: SocketAddr = std::env::var("CC_PROXY_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()
        .context("invalid CC_PROXY_BIND")?;
    let upstream =
        std::env::var("CC_PROXY_UPSTREAM").unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string());

    let token_source = match std::env::var("CC_PROXY_DISGUISE")
        .unwrap_or_else(|_| "keychain".to_string())
        .as_str()
    {
        "off" | "disabled" => TokenSource::Disabled,
        _ => TokenSource::Keychain,
    };
    let state = AppState::new(&upstream, token_source)?;
    let upstream_for_log = state.upstream.clone();
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    tracing::info!(addr = %bind, upstream = %upstream_for_log, "cc-proxy listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal");
}
