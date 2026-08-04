//! Platform-specific service installation.
//!
//! Writes a launchd plist (macOS), a systemd unit (Linux, user-level by
//! default or system-level with `--system`), or a `.cmd` wrapper + Task
//! Scheduler command (Windows). The exact path of the currently-running
//! binary is captured via `current_exe()` and embedded in the service file,
//! so `install` works whether the binary lives in `~/.local/bin`,
//! `/opt/homebrew/bin`, or a cargo target dir.
//!
//! Config flags (`--token-source`, `--bind`, `--upstream`) are rendered into
//! the descriptor as environment variables — the runtime already reads
//! MUR_MODEL_GATEWAY_TOKEN_SOURCE / MUR_MODEL_GATEWAY_BIND / MUR_MODEL_GATEWAY_UPSTREAM, so nothing else
//! is needed. In `--system` mode the env goes to a root-owned, mode-600
//! `/etc/mur-model-gateway.env` (referenced via `EnvironmentFile=`) so secrets like an
//! `env:VAR` token can be appended there without living in the unit file.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

pub const SERVICE_LABEL: &str = "run.mur-model-gateway";
pub const LINUX_SYSTEM_UNIT: &str = "/etc/systemd/system/mur-model-gateway.service";
pub const LINUX_SYSTEM_ENV_FILE: &str = "/etc/mur-model-gateway.env";

/// Install-time configuration collected from CLI flags.
#[derive(Default)]
pub struct InstallOpts {
    /// `--compress` / `--no-compress`; `None` = sniff MUR_MODEL_GATEWAY_COMPRESS env.
    pub compress: Option<bool>,
    /// MUR_MODEL_GATEWAY_TOKEN_SOURCE to bake in (already validated by the caller).
    pub token_source: Option<String>,
    /// MUR_MODEL_GATEWAY_BIND to bake in.
    pub bind: Option<String>,
    /// MUR_MODEL_GATEWAY_UPSTREAM to bake in.
    pub upstream: Option<String>,
    /// Linux only: system-level unit + /etc/mur-model-gateway.env instead of user unit.
    pub system: bool,
}

/// Service-file locations returned by [`InstallPaths::resolve`].
pub struct InstallPaths {
    /// Service descriptor (launchd plist / systemd unit / windows .cmd)
    pub service_file: PathBuf,
    /// Where the proxy writes stdout+stderr (journal on Linux; dir still used
    /// for macOS/Windows log files).
    pub log_dir: PathBuf,
    /// Resolved absolute path to the running binary (becomes ProgramArguments[0]).
    pub binary: PathBuf,
    /// Linux `--system` mode only: EnvironmentFile the unit references.
    pub env_file: Option<PathBuf>,
}

impl InstallPaths {
    pub fn resolve(system: bool) -> Result<Self> {
        let binary = std::env::current_exe().context("current_exe")?;
        let dirs = directories::BaseDirs::new().context("BaseDirs::new")?;
        let home = dirs.home_dir();
        let (service_file, log_dir, env_file) = if cfg!(target_os = "macos") {
            (
                home.join("Library/LaunchAgents")
                    .join(format!("{SERVICE_LABEL}.plist")),
                home.join("Library/Logs/mur-model-gateway"),
                None,
            )
        } else if cfg!(target_os = "linux") {
            if system {
                (
                    PathBuf::from(LINUX_SYSTEM_UNIT),
                    PathBuf::from("/var/log/mur-model-gateway"),
                    Some(PathBuf::from(LINUX_SYSTEM_ENV_FILE)),
                )
            } else {
                let cfg = dirs.config_dir();
                let state = dirs.state_dir().unwrap_or(cfg);
                (
                    cfg.join("systemd/user/mur-model-gateway.service"),
                    state.join("mur-model-gateway"),
                    None,
                )
            }
        } else {
            // Windows / other — fall back to %LOCALAPPDATA%\mur-model-gateway\.
            let local = dirs.config_local_dir();
            (
                local.join("mur-model-gateway/mur-model-gateway.cmd"),
                local.join("mur-model-gateway/logs"),
                None,
            )
        };
        Ok(Self {
            service_file,
            log_dir,
            binary,
            env_file,
        })
    }
}

