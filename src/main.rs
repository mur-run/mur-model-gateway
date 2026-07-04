use anyhow::Context;
use cc_proxy::{AppState, DEFAULT_BIND, DEFAULT_UPSTREAM_ANTHROPIC, DEFAULT_UPSTREAM_OPENAI, DEFAULT_UPSTREAM_GEMINI, TokenSource, build_router, install};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(
    name = "cc-proxy",
    about = "Multi-provider LLM API reverse proxy (Anthropic / OpenAI / Gemini) with Claude Code disguise layer",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the proxy in the foreground (default).
    Serve,
    /// Write the platform-specific service descriptor.
    Install {
        /// Bake CC_PROXY_COMPRESS=1 into the service (wire-level tool_result compression).
        #[arg(long, conflicts_with = "no_compress")]
        compress: bool,
        /// Force compression off, even if CC_PROXY_COMPRESS=1 is set in the environment.
        #[arg(long)]
        no_compress: bool,
    },
    /// Remove the platform-specific service descriptor.
    Uninstall,
    /// Print install paths and presence status.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve().await,
        Command::Install {
            compress,
            no_compress,
        } => install::install(if compress {
            Some(true)
        } else if no_compress {
            Some(false)
        } else {
            None // fall back to CC_PROXY_COMPRESS env sniff
        }),
        Command::Uninstall => install::uninstall(),
        Command::Status => install::status(),
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,cc_proxy=debug")),
        )
        .init();
}

async fn serve() -> anyhow::Result<()> {
    let bind: SocketAddr = std::env::var("CC_PROXY_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()
        .context("invalid CC_PROXY_BIND")?;
    let upstream_anthropic = resolve_upstream("CC_PROXY_UPSTREAM_ANTHROPIC", DEFAULT_UPSTREAM_ANTHROPIC);
    let upstream_openai = resolve_upstream("CC_PROXY_UPSTREAM_OPENAI", DEFAULT_UPSTREAM_OPENAI);
    let upstream_gemini = resolve_upstream("CC_PROXY_UPSTREAM_GEMINI", DEFAULT_UPSTREAM_GEMINI);

    let token_source = match std::env::var("CC_PROXY_TOKEN_SOURCE")
        .unwrap_or_else(|_| "keychain".to_string())
        .as_str()
    {
        "off" | "disabled" => TokenSource::Disabled,
        "keychain" => TokenSource::Keychain,
        spec if spec.starts_with("env:") => TokenSource::EnvVar(spec[4..].to_string()),
        other => {
            anyhow::bail!(
                "invalid CC_PROXY_TOKEN_SOURCE={other} (expected: keychain | off | env:VAR)"
            );
        }
    };

    let state = AppState::new(&upstream_anthropic, &upstream_openai, &upstream_gemini, token_source)?;
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    tracing::info!(
        addr = %bind,
        upstream_anthropic = %upstream_anthropic,
        upstream_openai = %upstream_openai,
        upstream_gemini = %upstream_gemini,
        "cc-proxy listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

/// Resolve an upstream URL: provider-specific var → generic CC_PROXY_UPSTREAM → default.
fn resolve_upstream(provider_var: &str, default: &str) -> String {
    std::env::var(provider_var)
        .or_else(|_| std::env::var("CC_PROXY_UPSTREAM"))
        .unwrap_or_else(|_| default.to_string())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal");
}
