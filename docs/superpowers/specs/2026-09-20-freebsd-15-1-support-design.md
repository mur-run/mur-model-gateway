# FreeBSD 15.1 First-Class Support

**Date**: 2026-09-20
**Status**: approved design

## Context

`mur-model-gateway` currently builds and installs services on macOS, Linux, and Windows. The installer has explicit launchd and systemd branches, but every other target falls into the Windows `.cmd`/Task Scheduler branch. On FreeBSD that fallback produces an unusable descriptor. The source and release setup scripts reject `FreeBSD`, and CI never compiles or tests on it.

The intended outcome is first-class FreeBSD 15.1 support: native build and runtime verification, an `rc.d` boot service, install/status/uninstall behavior, a downloadable `amd64` release artifact, release-installer support, and English/Traditional Chinese documentation. Existing macOS, Linux, and Windows behavior must remain unchanged.

## Decisions

| Question | Decision |
|---|---|
| Support level | First-class: runtime, native service lifecycle, CI, release artifact, installers, and docs |
| Service manager | Native FreeBSD `rc.d`, not a generic background wrapper |
| Service account | The human account that invoked installation; when run through `sudo`, resolve it from `SUDO_USER` |
| Credential home | Set `HOME` to that account's home so `~/.claude` and `~/.codex` continue to work |
| Install privilege | FreeBSD service installation is system-level and requires root-owned files |
| `--system` on FreeBSD | Accepted as an explicit no-op because FreeBSD has only the system `rc.d` mode |
| Service descriptor | `/usr/local/etc/rc.d/mur_model_gateway`, mode `0555` |
| Environment file | `/usr/local/etc/mur-model-gateway.env`, mode `0600` |
| Process supervision | FreeBSD `daemon(8)` with a supervised child and PID file |
| Logs | `/var/log/mur-model-gateway.log` |
| Runtime state | PID file under `/var/run` |
| Boot enablement | `sysrc mur_model_gateway_enable=YES` |
| CI | `vmactions/freebsd-vm`, FreeBSD 15.1, native fmt/Clippy/test/build checks |
| Release architecture | FreeBSD `amd64` only |
| Release asset suffix | `freebsd-amd64` |
| Existing platforms | No behavior changes |

## Considered approaches

### 1. Native `rc.d` integration — selected

This follows FreeBSD administration conventions: `service mur_model_gateway start`, boot enablement through `sysrc`, and service state reported by rc.subr. It adds platform-specific rendering and lifecycle logic, but gives operators the behavior they expect and makes FreeBSD genuinely supported rather than merely compilable.

### 2. Generic background wrapper

A shell wrapper could invoke the binary with `nohup` or `daemon(8)` without an `rc.d` script. This has less Rust platform code, but no standard boot integration, weaker status semantics, and ad-hoc cleanup. It is unsuitable for first-class support.

### 3. Build/runtime support only

The crate could simply be made to compile on FreeBSD and CI could test it. Users would still have to invent service management and release installation themselves, which does not meet the selected scope.

## Architecture

### Explicit platform dispatch

`src/install.rs` will gain an explicit `target_os = "freebsd"` branch in path resolution, installation, uninstallation, and status reporting. FreeBSD must be handled before the final Windows branch so no Unix target can accidentally receive a `.cmd` descriptor.

The platform branches remain deliberately separate:

- macOS: launchd user service;
- Linux: systemd user service or `--system` system service;
- FreeBSD: system `rc.d` service;
- Windows: `.cmd` wrapper and Task Scheduler guidance.

Unsupported platforms should fail with a clear error rather than silently falling through to Windows behavior.

### FreeBSD paths

The installer will use constants for the FreeBSD-owned paths:

```text
/usr/local/etc/rc.d/mur_model_gateway
/usr/local/etc/mur-model-gateway.env
/var/run/mur_model_gateway.pid
/var/log/mur-model-gateway.log
```

`InstallPaths::resolve` will expose the service descriptor, log location, and environment file without embedding path rules throughout install/status/uninstall. Tests should exercise path construction through a platform-independent helper rather than relying only on compile-time `cfg!` from a non-FreeBSD host.

### Installer-user resolution

The service runs as the person who requested installation, not as root and not as a newly-created account.

Resolution order:

1. If the effective installation is performed through `sudo`, use a non-empty, non-`root` `SUDO_USER`.
2. Otherwise use the current username.
3. Resolve that user's home directory from the system account database rather than constructing `/home/<user>`.
4. Reject an unresolved user or home directory with an actionable error before writing the service descriptor.

