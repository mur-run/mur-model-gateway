# Internal testing install guide

> 繁體中文:[staff-testing-tw.md](staff-testing-tw.md)

## What this is

mur-model-gateway is a local proxy that runs **on your own machine** (`127.0.0.1:8088`).

- **There is no shared server.** You are not connecting to anyone else's machine, and nobody connects to yours.
- It reads **your own** Claude Code credential. That credential never leaves your machine.
- Everyone installs, runs, and upgrades their own copy.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/mur-run/mur-model-gateway/main/scripts/install-release.sh -o install-release.sh
less install-release.sh          # reading a script before running it is a good habit
bash install-release.sh
```

It downloads the signed and notarized official release, **verifies its SHA-256**, installs to `~/.local/bin`, and registers a background service that starts at login.

## You will get one password prompt — that is expected

macOS will ask: **"mur-model-gateway" wants to access key "Claude Code-credentials" in your keychain.**

### Click "Always Allow", not "Allow"

| Button | Result |
|---|---|
| **Always Allow** | Authorizes once, never asks again ✅ |
| Allow | Authorizes this one read; it will keep asking |
| Deny | The gateway cannot read the credential and will not work |

**Why it needs access:** the gateway has to read your Claude Code credential to forward requests on your behalf. This dialog is macOS confirming you agree — it is not a fault.

If you click the wrong one, no harm done: re-run the health check below and it will ask again.

## Check it works

```bash
curl -s http://127.0.0.1:8088/__mur/health
```

Expected:

```json
{"claudeCredential":"oauth","codexCredential":"chatgpt","status":"ok","version":"0.3.0"}
```

**The field that matters is `claudeCredential`: it must be `"oauth"`.** If it says `"missing"`:

- You have never signed in on this machine → run `claude auth login`
- Or you clicked "Deny" on the prompt → re-run the health check to be asked again

## Use it

```bash
export ANTHROPIC_BASE_URL="http://127.0.0.1:8088"
```

Add it to `~/.zshenv` to make it permanent.

## When reporting a problem, include these two

```bash
curl -s http://127.0.0.1:8088/__mur/health      # carries the version — important
tail -50 ~/Library/Logs/mur-model-gateway/proxy.log
```

The `version` field tells us which build you are on. **Without it we cannot tell whether the problem you hit has already been fixed.**

Logs are designed not to contain tokens — every type holding a credential has a redacting `Debug` — but glance over them before pasting anyway.

## Upgrade

Re-run the install command. It fetches the latest release and restarts the service.

## Uninstall

```bash
launchctl bootout gui/$(id -u)/run.mur-model-gateway
rm ~/.local/bin/mur-model-gateway ~/Library/LaunchAgents/run.mur-model-gateway.plist
```

The keychain authorization stays behind afterwards (stale but harmless). To clear it too: Keychain Access → search `Claude Code-credentials` → ⌘I → **Access Control** → remove the `mur-model-gateway` entry. **Do not delete the `Claude Code-credentials` item itself** — that is your Claude Code login.
