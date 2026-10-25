//! `cc-rotate-ctl doctor`: health checks for the store, the profile, and Claude Code.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Serialize;
use serde_json::Value;

use crate::credstore::{self, Location};
use crate::paths::{self, Paths};
use crate::store::{Account, AccountState, Store};
use crate::{credfile, herdr, login, statusline, time, usage_check};

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
    checks.push(herdr_check(paths, herdr::detect()));

    let legacy = crate::setup::legacy_alias_files();
    if !legacy.is_empty() {
        let files: Vec<String> = legacy.iter().map(|p| p.display().to_string()).collect();
        checks.push(check(
            "cc-rotate command",
            Status::Warn,
            format!(
                "{} still defines the old cc-rotate PowerShell function, which hides the cc-rotate command; run `cc-rotate-ctl setup --remove-alias` and open a new terminal",
                files.join(" and ")
            ),
        ));
    }

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

    // Readings from other profiles and observe mode only count within the account's own
    // organization (section 9.1), so an account without one on record gets none of them.
    let no_org: Vec<&str> = accounts
        .iter()
        .filter(|a| a.org_uuid.is_none())
        .map(|a| a.label.as_str())
        .collect();
    if !no_org.is_empty() {
        checks.push(check(
            "Organizations",
            Status::Warn,
            format!(
                "{} {} no organization on record, so usage your other profiles see is not counted for {}; sign in again with `cc-rotate-ctl login --relogin <label>`",
                no_org.join(", "),
                if no_org.len() == 1 { "has" } else { "have" },
                if no_org.len() == 1 { "it" } else { "them" },
            ),
        ));
    }

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
    checks.push(encryption_check(store));
    checks.push(credential_storage_check(paths));

    if let Some(account) = active.first() {
        let file_fp = paths
            .credentials()
            .read_oauth()
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
        paths.storage_lock(),
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

    // Claude Code does not run custom status lines in the home folder (its trust there is
    // per-run only), so a session started there is invisible to cc-rotate.
    let home = paths::home_dir()
        .ok()
        .and_then(|h| dunce::canonicalize(h).ok());
    let in_home = session_cwds(paths)
        .iter()
        .filter(|cwd| dunce::canonicalize(cwd).ok() == home)
        .count();
    if in_home > 0 {
        checks.push(check(
            "Sessions",
            Status::Warn,
            format!(
                "{in_home} cc-rotate session(s) running in your home folder; Claude Code does not run status lines there, so cc-rotate cannot see their usage or swap them. Start cc-rotate in a project folder"
            ),
        ));
    }

    let last_tick = store.meta_i64(statusline::META_LAST_TICK_AT).ok().flatten();
    let had_usage = store
        .meta(statusline::META_LAST_TICK_HAD_USAGE)
        .ok()
        .flatten();
    checks.push(match (last_tick, had_usage.as_deref()) {
        (None, _) => check(
            "Statusline",
            Status::Warn,
            "has not run yet; start a cc-rotate session and send a message",
        ),
        (Some(ts), Some("1")) => check(
            "Statusline",
            Status::Ok,
            format!("last ran {}, with usage data", age(time::now_ms() - ts)),
        ),
        (Some(ts), _) => check(
            "Statusline",
            Status::Warn,
            format!(
                "last ran {}, but Claude Code sent no usage data (it does after the first reply on a subscription login)",
                age(time::now_ms() - ts)
            ),
        ),
    });

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
    checks.push(usage_checks_check(store, &accounts));

    checks
}

/// Whether usage checks of the accounts not in use work (section 20).
fn usage_checks_check(store: &Store, accounts: &[Account]) -> Check {
    let name = "Usage checks";
    if !store.settings().is_ok_and(|s| s.usage_checks) {
        return check(
            name,
            Status::Ok,
            "off; accounts not in use are only read from the status line and your other profiles",
        );
    }
    if let Ok(Some(reason)) = usage_check::blocked(store) {
        return check(
            name,
            Status::Warn,
            format!(
                "stopped: {reason}; after updating Claude Code, turn them on again with `cc-rotate-ctl config usage_checks true`"
            ),
        );
    }
    let mut failing = Vec::new();
    let mut last_read = None;
    for a in accounts {
        let Ok(st) = usage_check::state(store, a.id) else {
            continue;
        };
        last_read = last_read.max(st.last_read_at);
        // The account in use and one that needs a login are not checked; old problems of
        // theirs do not matter now.
        if !matches!(a.state, AccountState::Parked | AccountState::Paused) {
            continue;
        }
        if let Some(problem) = st.problem {
            failing.push(format!("{}: {problem}", a.label));
        }
    }
    match (failing.is_empty(), last_read) {
        (false, _) => check(name, Status::Warn, failing.join("; ")),
        (true, Some(ts)) => check(
            name,
            Status::Ok,
            format!("on; last reading {}", age(time::now_ms() - ts)),
        ),
        (true, None) => check(name, Status::Ok, "on; no account has been checked yet"),
    }
}

/// Working folders of the Claude Code sessions registered in the profile (`sessions/<pid>.json`)
/// whose process is still running.
fn session_cwds(paths: &Paths) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(paths.profile_dir.join("sessions")) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(|s| {
            s.get("pid")
                .and_then(Value::as_u64)
                .is_some_and(|pid| crate::process::alive(pid as u32))
        })
        .filter_map(|s| s.get("cwd").and_then(Value::as_str).map(PathBuf::from))
        .collect()
}

