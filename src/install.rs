//! Platform-specific service installation.
//!
//! Writes a launchd plist (macOS), a systemd user unit (Linux), or
//! prints Task Scheduler instructions (Windows). The exact path of
//! the currently-running binary is captured via `current_exe()` and
//! embedded in the service file, so `install` works whether the
//! binary lives in `~/.local/bin`, `/opt/homebrew/bin`, or a cargo
//! target dir.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub const SERVICE_LABEL: &str = "run.cc-proxy";

/// Service-file location returned by [`install_paths`].
pub struct InstallPaths {
    /// Service descriptor (launchd plist / systemd unit / etc.)
    pub service_file: PathBuf,
    /// Where the proxy writes stdout+stderr.
    pub log_dir: PathBuf,
    /// Resolved absolute path to the running binary (becomes ProgramArguments[0]).
    pub binary: PathBuf,
}

impl InstallPaths {
    pub fn resolve() -> Result<Self> {
        let binary = std::env::current_exe().context("current_exe")?;
        let dirs = directories::BaseDirs::new().context("BaseDirs::new")?;
        let home = dirs.home_dir();
        let (service_file, log_dir) = if cfg!(target_os = "macos") {
            (
                home.join("Library/LaunchAgents")
                    .join(format!("{SERVICE_LABEL}.plist")),
                home.join("Library/Logs/cc-proxy"),
            )
        } else if cfg!(target_os = "linux") {
            let cfg = dirs.config_dir();
            let state = dirs.state_dir().unwrap_or(cfg);
            (
                cfg.join("systemd/user/cc-proxy.service"),
                state.join("cc-proxy"),
            )
        } else {
            // Windows / other — fall back to %LOCALAPPDATA%\cc-proxy\.
            let local = dirs.config_local_dir();
            (
                local.join("cc-proxy/cc-proxy.cmd"),
                local.join("cc-proxy/logs"),
            )
        };
        Ok(Self {
            service_file,
            log_dir,
            binary,
        })
    }
}

pub fn install() -> Result<()> {
    let paths = InstallPaths::resolve()?;
    std::fs::create_dir_all(&paths.log_dir)
        .with_context(|| format!("mkdir {}", paths.log_dir.display()))?;
    if let Some(parent) = paths.service_file.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }

    let log_file = paths.log_dir.join("proxy.log");

    if cfg!(target_os = "macos") {
        let plist = render_macos_plist(&paths.binary, &log_file);
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
        let unit = render_linux_unit(&paths.binary);
        std::fs::write(&paths.service_file, unit)
            .with_context(|| format!("write {}", paths.service_file.display()))?;
        println!("wrote {}", paths.service_file.display());
        println!("next: systemctl --user daemon-reload");
        println!("      systemctl --user enable --now cc-proxy.service");
        println!("      journalctl --user -u cc-proxy.service -f");
    } else {
        let cmd = render_windows_cmd(&paths.binary);
        std::fs::write(&paths.service_file, cmd)
            .with_context(|| format!("write {}", paths.service_file.display()))?;
        println!("wrote {}", paths.service_file.display());
        println!("next: register a Task Scheduler entry to run that command at logon,");
        println!(
            "      e.g.  schtasks /Create /SC ONLOGON /TN cc-proxy /TR \"{}\"",
            paths.service_file.display()
        );
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let paths = InstallPaths::resolve()?;
    if paths.service_file.exists() {
        std::fs::remove_file(&paths.service_file)
            .with_context(|| format!("remove {}", paths.service_file.display()))?;
        println!("removed {}", paths.service_file.display());
    } else {
        println!(
            "nothing to remove ({} not present)",
            paths.service_file.display()
        );
    }
    if cfg!(target_os = "macos") {
        println!("next: launchctl bootout gui/$(id -u)/{SERVICE_LABEL}");
    } else if cfg!(target_os = "linux") {
        println!("next: systemctl --user disable --now cc-proxy.service");
    } else {
        println!("next: schtasks /Delete /TN cc-proxy /F");
    }
    Ok(())
}

pub fn status() -> Result<()> {
    let paths = InstallPaths::resolve()?;
    println!("binary       : {}", paths.binary.display());
    println!(
        "service file : {}{}",
        paths.service_file.display(),
        if paths.service_file.exists() {
            " ✓"
        } else {
            " (not installed)"
        }
    );
    println!("log dir      : {}", paths.log_dir.display());
    Ok(())
}

pub fn render_macos_plist(binary: &Path, log_file: &Path) -> String {
    let bin = binary.display();
    let log = log_file.display();
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
    <dict>
        <key>RUST_LOG</key>
        <string>info,cc_proxy=debug</string>
    </dict>
</dict>
</plist>
"#
    )
}

pub fn render_linux_unit(binary: &Path) -> String {
    let bin = binary.display();
    format!(
        r#"[Unit]
Description=cc-proxy local Anthropic API proxy
After=network-online.target
Wants=network-online.target

[Service]
ExecStart={bin}
Restart=on-failure
RestartSec=2
Environment=RUST_LOG=info,cc_proxy=debug
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
"#
    )
}

pub fn render_windows_cmd(binary: &Path) -> String {
    let bin = binary.display();
    format!("@echo off\r\nset RUST_LOG=info,cc_proxy=debug\r\n\"{bin}\"\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn macos_plist_embeds_binary_and_log_paths() {
        let p = render_macos_plist(
            &PathBuf::from("/usr/local/bin/cc-proxy"),
            &PathBuf::from("/tmp/proxy.log"),
        );
        assert!(p.contains("<string>/usr/local/bin/cc-proxy</string>"));
        assert!(p.contains("<string>/tmp/proxy.log</string>"));
        assert!(p.contains(&format!("<string>{SERVICE_LABEL}</string>")));
        assert!(p.contains("<key>KeepAlive</key>"));
        assert!(p.contains("<key>RunAtLoad</key>"));
    }

    #[test]
    fn linux_unit_embeds_binary() {
        let u = render_linux_unit(&PathBuf::from("/home/u/.local/bin/cc-proxy"));
        assert!(u.contains("ExecStart=/home/u/.local/bin/cc-proxy"));
        assert!(u.contains("Restart=on-failure"));
        assert!(u.contains("WantedBy=default.target"));
    }

    #[test]
    fn windows_cmd_quotes_binary() {
        let c = render_windows_cmd(&PathBuf::from(r"C:\Users\u\cc-proxy.exe"));
        assert!(c.contains(r#""C:\Users\u\cc-proxy.exe""#));
    }
}
