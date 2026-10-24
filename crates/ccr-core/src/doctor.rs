//! `cc-rotate-ctl doctor`: health checks for the store, the profile, and Claude Code.

use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::Value;

use crate::paths::Paths;
use crate::store::{AccountState, Store};
use crate::{credfile, login, time};

/// Claude Code versions whose internals (credential reload, refresh locks, statusline and hook
/// payloads) this release was verified against end to end.
pub const TESTED_CLAUDE_VERSIONS: &[&str] = &["2.1.310"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

fn check(name: &str, status: Status, detail: impl Into<String>) -> Check {
    let detail: String = detail.into();
    let mut chars = detail.chars();
    let detail = match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => detail,
    };
    Check {
        name: name.to_string(),
        status,
        detail,
    }
}

fn age(ms: i64) -> String {
    match ms / 1000 {
        s if s < 90 => format!("{s} seconds ago"),
        s if s < 5400 => format!("{} minutes ago", s / 60),
        s if s < 172_800 => format!("{} hours ago", s / 3600),
        s => format!("{} days ago", s / 86_400),
    }
}

pub fn claude_version(store: &Store) -> Option<String> {
    let claude = login::claude_executable(store).ok()?;
    let out = Command::new(claude)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace().next().map(str::to_string)
}

pub fn run(store: &Store, paths: &Paths) -> Vec<Check> {
    let mut checks = Vec::new();

    match claude_version(store) {
        Some(v) if TESTED_CLAUDE_VERSIONS.contains(&v.as_str()) => {
            checks.push(check("Claude Code version", Status::Ok, format!("{v} (tested)")));
        }
        Some(v) => checks.push(check(
            "Claude Code version",
            Status::Warn,
            format!("{v} is not on the tested list ({}); rotation still runs, but re-verify after updates", TESTED_CLAUDE_VERSIONS.join(", ")),
        )),
        None => checks.push(check("Claude Code version", Status::Fail, "could not run `claude --version`")),
    }

    checks.push(profile_settings_check(paths));

    let accounts = store.accounts().unwrap_or_default();
    let active: Vec<_> = accounts
        .iter()
        .filter(|a| a.state == AccountState::Active)
        .collect();
    let usable = accounts
        .iter()
        .filter(|a| matches!(a.state, AccountState::Active | AccountState::Parked))
        .count();
    checks.push(match (accounts.len(), active.len()) {
        (0, _) => check(
            "Accounts",
            Status::Warn,
            "no accounts yet; add one with `cc-rotate-ctl login`",
        ),
        (_, 1) => check(
            "Accounts",
            Status::Ok,
            format!(
                "{} accounts, {} usable, active: {}",
                accounts.len(),
                usable,
                active[0].label
            ),
        ),
        (_, 0) if usable == 0 => check(
            "Accounts",
            Status::Warn,
            "no account is active; every account is paused or needs a login",
        ),
        (_, 0) => check(
            "Accounts",
            Status::Fail,
            "accounts are parked but none is active; run `cc-rotate-ctl swap`",
        ),
        _ => check(
            "Accounts",
            Status::Fail,
            "more than one account is marked active",
        ),
    });

    let mut undecryptable = Vec::new();
    for account in &accounts {
        if store.credential(account.id).ok().flatten().is_none() {
            undecryptable.push(account.label.clone());
        }
    }
    checks.push(if undecryptable.is_empty() {
        check(
            "Stored logins",
            Status::Ok,
            format!("{} logins decrypt", accounts.len()),
        )
    } else {
        check(
            "Stored logins",
            Status::Fail,
            format!("missing or unreadable: {}", undecryptable.join(", ")),
        )
    });

    if let Some(account) = active.first() {
        let file_fp = credfile::read_oauth(&paths.credentials_file())
            .ok()
            .flatten()
            .as_ref()
            .and_then(credfile::oauth_fingerprint);
        let stored_fp = store
            .credential_meta(account.id)
            .ok()
            .flatten()
            .map(|m| m.refresh_fp);
        checks.push(match (file_fp, stored_fp) {
            (Some(f), Some(s)) if f == s => check("Profile login", Status::Ok, format!("matches {}", account.label)),
            (Some(_), Some(_)) => check(
                "Profile login",
                Status::Warn,
                "the profile holds a newer login than the store; the next statusline tick will sync it",
            ),
            (None, _) => check("Profile login", Status::Fail, "the profile has no usable login"),
            (_, None) => check("Profile login", Status::Fail, "the active account has no stored login"),
        });
    }

    for lock in [
        paths.refresh_lock(),
        paths.legacy_refresh_lock(),
        paths.swap_lock(),
    ] {
        if let Ok(meta) = std::fs::metadata(&lock) {
            let age = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if age > 60 {
                checks.push(check(
                    "Locks",
                    Status::Warn,
                    format!(
                        "{} is {age}s old and will be treated as stale",
                        lock.display()
                    ),
                ));
            }
        }
    }

    let last_sample = accounts
        .iter()
        .filter_map(|a| store.last_sample(a.id).ok().flatten())
        .map(|s| s.ts)
        .max();
    checks.push(match last_sample {
        Some(ts) => check(
            "Usage samples",
            Status::Ok,
            format!("last sample {}", age(time::now_ms() - ts)),
        ),
        None => check(
            "Usage samples",
            Status::Warn,
            "none yet; they arrive once a session runs in the profile",
        ),
    });

    checks
}

fn profile_settings_check(paths: &Paths) -> Check {
    let path = paths.settings_file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return check(
            "Profile",
            Status::Fail,
            format!("{} is missing; run `cc-rotate-ctl setup`", path.display()),
        );
    };
    let Ok(settings) = serde_json::from_str::<Value>(&text) else {
        return check(
            "Profile",
            Status::Fail,
            format!("{} is not valid JSON", path.display()),
        );
    };
    let status_ok = settings
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .is_some_and(|c| c.contains("cc-rotate-ctl") && c.contains("statusline"));
    let hook_ok = settings
        .pointer("/hooks/StopFailure")
        .is_some_and(|h| h.to_string().contains("cc-rotate-ctl"));
    match (status_ok, hook_ok) {
        (true, true) => check(
            "Profile",
            Status::Ok,
            paths.profile_dir.display().to_string(),
        ),
        (false, _) => check(
            "Profile",
            Status::Fail,
            "statusLine does not run `cc-rotate-ctl statusline`",
        ),
        (_, false) => check(
            "Profile",
            Status::Warn,
            "the StopFailure hook is not installed",
        ),
    }
}
