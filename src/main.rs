use anyhow::Context;
use clap::{Parser, Subcommand};
use mur_model_gateway::{
    AppState, DEFAULT_BIND, DEFAULT_UPSTREAM_ANTHROPIC, DEFAULT_UPSTREAM_CODEX,
    DEFAULT_UPSTREAM_GEMINI, DEFAULT_UPSTREAM_OPENAI, TokenSource, build_router, install,
};
use std::net::SocketAddr;

#[derive(Parser)]
#[command(
    name = "mur-model-gateway",
    about = "Multi-provider LLM API reverse proxy (Anthropic / OpenAI / Gemini / Codex) with Claude Code disguise layer",
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
        /// Bake MUR_MODEL_GATEWAY_COMPRESS=1 into the service (wire-level tool_result compression).
        #[arg(long, conflicts_with = "no_compress")]
        compress: bool,
        /// Force compression off, even if MUR_MODEL_GATEWAY_COMPRESS=1 is set in the environment.
        #[arg(long)]
        no_compress: bool,
        /// Bake MUR_MODEL_GATEWAY_TOKEN_SOURCE into the service
        /// (keychain | off | env:VAR | file | file:/path/to/credentials.json).
        #[arg(long)]
        token_source: Option<String>,
        /// Bake MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX into the service — credential
        /// source for the `/v1/responses` route (codex | off | env:VAR). Defaults
        /// to `codex` (reads `~/.codex/auth.json`) at runtime when unset.
        #[arg(long)]
        token_source_codex: Option<String>,
        /// Bake MUR_MODEL_GATEWAY_BIND into the service (e.g. 127.0.0.1:9099).
        #[arg(long)]
        bind: Option<String>,
        /// Bake MUR_MODEL_GATEWAY_UPSTREAM into the service.
        #[arg(long)]
        upstream: Option<String>,
        /// Linux only: install a system-level unit (/etc/systemd/system) with
        /// EnvironmentFile=/etc/mur-model-gateway.env — starts at boot without a login
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
            token_source_codex,
            bind,
            upstream,
            system,
        } => {
            if let Some(spec) = &token_source {
                // Fail at install time, not first service start.
                parse_token_source(spec)
                    .with_context(|| format!("invalid --token-source {spec}"))?;
            }
            if let Some(spec) = &token_source_codex {
                parse_token_source_codex(spec)
                    .with_context(|| format!("invalid --token-source-codex {spec}"))?;
            }
            install::install(install::InstallOpts {
                compress: if compress {
                    Some(true)
                } else if no_compress {
                    Some(false)
                } else {
                    None // fall back to MUR_MODEL_GATEWAY_COMPRESS env sniff
                },
                token_source,
                token_source_codex,
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,mur_model_gateway=debug")
            }),
        )
        .init();
}

