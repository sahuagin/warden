use anyhow::{Context, Result, bail};
use std::path::PathBuf;
use tokio::fs;
use tokio::process::Command;

const BASE_DATASET: &str = "zroot/jails/warden";
const JAILS_DATASET: &str = "zroot/jails";
const JAILS_PATH: &str = "/jails";
const JAIL_CONF_DIR: &str = "/home/tcovert/.config/warden/jails";

// nullfs mounts to include in every ephemeral jail
const NULLFS_MOUNTS: &[(&str, &str, &str)] = &[
    ("/home/tcovert/.ssh", "home/tcovert/.ssh", "ro"),
    ("/home/tcovert/.config/jj", "home/tcovert/.config/jj", "rw"),
    (
        "/home/tcovert/.config/git",
        "home/tcovert/.config/git",
        "rw",
    ),
    ("/home/tcovert/.gitconfig", "home/tcovert/.gitconfig", "rw"),
    (
        "/home/tcovert/src/claude-openrouter",
        "home/tcovert/src/claude-openrouter",
        "ro",
    ),
    (
        "/home/tcovert/.claude-openrouter",
        "home/tcovert/.claude-openrouter",
        "rw",
    ),
];

pub struct JailHandle {
    pub task_id: String,
    pub dataset: String,
    pub jail_name: String,
    pub conf_path: PathBuf,
}

/// Write a jail(8) config file for this task jail.
async fn write_conf(jail_name: &str, mountpoint: &str) -> Result<PathBuf> {
    fs::create_dir_all(JAIL_CONF_DIR)
        .await
        .context("create jail conf dir")?;

    let conf_path = PathBuf::from(JAIL_CONF_DIR).join(format!("{}.conf", jail_name));

    let mut mounts = String::new();
    for (host, rel, mode) in NULLFS_MOUNTS {
        mounts.push_str(&format!(
            "  mount += \"{host} {mountpoint}/{rel} nullfs {mode} 0 0\";\n",
            host = host,
            mountpoint = mountpoint,
            rel = rel,
            mode = mode,
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
pub async fn create(task_id: &str) -> Result<JailHandle> {
    let jail_name = format!("warden-{}", task_id);
    let snapshot = format!("{}@{}", BASE_DATASET, jail_name);
    let dataset = format!("{}/{}", JAILS_DATASET, jail_name);
    let mountpoint = format!("{}/{}", JAILS_PATH, jail_name);

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

    // Create mount point directories that don't exist in the base clone
    let mount_dirs = [
        "home/tcovert/src/claude-openrouter",
        "home/tcovert/.claude-openrouter",
    ];
    for dir in &mount_dirs {
        fs::create_dir_all(format!("{}/{}", mountpoint, dir))
            .await
            .with_context(|| format!("create mountpoint dir {}", dir))?;
    }

    // Disable services that should only run in the warden jail, not worker jails
    let status = Command::new("doas")
        .args(["/usr/sbin/sysrc", "-f", &format!("{}/etc/rc.conf", mountpoint), "etcd_enable=NO"])
        .status()
        .await
        .context("sysrc etcd_enable=NO")?;
    if !status.success() {
        bail!("sysrc etcd_enable=NO failed");
    }

    // Write jail config
    let conf_path = write_conf(&jail_name, &mountpoint).await?;

    Ok(JailHandle {
        task_id: task_id.to_string(),
        dataset,
        jail_name,
        conf_path,
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
    let snapshot = format!("{}@{}", BASE_DATASET, handle.jail_name);

    // Destroy the clone dataset
    let status = Command::new("zfs")
        .args(["destroy", &handle.dataset])
        .status()
        .await
        .context("zfs destroy dataset")?;
    if !status.success() {
        bail!("zfs destroy failed for {}", handle.dataset);
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
