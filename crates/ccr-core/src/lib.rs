//! cc-rotate core: run one Claude Code profile on a pool of accounts.
//!
//! The design is in `docs/specs/2026-10-24-cc-rotate.md`. Every entry point (the statusline,
//! the StopFailure hook, the CLI and the desktop app) goes through this crate.

pub mod budget;
pub mod credfile;
pub mod credstore;
pub mod doctor;
pub mod extra_usage;
pub mod forecast;
pub mod health;
pub mod herdr;
pub mod history;
pub mod lock;
pub mod login;
pub mod paths;
pub mod plan;
pub mod process;
pub mod profile_account;
pub mod profile_usage;
pub mod profiles;
pub mod scheduler;
pub mod secret;
pub mod setup;
pub mod snapshot;
pub mod statusline;
pub mod store;
pub mod swap;
pub mod time;
pub mod usage_check;
pub mod usage_report;
pub mod waiting;

use std::path::PathBuf;

use anyhow::Result;

pub use paths::Paths;
pub use store::Store;

/// Open the store and resolve paths. The profile directory comes from, in order: the explicit
/// override, `CC_ROTATE_PROFILE`, the `profile_dir` setting recorded by `setup`, `~/.claude-rotate`.
pub fn open(profile_override: Option<PathBuf>) -> Result<(Store, Paths)> {
    let data_dir = Paths::default_data_dir()?;
    let store = Store::open(&data_dir.join("store.db"))?;
    let from_env = std::env::var_os("CC_ROTATE_PROFILE")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    let profile = match profile_override.or(from_env) {
        Some(p) => p,
        None => match store.settings()?.profile_dir {
            Some(p) if !std::path::Path::new(&p).is_absolute() => anyhow::bail!(
                "profile_dir must be a full path. Set it with cc-rotate-ctl config profile_dir <path>."
            ),
            Some(p) => PathBuf::from(p),
            None => Paths::default_profile_dir()?,
        },
    };
    // Its login and settings (which can hold secrets in `env`) are the user's alone, even when
    // an older version made them with the default mode.
    secret::restrict_to_owner(&profile, 0o700);
    for file in ["settings.json", "settings.local.json"] {
        secret::restrict_to_owner(&profile.join(file), 0o600);
    }
    Ok((store, Paths::new(data_dir, profile)))
}
