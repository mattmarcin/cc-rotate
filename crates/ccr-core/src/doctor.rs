//! `cc-rotate-ctl doctor`: health checks for the store, the profile, and Claude Code.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Serialize;
use serde_json::Value;

use crate::credstore::{self, Location};
use crate::paths::{self, Paths};
use crate::store::{Account, AccountState, Store};
use crate::{credfile, herdr, login, statusline, swap, time, usage_check};

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

/// A detail is written as it is shown: it may start with an account label, which keeps its case.
fn check(name: &str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_string(),
        status,
        detail: detail.into(),
    }
}

/// `text` with its first letter in upper case, for a detail that starts with a message from
/// elsewhere.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// "45s ago", "12m ago", "3h ago", "2d ago", as the dashboard writes it.
fn age(ms: i64) -> String {
    match ms / 1000 {
        s if s < 60 => format!("{}s ago", s.max(0)),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn claude_version(store: &Store) -> Option<String> {
    let claude = login::claude_executable(store).ok()?;
    let out = crate::paths::program_command(&claude)
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
            checks.push(check(
                "Claude Code version",
                Status::Ok,
                format!("{v} (tested)"),
            ));
        }
        Some(v) => checks.push(check(
            "Claude Code version",
            Status::Warn,
            format!(
                "{v}, not yet tested with cc-rotate (tested: {})",
                TESTED_CLAUDE_VERSIONS.join(", ")
            ),
        )),
        None => checks.push(check(
            "Claude Code version",
            Status::Fail,
            match login::claude_executable(store) {
                // Why it wasn't found, or that one found was refused.
                Err(e) => e.to_string(),
                Ok(claude) if store.settings().is_ok_and(|s| s.claude_path.is_some()) => format!(
                    "Couldn't run `{} --version`. Check the claude_path setting, or set it again with `cc-rotate-ctl config claude_path <full path>`.",
                    claude.display()
                ),
                Ok(claude) => format!(
                    "Couldn't run `{} --version`. Reinstall Claude Code, or set where it is with `cc-rotate-ctl config claude_path <full path>`.",
                    claude.display()
                ),
            },
        )),
    }

    checks.push(profile_settings_check(paths));
    checks.push(environment_check(paths, |k| std::env::var(k).ok()));
    checks.push(herdr_check(paths, herdr::detect()));
    checks.push(desktop_check(
        &crate::desktop::read_setup(),
        std::fs::read_to_string(paths.settings_file())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|s| crate::setup::stop_hook_present(&s)),
    ));

    checks.push(relay_check(
        store,
        paths,
        crate::relay_service::status,
        |port| {
            crate::relay::RelayConfig::read(&paths.relay_config())
                .ok()
                .flatten()
                .filter(|cfg| cfg.port == port)
                .and_then(|cfg| crate::relay::running_version(&cfg))
        },
    ));

    let legacy = crate::setup::legacy_alias_files();
    if !legacy.is_empty() {
        let files: Vec<String> = legacy.iter().map(|p| p.display().to_string()).collect();
        checks.push(check(
            "cc-rotate command",
            Status::Warn,
            format!(
                "{} still defines the old cc-rotate PowerShell function, which hides the cc-rotate command. Run `cc-rotate-ctl setup --remove-alias`, then open a new terminal.",
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
            "No accounts yet. Add one with Add account or `cc-rotate-ctl login`.",
        ),
        (_, 1) => check(
            "Accounts",
            Status::Ok,
            format!(
                "{} {}, {} usable, {} in use",
                accounts.len(),
                if accounts.len() == 1 {
                    "account"
                } else {
                    "accounts"
                },
                usable,
                active[0].label
            ),
        ),
        (_, 0) if usable == 0 => check(
            "Accounts",
            Status::Warn,
            "No account in use: every account is paused or signed out",
        ),
        (_, 0) => check(
            "Accounts",
            Status::Fail,
            "No account in use, though some are ready. Use Swap now or run `cc-rotate-ctl swap`.",
        ),
        _ => check(
            "Accounts",
            Status::Fail,
            "More than one account is marked in use",
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
                "{} {} no organization on record, so usage seen in your other profiles isn't counted for {}. Sign in again from the app, or with `cc-rotate-ctl login --relogin <label>`.",
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
            "Saved logins",
            Status::Ok,
            match accounts.len() {
                1 => "The one saved login can be decrypted".to_string(),
                n => format!("All {n} can be decrypted"),
            },
        )
    } else {
        check(
            "Saved logins",
            Status::Fail,
            format!(
                "Missing or unreadable: {}. Sign in to {} again.",
                undecryptable.join(", "),
                if undecryptable.len() == 1 {
                    "it"
                } else {
                    "them"
                }
            ),
        )
    });
    checks.push(encryption_check(store));
    checks.push(credential_storage_check(paths));

    if let Some(c) = profile_login_check(store, paths) {
        checks.push(c);
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
                        "{} is {age}s old and will be ignored as stale",
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
                "{in_home} cc-rotate {} running in your home folder, where Claude Code doesn't run status lines. cc-rotate can't see their usage or swap them. Start cc-rotate in a project folder.",
                if in_home == 1 { "session is" } else { "sessions are" }
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
            "Status line",
            Status::Warn,
            "Hasn't run yet. Start a cc-rotate session and send a message.",
        ),
        (Some(ts), Some("1")) => check(
            "Status line",
            Status::Ok,
            format!("Last ran {}, with usage", age(time::now_ms() - ts)),
        ),
        (Some(ts), _) => check(
            "Status line",
            Status::Warn,
            format!(
                "Last ran {} without usage data. Claude Code sends it after the first reply on a subscription login.",
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
            "Usage data",
            Status::Ok,
            format!("Last update {}", age(time::now_ms() - ts)),
        ),
        None => check(
            "Usage data",
            Status::Warn,
            "None yet. It arrives once a session runs in the cc-rotate profile.",
        ),
    });
    checks.push(usage_checks_check(store, &accounts));
    if let Some(c) = warm_ups_check(store, paths, &accounts) {
        checks.push(c);
    }
    if crate::swap::pending_swap(store).ok().flatten().is_some()
        && !crate::lock::is_held(&paths.swap_lock(), crate::lock::CLAUDE_STALE)
    {
        checks.push(check(
            "Swap",
            Status::Warn,
            "A swap was interrupted and cc-rotate can't tell yet which account the profile is signed in to. Until it can, swaps are on hold and nothing is saved from the profile. Your sessions keep working. It retries every minute, which needs the `claude` command and a network connection.",
        ));
    }

    checks
}

