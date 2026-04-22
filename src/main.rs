use warden::{agent, cleanup, config::Config, jail};
use etcd_client::{Client, WatchOptions};
use rmcp::{ServerHandler, ServiceExt, handler::server::wrapper::Parameters, tool, tool_handler, tool_router};
use rusqlite::Connection;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::process::Command;
use tokio::sync::{Mutex, Notify};

// ── Path helpers (absorbed from orchestrator-mcp) ─────────────────────────────

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/tcovert".into()))
}

fn agent_bin() -> PathBuf {
    home().join(".local/bin/agent")
}

fn task_log_db() -> PathBuf {
    home().join(".local/share/task_log.sqlite")
}

fn rpc_watcher_bin() -> PathBuf {
    home().join("src/pi-claude-poc/rpc_watcher.py")
}

const ETCD_KEY_PREFIX: &str = "/warden/tasks/";

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SpawnAgentRequest {
    /// A unique identifier for this task.
    pub task_id: String,
    /// The task description or prompt to give the agent.
    pub description: String,
    /// Model profile to use: "anthropic-oauth" (real Claude via claude-proxy),
    /// "anthropic" / "openrouter" (gemma-4-31b-it via OpenRouter), "minimax"
    /// (MiniMax via OpenRouter), or host-executor profiles "pi-minimax" /
    /// "pi-gemma" (no jail).
    #[serde(default = "default_profile")]
    pub model_profile: String,
    /// Fallback profiles to try if the primary fails, in order.
    #[serde(default)]
    pub fallback_profiles: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct GetTaskRequest {
    /// The task_id to look up.
    pub task_id: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct KillTaskRequest {
    /// The task_id to cancel.
    pub task_id: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WaitForTaskRequest {
    /// The task_id to wait for.
    pub task_id: String,
    /// Maximum seconds to wait before returning a timeout error. Default: 600.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WatchTaskRequest {
    /// task_id to watch.
    pub task_id: String,
    /// How long (in seconds) to block waiting for a change notification. Default: 10.
    #[serde(default = "default_watch_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct ListTasksRequest {
    /// Filter by working directory (partial match). Leave empty for all.
    #[serde(default)]
    pub cwd: String,
    /// Number of recent tasks to return. Default: 10.
    #[serde(default = "default_limit")]
    pub limit: u32,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct PiPromptRequest {
    /// Prompt to send to pi (MiniMax via OpenRouter).
    pub prompt: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct MemoryRecallRequest {
    /// Free-form natural-language query. Semantically nearest memories are returned.
    pub query: String,
    /// Number of results to return. Default: 5.
    #[serde(default = "default_recall_k")]
    pub k: u32,
    /// Restrict to a memory type: "user" | "feedback" | "project" | "reference".
    /// Leave empty for all types.
    #[serde(default)]
    pub r#type: String,
    /// Include the full content body (not just name/description). Default: false.
    #[serde(default)]
    pub full: bool,
}

fn default_profile() -> String {
    "anthropic".to_string()
}

fn default_timeout() -> u64 {
    600
}

fn default_watch_secs() -> u64 { 10 }
fn default_limit() -> u32 { 10 }
fn default_recall_k() -> u32 { 5 }

#[derive(Clone)]
pub struct WardenServer {
    etcd: Arc<Mutex<Client>>,
    cfg: Arc<Config>,
    /// Count of in-flight background tasks.
    active_tasks: Arc<AtomicUsize>,
    /// Notified when active_tasks drops to zero.
    tasks_done: Arc<Notify>,
}

#[tool_router]
impl WardenServer {
    /// Spawn an agent to handle a task in an isolated jail.
    /// Returns immediately with the task_id; use get_task to poll for completion.
    #[tool(
        name = "spawn_agent",
        description = "Queue an isolated agent to handle a task. Returns immediately; use get_task to poll for results."
    )]
    async fn spawn_agent(&self, Parameters(req): Parameters<SpawnAgentRequest>) -> String {
        let key = format!("/warden/tasks/{}", req.task_id);

        // Write task as pending
        let value = serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "fallback_profiles": req.fallback_profiles,
            "status": "pending",
        })
        .to_string();

        {
            let mut client = self.etcd.lock().await;
            if let Err(e) = client.put(key.clone(), value, None).await {
                return format!("Failed to queue task {}: {}", req.task_id, e);
            }
        }

        // Spawn background task — lifecycle continues even if the MCP pipe closes.
        self.active_tasks.fetch_add(1, Ordering::SeqCst);
        let worker = self.clone();
        let task_id = req.task_id.clone();
        tokio::spawn(async move {
            worker.run_task(req, key).await;
            if worker.active_tasks.fetch_sub(1, Ordering::SeqCst) == 1 {
                worker.tasks_done.notify_waiters();
            }
        });

        format!("Task {} queued", task_id)
    }

    /// Get the current state of a task. Reads live state from etcd first,
    /// then falls back to task_log.sqlite for historical/completed tasks.
    #[tool(
        name = "get_task",
        description = "Get the current status and result of a previously spawned task. Reads etcd (live) first, then task_log.sqlite (history)."
    )]
    async fn get_task(&self, Parameters(req): Parameters<GetTaskRequest>) -> String {
        let key = format!("{}{}", ETCD_KEY_PREFIX, req.task_id);
        let etcd_result = {
            let mut client = self.etcd.lock().await;
            client.get(key, None).await
        };

        match etcd_result {
            Ok(resp) => {
                if let Some(kv) = resp.kvs().first() {
                    return kv.value_str().unwrap_or("(invalid utf-8)").to_string();
                }
            }
            Err(e) => {
                eprintln!("get_task: etcd read failed (falling through to task_log): {e}");
            }
        }

        // Fallback: task_log.sqlite
        match read_task_log_entry(&req.task_id) {
            Ok(Some(row)) => row,
            Ok(None) => json!({"error": "task not found", "task_id": req.task_id}).to_string(),
            Err(e) => json!({"error": format!("task_log read error: {e}"), "task_id": req.task_id}).to_string(),
        }
    }

    /// Block until a task reaches a terminal state (completed or failed).
    /// Polls etcd every 2 seconds. Returns the final task JSON.
    #[tool(
        name = "wait_for_task",
        description = "Block until a task completes or fails. Returns the final task JSON. Use this after spawn_agent to chain dependent tasks."
    )]
    async fn wait_for_task(&self, Parameters(req): Parameters<WaitForTaskRequest>) -> String {
        let key = format!("/warden/tasks/{}", req.task_id);
        let deadline = tokio::time::Instant::now()
            + tokio::time::Duration::from_secs(req.timeout_secs);

        loop {
            // Poll etcd — release the lock between iterations so other tools aren't blocked.
            let value = {
                let mut client = self.etcd.lock().await;
                match client.get(key.clone(), None).await {
                    Err(e) => return format!("etcd error: {}", e),
                    Ok(resp) => resp
                        .kvs()
                        .first()
                        .and_then(|kv| kv.value_str().ok())
                        .map(|s| s.to_string()),
                }
            };

            if let Some(json) = value {
                if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&json) {
                    let status = obj.get("status").and_then(|s| s.as_str()).unwrap_or("");
                    if matches!(status, "completed" | "failed" | "orphaned") {
                        return json;
                    }
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return format!(
                    "{{\"error\": \"timeout\", \"task_id\": \"{}\", \"timeout_secs\": {}}}",
                    req.task_id, req.timeout_secs
                );
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    /// Cancel an in-flight task: stop its jail, destroy the dataset, mark etcd as cancelled.
    /// No-op if the task is already in a terminal state.
    #[tool(
        name = "kill_task",
        description = "Cancel a running or pending task. Stops its jail and cleans up resources."
    )]
    async fn kill_task(&self, Parameters(req): Parameters<KillTaskRequest>) -> String {
        let key = format!("/warden/tasks/{}", req.task_id);

        // Read current state
        let json = {
            let mut client = self.etcd.lock().await;
            match client.get(key.clone(), None).await {
                Err(e) => return format!("etcd error: {}", e),
                Ok(resp) => match resp.kvs().first() {
                    None => return format!("task '{}' not found", req.task_id),
                    Some(kv) => kv.value_str().unwrap_or("").to_string(),
                },
            }
        };

        let obj: serde_json::Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(_) => return format!("task '{}' has invalid state in etcd", req.task_id),
        };

        let status = obj.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if matches!(status, "completed" | "failed" | "cancelled" | "orphaned") {
            return format!("task '{}' already in terminal state: {}", req.task_id, status);
        }

        // Build an Orphan descriptor from etcd data and config.
        let jail_name = obj
            .get("jail")
            .and_then(|s| s.as_str())
            .unwrap_or(&format!("warden-{}", req.task_id))
            .to_string();

        let orphan = cleanup::Orphan {
            jail_name: jail_name.clone(),
            dataset: format!("{}/{}", self.cfg.jails_dataset, jail_name),
            snapshot: format!("{}@{}", self.cfg.base_dataset, jail_name),
            conf_path: std::path::PathBuf::from(&self.cfg.jail_conf_dir)
                .join(format!("{}.conf", jail_name)),
            jail_running: true, // attempt stop regardless; destroy_orphan handles the not-running case
        };

        if let Err(e) = cleanup::destroy_orphan(&orphan, &self.cfg).await {
            // Log the error but still mark cancelled in etcd.
            eprintln!("kill_task: cleanup error for {}: {}", req.task_id, e);
        }

        // Mark as cancelled
        let _ = self.write_etcd(&key, serde_json::json!({
            "task_id": req.task_id,
            "status": "cancelled",
        })).await;

        format!("task '{}' cancelled", req.task_id)
    }

    /// Watch an etcd key for a running task. Returns current state immediately,
    /// then blocks up to `timeout_secs` for a change notification (push-based).
    /// Prefer this over wait_for_task when you want responsiveness to any
    /// intermediate state change, not just terminal states.
    #[tool(
        name = "watch_task",
        description = "Get the live etcd state of a running task and block up to timeout_secs for an update (push-based). Returns current state immediately on any change or timeout."
    )]
    async fn watch_task(&self, Parameters(req): Parameters<WatchTaskRequest>) -> String {
        let etcd_key = format!("{}{}", ETCD_KEY_PREFIX, req.task_id);
        let timeout = std::time::Duration::from_secs(req.timeout_secs);

        let current = {
            let mut client = self.etcd.lock().await;
            match client.get(etcd_key.as_str(), None).await {
                Err(e) => return json!({"error": format!("etcd get failed: {e}"), "task_id": req.task_id}).to_string(),
                Ok(resp) => resp.kvs().first().and_then(|kv| {
                    String::from_utf8(kv.value().to_vec()).ok()
                        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                }),
            }
        };

        let current_val = match current {
            None => return json!({"error": "task not found in etcd", "task_id": req.task_id}).to_string(),
            Some(v) => v,
        };

        let status = current_val.get("status").and_then(|s| s.as_str()).unwrap_or("");
        if matches!(status, "completed" | "failed" | "cancelled" | "orphaned") {
            return json!({
                "source": "etcd_current",
                "changed": false,
                "state": current_val,
            }).to_string();
        }

        // Open a fresh client for the watch stream so we don't hold the shared
        // mutex for the full timeout duration.
        let endpoints: Vec<String> = self.cfg.etcd_endpoints.clone();
        let endpoints_ref: Vec<&str> = endpoints.iter().map(String::as_str).collect();
        let mut watch_client = match Client::connect(endpoints_ref, None).await {
            Ok(c) => c,
            Err(e) => return json!({
                "source": "etcd_current",
                "changed": false,
                "watch_error": format!("connect: {e}"),
                "state": current_val,
            }).to_string(),
        };

        let mut watch_stream = match watch_client.watch(etcd_key.as_str(), Some(WatchOptions::new())).await {
            Ok(s) => s,
            Err(e) => return json!({
                "source": "etcd_current",
                "changed": false,
                "watch_error": format!("watch: {e}"),
                "state": current_val,
            }).to_string(),
        };

        let maybe_update = tokio::time::timeout(timeout, async move {
            loop {
                match watch_stream.message().await {
                    Ok(Some(resp)) => {
                        if resp.created() { continue; }
                        if let Some(evt) = resp.events().first() {
                            if let Some(kv) = evt.kv() {
                                let val_str = String::from_utf8(kv.value().to_vec()).unwrap_or_default();
                                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&val_str) {
                                    return Some(v);
                                }
                            }
                        }
                    }
                    _ => return None,
                }
            }
        }).await;

        match maybe_update {
            Err(_elapsed) => json!({
                "source": "etcd_current",
                "changed": false,
                "timed_out": true,
                "state": current_val,
            }).to_string(),
            Ok(None) => json!({
                "source": "etcd_current",
                "changed": false,
                "state": current_val,
            }).to_string(),
            Ok(Some(new_val)) => json!({
                "source": "etcd_watch",
                "changed": true,
                "state": new_val,
            }).to_string(),
        }
    }

    /// List recent tasks from task_log.sqlite. Includes tasks from any source
    /// (warden spawn_agent, legacy orchestrator-mcp, direct task_log writes).
    #[tool(
        name = "list_tasks",
        description = "List recent agent tasks from task_log.sqlite. Filter by cwd (partial match) or leave empty for all."
    )]
    async fn list_tasks(&self, Parameters(req): Parameters<ListTasksRequest>) -> String {
        match read_recent_tasks(&req.cwd, req.limit) {
            Ok(rows) => serde_json::to_string(&rows).unwrap_or_else(|e| e.to_string()),
            Err(e) => json!({"error": e.to_string()}).to_string(),
        }
    }

    /// Send a prompt to pi (MiniMax via OpenRouter) synchronously and return
    /// the response. Only suitable for short queries that DON'T use tools —
    /// anything involving tool calls can take minutes and block the MCP pipe.
    /// Prefer spawn_agent with model_profile="pi-minimax" for substantive work.
    #[tool(
        name = "pi_prompt",
        description = "Send a quick prompt to pi (MiniMax/OpenRouter) synchronously. For short, no-tool queries only. Use spawn_agent for anything substantive."
    )]
    async fn pi_prompt(&self, Parameters(req): Parameters<PiPromptRequest>) -> String {
        let watcher = rpc_watcher_bin();
        match Command::new("python3")
            .arg(&watcher)
            .arg(&req.prompt)
            .stdin(Stdio::null())
            .output()
            .await
        {
            Err(e) => json!({"error": e.to_string()}).to_string(),
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                json!({
                    "output": stdout,
                    "stderr": stderr,
                    "exit_code": out.status.code(),
                }).to_string()
            }
        }
    }

    /// Semantic recall from the agent memory store. Uses the `agent` CLI's
    /// embedding index (Qwen3-Embedding-8B via OpenRouter by default). Surfaces
    /// memories by meaning — complements keyword/lexical search.
    #[tool(
        name = "memory_recall",
        description = "Search agent memory by semantic similarity to a natural-language query. Returns top-k memories with cosine scores. Set 'full' to include content bodies."
    )]
    async fn memory_recall(&self, Parameters(req): Parameters<MemoryRecallRequest>) -> String {
        let agent = agent_bin();
        if !agent.exists() {
            return json!({"error": format!("agent CLI not found at {}", agent.display())}).to_string();
        }

        let mut cmd = Command::new(&agent);
        cmd.arg("memory")
            .arg("recall")
            .arg(&req.query)
            .arg("--json")
            .arg("--k")
            .arg(req.k.to_string());
        if !req.r#type.is_empty() {
            cmd.arg("--type").arg(&req.r#type);
        }
        if req.full {
            cmd.arg("--full");
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let out = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return json!({"error": format!("spawn failed: {e}")}).to_string(),
        };

        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return json!({
                "error": format!("agent exited with status {}: {}", out.status.code().unwrap_or(-1), stderr),
            }).to_string();
        }

        let stdout = String::from_utf8_lossy(&out.stdout);
        match serde_json::from_str::<serde_json::Value>(stdout.trim()) {
            Ok(v) => v.to_string(),
            Err(e) => json!({
                "error": format!("agent output was not JSON: {e}"),
                "raw": stdout.trim(),
            }).to_string(),
        }
    }
}