The renderer receives the resolved username and home as explicit inputs. It does not read process-global environment internally, keeping selection logic independently testable. User and home values must be validated so they cannot inject shell or rc.conf syntax.

### `rc.d` descriptor

The generated script will be a standard `/bin/sh` rc.subr service:

- source `/etc/rc.subr`;
- define `name="mur_model_gateway"` and `rcvar="mur_model_gateway_enable"`;
- default enablement to `NO` so installation never silently changes boot policy;
- use a `required_files` check for the binary and environment file;
- run the binary as the selected user with `HOME` set to the resolved account home;
- import the managed environment file without evaluating arbitrary shell commands;
- invoke `/usr/sbin/daemon` in supervised/restart mode with a PID file and append stdout/stderr to `/var/log/mur-model-gateway.log`;
- expose normal `start`, `stop`, `restart`, and `status` through rc.subr.

The environment file remains a simple `KEY=VALUE` file generated from the same validated install options used by Linux system mode. Because values are rejected when they contain whitespace or shell metacharacters, the rc script can load only validated assignments. Operator-added secret lines are preserved on reinstall through the existing merge behavior.

The script will validate the binary, environment file, service user, and home directory before launch and fail visibly through `service(8)` if any prerequisite is missing.

### Install flow

On FreeBSD, `mur-model-gateway install` and `install --system` have identical semantics:

1. Resolve the installing user and home.
2. Render and merge the environment file.
3. Write the environment file as root-owned mode `0600`.
4. Write the `rc.d` script as root-owned mode `0555`.
5. Print the exact activation commands:

```sh
sysrc mur_model_gateway_enable=YES
service mur_model_gateway start
```

6. Print the log command:

```sh
tail -f /var/log/mur-model-gateway.log
```

If either root-owned write is denied, installation stops with a `sudo` re-run command. The installer never invokes privilege escalation itself.

If an `env:VAR` token source was selected, installation prints a safe instruction to add the secret to `/usr/local/etc/mur-model-gateway.env`; it never echoes the secret into shell history.

### Status and uninstall

`status` reports the binary, rc.d script, environment file, PID file, and log file. It also prints `service mur_model_gateway status` as the authoritative live-state command; the command itself remains operator-controlled rather than being hidden behind parsing in the Rust CLI.

`uninstall` removes only files generated by the gateway:

- `/usr/local/etc/rc.d/mur_model_gateway`;
- `/usr/local/etc/mur-model-gateway.env`.

It does not delete logs, PID files, the installed binary, or unrelated configuration. It does not silently edit `/etc/rc.conf`; instead it prints the lifecycle cleanup explicitly:

```sh
service mur_model_gateway stop
sysrc -x mur_model_gateway_enable
```

The setup scripts stop the service before invoking file removal, so their automated uninstall path remains orderly. Direct CLI uninstall remains conservative and reports any root permission requirement.

## Shell installers

### Source setup (`scripts/setup.sh`)

The platform detector will recognize `FreeBSD`. FreeBSD always takes the system-service path:

- stop: `service mur_model_gateway stop`;
- install descriptor: invoke the installed binary under `sudo` while preserving the original user identity through `SUDO_USER`;
- enable/start: `sudo sysrc mur_model_gateway_enable=YES` then `sudo service mur_model_gateway start`;
- logs: `/var/log/mur-model-gateway.log`;
- verification: use a portable loopback HTTP check rather than Linux-only `/dev/tcp` assumptions where needed.

`--system` is accepted on FreeBSD and changes nothing. `--musl` remains Linux-only and errors clearly on FreeBSD.

### Released-binary installer (`scripts/install-release.sh`)

The release installer will recognize `uname -s = FreeBSD`, require `uname -m = amd64`, select the `freebsd-amd64` asset, verify it with FreeBSD's available SHA-256 utility, install the binary, and register/start the same rc.d service with `sudo`.

The script's service, log, removal, and health-check output becomes platform-specific rather than using the current macOS-versus-Linux binary choice.

### Auto installer (`scripts/auto.sh`)

FreeBSD skips Linux GLIBC and user-session detection. It relies on `setup.sh`'s native FreeBSD system-service path while retaining the existing credential-file detection.

## CI and release

### Pull-request CI

Add a separate FreeBSD job to `.github/workflows/ci.yml` because FreeBSD is hosted inside a VM rather than selected through `runs-on`:

- Ubuntu GitHub runner host;
- `vmactions/freebsd-vm` configured with `release: 15.1`;
- install/use the stable Rust toolchain and required native build prerequisites inside the VM;
- run `cargo fmt --check`;
- run `cargo clippy --all-targets -- -D warnings`;
- run `cargo test`;
- run `cargo build --release`.