/// The env lines a descriptor carries: RUST_LOG plus every opted-in var.
/// Single source of truth for all three render formats.
pub fn env_pairs(opts: &InstallOpts, compress: bool) -> Result<Vec<(String, String)>> {
    let mut pairs = vec![(
        "RUST_LOG".to_string(),
        "info,mur_model_gateway=debug".to_string(),
    )];
    if compress {
        pairs.push(("MUR_MODEL_GATEWAY_COMPRESS".to_string(), "1".to_string()));
    }
    for (key, val) in [
        ("MUR_MODEL_GATEWAY_TOKEN_SOURCE", &opts.token_source),
        ("MUR_MODEL_GATEWAY_BIND", &opts.bind),
        ("MUR_MODEL_GATEWAY_UPSTREAM", &opts.upstream),
    ] {
        if let Some(v) = val {
            // Injection guard: these values get spliced verbatim into plist
            // XML / unit files / cmd scripts.
            if v.chars()
                .any(|c| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '&'))
            {
                bail!("invalid {key} value {v:?}: whitespace and <>\"& are not allowed");
            }
            pairs.push((key.to_string(), v.clone()));
        }
    }
    Ok(pairs)
}

pub fn install(opts: InstallOpts) -> Result<()> {
    if opts.system && !cfg!(target_os = "linux") {
        bail!("--system is only supported on Linux (systemd)");
    }
    let paths = InstallPaths::resolve(opts.system)?;
    if let Some(parent) = paths.service_file.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    if !opts.system {
        std::fs::create_dir_all(&paths.log_dir)
            .with_context(|| format!("mkdir {}", paths.log_dir.display()))?;
    }

    let log_file = paths.log_dir.join("proxy.log");
    // Flag wins; otherwise capture the install-time env opt-in (default off).
    let compress = opts.compress.unwrap_or_else(|| {
        let env_on = std::env::var("MUR_MODEL_GATEWAY_COMPRESS").is_ok_and(|v| v == "1");
        if env_on {
            eprintln!(
                "note: MUR_MODEL_GATEWAY_COMPRESS=1 detected in environment, baking into service descriptor.\n\
                 \x20      Pass --no-compress to override."
            );
        }
        env_on
    });
    let env = env_pairs(&opts, compress)?;

    if cfg!(target_os = "macos") {
        let plist = render_macos_plist(&paths.binary, &log_file, &env);
        std::fs::write(&paths.service_file, plist)
            .with_context(|| format!("write {}", paths.service_file.display()))?;
        println!("wrote {}", paths.service_file.display());
        println!(
            "next: launchctl bootstrap gui/$(id -u) {}",
            paths.service_file.display()
        );
        println!("      launchctl enable gui/$(id -u)/{SERVICE_LABEL}");
        println!("      tail -f {}", log_file.display());
    } else if cfg!(target_os = "linux") {
        if opts.system {
            let env_file = paths.env_file.as_ref().expect("system mode sets env_file");
            let user = whoami::username();
            let unit = render_linux_system_unit(&paths.binary, &user, env_file);
            let existing = std::fs::read_to_string(env_file).unwrap_or_default();
            write_root_owned(env_file, &merge_env_file(&existing, &env), 0o600)?;
            write_root_owned(&paths.service_file, &unit, 0o644)?;
            println!("wrote {}", paths.service_file.display());
            println!("wrote {} (mode 600)", env_file.display());
            if let Some(var) = opts
                .token_source
                .as_deref()
                .and_then(|spec| spec.strip_prefix("env:"))
            {
                println!(
                    "token: append `{var}=<your sk-ant-oat01-… token>` to {} yourself \
                     (never echo it through a shared shell history)",
                    env_file.display()
                );
            }
            println!("next: sudo systemctl daemon-reload");
            println!("      sudo systemctl enable --now mur-model-gateway.service");
            println!("      journalctl -u mur-model-gateway.service -f");
        } else {
            let unit = render_linux_unit(&paths.binary, &env);
            std::fs::write(&paths.service_file, unit)
                .with_context(|| format!("write {}", paths.service_file.display()))?;
            println!("wrote {}", paths.service_file.display());
            println!("next: systemctl --user daemon-reload");
            println!("      systemctl --user enable --now mur-model-gateway.service");
            println!("      journalctl --user -u mur-model-gateway.service -f");
            println!(
                "note: user units only run while you're logged in; for headless/boot\n\
                 \x20     start use `mur-model-gateway install --system` or `loginctl enable-linger $USER`"
            );
        }
    } else {
        let cmd = render_windows_cmd(&paths.binary, &log_file, &env);
        std::fs::write(&paths.service_file, cmd)
            .with_context(|| format!("write {}", paths.service_file.display()))?;
        println!("wrote {}", paths.service_file.display());
        println!("next (run in an elevated prompt to register the logon task):");
        println!(
            "      schtasks /Create /F /SC ONLOGON /TN mur-model-gateway /TR \"\\\"{}\\\"\"",
            paths.service_file.display()
        );
        println!("      schtasks /Run /TN mur-model-gateway");
        println!("      logs: {}", log_file.display());
    }
    Ok(())
}