impl WardenServer {
    /// Full jail lifecycle for a task. Called from a background tokio task.
    async fn run_task(&self, req: SpawnAgentRequest, key: String) {
        // Host-executor profiles (pi-gemma, pi-minimax) skip the jail lifecycle entirely.
        if agent::is_host_executor(&req.model_profile) {
            let _ = self.write_etcd(&key, serde_json::json!({
                "task_id": req.task_id,
                "description": req.description,
                "model_profile": req.model_profile,
                "status": "running",
            })).await;

            let result = match agent::run("", &req.model_profile, &req.description, &self.cfg).await {
                Ok(output) => output,
                Err(e) => format!("agent error: {}", e),
            };

            let _ = self.write_etcd(&key, serde_json::json!({
                "task_id": req.task_id,
                "description": req.description,
                "model_profile": req.model_profile,
                "status": "completed",
                "result": result,
            })).await;
            return;
        }

        // Create jail
        let handle = match jail::create(&req.task_id, &self.cfg).await {
            Ok(h) => h,
            Err(e) => {
                let _ = self.write_etcd(&key, serde_json::json!({
                    "task_id": req.task_id,
                    "status": "failed",
                    "error": format!("create jail: {}", e),
                })).await;
                return;
            }
        };

        // Start jail
        if let Err(e) = jail::start(&handle).await {
            let _ = jail::destroy(&handle).await;
            let _ = self.write_etcd(&key, serde_json::json!({
                "task_id": req.task_id,
                "status": "failed",
                "error": format!("start jail: {}", e),
            })).await;
            return;
        }

        // Update status to running
        let _ = self.write_etcd(&key, serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "status": "running",
            "jail": handle.jail_name,
        })).await;

        // Run agent inside jail
        let result = match agent::run(&handle.jail_name, &req.model_profile, &req.description, &self.cfg).await {
            Ok(output) => output,
            Err(e) => format!("agent error: {}", e),
        };

        // Stop and destroy jail
        let _ = jail::stop(&handle).await;
        let _ = jail::destroy(&handle).await;

        // Write final result to etcd
        let _ = self.write_etcd(&key, serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "status": "completed",
            "result": result,
        })).await;
    }

    async fn write_etcd(&self, key: &str, value: serde_json::Value) -> anyhow::Result<()> {
        let mut client = self.etcd.lock().await;
        client.put(key, value.to_string(), None).await?;
        Ok(())
    }
}

