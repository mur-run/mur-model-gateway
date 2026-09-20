# FreeBSD 15.1 First-Class Support Implementation Plan

> **Execution:** Use `mur-executing-plans` to implement this plan task-by-task. Keep the checklist current and stop at each commit/review gate.

**Goal:** Add tested, first-class FreeBSD 15.1 amd64 support with native `rc.d` lifecycle management, source and release installers, CI, release artifacts, and bilingual documentation while preserving existing macOS, Linux, and Windows behavior.

**Architecture:** Replace implicit “non-macOS/non-Linux means Windows” selection with an explicit internal platform enum and deterministic path resolver. FreeBSD installation resolves the invoking human through `SUDO_USER` plus the system account database, renders a root-owned environment file and native `rc.d` service, and exposes testable lifecycle guidance. Shell installers dispatch FreeBSD separately, while pinned FreeBSD 15.1 VM jobs provide native compile/test/package evidence.

**Tech stack:** Rust 2024, `anyhow`, `directories`, `libc` on Unix, POSIX/FreeBSD shell utilities, Bash installer scripts, GitHub Actions, `vmactions/freebsd-vm` pinned to commit `4469451fe39bee80be4066836c5170362e9349f3`.

## Global Constraints

- FreeBSD support claims are limited to **FreeBSD 15.1 amd64**.
- FreeBSD uses native system-level `rc.d`; `install` and `install --system` have identical behavior there.
- The service runs as the human installer; a valid non-root `SUDO_USER` wins, otherwise use the current non-root username.
- Resolve the service account home from the system account database; never construct `/home/<user>`.
- Write `/usr/local/etc/rc.d/mur_model_gateway` mode `0555` and `/usr/local/etc/mur-model-gateway.env` mode `0600`.
- Supervise with FreeBSD `/usr/sbin/daemon`, PID state below `/var/run`, and append logs to `/var/log/mur-model-gateway.log`.
- Installation does not silently enable boot startup; print `sysrc mur_model_gateway_enable=YES` and `service mur_model_gateway start`.
- Uninstall removes only generated service/env files; it preserves logs, PID state, binary, and `/etc/rc.conf`, and prints explicit stop/`sysrc -x` commands.
- Environment values, usernames, and home paths must not permit shell or rc.conf injection.
- Existing operator-added env-file secret lines remain preserved on reinstall.
- Unsupported targets return an explicit error and never receive a Windows `.cmd` descriptor.
- `--musl` remains Linux-only and errors clearly on FreeBSD.
- The released FreeBSD artifact suffix is exactly `freebsd-amd64`; no FreeBSD arm64 artifact is in scope.
- `vmactions/freebsd-vm` must remain pinned to immutable commit `4469451fe39bee80be4066836c5170362e9349f3`, with `release: 15.1`.
- Existing macOS, Linux, and Windows behavior must remain unchanged.
- Use path APIs in Rust; do not introduce host-dependent path joins into cross-platform code.
- Run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` before every implementation commit.
- Never commit the pre-existing `.serena/project.yml` modification.
- Spec: `docs/superpowers/specs/2026-09-20-freebsd-15-1-support-design.md`.

## File structure

| File | Action | Responsibility |
|---|---|---|
| `Cargo.toml` | Modify | Declare direct Unix `libc` dependency for account database lookup. |
| `Cargo.lock` | Modify if Cargo changes metadata | Lock the direct dependency graph without unrelated upgrades. |
| `src/install.rs` | Modify | Explicit platform dispatch, FreeBSD paths/account lookup, `rc.d` rendering, lifecycle behavior, and unit tests. |
| `scripts/setup.sh` | Modify | Build-from-source FreeBSD detection, service lifecycle, portable health check, and `--system`/`--musl` semantics. |
| `scripts/install-release.sh` | Modify | FreeBSD amd64 artifact selection, checksum verification, and service registration. |
| `scripts/auto.sh` | Modify | Route FreeBSD directly to native setup without Linux-only detection. |
| `tests/freebsd-install-scripts.sh` | Create | Mocked command/path smoke tests for FreeBSD branches in all three shell installers. |
| `.github/workflows/ci.yml` | Modify | Add pinned native FreeBSD 15.1 fmt/Clippy/test/release-build job. |
| `.github/workflows/release.yml` | Modify | Build, test, package, checksum, upload, and gate publishing on FreeBSD amd64. |
| `README.md` | Modify | State tested FreeBSD 15.1 amd64 and `rc.d` support. |
| `README-tw.md` | Modify | Traditional Chinese support statement matching English. |
| `docs/install.md` | Modify | English FreeBSD install/status/log/uninstall/credential-home guidance. |
| `docs/install-tw.md` | Modify | Traditional Chinese equivalent of FreeBSD operational guidance. |

---

### Task 1: Make platform selection explicit and test FreeBSD paths

**Files:**
- Modify: `src/install.rs`

**Interfaces:**
- Consumes: existing `InstallPaths`, `directories::BaseDirs`, `std::env::current_exe`.
- Produces:
  - `enum InstallPlatform { Macos, Linux, Freebsd, Windows }`
  - `InstallPlatform::current() -> Result<Self>`
  - `InstallPaths::resolve_for(platform, system, binary, dirs) -> Result<Self>` (a private deterministic helper; use a small `PathRoots` input containing home/config/local-config/state paths)
  - FreeBSD constants `FREEBSD_RC_SCRIPT`, `FREEBSD_ENV_FILE`, `FREEBSD_PID_FILE`, `FREEBSD_LOG_FILE`.

- [ ] **Step 1: Add failing path and unsupported-platform tests**

Add table-driven tests in `src/install.rs` that pass fake roots and `/opt/mur/bin/mur-model-gateway` to the deterministic resolver. Assert:

```rust
assert_eq!(freebsd.service_file, PathBuf::from("/usr/local/etc/rc.d/mur_model_gateway"));
assert_eq!(freebsd.env_file, Some(PathBuf::from("/usr/local/etc/mur-model-gateway.env")));
assert_eq!(freebsd.log_dir, PathBuf::from("/var/log"));
assert_eq!(freebsd.binary, PathBuf::from("/opt/mur/bin/mur-model-gateway"));
```

Also preserve assertions for macOS plist, Linux user/system units, and Windows `.cmd`; this is the regression guard against changing existing paths.

- [ ] **Step 2: Run the focused test and observe failure**

```bash
cargo test --lib install::tests::platform_paths_are_explicit
```

Expected: compile failure because `InstallPlatform`, `PathRoots`, and `resolve_for` do not exist.

- [ ] **Step 3: Implement explicit dispatch**

Add these constants:

```rust
pub const FREEBSD_RC_SCRIPT: &str = "/usr/local/etc/rc.d/mur_model_gateway";
pub const FREEBSD_ENV_FILE: &str = "/usr/local/etc/mur-model-gateway.env";
pub const FREEBSD_PID_FILE: &str = "/var/run/mur_model_gateway.pid";
pub const FREEBSD_LOG_FILE: &str = "/var/log/mur-model-gateway.log";
```

Use compile-time `#[cfg]` arms in `InstallPlatform::current`; the final arm must be:

