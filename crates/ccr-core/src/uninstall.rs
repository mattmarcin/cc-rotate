//! Removing cc-rotate (spec section 4.5): first what its install and its use leave outside its own
//! folders (the app starting at login, the macOS Keychain items of the rotation profile and of
//! leftover scratch profiles, observe mode in the user's other profiles, the PowerShell function
//! older versions added, the app's webview data), then the programs, the rotation profile and the
//! data folder.
//!
//! `plan` only reads; `run` carries a plan out. Whatever reaches beyond the user's files (the
//! running app, start at login, the Keychain, the PowerShell profiles, the app's webview data)
//! goes through `System`, so tests can never touch the real install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::credstore::Credentials;
use crate::paths::{Paths, in_build_folder, legacy_lock_for};
use crate::store::Store;
use crate::{credfile, profiles, secret, setup, statusline, time};

/// The name the app registers to start at login under (the Windows `Run` value, the macOS
/// LaunchAgent label and plist, the Linux `.desktop` file), and its executable's name.
pub const AUTOSTART_NAME: &str = "cc-rotate";
pub const APP_NAME: &str = "cc-rotate-app";
/// The app's identifier (`tauri.conf.json`), which names the folders its webview keeps data in.
pub const APP_IDENTIFIER: &str = "com.ccrotate.app";
/// The programs an install puts in its folder (`scripts/install.*`).
pub const PROGRAMS: [&str; 3] = ["cc-rotate", "cc-rotate-ctl", "cc-rotate-app"];
/// The data folder's name where cc-rotate puts it (`Paths::default_data_dir`).
const DATA_DIR_NAME: &str = "cc-rotate";

/// How long quitting the app may take.
const STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// A status line tick within this long means a session in the cc-rotate profile is open: open
/// sessions tick at least every 30 s (`refreshInterval`), idle or not.
pub const SESSION_OPEN_MS: i64 = 2 * 60_000;

/// What removing cc-rotate would do, and what it leaves alone and why.
#[derive(Debug, Default)]
pub struct Plan {
    /// The app's start-at-login entry, described for the user.
    pub autostart: Option<String>,
    /// An entry by that name that is not the app's, left alone.
    pub autostart_foreign: Option<String>,
    /// The rotation profile's folder, whether it is there or not.
    pub profile: PathBuf,
    /// Claude Code's lock beside the profile (`paths::legacy_lock_for`), when it is there and the
    /// profile goes or is gone.
    pub profile_lock: Option<PathBuf>,
    /// The rotation profile's login in the macOS Keychain.
    pub keychain: KeychainItem,
    /// Leftover scratch profiles (`staging/`) whose login is in the macOS Keychain.
    pub staging_items: Vec<PathBuf>,
    /// Other profiles whose status line runs cc-rotate in observe mode.
    pub observe: Vec<ObserveMode>,
    /// PowerShell profiles that still define the function older versions added.
    pub legacy_alias: Vec<PathBuf>,
    pub profile_dir: Removal,
    pub data_dir: DataRemoval,
    /// The folders the app's webview keeps its data in.
    pub app_data: Vec<PathBuf>,
    /// The installed programs, and copies earlier upgrades renamed aside (Windows).
    pub programs: Vec<PathBuf>,
    /// Why the programs are left alone, when they are.
    pub programs_kept: Option<String>,
    pub sessions: Sessions,
}

impl Plan {
    /// Whether `run` deletes the rotation profile's Keychain item: it is there, and the profile
    /// goes too or is gone already.
    pub fn deletes_keychain_item(&self) -> bool {
        matches!(self.keychain, KeychainItem::Present(_))
            && !matches!(self.profile_dir, Removal::Keep(..))
    }
}

/// Whether sessions in the cc-rotate profile are open (`SESSION_OPEN_MS`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Sessions {
    #[default]
    Closed,
    Open,
    /// The store could not be read to tell.
    Unknown,
}

/// A login in the macOS Keychain (`credstore`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum KeychainItem {
    /// Not macOS, or nothing there.
    #[default]
    None,
    Present(String),
    /// This session cannot use the Keychain (over SSH): the item may be there.
    Unreachable(String),
}

/// A folder `run` deletes, or leaves alone for the reason given.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Removal {
    #[default]
    Absent,
    Delete(PathBuf),
    Keep(PathBuf, String),
}

/// The data folder: deleted whole when it is where cc-rotate puts it, else only cc-rotate's
/// entries in it (`data_entries`), its folders and itself going too when that empties them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum DataRemoval {
    #[default]
    Absent,
    Whole(PathBuf),
    Entries(PathBuf, Vec<PathBuf>),
    Keep(PathBuf, String),
}

/// A profile whose status line runs `cc-rotate-ctl statusline --observe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveMode {
    pub settings: PathBuf,
    /// The status line command as it is now; `run` changes it only if it still is.
    pub command: String,
    pub restore: Restore,
}

/// How a profile's status line comes back once cc-rotate is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    /// Observe mode was all it ran: the status line goes.
    Remove,
    /// The command observe mode ran after cc-rotate's (`--then`) becomes the status line.
    Command(String),
    /// More than cc-rotate's observe mode, or written in a way not taken apart: the user changes
    /// it.
    ByHand,
}

/// What `run` did, step by step.
#[derive(Debug, Default)]
pub struct Outcome {
    pub done: Vec<String>,
    pub failed: Vec<String>,
    /// This program, which Windows will not delete while it runs: renamed aside, and deleted by
    /// a helper once this process exits.
    pub deleted_later: Vec<PathBuf>,
    /// Programs another process still runs (an open session), renamed aside: the helper tries
    /// them too for a minute, but they may still run then, so the user deletes what is left.
    pub left: Vec<PathBuf>,
    /// What stays in a data folder that held more than cc-rotate's files.
    pub data_left: Option<PathBuf>,
    /// An earlier step failed, so the programs stay for running it again.
    pub programs_kept: bool,
}

/// Whatever reaches beyond the user's files.
pub trait System {
    /// The app's start-at-login entry: `Ok(Some((describes, is_the_app)))` when one by its name
    /// is there.
    fn autostart(&self) -> Result<Option<(String, bool)>>;
    fn remove_autostart(&self) -> Result<()>;
    /// Quit the app, if it runs.
    fn stop_app(&self) -> Result<()>;
    /// The Keychain item of the profile Claude Code runs with `CLAUDE_CONFIG_DIR=<profile>`.
    fn keychain(&self, profile: &Path) -> Result<KeychainItem>;
    fn delete_keychain_item(&self, profile: &Path) -> Result<()>;
    /// PowerShell profiles with the function older versions added (`setup`).
    fn legacy_alias(&self) -> Vec<PathBuf>;
    fn remove_legacy_alias(&self) -> Result<()>;
    /// The folders the app's webview keeps its data in, those that are there.
    fn app_data(&self) -> Vec<PathBuf>;
}

/// The real system, for the names and places an install uses. Tests point it at names and places
/// of their own.
pub struct RealSystem {
    pub autostart_name: String,
    pub app_name: String,
    /// Windows: the `Run` key and its `StartupApproved` twin, under `HKEY_CURRENT_USER`.
    pub run_key: String,
    pub approved_key: String,
    /// macOS: where LaunchAgents live; whether to unload the agent with `launchctl`.
    pub launch_agents: PathBuf,
    pub launchctl: bool,
    /// Linux: the autostart folder the app writes to (`~/.config/autostart`, whatever
    /// `XDG_CONFIG_HOME` says, as the `auto-launch` crate does).
    pub xdg_autostart: PathBuf,
    /// Where the app's webview may keep data.
    pub app_data: Vec<PathBuf>,
}

