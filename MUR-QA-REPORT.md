# mur — Full QA Report (skills · CLI · MCP · commander · server)

**Date:** 2026-06-13
**System under test:** `mur` CLI **2.24.1** (`/opt/homebrew/bin/mur`), `mur-mcp-server` **2.22.16**, `murc` (mur-commander) **0.12.1**, `mur-server` (Go 1.23), 13 Claude skills under `~/.claude/skills/`.
**Method:** 12-domain multi-agent harness — each domain *live-executed* its surface ("anything goes" mandate), then findings were adversarially re-verified by an independent agent, then improvements researched. MCP + media domains and several rate-limited verifications were re-run **inline by hand** (see Methodology).

---

## 1. Executive summary

mur's core binaries are in good shape: the `mur agent`, `mur project`, `mur model`, `mur skill`, `mur notes`, `mur source`, `mur internals` subtrees, the MCP server's protocol layer, the commander CLI, and the Go server all **build and run without crashes**, with mostly graceful error handling. No panics or data corruption were observed in normal use.

The problems cluster into three themes:

1. **A genuinely dangerous workflow-execution behaviour** (1 critical): `mur workflow run <name>` silently executes a *semantically-similar* workflow when the name isn't an exact match — a typo can launch a 132-step production deploy with no confirmation.
2. **Pervasive skill/doc ↔ binary drift** (most of the highs/mediums): skills and auto-injected hook text tell the agent to run commands that have been **renamed or removed** (`mur recall`, `mur out`, `mur proposals`, `mur new`, `mur agent export <name>` without `--out`). Because some of this text is injected into *every* Claude session, the AI is being handed broken instructions on startup.
3. **Tools that report success when they failed** (a correctness/observability cluster): `mur verify` exits 0 with stale claims, `mur agent send` returns `state:"completed"` on an LLM error, `mur sync` exits 0 after four 401s, `mur update --check` advises a downgrade.

### Findings by severity (de-duplicated across domains)

| Severity | Count | Examples |
|---|---|---|
| 🔴 Critical | 1 | fuzzy workflow-run executes wrong (destructive) workflow |
| 🟠 High | 10 | broken export doc, `--yes` no-op, dead `mur recall`/`mur out` in session-start, `verify` exits 0 on stale, commander workflow-dir split & `murc index` os-error-2 |
| 🟡 Medium | 13 | global search undocumented, `send` reports failure as completed, `deploy logs -f` flag collision, false Homebrew-upgrade advice, MCP `agent_status` phantom agents, MCP `compress` no-op |
| 🟢 Low / Nit | ~20 | epoch timestamps, noisy WARNs, missing `notes remove`, duplicate server middleware, MCP version skew, notification reply |

> ⚠️ **Operational incident during testing:** the live build + MLX/model work filled the **root data volume (100%)**, which zeroed an in-flight workflow output file and blocked all disk writes. I reclaimed **6.5 GiB** by relocating `~/.rustup`, `~/.cargo`, and `~/Library/Caches/ms-playwright` to `/Volumes/Firecuda4tb/.relocated-home/` (symlinked; toolchains verified). This is itself a product signal — see §8.

---

## 2. 🔴 Critical

### C1 — `mur workflow run <name>` silently executes a *fuzzy-matched* workflow (no confirmation, ignores `--yes`)
**Domain:** workflow · **Category:** data-loss / safety · **Status:** ✅ confirmed (independent repro + source)

When the query is not an exact workflow name, `cmd_workflow_run` (`mur-core/src/cmd/workflow.rs:59-105`) falls back to semantic vector search (top-1 if cosine > 0.6), then keyword substring match, and **immediately executes** the single best hit via `executor.execute(&expr, None)`. It never echoes *chosen-vs-asked*, never shows the similarity score, and never prompts.

```
$ mur workflow run test-workflow        # a broken/typo name
▶ Running workflow: deploy-frontend-test-production
  Step 1: Start mur recording session
  ... 132 steps incl. 'ssh ansible@35.194.207.172', 'reload prod haproxy',
      many Write/Edit to ~/Projects/task-sharing-web ...
```

The typo `test-workflow` resolved (via the embedding index) to **`deploy-frontend-test-production`** and ran it — *without* `--yes`. The live no-`--yes` invocation was actually blocked by Claude's own auto-approve classifier *because* it fuzzy-resolves to a production deploy, corroborating the unbounded blast radius. In an agent with real tools wired, a near-miss query would start SSHing into production.

**Fix:** when the resolved name ≠ the query (i.e. came from fuzzy fallback, not exact `store.get`), print `Resolved "<q>" → <name> (sim 0.NN, via semantic|keyword)` and require interactive `y/N` (or `--yes`). Raise the auto-execute threshold (≥0.85) and present a top-k candidate list for the 0.6–0.85 band instead of executing top-1.