```rust
#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "windows"
)))]
{
    bail!("unsupported platform: {}", std::env::consts::OS)
}
```

`InstallPaths::resolve(system)` gathers real roots and delegates to `resolve_for`. `resolve_for` must match all four enum variants; FreeBSD ignores `system`, uses the constants above, and Windows is selected only by `InstallPlatform::Windows`. Update module comments and field comments to name all four service formats.

- [ ] **Step 4: Run path/regression tests**

```bash
cargo test --lib install::tests::platform_paths_are_explicit
cargo test --lib install
```

Expected: PASS; no platform path can fall through to Windows.

- [ ] **Step 5: Run quality gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add src/install.rs
git commit -m "refactor(install): make platform dispatch explicit"
```

---

### Task 2: Resolve and validate the FreeBSD service account

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock` if changed
- Modify: `src/install.rs`

**Interfaces:**
- Consumes: `SUDO_USER`, `whoami::username()`, Unix `getpwnam_r`.
- Produces:
  - `struct ServiceAccount { username: String, home: PathBuf }`
  - `fn choose_installer_username(sudo_user: Option<&str>, current_user: &str) -> Result<String>`
  - `fn validate_account_field(label: &str, value: &str) -> Result<()>`
  - `#[cfg(unix)] fn lookup_account_home(username: &str) -> Result<PathBuf>`
  - `fn resolve_freebsd_service_account(...) -> Result<ServiceAccount>` with injectable chooser/lookup seam for unit tests.

