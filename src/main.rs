use warden::{agent, jail};
use etcd_client::Client;
use rmcp::{ServerHandler, ServiceExt, handler::server::wrapper::Parameters, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

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

fn default_profile() -> String {
    "anthropic".to_string()
}

#[derive(Clone)]
pub struct WardenServer {
    etcd: Arc<Mutex<Client>>,
}

#[tool_router]
impl WardenServer {
    /// Spawn an agent to handle a task in an isolated jail.
    #[tool(
        name = "spawn_agent",
        description = "Spawn an isolated agent to handle a task. Returns when the agent completes."
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

        // Create jail
        let handle = match jail::create(&req.task_id).await {
            Ok(h) => h,
            Err(e) => {
                return format!("Failed to create jail for task {}: {}", req.task_id, e);
            }
        };

        // Start jail
        if let Err(e) = jail::start(&handle).await {
            let _ = jail::destroy(&handle).await;
            return format!("Failed to start jail for task {}: {}", req.task_id, e);
        }

        // Update status to running
        let value = serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "status": "running",
            "jail": handle.jail_name,
        })
        .to_string();

        {
            let mut client = self.etcd.lock().await;
            if let Err(e) = client.put(key.clone(), value, None).await {
                return format!("Jail started but failed to update etcd: {}", e);
            }
        }

        // Run agent inside jail
        let result = match agent::run(&handle.jail_name, &req.model_profile, &req.description).await {
            Ok(output) => output,
            Err(e) => format!("agent error: {}", e),
        };

        // Stop and destroy jail
        let _ = jail::stop(&handle).await;
        let _ = jail::destroy(&handle).await;

        // Write final result to etcd
        let value = serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "status": "completed",
            "result": result,
        })
        .to_string();

        {
            let mut client = self.etcd.lock().await;
            let _ = client.put(key, value, None).await;
        }

        format!("Task {} completed in jail '{}'", req.task_id, handle.jail_name)
    }
}

#[tool_handler]
impl ServerHandler for WardenServer {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let etcd = Client::connect(["127.0.0.1:2379"], None).await?;

    let server = WardenServer {
        etcd: Arc::new(Mutex::new(etcd)),
    };

    let transport = rmcp::transport::stdio();
    server.serve(transport).await?.waiting().await?;

    Ok(())
}
