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
        let value = serde_json::json!({
            "task_id": req.task_id,
            "description": req.description,
            "model_profile": req.model_profile,
            "fallback_profiles": req.fallback_profiles,
            "status": "pending",
        })
        .to_string();

        let mut client = self.etcd.lock().await;
        match client.put(key.clone(), value, None).await {
            Ok(_) => format!("Task {} queued with profile '{}'", req.task_id, req.model_profile),
            Err(e) => format!("Failed to queue task {}: {}", req.task_id, e),
        }
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