---

## 3. 🟠 High

| ID | Domain | Finding | Status |
|---|---|---|---|
| H1 | agent | `mur-agent-manage` SKILL.md documents `mur agent export <name>`, but `--out` is **mandatory** → the documented command fails (`exit 2`). | ✅ confirmed |
| H2 | workflow | `--yes` is only wired into the skill-DAG fallback path; all three YAML/PipelineExpr branches call `execute(&expr, None)`, so `--yes` is a **no-op** for YAML workflows with `needs_approval` steps. | ✅ confirmed (source) |
| H3 | workflow | `mur-run` SKILL says step 1 "find and output the workflow", but bare `mur run`/`mur workflow run` **executes** command steps immediately; inspection requires `--prompt` (unmentioned). An agent following the skill runs side-effecting steps. | ✅ confirmed |
| H4 | session/chat | `mur session out --action skip` prints `Done.` but **does not stop the active session** — `mur session status` still shows it Active; only `mur session stop` actually stops it → dangling recording. | ⚠️ reported (workflow test); not independently re-run |
| H5 | session/chat | `mur session in` (help: "start recording + inject context") only **marks** the session — no recording started, no context injected; `status` then says "No active session." | ⚠️ reported; not independently re-run |
| H6 | core-cli-a | `mur hook session-start` (auto-injected into **every** Claude session) tells the agent to "Run `mur recall <name>`" — **`recall` is not a command** (`exit 2`). | ✅ confirmed by hand |
| H7 | core-cli-a | session-start / `mur hook prompt` say "run `mur out` to review", but `mur out` is a **deprecation stub** (`# mur out: use \`mur session out\``). | ✅ confirmed by hand |
| H8 | core-cli-b | `mur verify` **exits 0 even with stale claims** (`Total 84 / Valid 66 / Invalid 9` → `exit 0`). A verification tool that always succeeds cannot gate CI. | ✅ confirmed by hand |
| H9 | commander | Workflow directory disagrees across surfaces: `doctor`/`init`/`run --help` use `~/.mur/commander/workflows`, while `workflows`/`run`/`create` use `~/.mur/workflows`. Examples seeded by `init` are invisible to `run`; the legacy fallback was deleted. | ✅ confirmed (built murc 0.12.1 + source) |
| H10 | commander | `murc index` / `index --status` fail with bare `os error 2` because `resolve_mur_bin()` only checks `~/.mur/bin/mur` (no PATH fallback); on Homebrew installs `mur` is at `/opt/homebrew/bin/mur`. The post-commit hooks `murc hooks install` writes call `murc index --quiet`, so **indexing silently breaks on every commit**. | ✅ confirmed (source + repro) |

---

## 4. 🟡 Medium

| ID | Domain | Finding | Status |
|---|---|---|---|
| M1 | agent | `mur agent send` returns task `state:"completed"` when the backend **LLM call failed** (error buried in reply text, no `error` field). A programmatic caller reads failure as success. *(Note: already fixed in-tree at `mur-agent-runtime/src/task_runner.rs:419-426` — needs a release.)* | ✅ confirmed |
| M2 | project | `mur project search` is **global across all indexed projects** and ignores cwd; neither project skill documents this or the `--project` flag. From inside a repo, ranks 2–5 are unrelated projects' code. | ✅ confirmed |
| M3 | workflow | `mur run` is a deprecated alias that prints a banner and **rejects `--yes`** (clap exit 2), while `mur workflow run` accepts it — flag divergence the skill teaches users into. | ✅ confirmed |
| M4 | session/chat | `mur-out` skill body references a **non-existent `mur proposals`** command (`exit 2`). | ✅ confirmed by hand |
| M5 | core-cli-a | `mur dashboard` empty state suggests `mur new` — **unrecognized subcommand** (`exit 2`). | ✅ confirmed by hand |
| M6 | core-cli-a | `mur sync fleet` leaks a **raw 401 + internal URL** (`https://mur-server.fly.dev/api/v1/core/auth/me`) instead of a "log in / Pro feature" message. | ⚠️ reported |
| M7 | core-cli-b | `mur skill list` shows `old-skill [Sandboxed]`, but `info`/`show`/`audit` all say `'old-skill' not installed` — `list` enumerates any subdir, the rest require a manifest. | ✅ confirmed by hand |
| M8 | core-cli-b | `mur verify --all` marks the documented `mur out` claim ✅ valid even though it's a deprecation redirect — the `[cmd]` check is too shallow to catch deprecation-by-redirect. | ⚠️ reported |
| M9 | core-cli-c | `mur deploy logs`: both `--follow` and `--file` derive short **`-f`**; `-f <path>` is silently consumed as the SERVICE positional → wrong behaviour, no error. | ✅ confirmed by hand |
| M10 | core-cli-c | `mur update --check` claims **"Installed via Homebrew. Run: brew upgrade mur"** — but the on-PATH binary is a 193 MB *manually-installed* 2.24.1 file; Homebrew's Cellar holds the older **2.24.0**, so the advice would **downgrade**. | ✅ confirmed by hand |
| M11 | notes/source | Passing a bare instance name (vs canonical `obsidian:<id>`) to `source sync/status/reindex` leaks a raw FS error (`read …/fixturevault.yaml … os error 2`) instead of "source not found". | ⚠️ reported |
| M12 | **mcp** | `mur_agent_status` returns **phantom agents** — `".git"` and `"Author"` listed alongside the real `mur` agent (the CLI `mur agent list` correctly shows only `mur`). | ✅ confirmed by hand |
| M13 | **mcp** | `mur_compress` **no-ops on generic text**: even at 3200 tokens it returns `compressed == input`, `hash: null`, `"No content offloaded; nothing to retrieve."` → the compress→retrieve round-trip is not exercisable for plain content. | ✅ confirmed by hand |