impl RealSystem {
    pub fn installed(home: &Path) -> RealSystem {
        let mut app_data = Vec::new();
        if cfg!(target_os = "macos") {
            let library = home.join("Library");
            for dir in ["WebKit", "Caches", "Application Support"] {
                app_data.push(library.join(dir).join(APP_IDENTIFIER));
            }
        } else {
            for base in [dirs::data_local_dir(), dirs::data_dir(), dirs::cache_dir()]
                .into_iter()
                .flatten()
            {
                let dir = base.join(APP_IDENTIFIER);
                if !app_data.contains(&dir) {
                    app_data.push(dir);
                }
            }
        }
        RealSystem {
            autostart_name: AUTOSTART_NAME.into(),
            app_name: APP_NAME.into(),
            run_key: r"Software\Microsoft\Windows\CurrentVersion\Run".into(),
            approved_key: r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
                .into(),
            launch_agents: home.join("Library").join("LaunchAgents"),
            launchctl: true,
            xdg_autostart: home.join(".config").join("autostart"),
            app_data,
        }
    }

    /// The file macOS or Linux starts the app from at login.
    fn autostart_file(&self) -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            Some(
                self.launch_agents
                    .join(format!("{}.plist", self.autostart_name)),
            )
        } else if cfg!(unix) {
            Some(
                self.xdg_autostart
                    .join(format!("{}.desktop", self.autostart_name)),
            )
        } else {
            None
        }
    }
}

impl System for RealSystem {
    fn autostart(&self) -> Result<Option<(String, bool)>> {
        #[cfg(windows)]
        {
            Ok(
                registry::read(&self.run_key, &self.autostart_name)?.map(|command| {
                    let ours = command.contains(&self.app_name);
                    (
                        format!(
                            r"the {} entry in HKEY_CURRENT_USER\{} ({command})",
                            self.autostart_name, self.run_key
                        ),
                        ours,
                    )
                }),
            )
        }
        #[cfg(not(windows))]
        {
            let Some(file) = self.autostart_file() else {
                return Ok(None);
            };
            match std::fs::read_to_string(&file) {
                Ok(text) => Ok(Some((
                    file.display().to_string(),
                    text.contains(&self.app_name),
                ))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
            }
        }
    }

    fn remove_autostart(&self) -> Result<()> {
        #[cfg(windows)]
        {
            registry::delete(&self.run_key, &self.autostart_name)?;
            // Task Manager's on or off for it; meaningless without the entry.
            registry::delete(&self.approved_key, &self.autostart_name)?;
        }
        if cfg!(target_os = "macos") && self.launchctl {
            // Unloaded first, so launchd does not start it again; fails when not loaded.
            let _ = std::process::Command::new("/bin/launchctl")
                .arg("bootout")
                .arg(format!("gui/{}/{}", uid(), self.autostart_name))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        if let Some(file) = self.autostart_file() {
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("deleting {}", file.display())),
            }
        }
        Ok(())
    }

    fn stop_app(&self) -> Result<()> {
        let mut cmd = if cfg!(windows) {
            let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
            let mut cmd = std::process::Command::new(
                PathBuf::from(root).join("System32").join("taskkill.exe"),
            );
            // Only this user's. Not `/T` with it: together they hang. Its WebView2 processes
            // end with it, and deleting their folder is tried for a few seconds (`remove_dir`).
            cmd.args(["/F", "/IM"]).arg(format!(
                "{}{}",
                self.app_name,
                std::env::consts::EXE_SUFFIX
            ));
            if let Some(user) = std::env::var_os("USERNAME") {
                let mut filter = std::ffi::OsString::from("USERNAME eq ");
                filter.push(user);
                cmd.arg("/FI").arg(filter);
            }
            cmd
        } else {
            let mut cmd = std::process::Command::new("pkill");
            cmd.arg("-x")
                .arg("-u")
                .arg(uid().to_string())
                .arg(&self.app_name);
            cmd
        };
        let mut child = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("running taskkill or pkill")?;
        let deadline = std::time::Instant::now() + STOP_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("it didn't answer within {} s", STOP_TIMEOUT.as_secs());
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        // taskkill: 0 stopped, or with a filter nothing to stop, 128 nothing to stop; pkill: 0
        // stopped, 1 nothing to stop.
        match status.code() {
            Some(0) => {
                // Until it lets go of its files.
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            }
            Some(1) if !cfg!(windows) => Ok(()),
            Some(128) if cfg!(windows) => Ok(()),
            other => bail!("exit {other:?}"),
        }
    }

    fn keychain(&self, profile: &Path) -> Result<KeychainItem> {
        let creds = Credentials::for_profile(profile);
        let Some(keychain) = creds.keychain() else {
            return Ok(KeychainItem::None);
        };
        let service = keychain.service.clone();
        if creds.keychain_locked(Duration::from_secs(5))? {
            return Ok(KeychainItem::Unreachable(service));
        }
        Ok(match keychain.read()? {
            Some(_) => KeychainItem::Present(service),
            None => KeychainItem::None,
        })
    }

    fn delete_keychain_item(&self, profile: &Path) -> Result<()> {
        Credentials::for_profile(profile).delete_keychain_item()
    }

    fn legacy_alias(&self) -> Vec<PathBuf> {
        setup::legacy_alias_files()
    }

    fn remove_legacy_alias(&self) -> Result<()> {
        setup::remove_powershell_alias().map(|_| ())
    }

    fn app_data(&self) -> Vec<PathBuf> {
        self.app_data
            .iter()
            .filter(|d| d.is_dir())
            .cloned()
            .collect()
    }
}

#[cfg(unix)]
fn uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn uid() -> u32 {
    0
}

/// What removing cc-rotate would do. Reads only. `store` is the store opened read-only, `None`
/// when there is none or it could not be read; `this_exe` is the running `cc-rotate-ctl`, whose
/// folder is the install folder.
pub fn plan(
    system: &dyn System,
    paths: &Paths,
    store: Option<&Store>,
    this_exe: &Path,
    now_ms: i64,
) -> Result<Plan> {
    let mut plan = Plan {
        profile: paths.profile_dir.clone(),
        ..Default::default()
    };
    match system.autostart()? {
        Some((entry, true)) => plan.autostart = Some(entry),
        Some((entry, false)) => plan.autostart_foreign = Some(entry),
        None => {}
    }
    plan.keychain = system.keychain(&paths.profile_dir)?;
    plan.observe = observe_modes(paths)?;
    plan.legacy_alias = system.legacy_alias();
    plan.profile_dir = profile_removal(paths);
    if !matches!(plan.profile_dir, Removal::Keep(..)) {
        let lock = legacy_lock_for(&paths.profile_dir);
        plan.profile_lock = std::fs::symlink_metadata(&lock).is_ok().then_some(lock);
    }
    plan.data_dir = data_removal(paths)?;
    let reachable = !matches!(plan.keychain, KeychainItem::Unreachable(_));
    if reachable && !matches!(plan.data_dir, DataRemoval::Absent | DataRemoval::Keep(..)) {
        for dir in subfolders(&paths.staging_root()) {
            if matches!(system.keychain(&dir)?, KeychainItem::Present(_)) {
                plan.staging_items.push(dir);
            }
        }
    }
    plan.app_data = system.app_data();
    plan.sessions = match store {
        Some(store) => {
            let ticked = store.meta_i64(statusline::META_LAST_TICK_AT)?;
            if ticked.is_some_and(|t| now_ms - t < SESSION_OPEN_MS) {
                Sessions::Open
            } else {
                Sessions::Closed
            }
        }
        None if paths.store_db().exists() => Sessions::Unknown,
        None => Sessions::Closed,
    };
    if in_build_folder(this_exe) {
        plan.programs_kept = Some(format!(
            "{} is a build's output, not an install",
            this_exe.display()
        ));
    } else if let Some(dir) = this_exe.parent() {
        plan.programs = programs_in(dir)?;
    }
    Ok(plan)
}

