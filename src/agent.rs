use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use std::process::Stdio;

use crate::config::Config;

/// Returns true for profiles that run directly on the host (no jail).
pub fn is_host_executor(profile: &str) -> bool {
    matches!(profile, "pi-gemma" | "pi-minimax")
}

/// Run an agent: dispatches to jail or host executor based on the model profile.
pub async fn run(jail_name: &str, model_profile: &str, prompt: &str, cfg: &Config) -> Result<String> {
    if is_host_executor(model_profile) {
        run_host(model_profile, prompt).await
    } else {
        run_in_jail(jail_name, model_profile, prompt, cfg).await
    }
}

/// Host executor: runs pi directly on the host. No jail created or destroyed.
/// Used for cheap monitoring/triage tasks (pi-gemma, pi-minimax).
async fn run_host(model_profile: &str, prompt: &str) -> Result<String> {
    let key_path = "/home/tcovert/src/claude-openrouter/api-key";
    let or_key = std::fs::read_to_string(key_path)
        .context("read openrouter api key")?;
    let or_key = or_key.trim().to_string();

    let model = match model_profile {
        "pi-minimax" => "minimax/minimax-m2.7",
        _            => "google/gemma-4-31b-it",
    };

    let child = Command::new("/home/tcovert/.npm-packages/bin/pi")
        .args([
            "--provider", "openrouter",
            "--model", model,
            "--no-context-files",
            "-p", prompt,
        ])
        .env("OPENROUTER_API_KEY", or_key)
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/home/tcovert/.npm-packages/bin")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn pi agent")?;

    let output = child.wait_with_output().await.context("wait for pi agent")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("pi agent exited with {}: {}", output.status, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Jail executor: runs the agent inside the named jail via jexec.
/// Pipes the prompt via stdin to the configured claude script in non-interactive mode.
async fn run_in_jail(jail_name: &str, model_profile: &str, prompt: &str, cfg: &Config) -> Result<String> {
    let mut cmd = Command::new("doas");
    cmd.env("PATH", "/usr/local/bin:/usr/bin:/bin:/home/tcovert/.cargo/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    match model_profile {
        // Real Anthropic via claude-proxy. Jails share the host stack, so
        // 127.0.0.1:3180 from inside the jail reaches the proxy on the host.
        // claude-proxy strips Authorization/x-api-key and injects its own
        // OAuth bearer; the dummy ANTHROPIC_API_KEY only gets past CC's
        // client-side startup check.
        "anthropic-oauth" => {
            cmd.args([
                "/usr/sbin/jexec", "-U", "tcovert", jail_name,
                "/usr/bin/env",
                "ANTHROPIC_BASE_URL=http://127.0.0.1:3180",
                "ANTHROPIC_API_KEY=proxied",
                "/usr/local/bin/claude",
                "-p",
                "--dangerously-skip-permissions",
            ]);
        }
        other => {
            let profile_arg = if other == "minimax" { "minimax" } else { "paid" };
            cmd.args([
                "/usr/sbin/jexec", "-U", "tcovert", jail_name,
                &cfg.claude_script,
                profile_arg,
                "-p",
                "--dangerously-skip-permissions",
            ]);
        }
    }

    let mut child = cmd.spawn().context("jexec agent")?;

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await.context("write prompt")?;
    }

    let output = child.wait_with_output().await.context("wait for agent")?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        anyhow::bail!(
            "agent exited with {}: stdout={:?} stderr={:?}",
            output.status,
            stdout.trim(),
            stderr.trim(),
        );
    }

    Ok(stdout)
}