async fn serve() -> anyhow::Result<()> {
    let bind: SocketAddr = std::env::var("MUR_MODEL_GATEWAY_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string())
        .parse()
        .context("invalid MUR_MODEL_GATEWAY_BIND")?;
    let upstream_anthropic = resolve_upstream(
        "MUR_MODEL_GATEWAY_UPSTREAM_ANTHROPIC",
        DEFAULT_UPSTREAM_ANTHROPIC,
    );
    let upstream_openai =
        resolve_upstream("MUR_MODEL_GATEWAY_UPSTREAM_OPENAI", DEFAULT_UPSTREAM_OPENAI);
    let upstream_gemini =
        resolve_upstream("MUR_MODEL_GATEWAY_UPSTREAM_GEMINI", DEFAULT_UPSTREAM_GEMINI);
    let upstream_codex =
        resolve_upstream("MUR_MODEL_GATEWAY_UPSTREAM_CODEX", DEFAULT_UPSTREAM_CODEX);

    let token_source = parse_token_source(
        &std::env::var("MUR_MODEL_GATEWAY_TOKEN_SOURCE").unwrap_or_else(|_| "keychain".to_string()),
    )
    .context("invalid MUR_MODEL_GATEWAY_TOKEN_SOURCE")?;

    let mut state = AppState::new(
        &upstream_anthropic,
        &upstream_openai,
        &upstream_gemini,
        token_source,
    )?
    // Fix round 1, finding 6: `AppState::new`/`with_version` default
    // `token_source_codex` to `Disabled`. This explicit call is what wires
    // an unconfigured gateway to the user's real `~/.codex/auth.json` —
    // production-only, deliberately not part of the constructor itself.
    .with_default_codex_source()
    .with_upstream_codex(&upstream_codex)
    // Same discipline as with_default_codex_source above: AppState::new/
    // with_version default auth_probe to Disabled, and this is the one
    // place that arms it. MUR_MODEL_GATEWAY_NO_AUTH_PROBE=1 opts back out.
    .with_default_auth_probe();
    if let Ok(spec) = std::env::var("MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX") {
        state.token_source_codex = parse_token_source_codex(&spec)
            .context("invalid MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX")?;
    }
    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    tracing::info!(
        addr = %bind,
        upstream_anthropic = %upstream_anthropic,
        upstream_openai = %upstream_openai,
        upstream_gemini = %upstream_gemini,
        upstream_codex = %upstream_codex,
        "mur-model-gateway listening"
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("axum serve")?;
    Ok(())
}

/// Parse a token-source spec:
/// `keychain` (default) | `off`/`disabled` | `env:VAR` | `file` | `file:/path` | `codex`.
/// This is the general-purpose parser, shared by the generic `--token-source` /
/// `MUR_MODEL_GATEWAY_TOKEN_SOURCE` knob and (indirectly, via
/// [`parse_token_source_codex`]) the Codex-specific one. `keychain` and
/// `file`/`file:` read Claude Code's Anthropic OAuth credential — meaningful
/// for the generic knob, meaningless (and dangerous) for Codex.
fn parse_token_source(spec: &str) -> anyhow::Result<TokenSource> {
    match spec {
        "off" | "disabled" => Ok(TokenSource::Disabled),
        "keychain" => Ok(TokenSource::Keychain),
        "file" => mur_model_gateway::keychain::default_credentials_path()
            .map(TokenSource::CredentialsFile)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir for default credentials path")),
        "codex" => mur_model_gateway::codex::default_auth_path()
            .map(TokenSource::Codex)
            .ok_or_else(|| anyhow::anyhow!("cannot resolve home dir for default codex auth path")),
        _ => {
            if let Some(var) = spec.strip_prefix("env:") {
                Ok(TokenSource::EnvVar(var.to_string()))
            } else if let Some(path) = spec.strip_prefix("file:") {
                Ok(TokenSource::CredentialsFile(path.into()))
            } else {
                anyhow::bail!(
                    "invalid token source {spec} (expected: keychain | off | env:VAR | file | file:/path | codex)"
                )
            }
        }
    }
}

/// Parse a token-source spec for the Codex route specifically (fix round 3,
/// finding I1). Delegates to [`parse_token_source`] but rejects `keychain`
/// and `file`/`file:` — those resolve to Claude Code's Anthropic OAuth
/// credential, which is meaningless as a Codex credential and, if ever
/// resolved and sent, would leak the user's Anthropic subscription token to
/// chatgpt.com. Valid specs: `codex` (default) | `off`/`disabled` |
/// `env:VAR` — matching what `--token-source-codex` and
/// `MUR_MODEL_GATEWAY_TOKEN_SOURCE_CODEX` document.
fn parse_token_source_codex(spec: &str) -> anyhow::Result<TokenSource> {
    match parse_token_source(spec)? {
        TokenSource::Keychain | TokenSource::CredentialsFile(_) => anyhow::bail!(
            "invalid Codex token source {spec} (expected: codex | off | env:VAR — \
             keychain/file read Claude Code's Anthropic credential, not a Codex one)"
        ),
        ts => Ok(ts),
    }
}

/// Resolve an upstream URL: provider-specific var → generic MUR_MODEL_GATEWAY_UPSTREAM → default.
fn resolve_upstream(provider_var: &str, default: &str) -> String {
    std::env::var(provider_var)
        .or_else(|_| std::env::var("MUR_MODEL_GATEWAY_UPSTREAM"))
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
        match parse_token_source("codex").unwrap() {
            TokenSource::Codex(p) => assert!(p.ends_with(".codex/auth.json")),
            _ => panic!("expected Codex"),
        }
    }

    #[test]
    fn token_source_rejects_unknown() {
        assert!(parse_token_source("vault").is_err());
        assert!(parse_token_source("").is_err());
    }

    /// Fix round 3, finding I1: `keychain` and `file`/`file:` resolve to
    /// Claude Code's Anthropic OAuth credential, which is meaningless (and
    /// dangerous — it would be sent to chatgpt.com) as a Codex credential.
    /// The Codex-specific parser must reject them even though the generic
    /// one accepts them.
    #[test]
    fn token_source_codex_rejects_anthropic_only_specs() {
        assert!(parse_token_source_codex("keychain").is_err());
        assert!(parse_token_source_codex("file").is_err());
        assert!(parse_token_source_codex("file:/opt/creds.json").is_err());
    }

    #[test]
    fn token_source_codex_accepts_documented_specs() {
        assert!(matches!(
            parse_token_source_codex("codex").unwrap(),
            TokenSource::Codex(_)
        ));
        assert!(matches!(
            parse_token_source_codex("off").unwrap(),
            TokenSource::Disabled
        ));
        assert!(matches!(
            parse_token_source_codex("disabled").unwrap(),
            TokenSource::Disabled
        ));
        match parse_token_source_codex("env:MY_TOKEN").unwrap() {
            TokenSource::EnvVar(v) => assert_eq!(v, "MY_TOKEN"),
            _ => panic!("expected EnvVar"),
        }
    }
}