/// Whether the profile's login is the one cc-rotate saved for the account the profile belongs
/// to: the active account, or in relay mode the home account, whichever account is active.
/// `None` when the profile belongs to no account.
fn profile_login_check(store: &Store, paths: &Paths) -> Option<Check> {
    let relay_mode = crate::relay::home(store).ok().flatten().is_some();
    let account = crate::relay::profile_account(store).ok().flatten()?;
    let locked_away = !paths.credentials().complete().unwrap_or(true);
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
    let foreign = store
        .meta(swap::META_PROFILE_FOREIGN)
        .ok()
        .flatten()
        .as_deref()
        == Some("1");
    let identifies = usage_check::identifies_logins(store, paths).unwrap_or(false);
    // How long Claude Code has not been able to say whose the profile's login is.
    let unidentified_ms = store
        .meta(swap::META_LOGIN_UNIDENTIFIED)
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_str::<swap::Unidentified>(&v).ok())
        .map(|u| time::now_ms() - u.at);
    Some(match (file_fp, stored_fp) {
        (Some(f), Some(s)) if f == s => check(
            "Profile login",
            Status::Ok,
            format!("Matches {}", account.label),
        ),
        (Some(_), Some(_)) if foreign => check(
            "Profile login",
            Status::Warn,
            "The profile is signed in to an account that isn't in cc-rotate (from a /login). Nothing is saved from it until the next swap or sign-in.",
        ),
        (Some(_), Some(_)) if !identifies => check(
            "Profile login",
            Status::Warn,
            "The profile has a newer login than cc-rotate saved. It's saved on the next status line update.",
        ),
        (Some(_), Some(_)) => match unidentified_ms.filter(|ms| *ms >= 2 * 60_000) {
            None => check(
                "Profile login",
                Status::Ok,
                format!(
                    "The profile has a newer login. It's saved once Claude Code confirms it belongs to {}.",
                    account.label
                ),
            ),
            Some(ms) => check(
                "Profile login",
                Status::Warn,
                format!(
                    "Claude Code hasn't confirmed for {} min which account the profile's login belongs to. Swaps wait until it does. After {} min, the login is saved as {}'s.",
                    ms / 60_000,
                    swap::UNIDENTIFIED_FOR_AT_MOST_MS / 60_000,
                    account.label
                ),
            ),
        },
        (None, _) if locked_away => check(
            "Profile login",
            Status::Warn,
            "Can't be read from this session (see Claude Code login storage)",
        ),
        (None, _) => check(
            "Profile login",
            Status::Fail,
            "The profile has no usable login",
        ),
        (_, None) => check(
            "Profile login",
            Status::Fail,
            if relay_mode {
                format!(
                    "The profile keeps the login of the home account {}, which has no saved login. Sign in to it again.",
                    account.label
                )
            } else {
                format!(
                    "{} is in use but has no saved login. Sign in to it again.",
                    account.label
                )
            },
        ),
    })
}

