# Warden — Agentic Orchestration Refresher

**Purpose of this file:** give a cold-context reader (human or Claude) what they need to extend, refactor, or *use* the agentic framework. Read this before making architectural changes.

## What warden is

Rust MCP stdio server that orchestrates agent tasks on FreeBSD. Each non-trivial task runs in an ephemeral ZFS-cloned jail, coordinated through etcd, and journaled to a local SQLite history. Cheap monitoring tasks bypass the jail entirely via a "host-executor" fast path.

Current status: **source ready, not deployed.** Binary is built in the warden jail but is not installed at `~/.local/bin/warden` and not registered with Claude Code. See `docs/runbook.md` for the deploy plan.

## The three-box agentic framework

| Component | Role | Shipped? | Key files |
|---|---|---|---|
| **warden** (this repo) | MCP stdio server — task orchestration, jail lifecycle, memory access | Not yet deployed | `src/main.rs`, `src/jail.rs`, `src/agent.rs`, `src/cleanup.rs`, `src/config.rs` |
| **orchestrator-mcp** (`/jails/warden/home/tcovert/src/orchestrator-mcp`) | Predecessor MCP server — no jail isolation, host-only. Currently the registered MCP server in Claude Code. Being absorbed into warden. | Deployed at `~/.local/bin/orchestrator-mcp` | `src/main.rs` |
| **claude-proxy** (`~/src/public_github/claude-proxy`) | HTTP reverse proxy with multi-backend failover for Anthropic API calls. Not an MCP server. Complements warden, doesn't replace it. | Build-ready | `src/main.rs`, `src/proxy.rs`, `src/config.rs`, `src/metrics.rs`, `src/mgmt.rs` |

## Tools warden exposes (as of merge)

Eight MCP tools, four inherited from the original warden design and four absorbed from orchestrator-mcp:

