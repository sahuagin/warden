/// Orphan cleanup binary for warden.
///
/// Lists and destroys jail datasets/snapshots left behind when warden exits mid-task.
///
/// Usage (from HOST, not warden jail):
///   warden-cleanup              -- list orphans and prompt before destroying
///   warden-cleanup --dry-run    -- list only, make no changes
///   warden-cleanup --yes        -- destroy without prompting

use anyhow::Result;
use warden::{cleanup, config::Config};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let yes = args.iter().any(|a| a == "--yes");

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("warden-cleanup [--dry-run] [--yes]");
        eprintln!("  --dry-run  list orphans without making any changes");
        eprintln!("  --yes      destroy without prompting for confirmation");
        return Ok(());
    }

    let cfg = Config::load()?;

    if dry_run {
        cleanup::cleanup_all(&cfg, true).await?;
        return Ok(());
    }

    // Find orphans first so we can prompt before destroying.
    let orphans = cleanup::find_orphans(&cfg).await?;

    if orphans.is_empty() {
        eprintln!("No orphaned warden jails found.");
        return Ok(());
    }

    eprintln!("Found {} orphaned jail(s):", orphans.len());
    for o in &orphans {
        let running = if o.jail_running { " [RUNNING]" } else { "" };
        eprintln!("  {}{}", o.jail_name, running);
        eprintln!("    dataset:  {}", o.dataset);
        eprintln!("    snapshot: {}", o.snapshot);
    }

    if !yes {
        eprint!("\nDestroy all of the above? [y/N] ");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        if !line.trim().eq_ignore_ascii_case("y") {
            eprintln!("Aborted.");
            return Ok(());
        }
    }

    eprintln!();
    let mut cleaned = 0;
    let mut failed = 0;
    for orphan in &orphans {
        eprintln!("Cleaning up {}...", orphan.jail_name);
        match cleanup::destroy_orphan(orphan, &cfg).await {
            Ok(()) => {
                eprintln!("  done.");
                cleaned += 1;
            }
            Err(e) => {
                eprintln!("  error: {}", e);
                failed += 1;
            }
        }
    }

    eprintln!("\nCleaned: {}  Failed: {}", cleaned, failed);
    if failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}
