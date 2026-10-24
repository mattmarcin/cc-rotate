use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default name of the rotation profile directory under the user's home.
pub const PROFILE_DIR_NAME: &str = ".claude-rotate";

/// Every on-disk location cc-rotate reads or writes.
///
/// `data_dir` holds cc-rotate's own state (store, locks, staging, backups).
/// `profile_dir` is the Claude Code config directory (`CLAUDE_CONFIG_DIR`) that the pool backs.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub profile_dir: PathBuf,
}

impl Paths {
    pub fn new(data_dir: impl Into<PathBuf>, profile_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            profile_dir: profile_dir.into(),
        }
    }

    /// `CC_ROTATE_DATA_DIR`, else the platform's local data directory plus `cc-rotate`.
    pub fn default_data_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("CC_ROTATE_DATA_DIR").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        let base =
            dirs::data_local_dir().context("could not determine the local data directory")?;
        Ok(base.join("cc-rotate"))
    }

    /// `CC_ROTATE_PROFILE`, else `~/.claude-rotate`.
    pub fn default_profile_dir() -> Result<PathBuf> {
        if let Some(dir) = std::env::var_os("CC_ROTATE_PROFILE").filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(dir));
        }
        Ok(home_dir()?.join(PROFILE_DIR_NAME))
    }

    pub fn store_db(&self) -> PathBuf {
        self.data_dir.join("store.db")
    }

    pub fn swap_lock(&self) -> PathBuf {
        self.data_dir.join("swap.lock")
    }

    pub fn staging_root(&self) -> PathBuf {
        self.data_dir.join("staging")
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn credentials_file(&self) -> PathBuf {
        self.profile_dir.join(".credentials.json")
    }

    /// The profile's credentials wherever Claude Code keeps them on this platform.
    pub fn credentials(&self) -> crate::credstore::Credentials {
        crate::credstore::Credentials::for_profile(&self.profile_dir)
    }

    /// Claude Code's lock around every read-modify-write of the credentials:
    /// `<profile>/.storage-write.lock`.
    pub fn storage_lock(&self) -> PathBuf {
        self.profile_dir.join(".storage-write.lock")
    }

    pub fn profile_config(&self) -> PathBuf {
        self.profile_dir.join(".claude.json")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.profile_dir.join("settings.json")
    }

    /// Claude Code's primary OAuth refresh lock: `<profile>/.oauth_refresh.lock`.
    pub fn refresh_lock(&self) -> PathBuf {
        self.profile_dir.join(".oauth_refresh.lock")
    }

    /// Claude Code's legacy refresh lock: `<realpath(profile)>.lock`, a sibling of the profile directory.
    pub fn legacy_refresh_lock(&self) -> PathBuf {
        legacy_lock_for(&self.profile_dir)
    }
}

pub fn legacy_lock_for(profile_dir: &Path) -> PathBuf {
    let real = dunce::canonicalize(profile_dir).unwrap_or_else(|_| profile_dir.to_path_buf());
    let mut s = real.into_os_string();
    s.push(".lock");
    PathBuf::from(s)
}

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("could not determine the home directory")
}

/// The template profile Claude Code uses when `CLAUDE_CONFIG_DIR` is unset: `~/.claude`.
pub fn default_template_dir() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude"))
}

/// Render a path with forward slashes, which both cmd.exe and Git Bash accept in commands.
pub fn forward_slashes(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}