- [ ] **Step 1: Add failing account-selection and validation tests**

Cover these exact cases:

| `SUDO_USER` | current user | Expected |
|---|---|---|
| `alice` | `root` | `alice` |
| empty | `david` | `david` |
| `root` | `david` | `david` |
| absent | `root` | actionable error |
| `bad;name` | `root` | validation error |
| `alice` with home `/home/a$(id)` | `root` | validation error |
| unknown lookup | any | error before descriptor write |

Permit only ASCII username characters `[A-Za-z0-9_.-]`, reject leading `-`, NUL/newline/whitespace, and reject home paths containing newline, carriage return, shell metacharacters, or non-absolute paths.

- [ ] **Step 2: Verify red state**

```bash
cargo test --lib install::tests::freebsd_account
```

Expected: compile failure for missing account helpers.

- [ ] **Step 3: Add the direct Unix dependency and implementation**

In `Cargo.toml` add:

```toml
[target.'cfg(unix)'.dependencies]
libc = "0.2"
```

Implement `lookup_account_home` with `libc::getpwnam_r`, a `CString`, and a growable buffer beginning at 4096 bytes. Retry on `ERANGE`; reject null results and non-UTF-8/empty homes. Do not call `getent`, parse `/etc/passwd`, or infer `/home/<user>`. Keep all unsafe operations in this one helper and document each pointer/lifetime invariant with `// SAFETY:`.

`resolve_freebsd_service_account` reads `SUDO_USER`, chooses the username, looks it up, validates both fields, and returns explicit values to the renderer; the renderer must never read process-global environment.

- [ ] **Step 4: Run tests and dependency check**

```bash
cargo test --lib install::tests::freebsd_account
cargo test --lib install
cargo tree -i libc
```

Expected: account tests pass; `libc` is a direct Unix dependency and no unrelated package versions move.

- [ ] **Step 5: Run quality gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add Cargo.toml Cargo.lock src/install.rs
git commit -m "feat(freebsd): resolve service account from passwd database"
```

---

### Task 3: Render the native `rc.d` service and managed environment

**Files:**
- Modify: `src/install.rs`

**Interfaces:**
- Consumes: `ServiceAccount`, FreeBSD constants, `render_env_file`, `merge_env_file`.
- Produces:
  - `pub fn render_freebsd_rc(binary: &Path, account: &ServiceAccount, env_file: &Path) -> String`
  - Reusable `freebsd_install_guidance()`, `freebsd_status_guidance()`, and `freebsd_uninstall_guidance()` strings.

- [ ] **Step 1: Write failing renderer tests**

Assert the rendered file contains all of:

```text
#!/bin/sh
. /etc/rc.subr
name="mur_model_gateway"
rcvar="mur_model_gateway_enable"
: ${mur_model_gateway_enable:="NO"}
pidfile="/var/run/mur_model_gateway.pid"
required_files="<binary> /usr/local/etc/mur-model-gateway.env"
command="/usr/sbin/daemon"
```

Also assert it names the selected user/home, appends both stdout/stderr to `/var/log/mur-model-gateway.log`, runs `daemon` in supervised/restart mode, and calls `run_rc_command "$1"`. Assert it imports only validated `KEY=VALUE` lines and does not `source`/`.` the env file as arbitrary shell. Keep the existing secret-preservation test and extend it to use `FREEBSD_ENV_FILE` context.

- [ ] **Step 2: Verify renderer test fails**

```bash
cargo test --lib install::tests::freebsd_rc
```

Expected: compile failure because `render_freebsd_rc` is undefined.

- [ ] **Step 3: Implement the renderer**

Render a standard rc.subr script that:

1. defines `start_precmd="mur_model_gateway_precmd"`;
2. checks the binary is executable, env file exists, account resolves via `pw usershow`, and home is a directory;
3. reads env lines with `while IFS= read -r line`, accepts only `^[A-Za-z_][A-Za-z0-9_]*=[^[:space:]]+$`, and exports the split key/value without `eval`;
4. invokes `/usr/sbin/daemon -f -r -P "$pidfile" -o /var/log/mur-model-gateway.log -u <user> -t mur_model_gateway env HOME=<home> <binary>`;
5. keeps all interpolated binary/user/home/env paths shell-safe via the validation boundary.

The guidance functions must return exact operator commands so unit tests can assert them:

```text
sysrc mur_model_gateway_enable=YES
service mur_model_gateway start
service mur_model_gateway status
tail -f /var/log/mur-model-gateway.log
service mur_model_gateway stop
sysrc -x mur_model_gateway_enable
```

- [ ] **Step 4: Run focused and full installer tests**

```bash
cargo test --lib install::tests::freebsd_rc
cargo test --lib install
```

Expected: PASS, including macOS/Linux/Windows renderer tests.

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add src/install.rs
git commit -m "feat(freebsd): render native rc.d service"
```