/// Write a file that may live under /etc. Plain write first; on permission
/// denied, fail with a sudo hint instead of trying to escalate ourselves.
fn write_root_owned(path: &Path, content: &str, mode: u32) -> Result<()> {
    match std::fs::write(path, content) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                    .with_context(|| format!("chmod {mode:o} {}", path.display()))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            bail!(
                "cannot write {} (permission denied) — re-run with sudo: \
                 sudo {} install --system …",
                path.display(),
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "mur-model-gateway".into())
            );
        }
        Err(e) => Err(e).with_context(|| format!("write {}", path.display())),
    }
}

pub fn uninstall() -> Result<()> {
    // Probe both user and system paths on Linux; only one exists elsewhere.
    let mut removed_any = false;
    for system in [false, true] {
        if system && !cfg!(target_os = "linux") {
            break;
        }
        let paths = InstallPaths::resolve(system)?;
        let mut targets = vec![paths.service_file];
        if let Some(env_file) = paths.env_file {
            targets.push(env_file);
        }
        for f in targets {
            if f.exists() {
                match std::fs::remove_file(&f) {
                    Ok(()) => {
                        println!("removed {}", f.display());
                        removed_any = true;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                        println!(
                            "cannot remove {} — run: sudo rm {}",
                            f.display(),
                            f.display()
                        );
                    }
                    Err(e) => {
                        return Err(e).with_context(|| format!("remove {}", f.display()));
                    }
                }
            }
        }
    }
    if !removed_any {
        println!("nothing to remove (no service files present)");
    }
    if cfg!(target_os = "macos") {
        println!("next: launchctl bootout gui/$(id -u)/{SERVICE_LABEL}");
    } else if cfg!(target_os = "linux") {
        println!("next: systemctl --user disable --now mur-model-gateway.service");
        println!("      (system mode: sudo systemctl disable --now mur-model-gateway.service)");
    } else {
        println!("next: schtasks /Delete /TN mur-model-gateway /F");
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let paths = InstallPaths::resolve(false)?;
    println!("binary       : {}", paths.binary.display());
    print_file_status("service file", &paths.service_file);
    if cfg!(target_os = "linux") {
        let sys = InstallPaths::resolve(true)?;
        print_file_status("system unit ", &sys.service_file);
        if let Some(env_file) = sys.env_file {
            print_file_status("system env  ", &env_file);
        }
    }
    println!("log dir      : {}", paths.log_dir.display());
    Ok(())
}

fn print_file_status(label: &str, path: &Path) {
    println!(
        "{label} : {}{}",
        path.display(),
        if path.exists() {
            " ✓"
        } else {
            " (not installed)"
        }
    );
}

pub fn render_macos_plist(binary: &Path, log_file: &Path, env: &[(String, String)]) -> String {
    let bin = binary.display();
    let log = log_file.display();
    let env_entries: String = env
        .iter()
        .map(|(k, v)| format!("\n        <key>{k}</key>\n        <string>{v}</string>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{SERVICE_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
    <key>EnvironmentVariables</key>
    <dict>{env_entries}
    </dict>
</dict>
</plist>
"#
    )
}

pub fn render_linux_unit(binary: &Path, env: &[(String, String)]) -> String {
    let env_lines: String = env
        .iter()
        .map(|(k, v)| format!("Environment={k}={v}\n"))
        .collect();
    let bin = binary.display();
    format!(
        r#"[Unit]
Description=mur-model-gateway local Anthropic API proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={bin}
Restart=on-failure
RestartSec=2
{env_lines}StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"#
    )
}

/// System-level unit: boots without a login session, runs as `user`, and
/// pulls all env (including secrets) from a root-owned mode-600 env file.
pub fn render_linux_system_unit(binary: &Path, user: &str, env_file: &Path) -> String {
    let bin = binary.display();
    let envf = env_file.display();
    format!(
        r#"[Unit]
Description=mur-model-gateway local Anthropic API proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={bin}
Restart=on-failure
RestartSec=2
User={user}
EnvironmentFile={envf}
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
"#
    )
}

/// KEY=VALUE lines for the systemd EnvironmentFile.
pub fn render_env_file(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("{k}={v}\n")).collect()
}

/// Re-render the env file while preserving manually-added lines (e.g. the
/// secret token appended by the operator). Managed keys are replaced; any
/// existing line whose KEY isn't in `env` survives verbatim.
pub fn merge_env_file(existing: &str, env: &[(String, String)]) -> String {
    let managed: std::collections::HashSet<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    let kept: String = existing
        .lines()
        .filter(|line| {
            let key = line.split('=').next().unwrap_or("").trim();
            !key.is_empty() && !key.starts_with('#') && !managed.contains(key)
        })
        .map(|l| format!("{l}\n"))
        .collect();
    format!("{}{kept}", render_env_file(env))
}

pub fn render_windows_cmd(binary: &Path, log_file: &Path, env: &[(String, String)]) -> String {
    let set_lines: String = env
        .iter()
        .map(|(k, v)| format!("set {k}={v}\r\n"))
        .collect();
    let bin = binary.display();
    let log = log_file.display();
    format!("@echo off\r\n{set_lines}\"{bin}\" >> \"{log}\" 2>&1\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn base_env() -> Vec<(String, String)> {
        vec![(
            "RUST_LOG".to_string(),
            "info,mur_model_gateway=debug".to_string(),
        )]
    }

    #[test]
    fn env_pairs_orders_and_includes_opted_in_vars() {
        let opts = InstallOpts {
            token_source: Some("env:MUR_MODEL_GATEWAY_OAUTH_TOKEN".into()),
            bind: Some("127.0.0.1:9099".into()),
            upstream: Some("https://api.example.com".into()),
            ..Default::default()
        };
        let env = env_pairs(&opts, true).unwrap();
        let keys: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            [
                "RUST_LOG",
                "MUR_MODEL_GATEWAY_COMPRESS",
                "MUR_MODEL_GATEWAY_TOKEN_SOURCE",
                "MUR_MODEL_GATEWAY_BIND",
                "MUR_MODEL_GATEWAY_UPSTREAM"
            ]
        );
    }

    #[test]
    fn env_pairs_rejects_injection() {
        for bad in ["a b", "a<b", "a\"b", "a&b", "a\nb"] {
            let opts = InstallOpts {
                bind: Some(bad.into()),
                ..Default::default()
            };
            assert!(env_pairs(&opts, false).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn macos_plist_embeds_binary_log_and_env() {
        let mut env = base_env();
        env.push(("MUR_MODEL_GATEWAY_TOKEN_SOURCE".into(), "file".into()));
        let p = render_macos_plist(
            &PathBuf::from("/usr/local/bin/mur-model-gateway"),
            &PathBuf::from("/tmp/proxy.log"),
            &env,
        );
        assert!(p.contains("<string>/usr/local/bin/mur-model-gateway</string>"));
        assert!(p.contains("<string>/tmp/proxy.log</string>"));
        assert!(p.contains(&format!("<string>{SERVICE_LABEL}</string>")));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
        assert!(p.contains("<key>MUR_MODEL_GATEWAY_TOKEN_SOURCE</key>"));
        assert!(p.contains("<string>file</string>"));
        assert!(!p.contains("MUR_MODEL_GATEWAY_COMPRESS"));
    }

    #[test]
    fn compress_env_passes_through_when_opted_in() {
        let opts = InstallOpts::default();
        let env = env_pairs(&opts, true).unwrap();

        let p = render_macos_plist(
            &PathBuf::from("/usr/local/bin/mur-model-gateway"),
            &PathBuf::from("/tmp/proxy.log"),
            &env,
        );
        assert!(p.contains("<key>MUR_MODEL_GATEWAY_COMPRESS</key>"));
        assert!(p.contains("<string>1</string>"));

        let u = render_linux_unit(&PathBuf::from("/home/u/.local/bin/mur-model-gateway"), &env);
        assert!(u.contains("Environment=MUR_MODEL_GATEWAY_COMPRESS=1"));

        let c = render_windows_cmd(
            &PathBuf::from(r"C:\Users\u\mur-model-gateway.exe"),
            &PathBuf::from(r"C:\Users\u\logs\proxy.log"),
            &env,
        );
        assert!(c.contains("set MUR_MODEL_GATEWAY_COMPRESS=1"));
    }

    #[test]
    fn linux_unit_embeds_binary_and_env() {
        let mut env = base_env();
        env.push(("MUR_MODEL_GATEWAY_BIND".into(), "127.0.0.1:9099".into()));
        let u = render_linux_unit(&PathBuf::from("/home/u/.local/bin/mur-model-gateway"), &env);
        assert!(u.contains("ExecStart=/home/u/.local/bin/mur-model-gateway"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
        assert!(u.contains("Environment=MUR_MODEL_GATEWAY_BIND=127.0.0.1:9099"));
        assert!(!u.contains("MUR_MODEL_GATEWAY_COMPRESS"));
    }

    #[test]
    fn linux_system_unit_runs_as_user_with_env_file() {
        let u = render_linux_system_unit(
            &PathBuf::from("/usr/local/bin/mur-model-gateway"),
            "karajan",
            &PathBuf::from(LINUX_SYSTEM_ENV_FILE),
        );
        assert!(u.contains("ExecStart=/usr/local/bin/mur-model-gateway"));
        assert!(u.contains("User=karajan"));
        assert!(u.contains("EnvironmentFile=/etc/mur-model-gateway.env"));
        assert!(u.contains("WantedBy=multi-user.target"));
    }

    #[test]
    fn merge_env_file_preserves_operator_lines() {
        let existing =
            "RUST_LOG=old\nMUR_MODEL_GATEWAY_OAUTH_TOKEN=sk-ant-oat01-secret\n# comment\n";
        let env = vec![
            ("RUST_LOG".to_string(), "info".to_string()),
            (
                "MUR_MODEL_GATEWAY_TOKEN_SOURCE".to_string(),
                "env:MUR_MODEL_GATEWAY_OAUTH_TOKEN".to_string(),
            ),
        ];
        let merged = merge_env_file(existing, &env);
        assert!(merged.contains("RUST_LOG=info\n"));
        assert!(!merged.contains("RUST_LOG=old"));
        assert!(merged.contains("MUR_MODEL_GATEWAY_OAUTH_TOKEN=sk-ant-oat01-secret\n"));
        assert!(
            merged.contains("MUR_MODEL_GATEWAY_TOKEN_SOURCE=env:MUR_MODEL_GATEWAY_OAUTH_TOKEN\n")
        );
        assert!(!merged.contains("# comment"));
    }

    #[test]
    fn env_file_renders_key_value_lines() {
        let env = vec![
            ("RUST_LOG".to_string(), "info".to_string()),
            (
                "MUR_MODEL_GATEWAY_TOKEN_SOURCE".to_string(),
                "env:T".to_string(),
            ),
        ];
        assert_eq!(
            render_env_file(&env),
            "RUST_LOG=info\nMUR_MODEL_GATEWAY_TOKEN_SOURCE=env:T\n"
        );
    }

    #[test]
    fn windows_cmd_quotes_binary_and_redirects_log() {
        let c = render_windows_cmd(
            &PathBuf::from(r"C:\Users\u\mur-model-gateway.exe"),
            &PathBuf::from(r"C:\Users\u\logs\proxy.log"),
            &base_env(),
        );
        assert!(c.contains(r#""C:\Users\u\mur-model-gateway.exe""#));
        assert!(c.contains(r#">> "C:\Users\u\logs\proxy.log" 2>&1"#));
        assert!(!c.contains("MUR_MODEL_GATEWAY_COMPRESS"));
    }
}