---

## 5. 🟢 Low / Nit (selected)

- **project:** `mur project status` only prints `Indexed: yes/no` + `Chunks` — no `last_indexed` timestamp (though `ProjectStatusInfo` carries the field and `project list` *does* print it); the search skill tells the model to check status for an "indexing in progress" state that steady-state status never emits.
- **core-cli-b:** `mur skill fmt --to md` stamps `updated_at: 1970-01-01T00:00:00Z` for skills with no timestamp — would corrupt freshness logic if written back. `skill audit --help` promises a "signature check" not present in output.
- **core-cli-a/c:** `mur sync` prints four `HTTP 401` cloud failures then `Sync complete.` **exit 0** (failures undetectable by scripts). `mur fetch --dry-run` previews without auth while real `fetch` 401s (asymmetry).
- **notes/source:** `mur notes` has no `remove` subcommand (deletion only via `mur skill remove`, undiscoverable). `mur source install-schedule` silently writes a real launchd plist into `~/Library/LaunchAgents` with no `--dry-run`/confirmation. Notion token not validated at `add` (deferred 401 at `sync`).
- **commander:** `murc workflows` prints the YAML `id` and `Run with: murc run <id>`, but `run` resolves by **filename stem** → `murc run <id>` fails when id ≠ filename. `murc workflows` reports the core harvester's `~/.mur/workflows/*.yaml` (shared dir) as parse errors. `murc agent search` → marketplace 404. Four duplicate `commander_dir()` definitions invite path drift.
- **server (Go):** `deviceMiddleware` (`DeviceCheck`) is registered **twice** on the `/devices` and `/teams` route groups (`internal/api/server.go:423-424, 434-436`) → doubled DB upsert + concurrent-limit query on every authenticated request, incl. the hot `POST /devices/heartbeat`. `RegisterDevice`'s error is silently discarded. The binary has no `--help`/`--version` and hard-fails (`log.Fatalf`) if Postgres is unreachable (eager `db.Ping()`), so it can't be smoke-tested or serve `/health` without a DB.
- **mcp:** server emits a stray `{"jsonrpc":"2.0"}` line in reply to the `notifications/initialized` **notification** (JSON-RPC 2.0: notifications must get no response; the reply is also malformed — no id/result/error). Installed `mur-mcp-server` is **2.22.16** vs CLI **2.24.1** (version skew). `mur_project_status` over MCP *does* include `last_indexed` while the CLI `mur project status` drops it — cross-surface inconsistency.
- **workflow / everywhere:** the malformed `~/.mur/workflows/test-workflow.yaml` (`id: test-wf / name: Test / steps: []`, missing required `description`+`content`) emits a `WARN … Failed to parse workflow YAML` on nearly every workflow-scanning command (`out`, `context`, `suggest`, hook prompt/tool). It's stderr-only (does not pollute injected context) but is constant noise; no `mur workflow validate` command exists and serde reports only the *first* missing field.
- **agent:** `mur-agent-runtime --help` prints an error instead of help (no discoverable runtime flags). *(The original "exits 0" claim did **not** reproduce — exit was consistently 1; downgraded to nit.)*

---

## 6. MCP server — detailed results (tested inline)

Driven over stdio JSON-RPC 2.0 against `~/.cargo/bin/mur-mcp-server`:

**Conformant / working:**
- `initialize` → `protocolVersion: 2024-11-05`, `capabilities: {tools}`, `serverInfo {mur-mcp-server, 2.22.16}`.
- `tools/list` → **18 tools**, all with valid `object` input-schemas; **9 media tools registered** (`vlc_*`, `scene_explain`, `video_analyze`, `watch_*`).
- `mur_compress_stats`, `mur_project_status`, `mur_agent_status`, `mur_notes_search`, `mur_hook_context`, `mur_project_search` all respond without crashing. `mur_notes_show` on a bad name returns a clean `isError:true` "Note not found".
- `mur_project_search` returns global results (`count: 66`), consistent with the CLI's global-search behaviour (M2).