---

### Task 4: Wire FreeBSD install, status, and uninstall lifecycle

**Files:**
- Modify: `src/install.rs`
- Modify: `src/main.rs` only if CLI help must be corrected

**Interfaces:**
- Consumes: Tasks 1–3 platform/path/account/renderer APIs.
- Produces: full FreeBSD behavior from public `install`, `status`, and `uninstall` functions; `--system` accepted as a FreeBSD no-op.

- [ ] **Step 1: Add failing behavior tests around pure helpers**

Factor plan/apply decisions away from filesystem writes so tests can assert:

- FreeBSD accepts both `system = false` and `system = true`.
- FreeBSD installation targets env first (`0600`) and rc script second (`0555`).
- status includes binary, rc script, env file, PID file, log file, and `service mur_model_gateway status`.
- uninstall targets only rc script and env file and prints stop/`sysrc -x`; logs/PID/binary are absent from deletion targets.
- non-FreeBSD `--system` behavior remains unchanged.
- permission errors mention the exact `sudo <binary> install ...` or `sudo rm <path>` remedy.

- [ ] **Step 2: Verify red state**

```bash
cargo test --lib install::tests::freebsd_lifecycle
```

Expected: failure until lifecycle helpers and dispatch exist.

- [ ] **Step 3: Implement public lifecycle dispatch**

Replace `cfg!(...)` chains in `install`, `uninstall`, and `status` with `match InstallPlatform::current()?`. In the FreeBSD install arm:

1. resolve account before creating/writing service files;
2. read and merge any existing env file;
3. write env mode `0600` then rc script mode `0555`;
4. print token-secret append guidance without printing a secret;
5. print activation/log guidance, but do not call `sysrc` or `service`.

Change the initial `--system` rejection to allow Linux and FreeBSD. Ensure the write-permission hint does not falsely require `--system` on FreeBSD, since both forms are valid. Update `InstallOpts::system` and CLI help comments to “Linux system unit; accepted/no-op on FreeBSD system rc.d”.

- [ ] **Step 4: Run lifecycle and regression tests**

```bash
cargo test --lib install::tests::freebsd_lifecycle
cargo test --lib install
cargo test
```

Expected: all pass; no actual `/usr/local/etc` writes occur in tests.

