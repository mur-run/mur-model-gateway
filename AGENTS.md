# AGENTS.md — mur-model-gateway

Notes for agents working in this repo. Keep them short and factual.

## Install scripts

- `scripts/install-with-omlx.sh` is the public installer and is tracked by git.
- `install-impl.sh` is the same installer plus a step that downloads the private impl from a gist. It is **gitignored** (`.gitignore:16`) on purpose, so never `git add -f` it: that would publish the private URL.
- **Keep both in sync.** Any change to the oMLX part of one must be applied to the other. Expected differences: the header comment, `GIST_RAW_URL`, the private-impl download block, and where the `# ─── 1. 前置檢查` heading sits.
- Check with `bash -n <script>` and `MUR_OMLX_MODE=uv bash <script> --check`. `--check` installs nothing and writes no log.

## oMLX modes

- If `oMLX.app` is found (`/Applications` or `~/Applications`), the installer asks for a mode: **UV** (default, runs under launchd as `com.mur.omlx` on port 8000) or **APP** (uses the app on port 8000).
- Setting `MUR_OMLX_MODE=uv|app` skips the question. It is **required** when there is no TTY (for example `curl | sh`). Without it the script stops rather than choosing a mode.
- In UV mode the app is moved to port **8001** so the two can run together. Expect double memory use when both have models loaded.

## oMLX version

- Each install resolves the newest **stable** release of `jundot/omlx` via the GitHub API (`resolve_omlx_release()`) and upgrades to it; if the venv already has that version, the download is skipped.
- Filter by tag name (`vX.Y.Z` only). Do not trust the `prerelease` flag or `/releases/latest`: upstream shipped `v0.7.0rc1` with `prerelease: false`.
- The wheel's SHA-256 comes from the release asset's `digest`. No valid digest means no install.
- If the API is unreachable, rate-limited (60/h unauthenticated), or `jq` is missing, the script falls back to `OMLX_FALLBACK_VERSION` / `OMLX_FALLBACK_SHA256` (currently 0.6.4) with a warning. Bump both together when a newer version is known good.
- `MUR_OMLX_VERSION=X.Y.Z` pins a version. An unknown version stops the install instead of silently installing something else.
- The digest proves integrity, not compatibility. After a new oMLX release, check `/v1/embeddings` (see Gotchas); if it breaks, pin the old version with `MUR_OMLX_VERSION`.

## Shared model library

- Both modes use `~/.omlx/models` (override with `MUR_MODEL_DIR`).
- For UV mode, set the directory with `--model-dir` in the launchd plist. The CLI `--model-dir` overrides `model_dirs` from settings (oMLX `settings.py:1246-1249`), so editing settings alone does nothing.
- For the app, the installer uses `jq` to put the directory first in `model_dirs` and backs up `~/.omlx/settings.json` before editing it.
- Uninstalling UV mode must **not** delete the shared library, because the app uses it too.
- The old location `~/.mur/omlx/models` is obsolete.

## Gotchas

- The gateway source clone may have `pull.rebase=true`. Use `git pull --ff-only --no-rebase`. If the clone has uncommitted changes, skip the pull and warn instead of aborting the install.
- To verify, call the endpoint. A `/v1/models` listing is not enough:
  `curl -s http://127.0.0.1:8000/v1/embeddings -H 'Content-Type: application/json' -d '{"model":"Qwen3-Embedding-0.6B-8bit","input":"hi"}'`
- The APP-mode "add to login items" step uses System Events and may fail with `-10004` (permissions). The script then tells the user to add the item by hand and keeps going.
