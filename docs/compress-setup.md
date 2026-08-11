# Enabling proxy compression (`MUR_MODEL_GATEWAY_COMPRESS`)

Install-time setup for wire-level `tool_result` compression. For the design background see
[specs/2026-07-03-mur-compress-design.md](specs/2026-07-03-mur-compress-design.md).

## install flags

```
mur-model-gateway install --compress      # service definition gets MUR_MODEL_GATEWAY_COMPRESS=1 (enabled)
mur-model-gateway install --no-compress   # force off, even if MUR_MODEL_GATEWAY_COMPRESS=1 is in the environment
mur-model-gateway install                 # no flag → inherit the environment at install time; absent everywhere means off (default)
```

Precedence: **flag > environment variable > off by default**.

All three platforms (launchd plist / systemd unit / Windows cmd) get it written into the service
definition, so re-running `setup.sh` does not lose it.

## Applying it to the local service

```bash
./scripts/setup.sh                          # rebuild + install + restart the service (compression off)
./scripts/setup.sh -- --compress            # same, with compression enabled
./scripts/setup.sh --system -- --compress   # system-wide install, compression enabled
```

Everything after `--` goes to `mur-model-gateway install`, so `--compress` is
the form that works everywhere. Prefer it to exporting
`MUR_MODEL_GATEWAY_COMPRESS=1` before `setup.sh`: the sniff only sees what
reaches the install process, and `--system` shells out through `sudo`, which
resets the environment — the exported variable is dropped and you get a service
with compression silently off.

## Verifying

```bash
# macOS: confirm it made it into the plist
grep -A2 MUR_MODEL_GATEWAY_COMPRESS ~/Library/LaunchAgents/run.mur-model-gateway.plist

# after one Claude Code round, check the shared stats
mur compress stats
```

Compressed originals live in `~/.mur/compress`, and the model can restore them through the
`mur_retrieve` MCP tool. A client with no mur MCP server attached only ever sees the compressed
summary and cannot restore it — confirm every mur agent is wired up before switching this on by
default everywhere.