- [ ] **Step 5: Quality gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add src/install.rs src/main.rs
git commit -m "feat(freebsd): add install status and uninstall lifecycle"
```

---

### Task 5: Add source/auto installer support with shell smoke tests

**Files:**
- Modify: `scripts/setup.sh`
- Modify: `scripts/auto.sh`
- Create: `tests/freebsd-install-scripts.sh`

**Interfaces:**
- Consumes: FreeBSD CLI lifecycle from Task 4.
- Produces: source install/uninstall/start/health behavior and test harness modes `setup` and `auto`.

- [ ] **Step 1: Create a failing mocked-shell harness**

`tests/freebsd-install-scripts.sh` must create a temp `mock-bin`, prepend it to `PATH`, and provide logging fakes for `uname`, `cargo`, `install`, `sudo`, `service`, `sysrc`, `curl`, and the installed gateway. Do not modify host services or `/usr/local`. Test these invocations:

```bash
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
```

Assertions:

- `uname -s` → `FreeBSD`; `uname -m` → `amd64`.
- setup invokes the installed binary through `sudo env SUDO_USER=<original-user> ... install`.
- setup invokes `sudo sysrc mur_model_gateway_enable=YES` and `sudo service mur_model_gateway start`.
- uninstall stops service before removing descriptor/env.
- `--system` is accepted and does not alter the FreeBSD command log.
- `--musl` exits 2 with “Linux-only”.
- health uses `curl` against `http://127.0.0.1:<port>/__mur/health`, not `/dev/tcp`.
- auto does not invoke `ldd`, `sort -V`, or `systemctl` on FreeBSD.

Run both modes and confirm they fail against current scripts.

- [ ] **Step 2: Implement FreeBSD setup branches**

In `scripts/setup.sh`:

- add `FreeBSD) PLATFORM=freebsd ;;`;
- make `--system` legal for Linux and FreeBSD and force system semantics on FreeBSD;
- reject `--musl` unless `PLATFORM=linux`;
- add FreeBSD cases to teardown/start/help/error output;
- invoke `sudo env "SUDO_USER=$(id -un)" "$INSTALL_PATH" install ...` so Rust can recover the human account;
- verify readiness with the existing HTTP health endpoint through `curl`, keeping `lsof`/`ss` only as optional diagnostics;
- update usage comments to state FreeBSD behavior.

In `scripts/auto.sh`, dispatch Linux-only GLIBC/session logic solely under Linux and document that FreeBSD uses setup’s system `rc.d` path.

- [ ] **Step 3: Run shell tests and static syntax checks**

```bash
bash -n scripts/setup.sh scripts/auto.sh tests/freebsd-install-scripts.sh
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
```

Expected: PASS with no privileged host operations.

- [ ] **Step 4: Run Rust gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add scripts/setup.sh scripts/auto.sh tests/freebsd-install-scripts.sh
git commit -m "feat(freebsd): support source and auto installers"
```

---

### Task 6: Add FreeBSD released-binary installation

**Files:**
- Modify: `scripts/install-release.sh`
- Modify: `tests/freebsd-install-scripts.sh`

**Interfaces:**
- Consumes: release suffix `freebsd-amd64`, CLI service installation from Task 4.
- Produces: harness mode `release` and native released-binary setup.

- [ ] **Step 1: Extend the harness with a failing release mode**

Mock `curl` to create a tarball/checksum fixture and assert:

- FreeBSD amd64 selects `mur-model-gateway-<version>-freebsd-amd64.tar.gz`;
- another FreeBSD architecture exits with “releases currently ship amd64 only”;
- checksum uses `sha256 -c` when available, otherwise `sha256sum -c`;
- service registration passes `SUDO_USER`, then runs `sysrc` and `service` through sudo;
- health failure points at `/var/log/mur-model-gateway.log`;
- final Logs/Remove output uses `tail`, `service ... stop`, and `sysrc -x`, not systemd.

```bash
bash tests/freebsd-install-scripts.sh release
```

Expected: FAIL because FreeBSD is currently rejected.

- [ ] **Step 2: Implement the release-installer branch**

Add `FreeBSD)` to platform detection, enforce `uname -m = amd64`, set `ASSET_SUFFIX=freebsd-amd64`, and select a checksum command actually present. Register the service with:

```bash
sudo env "SUDO_USER=$(id -un)" "$INSTALL_DIR/$BIN" install --compress
sudo sysrc mur_model_gateway_enable=YES
sudo service mur_model_gateway start
```

Keep macOS/Linux behavior byte-for-byte equivalent except where shared formatting is factored into a `case`.

- [ ] **Step 3: Run all shell tests**

```bash
bash -n scripts/install-release.sh tests/freebsd-install-scripts.sh
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
bash tests/freebsd-install-scripts.sh release
```

Expected: PASS.

- [ ] **Step 4: Run project gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
git add scripts/install-release.sh tests/freebsd-install-scripts.sh
git commit -m "feat(freebsd): install released amd64 binary"
```

