use std::path::PathBuf;

use anyhow::{Context, Result};
use nix::unistd::{User, getuid};

/// The user whose caches are cleaned, and where cendrillon keeps its own files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Env {
    pub home: PathBuf,
    pub user: String,
}

impl Env {
    /// The current user, from the password database rather than `$HOME`, which launchd
    /// does not always set.
    pub fn current() -> Result<Self> {
        let uid = getuid();
        let user = User::from_uid(uid)?.with_context(|| format!("no user with uid {uid}"))?;
        Ok(Self {
            home: user.dir,
            user: user.name,
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join(".config/cendrillon/config.toml")
    }

    pub fn state_file(&self) -> PathBuf {
        self.home
            .join("Library/Application Support/cendrillon/state.json")
    }

    pub fn log_file(&self) -> PathBuf {
        self.home.join("Library/Logs/cendrillon.log")
    }

    pub fn launch_agent_file(&self, label: &str) -> PathBuf {
        self.home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"))
    }
}
