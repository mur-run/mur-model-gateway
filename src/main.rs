use anyhow::Context;
use cc_proxy::{
    AppState, DEFAULT_BIND, DEFAULT_UPSTREAM_ANTHROPIC, DEFAULT_UPSTREAM_GEMINI,
    DEFAULT_UPSTREAM_OPENAI, TokenSource, build_router, install,
};
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
        /// Bake CC_PROXY_TOKEN_SOURCE into the service
        /// (keychain | off | env:VAR | file | file:/path/to/credentials.json).
        #[arg(long)]
        token_source: Option<String>,
        /// Bake CC_PROXY_BIND into the service (e.g. 127.0.0.1:9099).
        #[arg(long)]
        bind: Option<String>,
        /// Bake CC_PROXY_UPSTREAM into the service.
        #[arg(long)]
        upstream: Option<String>,
        /// Linux only: install a system-level unit (/etc/systemd/system) with
        /// EnvironmentFile=/etc/cc-proxy.env — starts at boot without a login
        /// session. Requires root for the /etc writes.
        #[arg(long)]
        system: bool,
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
            token_source,
            bind,
            upstream,
            system,
        } => {
            if let Some(spec) = &token_source {
                // Fail at install time, not first service start.
                parse_token_source(spec)
                    .with_context(|| format!("invalid --token-source {spec}"))?;
            }
            install::install(install::InstallOpts {
                compress: if compress {
                    Some(true)
                } else if no_compress {
                    Some(false)
                } else {
                    None // fall back to CC_PROXY_COMPRESS env sniff
                },
                token_source,
                bind,
                upstream,
                system,
            })
        }
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
    let upstream_anthropic =
        resolve_upstream("CC_PROXY_UPSTREAM_ANTHROPIC", DEFAULT_UPSTREAM_ANTHROPIC);
    let upstream_openai = resolve_upstream("CC_PROXY_UPSTREAM_OPENAI", DEFAULT_UPSTREAM_OPENAI);
    let upstream_gemini = resolve_upstream("CC_PROXY_UPSTREAM_GEMINI", DEFAULT_UPSTREAM_GEMINI);

    let token_source = parse_token_source(
        &std::env::var("CC_PROXY_TOKEN_SOURCE").unwrap_or_else(|_| "keychain".to_string()),
    )
    .context("invalid CC_PROXY_TOKEN_SOURCE")?;

    let state = AppState::new(
        &upstream_anthropic,
        &upstream_openai,
        &upstream_gemini,
        token_source,
    )?;
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

/// Parse a token-source spec:
/// `keychain` (default) | `off`/`disabled` | `env:VAR` | `file` | `file:/path`.
fn parse_token_source(spec: &str) -> anyhow::Result<TokenSource> {
    match spec {
        "off" | "disabled" => Ok(TokenSource::Disabled),
        "keychain" => Ok(TokenSource::Keychain),
        "file" => cc_proxy::keychain::default_credentials_path()
            .map(TokenSource::CredentialsFile)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir for default credentials path")),
        _ => {
            if let Some(var) = spec.strip_prefix("env:") {
                Ok(TokenSource::EnvVar(var.to_string()))
            } else if let Some(path) = spec.strip_prefix("file:") {
                Ok(TokenSource::CredentialsFile(path.into()))
            } else {
                anyhow::bail!(
                    "invalid token source {spec} (expected: keychain | off | env:VAR | file | file:/path)"
                )
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_source_parses_all_specs() {
        assert!(matches!(
            parse_token_source("keychain").unwrap(),
            TokenSource::Keychain
        ));
        assert!(matches!(
            parse_token_source("off").unwrap(),
            TokenSource::Disabled
        ));
        assert!(matches!(
            parse_token_source("disabled").unwrap(),
            TokenSource::Disabled
        ));
        match parse_token_source("env:MY_TOKEN").unwrap() {
            TokenSource::EnvVar(v) => assert_eq!(v, "MY_TOKEN"),
            _ => panic!("expected EnvVar"),
        }
        match parse_token_source("file:/opt/creds.json").unwrap() {
            TokenSource::CredentialsFile(p) => {
                assert_eq!(p, std::path::PathBuf::from("/opt/creds.json"))
            }
            _ => panic!("expected CredentialsFile"),
        }
        match parse_token_source("file").unwrap() {
            TokenSource::CredentialsFile(p) => {
                assert!(p.ends_with(".claude/.credentials.json"))
            }
            _ => panic!("expected CredentialsFile"),
        }
    }

    #[test]
    fn token_source_rejects_unknown() {
        assert!(parse_token_source("vault").is_err());
        assert!(parse_token_source("").is_err());
    }
}
