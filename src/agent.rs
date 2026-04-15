use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use std::process::Stdio;

use crate::config::Config;

/// Run an agent inside the named jail and return its output.
/// Pipes the prompt via stdin to the configured claude script in non-interactive mode.
pub async fn run(jail_name: &str, model_profile: &str, prompt: &str, cfg: &Config) -> Result<String> {
    let profile_arg = match model_profile {
        "minimax" => "minimax",
        _         => "paid",
    };

    let mut child = Command::new("doas")
        .args([
            "/usr/sbin/jexec",
            "-U", "tcovert",
            jail_name,
            &cfg.claude_script,
            profile_arg,
            "-p",
            "--dangerously-skip-permissions",
        ])
        .env("PATH", "/usr/local/bin:/usr/bin:/bin:/home/tcovert/.cargo/bin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("jexec agent")?;

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
