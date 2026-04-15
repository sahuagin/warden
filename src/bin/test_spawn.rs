/// Standalone test binary for the jail lifecycle + agent execution.
/// Run from the host: cargo run --bin warden-test-spawn -- <task_id> <prompt>
/// Example: cargo run --bin warden-test-spawn -- test-local "say hello and report your hostname"

use anyhow::Result;
use warden::{agent, jail};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let task_id = args.get(1).map(|s| s.as_str()).unwrap_or("test-local");
    let prompt = args.get(2).map(|s| s.as_str()).unwrap_or("say hello and report your hostname");
    let model_profile = args.get(3).map(|s| s.as_str()).unwrap_or("openrouter");

    eprintln!("==> Creating jail for task '{}'", task_id);
    let handle = jail::create(task_id).await?;
    eprintln!("==> Jail dataset: {}", handle.dataset);

    eprintln!("==> Starting jail '{}'", handle.jail_name);
    jail::start(&handle).await?;
    eprintln!("==> Jail started");

    eprintln!("==> Running agent (profile: {})", model_profile);
    eprintln!("==> Prompt: {}", prompt);
    match agent::run(&handle.jail_name, model_profile, prompt).await {
        Ok(output) => {
            eprintln!("==> Agent completed successfully");
            println!("{}", output);
        }
        Err(e) => {
            eprintln!("==> Agent error: {}", e);
        }
    }

    eprintln!("==> Stopping jail");
    jail::stop(&handle).await?;

    eprintln!("==> Destroying jail");
    jail::destroy(&handle).await?;

    eprintln!("==> Done");
    Ok(())
}