fn subfolders(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

/// The installed programs in `dir`, and (Windows) the copies upgrades renamed aside while they
/// ran (`<name>.exe.<time>.old`).
pub fn programs_in(dir: &Path) -> Result<Vec<PathBuf>> {
    let suffix = std::env::consts::EXE_SUFFIX;
    let names: Vec<String> = PROGRAMS.iter().map(|p| format!("{p}{suffix}")).collect();
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = names.iter().any(|n| {
            name == *n
                || (cfg!(windows)
                    && name
                        .strip_prefix(n.as_str())
                        .is_some_and(|rest| rest.starts_with('.') && rest.ends_with(".old")))
        });
        if ours && entry.file_type()?.is_file() {
            found.push(entry.path());
        }
    }
    found.sort();
    Ok(found)
}

/// The rotation profile, unless it is not one: the home folder, the default profile, or a folder
/// whose status line is not cc-rotate's are never deleted.
fn profile_removal(paths: &Paths) -> Removal {
    let dir = &paths.profile_dir;
    if !dir.exists() {
        return Removal::Absent;
    }
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(dir).unwrap_or_default();
        return Removal::Keep(
            dir.clone(),
            format!(
                "it is a link to {}; delete that folder yourself if it is the cc-rotate profile",
                target.display()
            ),
        );
    }
    if is_home_or_root(dir, &paths.home) || profiles::is_default_profile(dir, &paths.home) {
        return Removal::Keep(dir.clone(), "it is not a profile cc-rotate made".into());
    }
    if !runs_cc_rotate(dir) {
        return Removal::Keep(
            dir.clone(),
            "its status line is not cc-rotate's, so it may not be the cc-rotate profile".into(),
        );
    }
    Removal::Delete(dir.clone())
}

/// The data folder: whole when it is named as cc-rotate names it and holds its store or key;
/// otherwise (`CC_ROTATE_DATA_DIR` pointing at a folder of the user's) only cc-rotate's entries.
fn data_removal(paths: &Paths) -> Result<DataRemoval> {
    let dir = &paths.data_dir;
    if !dir.exists() {
        return Ok(DataRemoval::Absent);
    }
    if is_home_or_root(dir, &paths.home) || profiles::is_default_profile(dir, &paths.home) {
        return Ok(DataRemoval::Keep(
            dir.clone(),
            "it is not a folder cc-rotate made".into(),
        ));
    }
    if std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink()) {
        let target = std::fs::read_link(dir).unwrap_or_default();
        return Ok(DataRemoval::Keep(
            dir.clone(),
            format!(
                "it is a link to {}; delete that folder yourself, as it holds every saved login",
                target.display()
            ),
        ));
    }
    if !dir.join("store.db").exists() && !dir.join(secret::KEY_FILE).exists() {
        return Ok(DataRemoval::Keep(
            dir.clone(),
            "it holds neither cc-rotate's store nor its key".into(),
        ));
    }
    let default = dirs::data_local_dir().map(|d| d.join(DATA_DIR_NAME));
    let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if default.is_some_and(|d| canonical(&d) == canonical(dir)) {
        Ok(DataRemoval::Whole(dir.clone()))
    } else {
        Ok(DataRemoval::Entries(dir.clone(), data_entries(dir)?))
    }
}

/// cc-rotate's own entries in a data folder, by the names it gives them: the store and its
/// journal files, the key, the swap lock, and inside its lock, scratch, backup and log folders
/// the entries it makes there (those folders go once empty).
fn data_entries(dir: &Path) -> Result<Vec<PathBuf>> {
    const TOP: [&str; 6] = [
        "store.db",
        "store.db-wal",
        "store.db-shm",
        "store.db-journal",
        secret::KEY_FILE,
        "swap.lock",
    ];
    let mut found: Vec<PathBuf> = TOP
        .iter()
        .map(|name| dir.join(name))
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .collect();
    // What a crash can leave of writing the key or a file atomically (`secret`, `credfile`).
    for entry in std::fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let temp = name.starts_with('.')
            && name.ends_with(".tmp")
            && (name.starts_with(&format!(".{}.", secret::KEY_FILE))
                || name.contains(".cc-rotate-"));
        if temp {
            found.push(entry.path());
        }
    }
    /// A folder, and which names in it are cc-rotate's.
    type Folder = (&'static str, fn(&str) -> bool);
    let inside: [Folder; 4] = [
        ("locks", |n| {
            n.starts_with("account-") && n.ends_with(".lock")
        }),
        ("staging", |n| {
            // Older versions named sign-in folders by a bare id (`usage_check::sweep_logins`).
            let legacy = n.len() == 32 && n.bytes().all(|b| b.is_ascii_hexdigit());
            legacy
                || ["check-", "confirm-", "login-"]
                    .iter()
                    .any(|p| n.starts_with(p))
        }),
        ("backups", |n| {
            n.starts_with("credentials-") && (n.ends_with(".ccr") || n.ends_with(".dpapi"))
        }),
        ("logs", |n| n == "cc-rotate.log" || n == "cc-rotate.log.1"),
    ];
    for (folder, ours) in inside {
        let Ok(entries) = std::fs::read_dir(dir.join(folder)) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            if ours(&entry.file_name().to_string_lossy()) {
                found.push(entry.path());
            }
        }
    }
    found.sort();
    Ok(found)
}

/// The folders `data_entries` looks in, which go once empty.
const DATA_FOLDERS: [&str; 4] = ["locks", "staging", "backups", "logs"];

fn is_home_or_root(dir: &Path, home: &Path) -> bool {
    let dir = dunce::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let home = dunce::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    dir.parent().is_none() || dir == home || home.starts_with(&dir)
}

/// Whether the profile's status line is cc-rotate's own (not observe mode).
fn runs_cc_rotate(dir: &Path) -> bool {
    let Ok(Some(settings)) = credfile::read_document(&dir.join("settings.json")) else {
        return false;
    };
    status_command(&settings)
        .and_then(ctl_args)
        .is_some_and(|args| {
            args.first().is_some_and(|a| a == "statusline")
                && !args.iter().any(|a| a == "--observe")
        })
}

fn status_command(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
}

/// The arguments of a command that only runs `cc-rotate-ctl` (setup may write its path in the
/// Windows 8.3 form), without the global `--profile <dir>`; `None` for anything else.
fn ctl_args(command: &str) -> Option<Vec<String>> {
    let mut words = shell_words(command)?.into_iter();
    if !is_ctl(&words.next()?) {
        return None;
    }
    let mut args = Vec::new();
    while let Some(word) = words.next() {
        if word == "--profile" {
            words.next()?;
        } else if !word.starts_with("--profile=") {
            args.push(word);
        }
    }
    Some(args)
}

