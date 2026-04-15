use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use tokio::fs;
use tokio::process::Command;

use crate::config::Config;

pub struct JailHandle {
    pub task_id: String,
    pub dataset: String,
    pub jail_name: String,
    pub conf_path: PathBuf,
    /// Stored at create time so destroy can reconstruct the snapshot name.
    pub base_dataset: String,
}

/// Write a jail(8) config file for this task jail.
async fn write_conf(cfg: &Config, jail_name: &str, mountpoint: &str) -> Result<PathBuf> {
    fs::create_dir_all(&cfg.jail_conf_dir)
        .await
        .context("create jail conf dir")?;

    let conf_path = PathBuf::from(&cfg.jail_conf_dir).join(format!("{}.conf", jail_name));

    let mut mounts = String::new();
    for m in &cfg.nullfs_mounts {
        mounts.push_str(&format!(
            "  mount += \"{host} {mountpoint}/{jail} nullfs {mode} 0 0\";\n",
            host = m.host,
            mountpoint = mountpoint,
            jail = m.jail,
            mode = m.mode,
        ));
    }

    let conf = format!(
        "{jn} {{\n  host.hostname = {jn};\n  path = {mp};\n  exec.start = \"/bin/sh /etc/rc\";\n  exec.stop = \"/bin/sh /etc/rc.shutdown\";\n  mount.devfs;\n  allow.raw_sockets = 1;\n  ip4 = inherit;\n{mounts}}}\n",
        jn = jail_name,
        mp = mountpoint,
        mounts = mounts,
    );

    fs::write(&conf_path, conf)
        .await
        .context("write jail conf")?;

    Ok(conf_path)
}

/// Snapshot the base dataset and clone it for a new task jail.
pub async fn create(task_id: &str, cfg: &Config) -> Result<JailHandle> {
    let jail_name = format!("warden-{}", task_id);
    let snapshot = format!("{}@{}", cfg.base_dataset, jail_name);
    let dataset = format!("{}/{}", cfg.jails_dataset, jail_name);
    let mountpoint = format!("{}/{}", cfg.jails_path, jail_name);

    // Snapshot the base
    let status = Command::new("zfs")
        .args(["snapshot", &snapshot])
        .status()
        .await
        .context("zfs snapshot")?;
    if !status.success() {
        bail!("zfs snapshot failed for {}", snapshot);
    }

    // Clone snapshot to new dataset
    let status = Command::new("zfs")
        .args(["clone", &snapshot, &dataset])
        .status()
        .await
        .context("zfs clone")?;
    if !status.success() {
        bail!("zfs clone failed: {} -> {}", snapshot, dataset);
    }

    // Set mountpoint (requires root to create the mount directory)
    let status = Command::new("doas")
        .args([
            "/sbin/zfs",
            "set",
            &format!("mountpoint={}", mountpoint),
            &dataset,
        ])
        .status()
        .await
        .context("zfs set mountpoint")?;
    if !status.success() {
        bail!("zfs set mountpoint failed for {}", dataset);
    }

    // Create mount point directories that don't exist in the base clone.
    // Skip paths that already exist (e.g. files like .gitconfig cloned from the base).
    for m in &cfg.nullfs_mounts {
        let full = format!("{}/{}", mountpoint, m.jail);
        if fs::metadata(&full).await.is_err() {
            fs::create_dir_all(&full)
                .await
                .with_context(|| format!("create mountpoint dir {}", m.jail))?;
        }
    }

    // Disable services that should only run in the warden jail, not worker jails.
    let status = Command::new("doas")
        .args([
            "/usr/sbin/sysrc",
            "-f",
            &format!("{}/etc/rc.conf", mountpoint),
            "etcd_enable=NO",
        ])
        .status()
        .await
        .context("sysrc etcd_enable=NO")?;
    if !status.success() {
        bail!("sysrc etcd_enable=NO failed");
    }

    // Write jail config
    let conf_path = write_conf(cfg, &jail_name, &mountpoint).await?;

    Ok(JailHandle {
        task_id: task_id.to_string(),
        dataset,
        jail_name,
        conf_path,
        base_dataset: cfg.base_dataset.clone(),
    })
}

/// Start the jail using jail(8) with our generated config file.
pub async fn start(handle: &JailHandle) -> Result<()> {
    let status = Command::new("doas")
        .args([
            "/usr/sbin/jail",
            "-f",
            handle.conf_path.to_str().unwrap(),
            "-c",
            &handle.jail_name,
        ])
        .status()
        .await
        .context("jail -f -c")?;
    if !status.success() {
        bail!("failed to start jail {}", handle.jail_name);
    }
    Ok(())
}

/// Stop the jail.
pub async fn stop(handle: &JailHandle) -> Result<()> {
    let status = Command::new("doas")
        .args([
            "/usr/sbin/jail",
            "-f",
            handle.conf_path.to_str().unwrap(),
            "-r",
            &handle.jail_name,
        ])
        .status()
        .await
        .context("jail -f -r")?;
    if !status.success() {
        bail!("failed to stop jail {}", handle.jail_name);
    }
    Ok(())
}

/// Destroy the jail dataset and its origin snapshot.
pub async fn destroy(handle: &JailHandle) -> Result<()> {
    let snapshot = format!("{}@{}", handle.base_dataset, handle.jail_name);

    // Destroy the clone dataset — use -f to forcibly unmount before destroy.
    // Retry because devfs/nullfs mounts may still be settling after jail stop.
    let mut destroyed = false;
    for attempt in 0..10 {
        let status = Command::new("zfs")
            .args(["destroy", "-f", &handle.dataset])
            .status()
            .await
            .context("zfs destroy dataset")?;
        if status.success() {
            destroyed = true;
            break;
        }
        if attempt < 9 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }
    if !destroyed {
        bail!("zfs destroy failed for {} after retries", handle.dataset);
    }

    // Destroy the origin snapshot
    let status = Command::new("zfs")
        .args(["destroy", &snapshot])
        .status()
        .await
        .context("zfs destroy snapshot")?;
    if !status.success() {
        bail!("zfs destroy failed for {}", snapshot);
    }

    // Remove config file
    let _ = fs::remove_file(&handle.conf_path).await;

    Ok(())
}