#[tool_handler]
impl ServerHandler for WardenServer {}

// ── SQLite helpers (task_log.sqlite, read-only) ──────────────────────────────

fn read_task_log_entry(id: &str) -> anyhow::Result<Option<String>> {
    let db = task_log_db();
    if !db.exists() { return Ok(None); }
    let conn = Connection::open(&db)?;
    let result = conn.query_row(
        "SELECT id, description, status, result, tags, cwd, created_at
         FROM tasks WHERE id = ?1",
        [id],
        |r| Ok(json!({
            "id":          r.get::<_, String>(0)?,
            "description": r.get::<_, String>(1)?,
            "status":      r.get::<_, String>(2)?,
            "result":      r.get::<_, Option<String>>(3)?,
            "tags":        r.get::<_, Option<String>>(4)?,
            "cwd":         r.get::<_, String>(5)?,
            "created_at":  r.get::<_, String>(6)?,
        }).to_string()),
    );
    match result {
        Ok(s) => Ok(Some(s)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn read_recent_tasks(cwd_filter: &str, limit: u32) -> anyhow::Result<Vec<serde_json::Value>> {
    let db = task_log_db();
    if !db.exists() { return Ok(vec![]); }
    let conn = Connection::open(&db)?;
    let pat = if cwd_filter.is_empty() { "%".to_string() } else { format!("%{}%", cwd_filter) };
    let mut stmt = conn.prepare(
        "SELECT id, description, status, result, tags, cwd, created_at
         FROM tasks WHERE cwd LIKE ?1
         ORDER BY created_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![pat, limit], |r| {
        Ok(json!({
            "id":          r.get::<_, String>(0)?,
            "description": r.get::<_, String>(1)?,
            "status":      r.get::<_, String>(2)?,
            "result":      r.get::<_, Option<String>>(3)?,
            "tags":        r.get::<_, Option<String>>(4)?,
            "cwd":         r.get::<_, String>(5)?,
            "created_at":  r.get::<_, String>(6)?,
        }))
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    let endpoints: Vec<&str> = cfg.etcd_endpoints.iter().map(String::as_str).collect();
    let etcd = Client::connect(endpoints, None).await?;

    let active_tasks = Arc::new(AtomicUsize::new(0));
    let tasks_done = Arc::new(Notify::new());

    let server = WardenServer {
        etcd: Arc::new(Mutex::new(etcd)),
        cfg: Arc::new(cfg),
        active_tasks: active_tasks.clone(),
        tasks_done: tasks_done.clone(),
    };

    let transport = rmcp::transport::stdio();
    server.serve(transport).await?.waiting().await?;

    // MCP pipe closed — wait for any in-flight background tasks to finish.
    while active_tasks.load(Ordering::SeqCst) > 0 {
        tasks_done.notified().await;
    }

    Ok(())
}