/// Whether `program` names `cc-rotate-ctl`.
fn is_ctl(program: &str) -> bool {
    let named = |path: &Path| {
        path.file_stem().is_some_and(|s| {
            let s = s.to_string_lossy();
            if cfg!(windows) {
                s.eq_ignore_ascii_case("cc-rotate-ctl")
            } else {
                s == "cc-rotate-ctl"
            }
        })
    };
    let path = Path::new(program);
    // A short name (`CC-ROT~1.EXE`) says what it is once resolved.
    named(path) || (cfg!(windows) && dunce::canonicalize(path).is_ok_and(|p| named(&p)))
}

/// The words of a simple command: separated by whitespace, with single quotes taken literally and
/// double quotes allowing `\"` and `\\`. `None` when it is more than one simple command (pipes,
/// `;`, `&&`, redirections, substitutions) or a quote is not closed.
fn shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => words.extend(word.take()),
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => match chars.next()? {
                            n @ ('"' | '\\') => w.push(n),
                            n => {
                                w.push('\\');
                                w.push(n);
                            }
                        },
                        '$' | '`' => return None,
                        c => w.push(c),
                    }
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '$' | '`' => return None,
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);
    Some(words)
}

/// The settings files of other profiles whose status line runs cc-rotate in observe mode.
fn observe_modes(paths: &Paths) -> Result<Vec<ObserveMode>> {
    let mut found = Vec::new();
    let dirs = if paths.home.as_os_str().is_empty() {
        Vec::new()
    } else {
        profiles::profile_dirs(paths).unwrap_or_default()
    };
    for dir in dirs {
        for file in ["settings.json", "settings.local.json"] {
            let settings = dir.join(file);
            let Ok(Some(doc)) = credfile::read_document(&settings) else {
                continue;
            };
            let Some(command) = status_command(&doc) else {
                continue;
            };
            if let Some(restore) = restore_for(command) {
                found.push(ObserveMode {
                    settings,
                    command: command.to_string(),
                    restore,
                });
            }
        }
    }
    Ok(found)
}

/// How to restore a status line `command` that runs cc-rotate in observe mode, if it does. Only a
/// command that is exactly `cc-rotate-ctl [--profile <dir>] statusline --observe [--then <one
/// word>]` is taken apart; anything else that mentions observe mode is left for the user.
pub fn restore_for(command: &str) -> Option<Restore> {
    let mentions = command.contains("cc-rotate-ctl")
        && command.contains("statusline")
        && command.contains("--observe");
    let Some(args) = ctl_args(command) else {
        return mentions.then_some(Restore::ByHand);
    };
    let mut args = args.into_iter();
    if args.next().as_deref() != Some("statusline") {
        return mentions.then_some(Restore::ByHand);
    }
    let (mut observe, mut then) = (false, None);
    while let Some(arg) = args.next() {
        if arg == "--observe" {
            observe = true;
        } else if arg == "--then" {
            match args.next() {
                Some(next) => then = Some(next),
                None => return Some(Restore::ByHand),
            }
        } else if let Some(value) = arg.strip_prefix("--then=") {
            then = Some(value.to_string());
        } else {
            return mentions.then_some(Restore::ByHand);
        }
    }
    if !observe {
        return None;
    }
    Some(match then {
        None => Restore::Remove,
        Some(then) if then.trim().is_empty() => Restore::ByHand,
        Some(then) => Restore::Command(then),
    })
}

