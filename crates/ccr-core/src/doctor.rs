//! `cc-rotate-ctl doctor`: health checks for the store, the profile, and Claude Code.

use std::path::PathBuf;
use std::process::{Command, Stdio};

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
            "Couldn't run `claude --version`. Install Claude Code or check your PATH.",
        )),
    }

    checks.push(profile_settings_check(paths));
    checks.push(environment_check(paths, |k| std::env::var(k).ok()));
    checks.push(herdr_check(paths, herdr::detect()));

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

    if let Some(account) = active.first() {
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
        checks.push(match (file_fp, stored_fp) {
            (Some(f), Some(s)) if f == s => check("Profile login", Status::Ok, format!("Matches {}", account.label)),
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
                    format!("The profile has a newer login. It's saved once Claude Code confirms it belongs to {}.", account.label),
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
            (None, _) => check("Profile login", Status::Fail, "The profile has no usable login"),
            (_, None) => check(
                "Profile login",
                Status::Fail,
                format!("{} is in use but has no saved login. Sign in to it again.", account.label),
            ),
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
                "Not installed (optional). The profile is ready for it."
            } else {
                "Not installed (optional). If you start using it, run `cc-rotate-ctl setup --force` so it resumes sessions with cc-rotate."
            },
        );
    };
    // The check's name is "herdr", so the detail starts with what was found.
    let version = herdr::version(detected.bin());
    let found = match (&version, &detected) {
        (Some(v), _) => format!("Version {v}"),
        (None, herdr::Detected::InPane(_)) => "Running in a herdr pane".to_string(),
        (None, herdr::Detected::OnPath(p)) => format!("Found at {}", p.display()),
    };
    let (major, minor, patch) = herdr::MIN_VERSION;
    if let Some(v) = version.as_deref().and_then(herdr::parse_version) {
        if v < herdr::MIN_VERSION {
            return check(
                name,
                Status::Warn,
                format!(
                    "{found} resumes cc-rotate sessions with plain `claude` on your default profile. Update herdr to {major}.{minor}.{patch} or newer."
                ),
            );
        }
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
