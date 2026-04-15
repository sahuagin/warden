use warden::{agent, cleanup, config::Config, jail};
use etcd_client::Client;
use rmcp::{ServerHandler, ServiceExt, handler::server::wrapper::Parameters, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Mutex, Notify};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SpawnAgentRequest {
    /// A unique identifier for this task.
    pub task_id: String,
    /// The task description or prompt to give the agent.
    pub description: String,
    /// Model profile to use: "anthropic", "openrouter", or "minimax".
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

fn default_profile() -> String {
    "anthropic".to_string()
}

fn default_timeout() -> u64 {
    600
}

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

    /// Get the current state of a task from etcd.
    #[tool(
        name = "get_task",
        description = "Get the current status and result of a previously spawned task."
    )]
    async fn get_task(&self, Parameters(req): Parameters<GetTaskRequest>) -> String {
        let key = format!("/warden/tasks/{}", req.task_id);
        let mut client = self.etcd.lock().await;
        match client.get(key, None).await {
            Err(e) => format!("etcd error: {}", e),
            Ok(resp) => match resp.kvs().first() {
                None => format!("task '{}' not found", req.task_id),
                Some(kv) => kv.value_str().unwrap_or("(invalid utf-8)").to_string(),
            },
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
}

impl WardenServer {
    /// Full jail lifecycle for a task. Called from a background tokio task.
    async fn run_task(&self, req: SpawnAgentRequest, key: String) {
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