---

### Task 7: Add native FreeBSD 15.1 CI and release packaging

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: all implementation and shell tests; immutable VM action revision.
- Produces: CI job `freebsd` and release job `build-freebsd`, artifact `freebsd`.

- [ ] **Step 1: Add the CI job**

Add a separate Ubuntu-hosted job using exactly:

```yaml
- uses: vmactions/freebsd-vm@4469451fe39bee80be4066836c5170362e9349f3
  with:
    release: "15.1"
    usesh: true
    prepare: pkg install -y curl git pkgconf
    run: |
      curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --component rustfmt,clippy
      . "$HOME/.cargo/env"
      cargo fmt --check
      cargo clippy --all-targets -- -D warnings
      cargo test
      cargo build --release
      bash tests/freebsd-install-scripts.sh setup
      bash tests/freebsd-install-scripts.sh auto
      bash tests/freebsd-install-scripts.sh release
```

Validate YAML syntax with the repository’s available parser (Ruby `YAML.safe_load(..., aliases: true)` is acceptable if no workflow linter is installed).

- [ ] **Step 2: Add release build/package job**

Add `build-freebsd` after `build-linux`, depending on `test`, with the same pinned VM/action and FreeBSD 15.1. Inside the VM:

1. install Rust stable;
2. run `cargo test` then `cargo build --release`;
3. derive version exactly like existing jobs;
4. package `target/release/$BIN` as `dist/$BIN-$VERSION-freebsd-amd64.tar.gz`;
5. create `dist/$NAME.tar.gz.sha256` with FreeBSD `sha256` in a two-space filename format accepted by the installer.

Upload `dist/*` as artifact name `freebsd`. Change publishing dependency to:

```yaml
needs: [build-macos, build-linux, build-freebsd, build-windows]
```

- [ ] **Step 3: Statically verify workflow invariants**

```bash
rg -n 'freebsd-vm@4469451fe39bee80be4066836c5170362e9349f3|release: "15.1"|freebsd-amd64|build-freebsd' .github/workflows
ruby -e 'require "yaml"; ARGV.each { |f| YAML.safe_load(File.read(f), aliases: true) }' .github/workflows/ci.yml .github/workflows/release.yml
```

Expected: both files parse; no `vmactions/freebsd-vm@v1` or floating ref remains.