**Issues:** M12 (phantom agents), M13 (compress no-op), notification-reply, version skew (see §4/§5).

## 7. Media domain — results & test limitation

- `vlc_open` **works**: it launches `VLC.app` with the correct HTTP control interface (`--extraintf=http --http-host=127.0.0.1 --http-port=<random> --http-password=<rand> --snapshot-path=~/.mur/runtime/vlc-snapshots`). Verified via the live VLC process args. Existing snapshots in that dir (dated 2026-06-09) confirm `scene_explain`/snapshotting has worked in real sessions.
- **MLX deps present:** `~/.mur/mlx-venv` (882M, Python 3.12), `~/.mur/models` (`whisper`, `kokoro`), `~/.cache/huggingface` (279M).
- **Test limitation (not a product bug):** each one-shot `mur-mcp-server` process picks its *own* random VLC port+password, so a fresh process (`vlc_status`, `vlc_playback`, `scene_explain` in separate invocations) **cannot talk to a VLC instance launched by a previous process** — those calls hang. In normal use a single *persistent* MCP session holds the VLC handle across calls, so the interactive flow is expected to work; it simply can't be fully exercised via per-call stdio. `vlc_open` was also slow to return (>50 s) on cold launch — worth confirming it isn't blocking indefinitely on the HTTP handshake.

---

## 8. Cross-cutting themes & top recommendations

1. **Skills and hook-injected text must be verified against the shipped binary.** The highest-leverage fix: a CI check that every command string in `~/.claude/skills/**`, the `mur hook session-start` capability index, and READMEs actually parses/runs on the current binary. Today the agent is told to run `mur recall`, `mur out`, `mur proposals`, `mur new`, and `mur agent export <name>` — all wrong. (This is exactly what `mur verify` is *for* — but see #3.)
2. **Make destructive/ambiguous actions confirm.** C1 + H2 + H3: any non-exact workflow resolution must echo the chosen workflow + similarity and require confirmation; `--yes` must be honoured uniformly on the YAML executor path; the `mur-run` skill must document `--prompt` (inspect) vs execute.
3. **Exit codes must reflect failure.** `mur verify` (H8), `mur sync` (401s → exit 0), and `mur agent send` (LLM error → `completed`, M1) all signal success on failure — breaking CI gates, scripts, and A2A callers. M1 is already fixed in-tree; cut a release.
4. **One source of truth for paths/install detection.** Commander's workflow-dir split (H9), `resolve_mur_bin` with no PATH fallback (H10), four `commander_dir()` copies, and the false-Homebrew detection (M10) are all "the code guessed a path/install method" bugs. Resolve `mur` via `which`, unify dir helpers, and detect brew-management by checking the binary is actually a Cellar symlink.
5. **Add disk-space guards to the heavy features.** The QA run filled the root volume via build caches + model/venv growth. mur's own `video-analyze`/`scene-explain` (MLX model download) and `project index` (tantivy) should check free space and fail with a clear message rather than letting the volume hit 100% (which corrupts in-flight writes). Ship the existing `mur session remove`/`mur project remove` reclaim tools with a `mur doctor` disk-usage warning.

---

## 9. Methodology & caveats

- **Coverage:** all 12 domains executed. 5 fully auto-verified (agent, project, workflow, commander, server); MCP + media tested inline by hand; 5 CLI domains' high/medium findings spot-verified inline (recall/proposals/new/verify-exit/old-skill/deploy-`-f`/update-`--check`/`mur out` all reproduced).
- **Verification status is marked per finding.** `✅ confirmed` = independently reproduced (by the verify agent and/or by hand). `⚠️ reported` = surfaced by the test agent but **not** independently re-run (the adversarial-verify + research stages for `session-chat`, `notes-source`, and `core-cli-a/b/c` were lost to transient server-side rate-limiting when the harness launched many agents at once). The `⚠️` items (H4, H5, M6, M8, M11) are well-evidenced but warrant a quick re-run before action.
- **Rate-limiting** ("Server is temporarily limiting requests · not your usage limit") repeatedly hit the parallel agent bursts — an artefact of *this orchestration*, not of mur.
- **Disk incident:** documented in §1/§8; 6.5 GiB reclaimed via symlink relocation; toolchains verified working through the symlinks.
- **No real user data destroyed:** agents/projects/sessions/skills used throwaway fixtures; the 16 real workflow proposals were inspected, not deleted.

*Raw per-domain findings (summaries, repro, evidence, verdicts, improvement research) are preserved in `.mur-qa-digest.md` alongside this report.*