| Tool | Synchronous? | What it does |
|---|---|---|
| `spawn_agent` | Async (returns task_id) | Queue an agent task in an isolated jail (or host for `pi-*` profiles). Writes to etcd; full lifecycle runs in a background tokio task. |
| `get_task` | Sync | Current state of a task. Reads etcd first (live), falls back to `task_log.sqlite` (history). |
| `wait_for_task` | Sync (blocks) | Poll etcd every 2s until a task hits a terminal state (completed/failed/orphaned) or deadline. |
| `watch_task` | Sync (blocks up to timeout) | Push-based variant using etcd watch stream — returns on any state change, not just terminal. |
| `kill_task` | Sync | Cancel an in-flight task; stop/destroy jail, mark etcd as cancelled. |
| `list_tasks` | Sync | Recent agent tasks from `task_log.sqlite`, optionally filtered by cwd. |
| `pi_prompt` | Sync | Quick no-tool prompt to pi (MiniMax via OpenRouter). **Only for short queries.** Substantive work should use `spawn_agent` with `model_profile="pi-minimax"`. |
| `memory_recall` | Sync | Semantic search over the agent memory store (via the `agent` CLI's embedding index). |

## Model profiles (model_profile argument to spawn_agent)

Two execution classes:

### Jail-executor (full jail lifecycle)
- `anthropic-oauth` — real Claude Sonnet via **claude-proxy** on the host (`127.0.0.1:3180`). Jails share the host network stack, so the proxy is directly reachable. claude-proxy strips caller auth and injects an OAuth bearer from `~/.claude-personal/.credentials.json` (with `~/.claude/.credentials.json` as failover). This is the path that unlocks multi-account Anthropic failover for jailed agents.
- `anthropic` — Claude Code via the `claude-openrouter` wrapper script inside the jail. **Misnamed — actually routes to gemma-4-31b-it via OpenRouter.** Default, preserved for backward compatibility.
- `openrouter` — generic OpenRouter routing (same wrapper, different profile inside).
- `minimax` — MiniMax model via OpenRouter.

### Host-executor (no jail — `agent.rs::is_host_executor` returns true)
- `pi-minimax` — runs `pi` directly on the host with `minimax/minimax-m2.7`. Cheap, fast, no isolation.
- `pi-gemma` — same but `google/gemma-4-31b-it`.

Adding a new profile: update `src/agent.rs::is_host_executor` if host-only, otherwise extend `run_in_jail`'s profile→argument mapping. Update this doc.

## Data surfaces

| Store | Path | Who writes | Who reads | Lifetime |
|---|---|---|---|---|
| etcd | `http://127.0.0.1:2379`, keys under `/warden/tasks/{task_id}` | spawn_agent, kill_task, background lifecycle | get_task, watch_task, wait_for_task | In-memory, per-boot |
| task_log.sqlite | `~/.local/share/task_log.sqlite` | orchestrator-mcp (legacy), `task_log` CLI, agent delegations | list_tasks, get_task (fallback) | Persistent |
| agent.sqlite | `~/.local/share/agent.sqlite` | `agent memory add/update/reindex` | memory_recall (via `agent memory recall --json`) | Persistent |
| Embeddings | `memory_embeddings` table in agent.sqlite, BLOB f32 | `agent memory add/update/reindex` (Qwen3-8B via OpenRouter by default) | `agent memory recall` | Persistent |

## Environment variables

### Consumed by warden
- `WARDEN_*` — overrides any `Config` field (figment env-prefix loader).
- `HOME` — used throughout for path resolution.

### Consumed by spawned agents / subprocesses
- `OPENROUTER_API_KEY` — required for OpenRouter-backed profiles (`openrouter`, `minimax`, `pi-*`).
- `AGENT_EMBED_MODEL` (default `baai/bge-large-en-v1.5`) — embedding model for `agent memory recall` / `memory_recall` tool.
- `AGENT_EMBED_DIMS` (default 1024) — dimension count. For `qwen/qwen3-embedding-8b` set to **4096**.
- `ANTHROPIC_BASE_URL` — if set, Anthropic-backed profiles route through this URL. Used to interpose claude-proxy.

Persistent values for an individual user live in `~/.zsh/zenvironment` (owner-only perms; contains the secret).

## Config

`src/config.rs` uses `figment` layered sources (later wins):
1. Built-in defaults (sensible for this host).
2. `/etc/warden/config.toml` (optional, system-wide).
3. `~/.config/warden/config.toml` (optional, per-user).
4. `WARDEN_*` environment variables.

Key fields: `base_dataset` (ZFS template), `jails_dataset` (parent), `jail_conf_dir`, `etcd_endpoints`, `claude_script` (path inside jail), `nullfs_mounts` (read/write pass-through mounts — ssh keys, git/jj config, claude-openrouter wrapper).

## How claude-proxy fits in

claude-proxy is **infrastructure under** warden, not a peer. It sits on the wire between Claude Code (and any jailed agent) and `api.anthropic.com`.

**What it does:**
- HTTP reverse proxy (axum + reqwest) with multiple named backends, each with its own credentials file.
- Failover list (`failover.order`) + trigger statuses (`failover.triggers`, e.g. 429, 503, 529): on a trigger response, it transparently moves to the next backend.
- 401 handling: reloads OAuth token from credentials file, retries once with fresh token.
- Metrics (requests, failovers, last status per backend) exposed via optional mgmt HTTP API when `CLAUDE_PROXY_MGMT=host:port` is set.
- Fault injection rules (live-editable via mgmt API) for chaos testing.

**How warden can use it:**
1. **Transparent reliability for Anthropic profiles.** Point jailed agents' `ANTHROPIC_BASE_URL` at the proxy. No warden code changes needed — just pass the env var through `nullfs_mounts` or spawn env.
2. **Routing signal.** Warden can read claude-proxy's mgmt API (`GET /metrics`) to detect sustained Anthropic degradation, and prefer `pi-minimax` for triage until recovery. Not implemented yet — would live in `src/main.rs` as a periodic health check informing default profile selection.
3. **Shared failure observability.** Pushing claude-proxy failover events into etcd (same cluster warden uses) would let task-level and request-level reliability share one observability surface. Not implemented.

**What it is NOT:**
- Not an MCP tool. It has no `rmcp` dependencies.
- Not a replacement for warden's orchestration — warden decides *which agent runs where*; claude-proxy decides *which Anthropic endpoint answers*.

## Deployment plan (phased, non-destructive)

**Phase 1 (this commit): feature parity on the bench.** ✓ DONE
- Tools absorbed: memory_recall, list_tasks, pi_prompt, watch_task.
- `get_task` reads etcd → task_log.sqlite as fallback.
- Build succeeds in the warden jail.

**Phase 2: deploy side-by-side.** PENDING
- `cp target/release/warden ~/.local/bin/warden`
- Register as a second MCP server in `~/.claude-personal/.claude.json` alongside `orchestrator`.
- Fresh Claude Code session exposes both — verify warden tools work identically.

**Phase 3: cutover.** PENDING
- Remove `orchestrator` entry from `.claude.json`.
- Retire `~/.local/bin/orchestrator-mcp` after a week of no-issues.

## Adding a new tool

1. Define a request struct with `#[derive(Debug, Deserialize, Serialize, JsonSchema)]` near the other request types in `src/main.rs`.
2. Add an `async fn` in the `#[tool_router] impl WardenServer {}` block, decorated with `#[tool(name = ..., description = ...)]`. Takes `Parameters(req): Parameters<YourRequest>`, returns `String` (JSON).
3. For operations that shell out to other host binaries (`agent`, `pi`, `orchestrate`), use `tokio::process::Command` with `Stdio::null()` for stdin. Capture stdout/stderr for the response body.
4. Update this doc. Update `docs/runbook.md` if the tool has a deploy-time side effect.

## Known risks / rough edges

- `spawn_agent`'s background task runs even after the MCP pipe closes. The `active_tasks` counter + `Notify` on `tasks_done` handle graceful shutdown, but long-running tasks are at the mercy of host uptime.
- `kill_task` calls `cleanup::destroy_orphan` which issues `doas`/ZFS/jail commands — requires the running user to have the right sudoers rules.
- `pi_prompt` is synchronous and blocks the MCP pipe. If pi invokes tool calls it can stall the session for minutes. The tool description says so; honor it. Use `spawn_agent` with `pi-minimax` for anything that might take tools.
- `task_log.sqlite` has no schema migrations in this repo — it's written by the external `task_log` Python CLI. If its schema changes, `read_task_log_entry`/`read_recent_tasks` will break silently.
- Embedding model defaults are `bge-large-en-v1.5` (1024 dims) but the agent corpus was indexed with Qwen3-8B (4096 dims). Set `AGENT_EMBED_MODEL`/`AGENT_EMBED_DIMS` in the warden process's env to match, otherwise `memory_recall` returns an empty-result error.

## Related reading

- `docs/runbook.md` — deploy procedure (ZFS dataset seed, jail template, etcd boot, MCP registration).
- `README.md` — high-level pitch.
- `~/src/agent_tools/agent/src/embed.rs` — embedder implementation used by `memory_recall`.
- `~/src/public_github/claude-proxy/src/proxy.rs` — failover loop logic.