- [ ] **Step 4: Run local gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
bash tests/freebsd-install-scripts.sh release
git add .github/workflows/ci.yml .github/workflows/release.yml
git commit -m "ci: build and test on FreeBSD 15.1"
```

Note: local success proves workflow structure and host regressions only; the first GitHub run is required evidence for native FreeBSD behavior.

---

### Task 8: Document tested FreeBSD operations in English and Traditional Chinese

**Files:**
- Modify: `README.md`
- Modify: `README-tw.md`
- Modify: `docs/install.md`
- Modify: `docs/install-tw.md`

**Interfaces:**
- Consumes: exact paths/commands/artifact names implemented in Tasks 1–7.
- Produces: matching, bounded support claims and operator instructions.

- [ ] **Step 1: Update README support summaries**

Add “FreeBSD 15.1 amd64” to release artifact lists and `rc.d` to service-manager lists. Correct the install comment: the CLI writes descriptors and prints activation commands; setup scripts perform start/enable. Do not claim all FreeBSD versions or arm64.

- [ ] **Step 2: Add full per-platform guide sections**

In both install guides include:

```bash
sudo env SUDO_USER="$(id -un)" mur-model-gateway install --compress
sudo sysrc mur_model_gateway_enable=YES
sudo service mur_model_gateway start
service mur_model_gateway status
tail -f /var/log/mur-model-gateway.log
```

Explain:

- `install` and `install --system` are equivalent on FreeBSD;
- service and env paths/modes;
- service runs as the invoking human and `HOME` comes from the account database for `~/.claude`/`~/.codex`;
- direct CLI uninstall prints, but does not execute, stop and `sysrc -x` cleanup;
- `setup.sh --uninstall` performs orderly stop first;
- FreeBSD release artifacts are amd64-only;
- `--musl` is Linux-only.

Traditional Chinese text must convey the same operational facts rather than being a shortened summary.

- [ ] **Step 3: Check documentation consistency**

```bash
rg -n 'FreeBSD 15\.1|freebsd-amd64|rc\.d|mur_model_gateway|mur-model-gateway\.env' README.md README-tw.md docs/install.md docs/install-tw.md
rg -n 'all FreeBSD|FreeBSD arm64|FreeBSD.*universal' README.md README-tw.md docs/install.md docs/install-tw.md && exit 1 || true
git diff --check
```

Expected: all four docs name tested FreeBSD 15.1 amd64; no broader claim appears.

- [ ] **Step 4: Run quality gates and commit**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
bash tests/freebsd-install-scripts.sh release
git add README.md README-tw.md docs/install.md docs/install-tw.md
git commit -m "docs: add FreeBSD 15.1 installation guide"
```

---

### Task 9: Final regression, native evidence, and delivery review

**Files:**
- Modify only files required to fix failures found here; do not broaden scope.

**Interfaces:**
- Consumes: all prior task outputs.
- Produces: clean local evidence, pushed CI evidence, and a release-ready commit series.

- [ ] **Step 1: Run the complete local suite**

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
bash -n scripts/setup.sh scripts/install-release.sh scripts/auto.sh tests/freebsd-install-scripts.sh
bash tests/freebsd-install-scripts.sh setup
bash tests/freebsd-install-scripts.sh auto
bash tests/freebsd-install-scripts.sh release
git diff --check
git status --short
```

Expected: every command succeeds; status shows only the user-owned `.serena/project.yml` modification, if still present.

- [ ] **Step 2: Audit design coverage and unsafe/platform boundaries**

```bash
rg -n 'cfg!\(target_os|else \{.*Windows|Windows / other|freebsd-vm@v|release: 15\.1|freebsd-amd64|getpwnam_r|/home/' src scripts .github README.md README-tw.md docs/install.md docs/install-tw.md
```

Manually verify:

- no unsupported OS falls through to Windows;
- no `/home/<user>` inference exists;
- all unsafe code is confined to account lookup and has `SAFETY` comments;
- no script can touch host services in the smoke harness;
- FreeBSD uninstall preserves log/PID/binary and rc.conf;
- release publishing depends on FreeBSD build success.

- [ ] **Step 3: Obtain native CI evidence**

Push the branch and inspect both workflows. Required green evidence:

- CI `check` matrix on Ubuntu/macOS/Windows;
- CI `freebsd` on FreeBSD 15.1;
- release workflow dispatch `build-freebsd` produces exactly:
  - `mur-model-gateway-<sha-or-version>-freebsd-amd64.tar.gz`
  - `mur-model-gateway-<sha-or-version>-freebsd-amd64.tar.gz.sha256`.

If workflow dispatch is unavailable, report native verification as blocked; do not claim FreeBSD works from local macOS tests alone.

- [ ] **Step 4: Fix only evidenced failures, rerun the affected task plus full gates, and commit**

Use a focused commit message naming the failure. Do not amend away the task history unless the user asks.

- [ ] **Step 5: Final report**

Report:

- commits created;
- local command evidence;
- native FreeBSD workflow URL/result and artifact names;
- any remaining blocker;
- confirmation that `.serena/project.yml` was not staged or modified by this work.