/// Where Claude Code keeps the profile's login, which is where swaps write it.
fn credential_storage_check(paths: &Paths) -> Check {
    let name = "Claude Code login storage";
    if cfg!(windows)
        && credstore::windows_credman_enabled(
            &paths.profile_dir,
            std::env::var("CLAUDE_CODE_FORCE_WINDOWS_CREDMAN")
                .ok()
                .as_deref(),
        )
    {
        return check(
            name,
            Status::Fail,
            "Claude Code is set to keep this profile's login in the Windows Credential Manager (tengu_windows_credman or CLAUDE_CODE_FORCE_WINDOWS_CREDMAN), which cc-rotate does not support yet; swaps will not reach running sessions",
        );
    }
    let creds = paths.credentials();
    match creds.location() {
        Ok(Some(Location::Keychain)) => check(
            name,
            Status::Ok,
            format!(
                "{} (macOS support is experimental; a swap reaches running sessions within about 30 seconds)",
                creds.describe(Location::Keychain)
            ),
        ),
        Ok(Some(Location::File)) => check(name, Status::Ok, creds.describe(Location::File)),
        Ok(None) => check(
            name,
            Status::Ok,
            format!(
                "nothing yet; the first login goes to {}",
                creds.describe(Location::File)
            ),
        ),
        Err(e) => check(name, Status::Fail, format!("{e:#}")),
    }
}

/// Where the key that encrypts the stored logins lives, and whether only its owner can read it.
fn encryption_check(store: &Store) -> Check {
    let Some(path) = store.key_path() else {
        return check("Encryption", Status::Ok, "in-memory store");
    };
    if !path.is_file() {
        return check(
            "Encryption",
            Status::Ok,
            format!(
                "XChaCha20-Poly1305; the key file {} is created with the first login",
                path.display()
            ),
        );
    }
    match crate::secret::key_file_exposed(path) {
        Some(true) => check(
            "Encryption",
            Status::Warn,
            format!(
                "{} is readable by other users; run `chmod 600` on it",
                path.display()
            ),
        ),
        _ => check(
            "Encryption",
            Status::Ok,
            format!(
                "XChaCha20-Poly1305 with the key file {} ({})",
                path.display(),
                if cfg!(windows) {
                    "protected by your user profile's permissions"
                } else {
                    "readable only by you"
                }
            ),
        ),
    }
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
    let command = settings
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .unwrap_or("");
    let status_ok = command.contains("cc-rotate-ctl") && command.contains("statusline");
    let hook_ok = settings
        .pointer("/hooks/StopFailure")
        .is_some_and(|h| h.to_string().contains("cc-rotate-ctl"));
    // Older setups quoted the path, which PowerShell cannot run, so no usage was ever recorded.
    if status_ok && command.trim_start().starts_with('"') {
        return check(
            "Profile",
            Status::Fail,
            "the statusline command starts with a quoted path, which fails when Claude Code runs it through PowerShell; run `cc-rotate-ctl setup --force`",
        );
    }
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

/// herdr is optional: this only warns when herdr is here but would resume cc-rotate sessions
/// with plain `claude` (spec section 19).
fn herdr_check(paths: &Paths, detected: Option<herdr::Detected>) -> Check {
    let name = "herdr";
    let hook = std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|s| herdr::session_start_hook_installed(&s));
    let Some(detected) = detected else {
        return check(
            name,
            Status::Ok,
            if hook {
                "not detected (optional); the SessionStart hook is installed for when you use it"
            } else {
                "not detected (optional); if you start using it, run `cc-rotate-ctl setup --force` so it resumes sessions with cc-rotate"
            },
        );
    };
    // The check's name is "herdr", so the detail starts with what was found.
    let version = herdr::version(detected.bin());
    let found = match (&version, &detected) {
        (Some(v), _) => format!("version {v}"),
        (None, herdr::Detected::InPane(_)) => "running in a herdr pane".to_string(),
        (None, herdr::Detected::OnPath(p)) => format!("found at {}", p.display()),
    };
    let (major, minor, patch) = herdr::MIN_VERSION;
    if let Some(v) = version.as_deref().and_then(herdr::parse_version) {
        if v < herdr::MIN_VERSION {
            return check(
                name,
                Status::Warn,
                format!(
                    "{found} cannot be told how to resume cc-rotate sessions, so it resumes them with plain `claude` on your default profile; update herdr to {major}.{minor}.{patch} or newer"
                ),
            );
        }
    }
    if !hook {
        return check(
            name,
            Status::Warn,
            format!(
                "{found}, but the profile has no SessionStart hook for it, so herdr resumes cc-rotate sessions with plain `claude` on your default profile; run `cc-rotate-ctl setup --force`"
            ),
        );
    }
    check(
        name,
        Status::Ok,
        format!(
            "{found}; panes it restores resume with `cc-rotate --resume`, on the rotation profile"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_check_is_soft() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("data"), root.path().join("profile"));
        std::fs::create_dir_all(&paths.profile_dir).unwrap();
        let missing_bin = herdr::Detected::OnPath(root.path().join("no-such-herdr"));

        // No profile settings at all: herdr is optional, so this never fails.
        let c = herdr_check(&paths, None);
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("setup --force"), "{}", c.detail);

        let c = herdr_check(&paths, Some(missing_bin.clone()));
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("no SessionStart hook"), "{}", c.detail);

        std::fs::write(
            paths.settings_file(),
            serde_json::json!({"hooks": {"SessionStart": [{"hooks": [
                {"type": "command", "command": "/bin/cc-rotate-ctl hook session-start"}
            ]}]}})
            .to_string(),
        )
        .unwrap();
        let c = herdr_check(&paths, Some(missing_bin));
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert!(c.detail.contains("cc-rotate --resume"), "{}", c.detail);
        assert_eq!(herdr_check(&paths, None).status, Status::Ok);
    }
}
