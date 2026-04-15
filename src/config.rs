use anyhow::Result;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/home/tcovert".into())
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NullfsMount {
    /// Absolute path on the host.
    pub host: String,
    /// Path relative to the jail root.
    pub jail: String,
    /// Mount mode: "ro" or "rw".
    pub mode: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// ZFS dataset used as the base template for worker jail clones.
    /// e.g. "zroot/jails/warden"
    pub base_dataset: String,
    /// Parent ZFS dataset under which worker datasets are created.
    /// e.g. "zroot/jails"
    pub jails_dataset: String,
    /// Filesystem path where jail datasets are mounted.
    /// e.g. "/jails"
    pub jails_path: String,
    /// Directory where per-jail jail(8) config files are written.
    /// e.g. "~/.config/warden/jails"
    pub jail_conf_dir: String,
    /// etcd endpoints the warden server connects to.
    pub etcd_endpoints: Vec<String>,
    /// Absolute path to the claude-openrouter wrapper script inside a jail.
    pub claude_script: String,
    /// nullfs mounts to include in every ephemeral worker jail.
    pub nullfs_mounts: Vec<NullfsMount>,
}

impl Default for Config {
    fn default() -> Self {
        let h = home();
        Config {
            base_dataset: "zroot/jails/warden".into(),
            jails_dataset: "zroot/jails".into(),
            jails_path: "/jails".into(),
            jail_conf_dir: format!("{h}/.config/warden/jails"),
            etcd_endpoints: vec!["127.0.0.1:2379".into()],
            claude_script: format!("{h}/src/claude-openrouter/claude-openrouter"),
            nullfs_mounts: vec![
                NullfsMount {
                    host: format!("{h}/.ssh"),
                    jail: "home/tcovert/.ssh".into(),
                    mode: "ro".into(),
                },
                NullfsMount {
                    host: format!("{h}/.config/jj"),
                    jail: "home/tcovert/.config/jj".into(),
                    mode: "rw".into(),
                },
                NullfsMount {
                    host: format!("{h}/.config/git"),
                    jail: "home/tcovert/.config/git".into(),
                    mode: "rw".into(),
                },
                NullfsMount {
                    host: format!("{h}/.gitconfig"),
                    jail: "home/tcovert/.gitconfig".into(),
                    mode: "rw".into(),
                },
                NullfsMount {
                    host: format!("{h}/src/claude-openrouter"),
                    jail: "home/tcovert/src/claude-openrouter".into(),
                    mode: "ro".into(),
                },
                NullfsMount {
                    host: format!("{h}/.claude-openrouter"),
                    jail: "home/tcovert/.claude-openrouter".into(),
                    mode: "rw".into(),
                },
            ],
        }
    }
}

impl Config {
    /// Load config from layered sources (later sources win):
    ///   1. Built-in defaults
    ///   2. /etc/warden/config.toml  (system-wide, optional)
    ///   3. ~/.config/warden/config.toml  (user, optional)
    ///   4. WARDEN_* environment variables
    pub fn load() -> Result<Self> {
        let h = home();
        let cfg = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::file("/etc/warden/config.toml"))
            .merge(Toml::file(format!("{h}/.config/warden/config.toml")))
            .merge(Env::prefixed("WARDEN_"))
            .extract()?;
        Ok(cfg)
    }
}