The action reference must be pinned to an immutable commit SHA when implemented, while its VM release is pinned to `15.1`. This avoids floating third-party CI code.

### Release workflow

Add `build-freebsd` alongside macOS, Linux, and Windows:

1. Build natively on FreeBSD 15.1 through the same pinned VM action.
2. Run the FreeBSD test suite before packaging.
3. Package `target/release/mur-model-gateway` as:

```text
mur-model-gateway-<version>-freebsd-amd64.tar.gz
mur-model-gateway-<version>-freebsd-amd64.tar.gz.sha256
```

4. Upload a `freebsd` workflow artifact.
5. Make the publishing job depend on `build-freebsd` so a tag cannot publish a partial supported-platform release.

No FreeBSD `arm64` artifact is included in this scope.

## Documentation

Update all user-facing support claims and operational instructions:

- `README.md` and `README-tw.md`: list FreeBSD 15.1 amd64 release support and `rc.d` alongside launchd/systemd/Task Scheduler;
- `docs/install.md` and `docs/install-tw.md`: add install, enable/start, status, logs, uninstall, credential-home, and root-permission details;
- setup/release script usage comments: describe FreeBSD behavior and FreeBSD-only constraints accurately.

Documentation must distinguish “tested on FreeBSD 15.1 amd64” from broader unverified FreeBSD-version or architecture claims.

## Error handling

| Condition | Behavior |
|---|---|
| FreeBSD install is not privileged | Stop before partial service setup and print the exact `sudo ... install` command |
| `SUDO_USER` is empty, `root`, or invalid | Fall back only when a real current user exists; otherwise fail with an actionable user-resolution error |
| User home cannot be resolved | Fail before writing the descriptor; never guess `/home/<user>` |
| Binary or env file missing at service start | rc.d prerequisite check fails visibly |
| Invalid environment value | Existing installer validation rejects it before rendering |
| Existing operator-added secret line | Preserve it while replacing managed keys |
| Service file removal lacks permission | Print the exact `sudo rm` command and leave the file untouched |
| FreeBSD release installer runs on non-amd64 | Reject with a message that releases currently ship amd64 only |
| Third-party VM setup/build fails | CI/release job fails; no FreeBSD artifact and no tag release is published |
| Unsupported OS reaches installer | Return an explicit unsupported-platform error, never generate a Windows `.cmd` file |

## Testing

### Rust unit tests

Add deterministic tests for:

1. FreeBSD path selection.
2. Installer-user choice: valid `SUDO_USER`, ignored root/empty value, direct invocation fallback, and unresolved account failure.
3. FreeBSD username/home validation against descriptor injection.
4. `rc.d` rendering: rc.subr metadata, binary path, selected user, `HOME`, environment file, `daemon(8)` supervision, PID file, log file, and required-file checks.
5. Environment merge preservation for operator-added secret lines.
6. FreeBSD install/status/uninstall guidance, factored into renderable/testable messages where necessary.
7. Existing macOS/Linux/Windows renderer tests remain green.

Tests must use platform-neutral inputs and temporary paths; they must not need root or write under `/usr/local/etc`.

### Native FreeBSD validation

The FreeBSD 15.1 CI job proves that the complete crate compiles and tests natively. Release CI additionally proves that a native release binary can be packaged under the promised asset name.

Where VM permissions permit, add a smoke test that renders the descriptor and validates its shell syntax. Starting a long-running boot service inside CI is not required; unit tests cover command construction, while native cargo tests cover target-specific compilation and behavior.

### Regression checks

Before completion, run the existing host checks plus FreeBSD CI:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The GitHub workflows must then provide evidence for Linux, macOS, Windows, and FreeBSD 15.1. Local macOS success alone is not evidence that the FreeBSD service works.

## Delivery boundaries

### In scope

- Rust FreeBSD install/status/uninstall behavior;
- native rc.d descriptor and environment file;
- source and released-binary setup scripts;
- FreeBSD 15.1 CI;
- FreeBSD amd64 release artifact;
- English and Traditional Chinese documentation;
- explicit rejection for unsupported target fallthrough.

### Out of scope

- FreeBSD arm64 artifacts;
- pkg(8) repository/package creation;
- ports tree integration;
- creating a dedicated service account;
- jails-specific setup;
- pf firewall configuration;
- support claims for FreeBSD versions other than 15.1;
- changing service behavior on macOS, Linux, or Windows.