/// Whether usage checks of the accounts not in use work (section 20).
fn usage_checks_check(store: &Store, accounts: &[Account]) -> Check {
    let name = "Usage checks";
    if !store.settings().is_ok_and(|s| s.usage_checks) {
        return check(
            name,
            Status::Ok,
            "Off. Accounts not in use only get usage from your other Claude Code profiles.",
        );
    }
    if let Ok(Some(reason)) = usage_check::blocked(store) {
        return check(
            name,
            Status::Warn,
            format!(
                "Stopped: {reason}. After updating Claude Code, turn usage checks off and on again in Settings, or run `cc-rotate-ctl config usage_checks true`."
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
        (false, _) => check(name, Status::Warn, failing.join(". ")),
        (true, Some(ts)) => check(
            name,
            Status::Ok,
            format!("On. Last check {}.", age(time::now_ms() - ts)),
        ),
        (true, None) => check(name, Status::Ok, "On. No account checked yet."),
    }
}

/// Whether warm-ups keep the 5-hour windows running of the accounts the user chose (section
/// 23); nothing when no account has it on.
fn warm_ups_check(store: &Store, paths: &Paths, accounts: &[Account]) -> Option<Check> {
    let name = "Keeping 5-hour windows running";
    let kept: Vec<&Account> = accounts.iter().filter(|a| a.keep_running).collect();
    if kept.is_empty() {
        return None;
    }
    let labels = kept
        .iter()
        .map(|a| a.label.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if let Ok(Some(reason)) = usage_check::warm_ups_blocked(store) {
        return Some(check(
            name,
            Status::Warn,
            format!(
                "Stopped: {reason}. Click Start again in an account's More dialog, or run `cc-rotate-ctl keep-running <label> yes`."
            ),
        ));
    }
    if let Ok(Some(why)) = usage_check::warm_ups_paused(store, paths) {
        return Some(check(
            name,
            Status::Warn,
            format!("On for {labels}, but not running: {why}."),
        ));
    }
    let mut failing = Vec::new();
    let mut last = None;
    for a in &kept {
        let Ok(st) = usage_check::state(store, a.id) else {
            continue;
        };
        last = last.max(st.warmed_at);
        // Only a recent one: an account in use since starts its windows by itself.
        let recent = st
            .warm_attempt_at
            .is_some_and(|t| time::now_ms() - t < usage_check::WARM_PROBLEM_SHOWN_MS);
        if let Some(problem) = st
            .warm_problem
            .filter(|_| a.state == AccountState::Parked && recent)
        {
            failing.push(format!("{}: {problem}", a.label));
        }
    }
    Some(match (failing.is_empty(), last) {
        (false, _) => check(name, Status::Warn, failing.join(". ")),
        (true, Some(ts)) => check(
            name,
            Status::Ok,
            format!(
                "On for {labels}. Last started a window {}.",
                age(time::now_ms() - ts)
            ),
        ),
        (true, None) => check(
            name,
            Status::Ok,
            format!("On for {labels}. No window started yet."),
        ),
    })
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

/// What `doctor`, the `cc-rotate` launcher and a swap say when this session cannot use the macOS
/// Keychain (`Credentials::keychain_locked`).
pub const KEYCHAIN_LOCKED: &str = "This session can't use the macOS Keychain, where Claude Code keeps its login, so Claude Code started here can't read the login cc-rotate puts there or save a renewed one: it starts signed out, or loses its login at the next renewal. macOS gives the login Keychain only to programs started from the Mac's desktop session, not to ones started over SSH or by a program that was, such as a herdr or tmux server started from an SSH connection. Start cc-rotate (or that server) from the desktop session, or unlock the Keychain for this session with `security unlock-keychain`. See \"macOS over SSH\" in the README.";

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
            "Claude Code keeps this profile's login in Windows Credential Manager, which cc-rotate doesn't support yet. Swaps won't reach running sessions. Set by tengu_windows_credman or CLAUDE_CODE_FORCE_WINDOWS_CREDMAN.",
        );
    }
    let creds = paths.credentials();
    if !creds.complete().unwrap_or(true) {
        return check(name, Status::Fail, KEYCHAIN_LOCKED);
    }
    match creds.location() {
        Ok(Some(Location::Keychain)) => check(
            name,
            Status::Ok,
            format!(
                "{}. Swaps reach running sessions within about 30 seconds.",
                capitalized(&creds.describe(Location::Keychain))
            ),
        ),
        Ok(Some(Location::File)) => check(name, Status::Ok, creds.describe(Location::File)),
        Ok(None) => check(
            name,
            Status::Ok,
            format!(
                "Nothing yet. The first login is saved to {}.",
                creds.describe(Location::File)
            ),
        ),
        Err(e) => check(name, Status::Fail, capitalized(&format!("{e:#}"))),
    }
}

/// Where the key that encrypts the stored logins lives, and whether only its owner can read it.
fn encryption_check(store: &Store) -> Check {
    let Some(path) = store.key_path() else {
        return check("Encryption", Status::Ok, "In-memory store");
    };
    if !path.is_file() {
        return check(
            "Encryption",
            Status::Ok,
            format!(
                "Key file {} is created with the first login",
                path.display()
            ),
        );
    }
    match crate::secret::key_file_exposed(path) {
        Some(true) => check(
            "Encryption",
            Status::Warn,
            format!(
                "{} is readable by other users. Run `chmod 600 {}`.",
                path.display(),
                path.display()
            ),
        ),
        _ => check(
            "Encryption",
            Status::Ok,
            format!(
                "Key file {}, {}",
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

/// The desktop app's Code sessions (spec section 24.3).
fn desktop_check(setup: &crate::desktop::Setup, stop_hook: bool) -> Check {
    use crate::desktop::{Launcher, Platform, Policy, launcher_program};
    let name = "Desktop app";
    if setup.platform == Platform::Unsupported {
        return check(
            name,
            Status::Ok,
            "the desktop app's sessions can't use the pool on this platform",
        );
    }
    let fix = "Run `cc-rotate-ctl desktop` for the commands that set it up.";
    match (&setup.policy, &setup.launcher) {
        (Policy::On, Launcher::Ours) if stop_hook => check(
            name,
            Status::Ok,
            "the desktop app's Code sessions run on the pool (new sessions)",
        ),
        (Policy::On, Launcher::Ours) => check(
            name,
            Status::Warn,
            "the desktop app's Code sessions run on the pool, but the profile has no `hook stop`, so a woken one is reported as stuck. Run `cc-rotate-ctl setup --keep-existing`.",
        ),
        (Policy::On, Launcher::Other(value)) => {
            let missing = launcher_program(value).is_some_and(|p| p.is_absolute() && !p.exists());
            if missing {
                check(
                    name,
                    Status::Fail,
                    format!(
                        "Claude Code's managed settings name a launcher that is not there ({value}), so the desktop app can start no Code session. {fix}"
                    ),
                )
            } else {
                check(
                    name,
                    Status::Warn,
                    format!(
                        "the desktop app starts Code sessions through another launcher ({value}), so they don't run on the pool. {fix}"
                    ),
                )
            }
        }
        (Policy::On, Launcher::None) => check(
            name,
            Status::Warn,
            format!(
                "the app's launcher policy is on, but Claude Code's managed settings name no launcher. {fix}"
            ),
        ),
        (Policy::On, Launcher::Unreadable(why)) => check(
            name,
            Status::Warn,
            format!("Claude Code's managed settings can't be read ({why})"),
        ),
        (Policy::Off | Policy::Disabled, Launcher::Ours) => check(
            name,
            Status::Warn,
            format!(
                "Claude Code's managed settings name cc-rotate's launcher, but the app's launcher policy is off, so desktop sessions run on the app's account. {fix}"
            ),
        ),
        (Policy::Unknown(why), _) => check(
            name,
            Status::Warn,
            format!("the app's launcher policy can't be read ({why})"),
        ),
        (Policy::Off | Policy::Disabled, _) => check(
            name,
            Status::Ok,
            "the desktop app's Code sessions run on the app's account (`cc-rotate-ctl desktop` sets them up on the pool)",
        ),
    }
}

fn profile_settings_check(paths: &Paths) -> Check {
    let path = paths.settings_file();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return check(
            "Profile",
            Status::Fail,
            format!("{} is missing. Run `cc-rotate-ctl setup`.", path.display()),
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
    let hook_current = crate::setup::stop_failure_hook_current(&settings);
    // Older setups quoted the path, which PowerShell cannot run, so no usage was ever recorded.
    if status_ok && command.trim_start().starts_with('"') {
        return check(
            "Profile",
            Status::Fail,
            "The status line command starts with a quoted path, which fails under PowerShell. Run `cc-rotate-ctl setup --force`.",
        );
    }
    match (status_ok, hook_ok) {
        (true, true) if !hook_current => check(
            "Profile",
            Status::Warn,
            "The StopFailure hook is from an older cc-rotate, so a session stopped at a usage limit doesn't continue after a swap. Run `cc-rotate-ctl setup --keep-existing`.",
        ),
        (true, true) => check(
            "Profile",
            Status::Ok,
            paths.profile_dir.display().to_string(),
        ),
        (false, _) => check(
            "Profile",
            Status::Fail,
            "The profile's status line doesn't run cc-rotate. Run `cc-rotate-ctl setup --force`.",
        ),
        (_, false) => check(
            "Profile",
            Status::Warn,
            "The StopFailure hook isn't installed. Run `cc-rotate-ctl setup --force`.",
        ),
    }
}

/// Variables that make Claude Code ignore the profile's login, so a swap reaches no session.
const LOGIN_OVERRIDES: &[&str] = &["ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"];

/// Claude Code asks once before it uses this key in an interactive session, and remembers the
/// answer, so it only overrides the login when someone said yes (or in `claude -p`).
const API_KEY: &str = "ANTHROPIC_API_KEY";

/// With it, a request at a usage limit waits inside Claude Code holding the login it was built
/// with, so it never moves to the account cc-rotate swapped to (spec section 21).
const RETRY_WATCHDOG: &str = "CLAUDE_CODE_RETRY_WATCHDOG";

/// What in this environment or the profile's settings works against swaps: a login override, or
/// Claude Code's retry watchdog. `var` reads this process's environment (the shell `doctor` runs
/// in, which sessions started from it share).
fn environment_check(paths: &Paths, var: impl Fn(&str) -> Option<String>) -> Check {
    let settings: Value = std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    // Where a variable is set, and to what: the environment wins over the settings, as in
    // Claude Code.
    let lookup = |name: &str| -> Option<(String, &'static str)> {
        var(name)
            .filter(|v| !v.trim().is_empty())
            .map(|v| (v, "your environment"))
            .or_else(|| {
                settings
                    .pointer(&format!("/env/{name}"))
                    .and_then(Value::as_str)
                    .filter(|v| !v.trim().is_empty())
                    .map(|v| (v.to_string(), "the profile's settings.json"))
            })
    };
    let mut problems = Vec::new();
    for name in LOGIN_OVERRIDES {
        if let Some((_, place)) = lookup(name) {
            problems.push(format!(
                "{name} is set in {place}, so Claude Code uses it instead of the account cc-rotate swaps in."
            ));
        }
    }
    if settings
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .is_some_and(|v| !v.trim().is_empty())
    {
        problems.push(
            "The profile's settings.json has an apiKeyHelper, so Claude Code uses its key instead of the account cc-rotate swaps in."
                .to_string(),
        );
    }
    let overrides = !problems.is_empty();
    if let Some((_, place)) = lookup(API_KEY) {
        problems.push(format!(
            "{API_KEY} is set in {place}. If you told Claude Code to use it, it does instead of the account cc-rotate swaps in, and so does `claude -p`."
        ));
    }
    if let Some((_, place)) = lookup(RETRY_WATCHDOG)
        .filter(|(v, _)| v.trim() != "0" && !v.trim().eq_ignore_ascii_case("false"))
    {
        problems.push(format!(
            "{RETRY_WATCHDOG} is set in {place}: a session that reaches a usage limit waits on its old login and doesn't move to the account cc-rotate swaps to. Unset it for cc-rotate sessions."
        ));
    }
    match (problems.is_empty(), overrides) {
        (true, _) => check(
            "Claude Code environment",
            Status::Ok,
            "Nothing overrides the profile's login",
        ),
        (false, true) => check("Claude Code environment", Status::Fail, problems.join(" ")),
        (false, false) => check("Claude Code environment", Status::Warn, problems.join(" ")),
    }
}

/// Variables in the profile's `env` that take a session off the relay (a base URL or socket of
/// its own) or off the home account's login (a key or login of its own).
const RELAY_ENV_OVERRIDES: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_UNIX_SOCKET",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
];

/// Relay mode (relay spec, section 11): off, or on with the relay answering with this program's
/// version, or what is wrong with it.
///
/// `service` says whether the relay's service is installed (`relay_service::status`), and
/// `version` asks whatever answers on a port for its version (`relay::running_version`), `None`
/// when nothing answers or not with a version. Neither is asked while relay mode is off.
pub fn relay_check(
    store: &Store,
    paths: &Paths,
    service: impl FnOnce() -> crate::relay_service::ServiceState,
    version: impl Fn(u16) -> Option<String>,
) -> Check {
    use crate::relay::{self, RelayConfig};
    let name = "Relay";
    let home_id = match relay::home(store) {
        Ok(None) => return check(name, Status::Ok, "relay mode is off"),
        Ok(Some(id)) => id,
        Err(e) => return check(name, Status::Fail, format!("can't read relay mode: {e:#}")),
    };
    let home = store.account(home_id).ok().flatten();
    let label = home
        .as_ref()
        .map_or_else(|| format!("account {home_id}"), |a| a.label.clone());
    // What `relay on --home` takes to put things right: the home account again, or another one
    // when it is gone.
    let again = home
        .as_ref()
        .map_or_else(|| "<another account>".to_string(), |a| a.label.clone());
    // Each problem is a clause; failures come first.
    let mut fails = Vec::new();
    let mut warns = Vec::new();

    match &home {
        None => fails.push(format!(
            "the home account ({label}) is no longer in cc-rotate, so it can't be signed in again: name another with `cc-rotate-ctl relay on --home <another account>`"
        )),
        Some(a) if a.state == AccountState::NeedsLogin => fails.push(format!(
            "the home account {label} is signed out: every session fails until it signs in again (`cc-rotate-ctl login --relogin {label}`)"
        )),
        Some(_) => {}
    }
    if service() == crate::relay_service::ServiceState::Missing {
        fails.push(format!(
            "the relay's service is not installed, so nothing restarts the relay when it ends (`cc-rotate-ctl relay on --home {again}` installs it)"
        ));
    }
    let mut ok = None;
    match RelayConfig::read(&paths.relay_config()) {
        Ok(Some(cfg)) => match version(cfg.port) {
            Some(v) if v == env!("CARGO_PKG_VERSION") => {
                ok = Some(format!(
                    "on, with {label} as the home account; the relay answers on 127.0.0.1:{} (version {v})",
                    cfg.port
                ));
            }
            Some(v) => warns.push(format!(
                "the relay on 127.0.0.1:{} is version {v}, not this cc-rotate's {}: it changes over by itself within about 12 minutes (it looks for an upgrade every minute, then waits up to 11 minutes for the requests in flight)",
                cfg.port,
                env!("CARGO_PKG_VERSION")
            )),
            None => fails.push(format!(
                "the relay doesn't answer on 127.0.0.1:{}, so sessions get connection errors; {}",
                cfg.port,
                last_relay_log_line(paths)
            )),
        },
        Ok(None) => fails.push(format!(
            "relay mode is on but {} is not there, so new sessions start without the relay (`cc-rotate-ctl relay on --home {again}` writes it)",
            paths.relay_config().display()
        )),
        Err(e) => fails.push(format!("{e:#}")),
    }

    let settings: Value = std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let set: Vec<&str> = RELAY_ENV_OVERRIDES
        .iter()
        .copied()
        .filter(|n| {
            settings
                .pointer(&format!("/env/{n}"))
                .and_then(Value::as_str)
                .is_some_and(|v| !v.trim().is_empty())
        })
        .collect();
    if !set.is_empty() {
        warns.push(format!(
            "the profile's settings.json sets {} in env, which takes sessions off the relay or off the home account's login",
            set.join(" and ")
        ));
    }
    match relay::bypass_count(store, time::now_ms()) {
        Ok(0) | Err(_) => {}
        Ok(n) => warns.push(format!(
            "{n} {} without the relay's address in the last 10 minutes: they run on the home account directly, off the pool, until restarted",
            if n == 1 { "session ran" } else { "sessions ran" }
        )),
    }

    let status = if !fails.is_empty() {
        Status::Fail
    } else if !warns.is_empty() {
        Status::Warn
    } else {
        Status::Ok
    };
    let problems: Vec<String> = fails.into_iter().chain(warns).collect();
    let detail = match (problems.is_empty(), ok) {
        (true, Some(ok)) => ok,
        (_, Some(ok)) => format!("{ok}, but {}", problems.join("; ")),
        (_, None) => problems.join("; "),
    };
    check(name, status, detail)
}

/// The relay log's last line, without its time stamp, for when the relay doesn't answer.
fn last_relay_log_line(paths: &Paths) -> String {
    let path = paths.relay_log();
    let last = std::fs::read_to_string(&path).ok().and_then(|text| {
        text.lines().rev().find(|l| !l.trim().is_empty()).map(|l| {
            // Each line starts with the time in milliseconds.
            let stamped = l
                .split_once(' ')
                .and_then(|(ts, rest)| Some((ts.parse::<i64>().ok()?, rest)));
            match stamped {
                Some((ts, rest)) => format!("{} ({})", rest.trim(), age(time::now_ms() - ts)),
                None => l.trim().to_string(),
            }
        })
    });
    match last {
        Some(line) => format!("the last line of {}: {line}", path.display()),
        None => format!("{} has nothing in it", path.display()),
    }
}

/// herdr is optional: this only warns when herdr is here but would resume cc-rotate sessions
/// with plain `claude` (spec section 19).
fn herdr_check(paths: &Paths, detected: Result<herdr::Detected, crate::paths::NotFound>) -> Check {
    let name = "herdr";
    let hook = std::fs::read_to_string(paths.settings_file())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .is_some_and(|s| herdr::session_start_hook_installed(&s));
    let detected = match detected {
        Ok(detected) => detected,
        Err(refused @ crate::paths::NotFound::Refused { .. }) => {
            return check(
                name,
                Status::Warn,
                format!(
                    "{}. Add its folder to PATH if you trust it.",
                    refused.refusal("herdr").unwrap_or_default()
                ),
            );
        }
        Err(crate::paths::NotFound::Missing) => {
            return check(
                name,
                Status::Ok,
                if hook {
                    "Not installed (optional). The profile is ready for it."
                } else {
                    "Not installed (optional). If you start using it, run `cc-rotate-ctl setup --force` so it resumes sessions with cc-rotate."
                },
            );
        }
    };
    // The check's name is "herdr", so the detail starts with what was found.
    let version = herdr::version(detected.bin());
    let found = match (&version, &detected) {
        (Some(v), _) => format!("Version {v}"),
        (None, herdr::Detected::InPane(_)) => "Running in a herdr pane".to_string(),
        (None, herdr::Detected::Installed(p)) => format!("Found at {}", p.display()),
    };
    let (major, minor, patch) = herdr::MIN_VERSION;
    if let Some(v) = version.as_deref().and_then(herdr::parse_version)
        && v < herdr::MIN_VERSION
    {
        return check(
            name,
            Status::Warn,
            format!(
                "{found} resumes cc-rotate sessions with plain `claude` on your default profile. Update herdr to {major}.{minor}.{patch} or newer."
            ),
        );
    }
    if !hook {
        return check(
            name,
            Status::Warn,
            format!(
                "{found}, but the profile has no SessionStart hook for it, so herdr resumes cc-rotate sessions with plain `claude`. Run `cc-rotate-ctl setup --force`."
            ),
        );
    }
    check(
        name,
        Status::Ok,
        format!("{found}. Restored panes resume with `cc-rotate --resume`."),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_app_line() {
        use crate::desktop::{Launcher, Platform, Policy, Setup};
        let setup = |policy: Policy, launcher: Launcher| Setup {
            platform: Platform::MacOs,
            policy,
            policy_foreign: false,
            launcher,
            launcher_foreign: false,
        };
        let status = |s: &Setup, stop_hook: bool| desktop_check(s, stop_hook).status;

        let off = desktop_check(&setup(Policy::Off, Launcher::None), false);
        assert_eq!(off.name, "Desktop app");
        assert_eq!(off.status, Status::Ok);
        assert!(off.detail.contains("app's account"), "{}", off.detail);
        assert_eq!(status(&setup(Policy::On, Launcher::Ours), true), Status::Ok);
        let no_hook = desktop_check(&setup(Policy::On, Launcher::Ours), false);
        assert_eq!(no_hook.status, Status::Warn);
        assert!(
            no_hook
                .detail
                .contains("cc-rotate-ctl setup --keep-existing")
        );
        // Half set up.
        assert_eq!(
            status(&setup(Policy::On, Launcher::None), true),
            Status::Warn
        );
        assert_eq!(
            status(&setup(Policy::Off, Launcher::Ours), true),
            Status::Warn
        );
        assert_eq!(
            status(&setup(Policy::Disabled, Launcher::Ours), true),
            Status::Warn
        );
        // Another launcher that is there runs the sessions, just not on the pool; a missing one
        // stops the app from starting any.
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("corp-launcher");
        std::fs::write(&present, "").unwrap();
        let other = |p: &std::path::Path| Launcher::Other(p.to_string_lossy().into_owned());
        assert_eq!(
            status(&setup(Policy::On, other(&present)), true),
            Status::Warn
        );
        assert_eq!(
            status(&setup(Policy::On, other(&dir.path().join("gone"))), true),
            Status::Fail
        );
        assert_eq!(
            status(
                &setup(
                    Policy::On,
                    Launcher::Other(serde_json::json!([dir.path().join("gone"), "x"]).to_string())
                ),
                true
            ),
            Status::Fail
        );
        // Without the policy, another launcher is not the app's business.
        assert_eq!(
            status(&setup(Policy::Off, other(&dir.path().join("gone"))), true),
            Status::Ok
        );
        assert_eq!(
            status(&setup(Policy::On, Launcher::Unreadable("x".into())), true),
            Status::Warn
        );
        let unsupported = Setup {
            platform: Platform::Unsupported,
            ..setup(Policy::Off, Launcher::None)
        };
        assert_eq!(status(&unsupported, false), Status::Ok);
    }

    #[test]
    fn environment_check_finds_overrides_and_the_retry_watchdog() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("data"), dir.path().join("profile"));
        std::fs::create_dir_all(&paths.profile_dir).unwrap();
        let none = |_: &str| None;
        assert_eq!(environment_check(&paths, none).status, Status::Ok);

        // Claude Code asks before it uses an API key: a warning, not a failure.
        let key = |k: &str| (k == "ANTHROPIC_API_KEY").then(|| "sk-x".to_string());
        assert_eq!(environment_check(&paths, key).status, Status::Warn);

        let token = |k: &str| (k == "CLAUDE_CODE_OAUTH_TOKEN").then(|| "x".to_string());
        let c = environment_check(&paths, token);
        assert_eq!(c.status, Status::Fail);
        assert!(
            c.detail
                .contains("CLAUDE_CODE_OAUTH_TOKEN is set in your environment")
        );

        std::fs::write(
            paths.settings_file(),
            r#"{"env":{"CLAUDE_CODE_RETRY_WATCHDOG":"1"}}"#,
        )
        .unwrap();
        let c = environment_check(&paths, none);
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("profile's settings.json"), "{}", c.detail);

        let off = |k: &str| (k == RETRY_WATCHDOG).then(|| "0".to_string());
        std::fs::write(paths.settings_file(), r#"{"apiKeyHelper":"get-key"}"#).unwrap();
        let c = environment_check(&paths, off);
        assert_eq!(c.status, Status::Fail);
        assert!(c.detail.contains("apiKeyHelper"));
        assert!(!c.detail.contains(RETRY_WATCHDOG));
    }

    mod relay {
        use super::super::*;
        use crate::relay::{self as relay_mode, RelayConfig};
        use crate::relay_service::ServiceState;
        use crate::store::AccountIdentity;

        struct Fixture {
            store: Store,
            paths: Paths,
            home: i64,
            cfg: RelayConfig,
            _dir: tempfile::TempDir,
        }

        /// Relay mode on with `Haven` (parked) as the home account and `b` active, `relay.json`
        /// written, and an empty profile.
        fn on() -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path().join("data"), dir.path().join("profile"));
            std::fs::create_dir_all(&paths.profile_dir).unwrap();
            let store = Store::open_in_memory().unwrap();
            let home = store
                .insert_account(HOME, &AccountIdentity::default(), AccountState::Parked)
                .unwrap();
            store
                .insert_account("b", &AccountIdentity::default(), AccountState::Active)
                .unwrap();
            relay_mode::set_mode(&store, Some(home)).unwrap();
            let cfg = RelayConfig::create(&paths.relay_config()).unwrap();
            Fixture {
                store,
                paths,
                home,
                cfg,
                _dir: dir,
            }
        }

        const HOME: &str = "Haven";

        fn this_version(_: u16) -> Option<String> {
            Some(env!("CARGO_PKG_VERSION").to_string())
        }

        fn run(
            f: &Fixture,
            service: ServiceState,
            version: impl Fn(u16) -> Option<String>,
        ) -> Check {
            relay_check(&f.store, &f.paths, || service, version)
        }

        #[test]
        fn relay_off_ok() {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::new(dir.path().join("data"), dir.path().join("profile"));
            let store = Store::open_in_memory().unwrap();
            let c = relay_check(
                &store,
                &paths,
                || panic!("relay mode is off: no service to ask about"),
                |_| panic!("relay mode is off: nothing to ask"),
            );
            assert_eq!(c.name, "Relay");
            assert_eq!(c.status, Status::Ok);
            assert_eq!(c.detail, "relay mode is off");
        }

        #[test]
        fn relay_ok() {
            let f = on();
            let port = f.cfg.port;
            let c = run(&f, ServiceState::Installed, |p| {
                assert_eq!(p, port);
                this_version(p)
            });
            assert_eq!(c.status, Status::Ok, "{}", c.detail);
            assert!(
                c.detail
                    .contains(&format!("with {HOME} as the home account")),
                "{}",
                c.detail
            );
            assert!(c.detail.contains(&port.to_string()), "{}", c.detail);
            assert!(!c.detail.contains(&f.cfg.secret), "{}", c.detail);
        }

        #[test]
        fn relay_other_version_warns() {
            let f = on();
            let c = run(&f, ServiceState::Installed, |_| Some("0.0.1-old".into()));
            assert_eq!(c.status, Status::Warn, "{}", c.detail);
            assert!(c.detail.contains("0.0.1-old"), "{}", c.detail);
        }

        #[test]
        fn relay_not_answering_fails() {
            let f = on();
            std::fs::create_dir_all(f.paths.logs_dir()).unwrap();
            std::fs::write(
                f.paths.relay_log(),
                "1800000000000 GET /v1/models 200 req_1 account=-\n1800000000001 cannot start: the port is taken\n",
            )
            .unwrap();
            let c = run(&f, ServiceState::Installed, |_| None);
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
            assert!(
                c.detail.contains("cannot start: the port is taken"),
                "{}",
                c.detail
            );
            assert!(!c.detail.contains("/v1/models"), "{}", c.detail);

            // No log at all: still a failure, said plainly.
            std::fs::remove_file(f.paths.relay_log()).unwrap();
            let c = run(&f, ServiceState::Installed, |_| None);
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
        }

        #[test]
        fn relay_service_missing_fails() {
            let f = on();
            let c = run(&f, ServiceState::Missing, this_version);
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
            assert!(c.detail.contains("service"), "{}", c.detail);
        }

        #[test]
        fn home_signed_out_fails() {
            let f = on();
            f.store.set_state(f.home, AccountState::NeedsLogin).unwrap();
            let c = run(&f, ServiceState::Installed, this_version);
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
            assert!(
                c.detail.contains(
                    "the home account Haven is signed out: every session fails until it signs in again (`cc-rotate-ctl login --relogin Haven`)"
                ),
                "{}",
                c.detail
            );

            // Removed from cc-rotate while relay mode stays on.
            f.store
                .conn()
                .execute("DELETE FROM accounts WHERE id = ?1", [f.home])
                .unwrap();
            let c = run(&f, ServiceState::Missing, this_version);
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
            assert!(c.detail.contains("no longer in cc-rotate"), "{}", c.detail);
            // Nothing suggests signing in to, or reinstalling for, an account that is gone.
            assert!(!c.detail.contains("--relogin"), "{}", c.detail);
            assert!(
                !c.detail.contains(&format!("--home account {}", f.home)),
                "{}",
                c.detail
            );
            assert!(
                c.detail.contains("relay on --home <another account>"),
                "{}",
                c.detail
            );
        }

        fn oauth(refresh: &str) -> serde_json::Value {
            serde_json::json!({
                "accessToken": format!("access-{refresh}"),
                "refreshToken": refresh,
                "expiresAt": time::now_ms() + 3_600_000,
            })
        }

        /// In relay mode the profile's login is the home account's, whichever account is active.
        #[test]
        fn profile_login_is_the_home_accounts_in_relay_mode() {
            let f = on();
            let active = f.store.active_account().unwrap().unwrap().id;
            f.store
                .save_credential(f.home, &oauth("refresh-home"), "login")
                .unwrap();
            f.store
                .save_credential(active, &oauth("refresh-b"), "login")
                .unwrap();
            std::fs::write(
                f.paths.credentials_file(),
                serde_json::json!({ "claudeAiOauth": oauth("refresh-home") }).to_string(),
            )
            .unwrap();
            let c = profile_login_check(&f.store, &f.paths).unwrap();
            assert_eq!(c.status, Status::Ok, "{}", c.detail);
            assert_eq!(c.detail, format!("Matches {HOME}"));

            // With relay mode off the profile belongs to the active account: no match.
            relay_mode::set_mode(&f.store, None).unwrap();
            let c = profile_login_check(&f.store, &f.paths).unwrap();
            assert!(!c.detail.contains("Matches"), "{}", c.detail);
        }

        #[test]
        fn home_without_a_saved_login_is_not_called_in_use() {
            let f = on();
            std::fs::write(
                f.paths.credentials_file(),
                serde_json::json!({ "claudeAiOauth": oauth("refresh-home") }).to_string(),
            )
            .unwrap();
            let c = profile_login_check(&f.store, &f.paths).unwrap();
            assert_eq!(c.status, Status::Fail, "{}", c.detail);
            assert!(c.detail.contains("home account Haven"), "{}", c.detail);
            assert!(!c.detail.contains("in use"), "{}", c.detail);
        }

        #[test]
        fn relay_env_override_warns() {
            let f = on();
            for name in [
                "ANTHROPIC_BASE_URL",
                "ANTHROPIC_UNIX_SOCKET",
                "ANTHROPIC_API_KEY",
                "ANTHROPIC_AUTH_TOKEN",
                "CLAUDE_CODE_OAUTH_TOKEN",
            ] {
                std::fs::write(
                    f.paths.settings_file(),
                    serde_json::json!({ "env": { name: "x" } }).to_string(),
                )
                .unwrap();
                let c = run(&f, ServiceState::Installed, this_version);
                assert_eq!(c.status, Status::Warn, "{name}: {}", c.detail);
                assert!(c.detail.contains(name), "{}", c.detail);
            }
            // Other variables are none of the relay's business.
            std::fs::write(
                f.paths.settings_file(),
                r#"{"env":{"CLAUDE_CODE_MAX_OUTPUT_TOKENS":"1"}}"#,
            )
            .unwrap();
            assert_eq!(
                run(&f, ServiceState::Installed, this_version).status,
                Status::Ok
            );
        }

        #[test]
        fn relay_bypass_warns() {
            let f = on();
            let now = time::now_ms();
            relay_mode::note_bypass(&f.store, "s1", now - 1000).unwrap();
            relay_mode::note_bypass(&f.store, "s2", now - 2000).unwrap();
            let c = run(&f, ServiceState::Installed, this_version);
            assert_eq!(c.status, Status::Warn, "{}", c.detail);
            assert!(c.detail.contains('2'), "{}", c.detail);
        }
    }

    #[test]
    fn herdr_check_is_soft() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::new(root.path().join("data"), root.path().join("profile"));
        std::fs::create_dir_all(&paths.profile_dir).unwrap();
        let missing_bin = herdr::Detected::Installed(root.path().join("no-such-herdr"));

        // No profile settings at all: herdr is optional, so this never fails.
        let c = herdr_check(&paths, Err(crate::paths::NotFound::Missing));
        assert_eq!(c.status, Status::Ok);
        assert!(c.detail.contains("setup --force"), "{}", c.detail);

        let c = herdr_check(&paths, Ok(missing_bin.clone()));
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
        let c = herdr_check(&paths, Ok(missing_bin));
        assert_eq!(c.status, Status::Ok, "{}", c.detail);
        assert!(c.detail.contains("cc-rotate --resume"), "{}", c.detail);
        assert_eq!(
            herdr_check(&paths, Err(crate::paths::NotFound::Missing)).status,
            Status::Ok
        );
        // Found where someone else could have put it: said, not passed over in silence.
        let refused = crate::paths::NotFound::Refused {
            program: root.path().join("herdr"),
            step: root.path().to_path_buf(),
        };
        let c = herdr_check(&paths, Err(refused));
        assert_eq!(c.status, Status::Warn);
        assert!(c.detail.contains("can't confirm"), "{}", c.detail);
    }
}