/// Carry out `plan`, step by step, going on past a step that fails. The store must be closed
/// already (the data folder is deleted). On Windows, programs in use are renamed aside and
/// deleted by a helper once free (`Outcome::deleted_later`), or left for the user.
pub fn run(system: &dyn System, plan: &Plan, this_exe: &Path) -> Outcome {
    let mut out = Outcome::default();
    let mut data_left = None;
    let mut step = |done: String, failing: String, result: Result<()>| match result {
        Ok(()) => out.done.push(done),
        Err(e) => out.failed.push(format!("{failing}: {e:#}")),
    };
    step(
        "Quit the cc-rotate app".into(),
        "Quitting the cc-rotate app".into(),
        system.stop_app(),
    );
    if let Some(entry) = &plan.autostart {
        step(
            format!("Removed start at login ({entry})"),
            format!("Removing start at login ({entry})"),
            system.remove_autostart(),
        );
    }
    for o in &plan.observe {
        if o.restore != Restore::ByHand {
            let settings = o.settings.display();
            step(
                format!(
                    "Took observe mode out of {settings} (it was: {})",
                    o.command
                ),
                format!("Taking observe mode out of {settings}"),
                restore_status_line(&o.settings, &o.command, &o.restore),
            );
        }
    }
    if !plan.legacy_alias.is_empty() {
        step(
            "Removed the old cc-rotate PowerShell function".into(),
            "Removing the old cc-rotate PowerShell function".into(),
            system.remove_legacy_alias(),
        );
    }
    if plan.deletes_keychain_item()
        && let KeychainItem::Present(service) = &plan.keychain
    {
        step(
            format!("Deleted the Keychain item \"{service}\""),
            format!("Deleting the Keychain item \"{service}\""),
            system.delete_keychain_item(&plan.profile),
        );
    }
    for dir in &plan.staging_items {
        step(
            format!("Deleted the Keychain item of {}", dir.display()),
            format!("Deleting the Keychain item of {}", dir.display()),
            system.delete_keychain_item(dir),
        );
    }
    let mut profile_failed = false;
    if let Removal::Delete(dir) = &plan.profile_dir {
        let result = remove_dir(dir);
        profile_failed = result.is_err();
        step(
            format!("Deleted {}", dir.display()),
            format!("Deleting {}", dir.display()),
            result,
        );
    }
    if let Some(lock) = &plan.profile_lock {
        step(
            format!("Deleted {}", lock.display()),
            format!("Deleting {}", lock.display()),
            remove_any(lock),
        );
    }
    // The store says where a profile set with `config profile_dir` is: kept while the profile
    // could not go, so running it again finds it.
    let data_dir = if profile_failed {
        &DataRemoval::Absent
    } else {
        &plan.data_dir
    };
    match data_dir {
        DataRemoval::Whole(dir) => step(
            format!("Deleted {}", dir.display()),
            format!("Deleting {}", dir.display()),
            remove_dir(dir),
        ),
        DataRemoval::Entries(dir, entries) => {
            for entry in entries {
                step(
                    format!("Deleted {}", entry.display()),
                    format!("Deleting {}", entry.display()),
                    remove_any(entry),
                );
            }
            for folder in DATA_FOLDERS {
                // Only when empty: a folder of that name may hold the user's files too.
                let _ = std::fs::remove_dir(dir.join(folder));
            }
            let empty = std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none());
            if empty {
                step(
                    format!("Deleted {}", dir.display()),
                    format!("Deleting {}", dir.display()),
                    std::fs::remove_dir(dir).map_err(Into::into),
                );
            } else if dir.exists() {
                data_left = Some(dir.clone());
            }
        }
        DataRemoval::Absent | DataRemoval::Keep(..) => {}
    }
    for dir in &plan.app_data {
        step(
            format!("Deleted {}", dir.display()),
            format!("Deleting {}", dir.display()),
            remove_dir(dir),
        );
    }
    out.data_left = data_left;
    if !out.failed.is_empty() && !plan.programs.is_empty() {
        // Running it again finishes the job; without the programs it could not.
        out.programs_kept = true;
        return out;
    }
    let mut aside = Vec::new();
    for program in &plan.programs {
        match std::fs::remove_file(program) {
            Ok(()) => out.done.push(format!("Deleted {}", program.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // Windows will not delete a program that runs, but renames it.
            Err(_) if cfg!(windows) => match rename_aside(program) {
                Ok(renamed) => aside.push((program == this_exe, renamed)),
                Err(e) => out
                    .failed
                    .push(format!("Deleting {}: {e:#}", program.display())),
            },
            Err(e) => out
                .failed
                .push(format!("Deleting {}: {e}", program.display())),
        }
    }
    // This program is free once it exits; others in use (open sessions) may stay so.
    let all: Vec<PathBuf> = aside.iter().map(|(_, p)| p.clone()).collect();
    let helper = !all.is_empty() && delete_after_exit(&all).is_ok();
    for (own, path) in aside {
        if own && helper {
            out.deleted_later.push(path);
        } else {
            out.left.push(path);
        }
    }
    out
}

/// Delete a folder; one already gone (it was inside another) is fine. On Windows a process just
/// stopped (the app's WebView2) may hold a file a little longer, so it tries for a few seconds.
fn remove_dir(dir: &Path) -> Result<()> {
    let tries = if cfg!(windows) { 10 } else { 1 };
    for attempt in 1..=tries {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt == tries => return Err(e.into()),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => remove_dir(path),
        Ok(_) => std::fs::remove_file(path).map_err(Into::into),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn rename_aside(program: &Path) -> Result<PathBuf> {
    let renamed = PathBuf::from(format!("{}.{}.old", program.display(), time::now_ms()));
    std::fs::rename(program, &renamed)
        .with_context(|| format!("renaming {}", program.display()))?;
    Ok(renamed)
}

/// Delete `files` once they are free (Windows only lets a running program's file go once it
/// exits), with a `cmd` started on its own that tries every second for a minute. The paths reach
/// it through its environment, so no character in them means anything to `cmd`.
#[cfg(windows)]
fn delete_after_exit(files: &[PathBuf]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    // A console of its own, hidden, which outlives this process; a detached `cmd` would give each
    // `ping` it starts a console window of its own.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // Outliving a job that kills its processes when this one ends (some terminals, CI).
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let vars: Vec<String> = (1..=files.len())
        .map(|i| format!("CCR_DELETE_{i}"))
        .collect();
    let quoted: Vec<String> = vars.iter().map(|v| format!("\"%{v}%\"")).collect();
    let gone: String = quoted
        .iter()
        .map(|q| format!("if not exist {q} "))
        .collect();
    // `ping` by its full path: `cmd` would look in its working folder first.
    let script = format!(
        "/d /v:off /c for /l %i in (1,1,60) do @(\"%CCR_PING%\" -n 2 127.0.0.1 >nul & del /f /q {} 2>nul & {gone}exit)",
        quoted.join(" ")
    );
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let ping = PathBuf::from(&root).join("System32").join("PING.EXE");
    let spawn = |flags: u32| {
        let mut cmd =
            std::process::Command::new(PathBuf::from(&root).join("System32").join("cmd.exe"));
        cmd.raw_arg(&script)
            .current_dir(std::env::temp_dir())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(flags)
            .env("CCR_PING", &ping);
        for (var, file) in vars.iter().zip(files) {
            cmd.env(var, file);
        }
        cmd.spawn()
    };
    spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(CREATE_NO_WINDOW))
        .context("starting the helper that deletes the programs")?;
    Ok(())
}

#[cfg(not(windows))]
fn delete_after_exit(_files: &[PathBuf]) -> Result<()> {
    bail!("only Windows needs this")
}

/// Take observe mode out of a profile's status line, if it is still `command`: drop it, or put
/// back the command it ran after cc-rotate's. Everything else in the file stays as it is.
pub fn restore_status_line(settings: &Path, command: &str, restore: &Restore) -> Result<()> {
    // Through a link (a dotfiles manager's) to the file it points at, so the link stays one.
    let settings = &dunce::canonicalize(settings).unwrap_or_else(|_| settings.to_path_buf());
    let mut doc = credfile::read_document(settings)?
        .with_context(|| format!("{} is gone", settings.display()))?;
    if status_command(&doc) != Some(command) {
        bail!("its status line changed since; change it by hand");
    }
    match restore {
        Restore::Remove => {
            doc.remove("statusLine");
        }
        Restore::Command(then) => {
            let status = doc
                .get_mut("statusLine")
                .and_then(Value::as_object_mut)
                .context("statusLine is not an object")?;
            status.insert("command".into(), Value::String(then.clone()));
        }
        Restore::ByHand => bail!("the status line has to be changed by hand"),
    }
    let mut text = serde_json::to_string_pretty(&Value::Object(doc))?;
    text.push('\n');
    credfile::atomic_write(settings, text.as_bytes())
}

#[cfg(windows)]
mod registry {
    //! String values under `HKEY_CURRENT_USER`.
    use std::io;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{
        ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE,
    };
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    };

    pub(super) fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The string value `name` of `key`, if it is there; a value of another type reads as a
    /// description of it, which names no program.
    pub fn read(key: &str, name: &str) -> io::Result<Option<String>> {
        let (key, name) = (wide(key), wide(name));
        let mut size = 0u32;
        // SAFETY: the strings are NUL-terminated; a null buffer asks for the size.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                null_mut(),
                &mut size,
            )
        };
        match rc {
            ERROR_SUCCESS => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            ERROR_UNSUPPORTED_TYPE => return Ok(Some("a value that is not text".into())),
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
        }
        let mut buf = vec![0u16; (size as usize).div_ceil(2) + 1];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `size` is the buffer's length in bytes.
        let rc = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buf.as_mut_ptr().cast(),
                &mut size,
            )
        };
        match rc {
            ERROR_SUCCESS => {}
            ERROR_FILE_NOT_FOUND => return Ok(None),
            rc => return Err(io::Error::from_raw_os_error(rc as i32)),
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Ok(Some(String::from_utf16_lossy(&buf[..len])))
    }

    /// Delete the value `name` of `key`; no value or no key is fine.
    pub fn delete(key: &str, name: &str) -> io::Result<()> {
        let (key, name) = (wide(key), wide(name));
        // SAFETY: the strings are NUL-terminated.
        let rc = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        if rc == ERROR_SUCCESS || rc == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(rc as i32))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_mode_is_taken_apart_when_it_can_be() {
        let r = |c: &str| restore_for(c);
        assert_eq!(r("node statusline.js"), None);
        assert_eq!(
            r("cc-rotate-ctl statusline"),
            None,
            "the rotation profile's own"
        );
        assert_eq!(
            r("cc-rotate-ctl statusline --observe"),
            Some(Restore::Remove)
        );
        assert_eq!(
            r("C:/Users/me/.local/bin/cc-rotate-ctl.exe statusline --observe"),
            Some(Restore::Remove)
        );
        assert_eq!(
            r("cc-rotate-ctl --profile /p statusline --observe"),
            Some(Restore::Remove)
        );
        assert_eq!(
            r(r#"cc-rotate-ctl statusline --observe --then "node /path/to/statusline.js""#),
            Some(Restore::Command("node /path/to/statusline.js".into()))
        );
        assert_eq!(
            r(r#"cc-rotate-ctl statusline --then "node \"my line.js\"" --observe"#),
            Some(Restore::Command(r#"node "my line.js""#.into()))
        );
        assert_eq!(
            r("cc-rotate-ctl statusline --observe --then 'bash ~/line.sh | cut -c1-80'"),
            Some(Restore::Command("bash ~/line.sh | cut -c1-80".into()))
        );
        assert_eq!(
            r("cc-rotate-ctl statusline --observe --then=starship"),
            Some(Restore::Command("starship".into()))
        );
        for by_hand in [
            "cc-rotate-ctl statusline --observe --then node line.js",
            r#"cc-rotate-ctl statusline --observe --then "node" --verbose"#,
            "cc-rotate-ctl statusline --observe --then",
            "cc-rotate-ctl statusline --observe | ccstatusline",
            "cc-rotate-ctl statusline --observe >/dev/null; starship prompt",
            "FOO=1 cc-rotate-ctl statusline --observe",
            "sh -c 'cc-rotate-ctl statusline --observe'",
            "cc-rotate-ctl statusline --observe --then \"unclosed",
        ] {
            assert_eq!(r(by_hand), Some(Restore::ByHand), "{by_hand}");
        }
    }

    #[test]
    fn a_status_line_is_restored_only_while_it_is_what_was_planned() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let observe = "cc-rotate-ctl statusline --observe --then \"node a.js\"";
        let write = |v: Value| std::fs::write(&file, v.to_string()).unwrap();
        let read =
            || -> Value { serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap() };
        write(serde_json::json!({
            "model": "opus",
            "statusLine": {"type": "command", "command": observe, "padding": 1}
        }));
        assert!(restore_status_line(&file, "something else", &Restore::Remove).is_err());
        restore_status_line(&file, observe, &Restore::Command("node a.js".into())).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({"model": "opus", "statusLine": {"type": "command", "command": "node a.js", "padding": 1}})
        );
        restore_status_line(&file, "node a.js", &Restore::Remove).unwrap();
        assert_eq!(read(), serde_json::json!({"model": "opus"}));
    }

    #[test]
    fn programs_are_found_with_the_copies_upgrades_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::consts::EXE_SUFFIX;
        let mut expected = Vec::new();
        for name in ["cc-rotate", "cc-rotate-ctl", "cc-rotate-app"] {
            let path = dir.path().join(format!("{name}{exe}"));
            std::fs::write(&path, "").unwrap();
            expected.push(path);
        }
        let old = dir.path().join(format!("cc-rotate{exe}.1793000000000.old"));
        std::fs::write(&old, "").unwrap();
        if cfg!(windows) {
            expected.push(old);
        }
        for other in ["claude", "cc-rotate-helper", "cc-rotate.txt"] {
            std::fs::write(dir.path().join(other), "").unwrap();
        }
        expected.sort();
        assert_eq!(programs_in(dir.path()).unwrap(), expected);
    }

    #[derive(Default)]
    struct Fake {
        autostart: Option<(String, bool)>,
        keychain: KeychainItem,
        /// Keychain items of other profiles (scratch ones) that are there.
        items: Vec<PathBuf>,
        fail_alias: bool,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl System for Fake {
        fn autostart(&self) -> Result<Option<(String, bool)>> {
            Ok(self.autostart.clone())
        }
        fn remove_autostart(&self) -> Result<()> {
            self.calls.borrow_mut().push("remove_autostart".into());
            Ok(())
        }
        fn stop_app(&self) -> Result<()> {
            self.calls.borrow_mut().push("stop_app".into());
            Ok(())
        }
        fn keychain(&self, profile: &Path) -> Result<KeychainItem> {
            Ok(if self.items.iter().any(|p| p == profile) {
                KeychainItem::Present("scratch".into())
            } else if profile.ends_with(".claude-rotate") {
                self.keychain.clone()
            } else {
                KeychainItem::None
            })
        }
        fn delete_keychain_item(&self, profile: &Path) -> Result<()> {
            let name = profile.file_name().unwrap_or_default().to_string_lossy();
            self.calls
                .borrow_mut()
                .push(format!("delete_keychain_item {name}"));
            Ok(())
        }
        fn legacy_alias(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn remove_legacy_alias(&self) -> Result<()> {
            self.calls.borrow_mut().push("remove_legacy_alias".into());
            if self.fail_alias {
                bail!("the PowerShell profile is read-only");
            }
            Ok(())
        }
        fn app_data(&self) -> Vec<PathBuf> {
            Vec::new()
        }
    }

    struct Install {
        _root: tempfile::TempDir,
        paths: Paths,
        bin: PathBuf,
        ctl: PathBuf,
    }

    /// A home with the cc-rotate profile, a data folder, the programs, and two other profiles:
    /// one in observe mode, one not.
    fn install() -> Install {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let profile = home.join(".claude-rotate");
        let data = root.path().join("local").join(DATA_DIR_NAME);
        let bin = home.join(".local").join("bin");
        for dir in [
            &profile,
            &data,
            &bin,
            &home.join(".claude"),
            &home.join(".claude-work"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(
            profile.join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "cc-rotate-ctl statusline"}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude-work").join("settings.json"),
            r#"{"statusLine": {"type": "command", "command": "cc-rotate-ctl statusline --observe"}, "model": "opus"}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude").join("settings.json"),
            r#"{"model": "opus"}"#,
        )
        .unwrap();
        drop(Store::open(&data.join("store.db")).unwrap());
        let exe = std::env::consts::EXE_SUFFIX;
        for name in PROGRAMS {
            std::fs::write(bin.join(format!("{name}{exe}")), "").unwrap();
        }
        let ctl = bin.join(format!("cc-rotate-ctl{exe}"));
        let paths = Paths::new(data, profile).with_home(home);
        Install {
            _root: root,
            paths,
            bin,
            ctl,
        }
    }

    #[test]
    fn everything_cc_rotate_put_in_place_goes() {
        let i = install();
        let scratch = i.paths.staging_root().join("check-1");
        std::fs::create_dir_all(&scratch).unwrap();
        let lock = legacy_lock_for(&i.paths.profile_dir);
        std::fs::create_dir_all(&lock).unwrap();
        let fake = Fake {
            autostart: Some(("the entry".into(), true)),
            keychain: KeychainItem::Present("Claude Code-credentials-abc".into()),
            items: vec![scratch.clone()],
            ..Default::default()
        };
        let store = Store::open_read_only(&i.paths.store_db()).unwrap();
        let plan = plan(&fake, &i.paths, Some(&store), &i.ctl, time::now_ms()).unwrap();
        drop(store);
        assert_eq!(plan.autostart.as_deref(), Some("the entry"));
        assert_eq!(
            plan.profile_dir,
            Removal::Delete(i.paths.profile_dir.clone())
        );
        let DataRemoval::Entries(data, _) = &plan.data_dir else {
            panic!("{:?}", plan.data_dir)
        };
        assert_eq!(
            data, &i.paths.data_dir,
            "not where cc-rotate puts it, so by entry"
        );
        assert_eq!(plan.profile_lock.as_ref(), Some(&lock));
        assert_eq!(plan.staging_items, vec![scratch]);
        assert_eq!(plan.programs.len(), 3);
        assert_eq!(plan.sessions, Sessions::Closed);
        assert!(plan.deletes_keychain_item());
        let work = i.paths.home.join(".claude-work").join("settings.json");
        assert_eq!(
            plan.observe,
            vec![ObserveMode {
                settings: work.clone(),
                command: "cc-rotate-ctl statusline --observe".into(),
                restore: Restore::Remove
            }]
        );

        let out = run(&fake, &plan, &i.ctl);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(
            *fake.calls.borrow(),
            [
                "stop_app",
                "remove_autostart",
                "delete_keychain_item .claude-rotate",
                "delete_keychain_item check-1"
            ]
        );
        assert!(!i.paths.profile_dir.exists());
        assert!(!lock.exists());
        assert!(!i.paths.data_dir.exists());
        assert!(i.bin.exists(), "the install folder stays");
        assert_eq!(std::fs::read_dir(&i.bin).unwrap().count(), 0);
        assert_eq!(
            std::fs::read_to_string(&work).unwrap().trim(),
            "{\n  \"model\": \"opus\"\n}"
        );
        let main = i.paths.home.join(".claude").join("settings.json");
        assert_eq!(
            std::fs::read_to_string(main).unwrap(),
            r#"{"model": "opus"}"#
        );

        // Again: nothing left to do, nothing fails.
        let again = super::plan(&fake, &i.paths, None, &i.ctl, time::now_ms()).unwrap();
        assert_eq!(again.profile_dir, Removal::Absent);
        assert_eq!(again.data_dir, DataRemoval::Absent);
        assert!(run(&fake, &again, &i.ctl).failed.is_empty());
    }

    /// A profile deleted by hand leaves its Keychain item, which still goes; a profile kept keeps
    /// it.
    #[test]
    fn the_keychain_item_goes_with_the_profile_or_after_it() {
        let i = install();
        let fake = Fake {
            keychain: KeychainItem::Present("Claude Code-credentials-abc".into()),
            ..Default::default()
        };
        std::fs::remove_dir_all(&i.paths.profile_dir).unwrap();
        let p = plan(&fake, &i.paths, None, &i.ctl, time::now_ms()).unwrap();
        assert_eq!(p.profile_dir, Removal::Absent);
        assert!(p.deletes_keychain_item());
        run(&fake, &p, &i.ctl);
        assert!(
            fake.calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("delete_keychain_item"))
        );

        std::fs::create_dir_all(&i.paths.profile_dir).unwrap();
        std::fs::write(i.paths.profile_dir.join("settings.json"), "{}").unwrap();
        let fake = Fake {
            keychain: KeychainItem::Present("Claude Code-credentials-abc".into()),
            ..Default::default()
        };
        let p = plan(&fake, &i.paths, None, &i.ctl, time::now_ms()).unwrap();
        assert!(matches!(p.profile_dir, Removal::Keep(..)));
        assert!(!p.deletes_keychain_item());
        run(&fake, &p, &i.ctl);
        assert!(
            !fake
                .calls
                .borrow()
                .iter()
                .any(|c| c.starts_with("delete_keychain_item"))
        );
    }

    /// `CC_ROTATE_DATA_DIR` pointing at a folder of the user's: only cc-rotate's entries go, even
    /// inside folders named like its own.
    #[test]
    fn a_data_folder_of_the_users_keeps_what_is_not_cc_rotates() {
        let i = install();
        let shared = i.paths.home.join("Dropbox");
        for dir in [
            "logs",
            "backups",
            "locks/account-1.lock",
            "staging/check-1-2-x",
            "staging/0123456789abcdef0123456789abcdef",
            "staging/mine",
            "photos",
        ] {
            std::fs::create_dir_all(shared.join(dir)).unwrap();
        }
        for file in [
            "store.db",
            "store.db-wal",
            "master.key",
            "store.db.json",
            ".master.key.0a1b.tmp",
            "notes.txt",
            "logs/cc-rotate.log",
            "logs/mine.log",
            "backups/credentials-2026-10-26.ccr",
        ] {
            std::fs::write(shared.join(file), "").unwrap();
        }
        let paths =
            Paths::new(shared.clone(), i.paths.profile_dir.clone()).with_home(&i.paths.home);
        let p = plan(&Fake::default(), &paths, None, &i.ctl, time::now_ms()).unwrap();
        let DataRemoval::Entries(dir, entries) = &p.data_dir else {
            panic!("{:?}", p.data_dir)
        };
        assert_eq!(dir, &shared);
        let relative = |e: &PathBuf| {
            e.strip_prefix(&shared)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        };
        let names: Vec<_> = entries.iter().map(relative).collect();
        assert_eq!(
            names,
            [
                ".master.key.0a1b.tmp",
                "backups/credentials-2026-10-26.ccr",
                "locks/account-1.lock",
                "logs/cc-rotate.log",
                "master.key",
                "staging/0123456789abcdef0123456789abcdef",
                "staging/check-1-2-x",
                "store.db",
                "store.db-wal",
            ]
        );
        assert_eq!(p.sessions, Sessions::Unknown, "a store that wasn't read");
        let out = run(&Fake::default(), &p, &i.ctl);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(out.data_left, Some(shared.clone()));
        let mut left: Vec<_> = walk(&shared).iter().map(relative).collect();
        left.sort();
        assert_eq!(
            left,
            [
                "logs",
                "logs/mine.log",
                "notes.txt",
                "photos",
                "staging",
                "staging/mine",
                "store.db.json"
            ]
        );
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                found.extend(walk(&path));
            }
            found.push(path);
        }
        found
    }

    /// A step that fails keeps the programs, so running it again can finish.
    #[test]
    fn the_programs_stay_while_a_step_failed() {
        let i = install();
        let fake = Fake {
            fail_alias: true,
            ..Default::default()
        };
        let mut p = plan(&fake, &i.paths, None, &i.ctl, time::now_ms()).unwrap();
        p.legacy_alias = vec![i.paths.home.join("profile.ps1")];
        let out = run(&fake, &p, &i.ctl);
        assert_eq!(out.failed.len(), 1, "{:?}", out.failed);
        assert!(out.programs_kept);
        assert!(i.ctl.exists());
        assert!(!i.paths.profile_dir.exists(), "the rest went");
    }

    /// A profile that is a link is left alone: deleting the link would leave the login and the
    /// history where it points.
    #[cfg(unix)]
    #[test]
    fn a_linked_profile_is_left_alone() {
        let i = install();
        let real = i.paths.home.join("elsewhere");
        std::fs::rename(&i.paths.profile_dir, &real).unwrap();
        std::os::unix::fs::symlink(&real, &i.paths.profile_dir).unwrap();
        let p = plan_of(&i.paths, &i.ctl);
        let Removal::Keep(_, why) = &p.profile_dir else {
            panic!("{:?}", p.profile_dir)
        };
        assert!(why.contains("elsewhere"), "{why}");
    }

    /// A settings file a dotfiles manager links in stays a link; the file it points at changes.
    #[cfg(unix)]
    #[test]
    fn a_linked_settings_file_stays_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-settings.json");
        let observe = "cc-rotate-ctl statusline --observe";
        std::fs::write(
            &real,
            serde_json::json!({"statusLine": {"command": observe}, "model": "opus"}).to_string(),
        )
        .unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        restore_status_line(&link, observe, &Restore::Remove).unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&real).unwrap()).unwrap();
        assert_eq!(doc, serde_json::json!({"model": "opus"}));
    }

    #[test]
    fn what_may_not_be_cc_rotates_stays() {
        let i = install();
        // Pointed at the default profile, and at a data folder without a store.
        let other_data = i.paths.home.join("Documents");
        std::fs::create_dir_all(&other_data).unwrap();
        let paths =
            Paths::new(other_data.clone(), i.paths.home.join(".claude")).with_home(&i.paths.home);
        let fake = Fake {
            autostart: Some(("someone else's".into(), false)),
            ..Default::default()
        };
        let plan = plan(&fake, &paths, None, &i.ctl, time::now_ms()).unwrap();
        assert!(
            matches!(plan.profile_dir, Removal::Keep(..)),
            "{:?}",
            plan.profile_dir
        );
        assert!(
            matches!(plan.data_dir, DataRemoval::Keep(..)),
            "{:?}",
            plan.data_dir
        );
        assert_eq!(plan.autostart, None);
        assert!(plan.autostart_foreign.is_some());
        let out = run(&fake, &plan, &i.ctl);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(*fake.calls.borrow(), ["stop_app"]);
        assert!(i.paths.home.join(".claude").exists());
        assert!(other_data.exists());

        // The home folder itself, as a profile or data folder, even holding a store.
        drop(Store::open(&i.paths.home.join("store.db")).unwrap());
        let paths = Paths::new(i.paths.home.clone(), i.paths.home.clone()).with_home(&i.paths.home);
        let plan = plan_of(&paths, &i.ctl);
        assert!(matches!(plan.profile_dir, Removal::Keep(..)));
        assert!(matches!(plan.data_dir, DataRemoval::Keep(..)));
    }

    fn plan_of(paths: &Paths, ctl: &Path) -> Plan {
        plan(&Fake::default(), paths, None, ctl, time::now_ms()).unwrap()
    }

    #[test]
    fn an_open_session_and_a_build_are_noticed() {
        let i = install();
        let store = Store::open(&i.paths.store_db()).unwrap();
        let now = time::now_ms();
        store
            .set_meta(statusline::META_LAST_TICK_AT, now - 30_000)
            .unwrap();
        let p = plan(&Fake::default(), &i.paths, Some(&store), &i.ctl, now).unwrap();
        assert_eq!(p.sessions, Sessions::Open);
        let p = plan(
            &Fake::default(),
            &i.paths,
            Some(&store),
            &i.ctl,
            now + SESSION_OPEN_MS,
        )
        .unwrap();
        assert_eq!(p.sessions, Sessions::Closed);

        let target = i.paths.home.join("src").join("target");
        std::fs::create_dir_all(target.join("release")).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
        let built = target.join("release").join("cc-rotate-ctl");
        let p = plan(&Fake::default(), &i.paths, None, &built, now).unwrap();
        assert!(p.programs.is_empty());
        assert!(p.programs_kept.is_some());
    }

    /// Setup writes the program's path in the 8.3 form when its folder has a space.
    #[cfg(windows)]
    #[test]
    fn a_short_program_path_is_still_cc_rotate_ctl() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("Matt Marcin").join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let ctl = bin.join("cc-rotate-ctl.exe");
        std::fs::write(&ctl, "").unwrap();
        let Some(short) = setup::short_path(&ctl) else {
            // This volume keeps no short names.
            return;
        };
        let command = format!("{} statusline", crate::paths::forward_slashes(&short));
        assert!(!command.contains("cc-rotate-ctl"), "{command}");
        let profile = dir.path().join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(
            profile.join("settings.json"),
            serde_json::json!({"statusLine": {"command": command}}).to_string(),
        )
        .unwrap();
        assert!(runs_cc_rotate(&profile));
    }

    /// A program Windows will not delete while it runs is renamed aside, and the helper deletes
    /// it once it stops.
    #[cfg(windows)]
    #[test]
    fn a_running_program_goes_once_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::env::var_os("SystemRoot").unwrap();
        // A character `cmd` would read specially, were it on the command line.
        let folder = dir.path().join("Tom & Jerry %PATH%");
        std::fs::create_dir_all(&folder).unwrap();
        let program = folder.join("cc-rotate.exe");
        std::fs::copy(
            PathBuf::from(root).join("System32").join("ping.exe"),
            &program,
        )
        .unwrap();
        let mut running = std::process::Command::new(&program)
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let plan = Plan {
            programs: vec![program.clone()],
            ..Default::default()
        };
        let out = run(&Fake::default(), &plan, &folder.join("cc-rotate-ctl.exe"));
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!program.exists(), "renamed aside");
        assert!(out.deleted_later.is_empty());
        let [aside] = out.left.as_slice() else {
            panic!("{out:?}")
        };
        assert!(aside.exists());
        // Still running a few seconds on: the helper keeps trying.
        std::thread::sleep(Duration::from_secs(3));
        running.kill().unwrap();
        running.wait().unwrap();
        let start = std::time::Instant::now();
        while aside.exists() {
            assert!(start.elapsed() < Duration::from_secs(30), "never deleted");
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The real start-at-login entry is never touched: these use names of their own.
    #[test]
    fn start_at_login_is_found_and_removed() {
        let home = tempfile::tempdir().unwrap();
        let name = format!("cc-rotate-test-{}", uuid::Uuid::new_v4());
        let key = format!(r"Software\cc-rotate-tests\{name}");
        let system = RealSystem {
            autostart_name: name.clone(),
            app_name: "cc-rotate-test-app".into(),
            run_key: key.clone(),
            approved_key: key.clone(),
            launch_agents: home.path().join("LaunchAgents"),
            launchctl: false,
            xdg_autostart: home.path().join("autostart"),
            app_data: Vec::new(),
        };
        assert_eq!(system.autostart().unwrap(), None);
        #[cfg(windows)]
        test_registry::set(
            &key,
            &name,
            r"C:\nowhere\cc-rotate-test-app.exe --minimized",
        );
        #[cfg(unix)]
        {
            let file = system.autostart_file().unwrap();
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "Exec=/nowhere/cc-rotate-test-app --minimized\n").unwrap();
        }
        let (described, ours) = system.autostart().unwrap().unwrap();
        assert!(ours, "{described}");
        system.remove_autostart().unwrap();
        assert_eq!(system.autostart().unwrap(), None);
        // Removing nothing is fine.
        system.remove_autostart().unwrap();
        #[cfg(windows)]
        {
            // A value that is not text is someone else's.
            test_registry::set_number(&key, &name);
            let (_, ours) = system.autostart().unwrap().unwrap();
            assert!(!ours);
            test_registry::delete_tree(r"Software\cc-rotate-tests");
        }
    }

    #[cfg(windows)]
    mod test_registry {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, REG_DWORD, REG_SZ, RegDeleteTreeW, RegSetKeyValueW,
        };

        use super::super::registry::wide;

        pub fn set(key: &str, name: &str, value: &str) {
            let data = wide(value);
            // SAFETY: NUL-terminated strings; the data length is in bytes.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    wide(key).as_ptr(),
                    wide(name).as_ptr(),
                    REG_SZ,
                    data.as_ptr().cast(),
                    (data.len() * 2) as u32,
                )
            };
            assert_eq!(rc, ERROR_SUCCESS);
        }

        pub fn set_number(key: &str, name: &str) {
            let data = 1u32;
            // SAFETY: NUL-terminated strings; four bytes of data.
            let rc = unsafe {
                RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    wide(key).as_ptr(),
                    wide(name).as_ptr(),
                    REG_DWORD,
                    (&data as *const u32).cast(),
                    4,
                )
            };
            assert_eq!(rc, ERROR_SUCCESS);
        }

        pub fn delete_tree(key: &str) {
            // SAFETY: a NUL-terminated string.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(key).as_ptr()) };
        }
    }
}
