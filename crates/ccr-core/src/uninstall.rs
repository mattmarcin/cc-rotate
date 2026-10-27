//! Removing cc-rotate (spec section 4.5): first what its install and its use leave outside its own
//! folders (the app starting at login, the rotation profile's macOS Keychain item, observe mode in
//! the user's other profiles, the PowerShell function older versions added), then the programs,
//! the rotation profile and the data folder.
//!
//! `plan` only reads; `run` carries a plan out. Whatever reaches beyond the user's files (the
//! running app, start at login, the Keychain) goes through `System`, so tests can never touch the
//! real install.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use crate::credstore::Credentials;
use crate::paths::{Paths, in_build_folder};
use crate::store::Store;
use crate::{credfile, profiles, setup, statusline, time};

/// The name the app registers to start at login under (the Windows `Run` value, the macOS
/// LaunchAgent label and plist, the Linux `.desktop` file), and its executable's name.
pub const AUTOSTART_NAME: &str = "cc-rotate";
pub const APP_NAME: &str = "cc-rotate-app";
/// The programs an install puts in its folder (`scripts/install.*`).
pub const PROGRAMS: [&str; 3] = ["cc-rotate", "cc-rotate-ctl", "cc-rotate-app"];

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
    pub keychain: KeychainItem,
    /// Other profiles whose status line runs cc-rotate in observe mode.
    pub observe: Vec<ObserveMode>,
    /// PowerShell profiles that still define the function older versions added.
    pub legacy_alias: Vec<PathBuf>,
    pub profile_dir: Removal,
    pub data_dir: Removal,
    /// The installed programs, and copies earlier upgrades renamed aside (Windows).
    pub programs: Vec<PathBuf>,
    /// Why the programs are left alone, when they are.
    pub programs_kept: Option<String>,
    /// A session in the cc-rotate profile ticked within `SESSION_OPEN_MS`.
    pub sessions_open: bool,
}

/// The rotation profile's login in the macOS Keychain (`credstore`).
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

/// A profile whose status line runs `cc-rotate-ctl statusline --observe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserveMode {
    pub settings: PathBuf,
    pub restore: Restore,
}

/// How a profile's status line comes back once cc-rotate is gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Restore {
    /// Observe mode was all it ran: the status line goes.
    Remove,
    /// The command observe mode ran after cc-rotate's (`--then`) becomes the status line.
    Command(String),
    /// A `--then` that cannot be taken out reliably: the user changes it.
    ByHand,
}

/// What `run` did, step by step.
#[derive(Debug, Default)]
pub struct Outcome {
    pub done: Vec<String>,
    pub failed: Vec<String>,
    /// This program, which Windows will not delete while it runs: renamed aside, and deleted by
    /// a helper a few seconds after this process exits.
    pub deleted_later: Vec<PathBuf>,
    /// Programs another process still runs (an open session), renamed aside: the helper tries
    /// them too, but they may still run then, so the user deletes what is left.
    pub left: Vec<PathBuf>,
}

/// Whatever reaches beyond the user's files.
pub trait System {
    /// The app's start-at-login entry: `Ok(Some((describes, is_the_app)))` when one by its name
    /// is there.
    fn autostart(&self) -> Result<Option<(String, bool)>>;
    fn remove_autostart(&self) -> Result<()>;
    /// Quit the app, if it runs.
    fn stop_app(&self) -> Result<()>;
    fn keychain(&self, profile: &Path) -> Result<KeychainItem>;
    fn delete_keychain_item(&self, profile: &Path) -> Result<()>;
    /// PowerShell profiles with the function older versions added (`setup`).
    fn legacy_alias(&self) -> Vec<PathBuf>;
    fn remove_legacy_alias(&self) -> Result<()>;
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
    /// Linux: the XDG autostart folder.
    pub xdg_autostart: PathBuf,
}

impl RealSystem {
    pub fn installed(home: &Path) -> RealSystem {
        RealSystem {
            autostart_name: AUTOSTART_NAME.into(),
            app_name: APP_NAME.into(),
            run_key: r"Software\Microsoft\Windows\CurrentVersion\Run".into(),
            approved_key: r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run"
                .into(),
            launch_agents: home.join("Library").join("LaunchAgents"),
            launchctl: true,
            xdg_autostart: dirs::config_dir()
                .unwrap_or_else(|| home.join(".config"))
                .join("autostart"),
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
            cmd.args(["/F", "/IM"]).arg(format!(
                "{}{}",
                self.app_name,
                std::env::consts::EXE_SUFFIX
            ));
            cmd
        } else {
            let mut cmd = std::process::Command::new("pkill");
            cmd.arg("-x").arg(&self.app_name);
            cmd
        };
        let status = cmd
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("running taskkill or pkill")?;
        // taskkill: 0 stopped, 128 not running; pkill: 0 stopped, 1 not running.
        match status.code() {
            Some(0) => {
                // Until it lets go of its files.
                std::thread::sleep(Duration::from_secs(1));
                Ok(())
            }
            Some(1) if !cfg!(windows) => Ok(()),
            Some(128) if cfg!(windows) => Ok(()),
            other => bail!("couldn't stop {} (exit {other:?})", self.app_name),
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

/// What removing cc-rotate would do. Reads only; `store` is the store opened read-only, if there
/// is one; `bin_dir` is where the running `cc-rotate-ctl` is (the install folder), with
/// `this_exe` the running program itself.
pub fn plan(
    system: &dyn System,
    paths: &Paths,
    store: Option<&Store>,
    this_exe: &Path,
    now_ms: i64,
) -> Result<Plan> {
    let mut plan = Plan::default();
    match system.autostart()? {
        Some((entry, true)) => plan.autostart = Some(entry),
        Some((entry, false)) => plan.autostart_foreign = Some(entry),
        None => {}
    }
    plan.keychain = system.keychain(&paths.profile_dir)?;
    plan.observe = observe_modes(paths)?;
    plan.legacy_alias = system.legacy_alias();
    plan.profile_dir = profile_removal(paths);
    plan.data_dir = data_removal(paths);
    if let Some(store) = store {
        plan.sessions_open = store
            .meta_i64(statusline::META_LAST_TICK_AT)?
            .is_some_and(|t| now_ms - t < SESSION_OPEN_MS);
    }
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

/// The data folder, unless it does not hold cc-rotate's store or key.
fn data_removal(paths: &Paths) -> Removal {
    let dir = &paths.data_dir;
    if !dir.exists() {
        return Removal::Absent;
    }
    if is_home_or_root(dir, &paths.home) {
        return Removal::Keep(dir.clone(), "it is not a folder cc-rotate made".into());
    }
    if !dir.join("store.db").exists() && !dir.join("master.key").exists() {
        return Removal::Keep(
            dir.clone(),
            "it holds neither cc-rotate's store nor its key".into(),
        );
    }
    Removal::Delete(dir.clone())
}

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
    status_command(&settings).is_some_and(|c| {
        c.contains("cc-rotate-ctl") && c.contains("statusline") && !c.contains("--observe")
    })
}

fn status_command(settings: &Map<String, Value>) -> Option<&str> {
    settings
        .get("statusLine")
        .and_then(|s| s.get("command"))
        .and_then(Value::as_str)
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
            if let Some(restore) = status_command(&doc).and_then(restore_for) {
                found.push(ObserveMode { settings, restore });
            }
        }
    }
    Ok(found)
}

/// How to restore a status line `command` that runs cc-rotate in observe mode, if it does.
pub fn restore_for(command: &str) -> Option<Restore> {
    if !(command.contains("cc-rotate-ctl")
        && command.contains("statusline")
        && command.contains("--observe"))
    {
        return None;
    }
    let Some(at) = command.find("--then") else {
        return Some(Restore::Remove);
    };
    let rest = command[at + "--then".len()..].trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim_end();
    Some(match unquote(rest) {
        Some(inner) if !inner.trim().is_empty() => Restore::Command(inner),
        _ => Restore::ByHand,
    })
}

/// One shell word, as written after `--then`: double-quoted (with `\"` and `\\` inside),
/// single-quoted, or bare. Anything more than one word is not taken apart.
fn unquote(word: &str) -> Option<String> {
    if let Some(body) = word.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(n @ ('"' | '\\')) => out.push(n),
                    Some(n) => {
                        out.push('\\');
                        out.push(n);
                    }
                    None => return None,
                },
                '"' => return chars.as_str().is_empty().then_some(out),
                c => out.push(c),
            }
        }
        None
    } else if let Some(body) = word.strip_prefix('\'') {
        let inner = body.strip_suffix('\'')?;
        (!inner.contains('\'')).then(|| inner.to_string())
    } else {
        let bare = !word.is_empty()
            && !word
                .chars()
                .any(|c| c.is_whitespace() || c == '"' || c == '\'');
        bare.then(|| word.to_string())
    }
}

/// Carry out `plan`, step by step, going on past a step that fails. `store` must be closed
/// already (the data folder is deleted). On Windows, programs in use are renamed aside and
/// deleted by a helper once free (`Outcome::deleted_later`), or left for the user.
pub fn run(system: &dyn System, plan: &Plan, this_exe: &Path) -> Outcome {
    let mut out = Outcome::default();
    let mut step = |what: String, result: Result<()>| match result {
        Ok(()) => out.done.push(what),
        Err(e) => out.failed.push(format!("{what}: {e:#}")),
    };
    step("Quit the cc-rotate app".into(), system.stop_app());
    if let Some(entry) = &plan.autostart {
        step(
            format!("Removed start at login ({entry})"),
            system.remove_autostart(),
        );
    }
    for observe in &plan.observe {
        if observe.restore != Restore::ByHand {
            step(
                format!("Took observe mode out of {}", observe.settings.display()),
                restore_status_line(&observe.settings, &observe.restore),
            );
        }
    }
    if !plan.legacy_alias.is_empty() {
        step(
            "Removed the old cc-rotate PowerShell function".into(),
            system.remove_legacy_alias(),
        );
    }
    if let (KeychainItem::Present(service), Removal::Delete(dir)) =
        (&plan.keychain, &plan.profile_dir)
    {
        step(
            format!("Deleted the Keychain item \"{service}\""),
            system.delete_keychain_item(dir),
        );
    }
    for removal in [&plan.profile_dir, &plan.data_dir] {
        if let Removal::Delete(dir) = removal {
            step(
                format!("Deleted {}", dir.display()),
                std::fs::remove_dir_all(dir).with_context(|| format!("deleting {}", dir.display())),
            );
        }
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

fn rename_aside(program: &Path) -> Result<PathBuf> {
    let renamed = PathBuf::from(format!("{}.{}.old", program.display(), time::now_ms()));
    std::fs::rename(program, &renamed)
        .with_context(|| format!("renaming {}", program.display()))?;
    Ok(renamed)
}

/// Delete `files` a few seconds after this process exits (Windows only lets a running program's
/// file go then), with a `cmd` started on its own. Paths `cmd` would read specially are refused.
#[cfg(windows)]
fn delete_after_exit(files: &[PathBuf]) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut quoted = Vec::new();
    for file in files {
        let text = file.display().to_string();
        if text.chars().any(|c| "\"%^&|<>!".contains(c)) {
            bail!("{text} has a character cmd treats specially");
        }
        quoted.push(format!("\"{text}\""));
    }
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    std::process::Command::new(PathBuf::from(root).join("System32").join("cmd.exe"))
        .raw_arg(format!(
            "/d /c ping -n 4 127.0.0.1 >nul & del /f /q {}",
            quoted.join(" ")
        ))
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
        .spawn()
        .context("starting the helper that deletes the programs")?;
    Ok(())
}

#[cfg(not(windows))]
fn delete_after_exit(_files: &[PathBuf]) -> Result<()> {
    bail!("only Windows needs this")
}

/// Take observe mode out of a profile's status line: drop it, or put back the command it ran
/// after cc-rotate's. Everything else in the file stays as it is.
pub fn restore_status_line(settings: &Path, restore: &Restore) -> Result<()> {
    let mut doc = credfile::read_document(settings)?
        .with_context(|| format!("{} is gone", settings.display()))?;
    match restore {
        Restore::Remove => {
            doc.remove("statusLine");
        }
        Restore::Command(command) => {
            let status = doc
                .get_mut("statusLine")
                .and_then(Value::as_object_mut)
                .context("statusLine is not an object")?;
            status.insert("command".into(), Value::String(command.clone()));
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

    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW,
    };

    pub(super) fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(Some(0)).collect()
    }

    /// The string value `name` of `key`, if it is there.
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
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
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
        if rc == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if rc != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(rc as i32));
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
            r(r#"cc-rotate-ctl statusline --observe --then "node /path/to/statusline.js""#),
            Some(Restore::Command("node /path/to/statusline.js".into()))
        );
        assert_eq!(
            r(r#"cc-rotate-ctl statusline --observe --then "node \"my line.js\"""#),
            Some(Restore::Command(r#"node "my line.js""#.into()))
        );
        assert_eq!(
            r("cc-rotate-ctl statusline --observe --then 'bash ~/line.sh'"),
            Some(Restore::Command("bash ~/line.sh".into()))
        );
        assert_eq!(
            r("cc-rotate-ctl statusline --observe --then=starship"),
            Some(Restore::Command("starship".into()))
        );
        for by_hand in [
            "cc-rotate-ctl statusline --observe --then node line.js",
            r#"cc-rotate-ctl statusline --observe --then "node" --verbose"#,
            "cc-rotate-ctl statusline --observe --then 'it'\\''s'",
            "cc-rotate-ctl statusline --observe --then",
        ] {
            assert_eq!(r(by_hand), Some(Restore::ByHand), "{by_hand}");
        }
    }

    #[test]
    fn a_status_line_is_restored_and_the_rest_kept() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        let write = |v: Value| std::fs::write(&file, v.to_string()).unwrap();
        let read =
            || -> Value { serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap() };
        write(serde_json::json!({
            "model": "opus",
            "statusLine": {"type": "command", "command": "cc-rotate-ctl statusline --observe --then \"node a.js\"", "padding": 1}
        }));
        restore_status_line(&file, &Restore::Command("node a.js".into())).unwrap();
        assert_eq!(
            read(),
            serde_json::json!({"model": "opus", "statusLine": {"type": "command", "command": "node a.js", "padding": 1}})
        );
        restore_status_line(&file, &Restore::Remove).unwrap();
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
        fn keychain(&self, _profile: &Path) -> Result<KeychainItem> {
            Ok(self.keychain.clone())
        }
        fn delete_keychain_item(&self, _profile: &Path) -> Result<()> {
            self.calls.borrow_mut().push("delete_keychain_item".into());
            Ok(())
        }
        fn legacy_alias(&self) -> Vec<PathBuf> {
            Vec::new()
        }
        fn remove_legacy_alias(&self) -> Result<()> {
            self.calls.borrow_mut().push("remove_legacy_alias".into());
            Ok(())
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
        let data = root.path().join("data");
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
        let store = Store::open(&data.join("store.db")).unwrap();
        drop(store);
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
        let fake = Fake {
            autostart: Some(("the entry".into(), true)),
            keychain: KeychainItem::Present("Claude Code-credentials-abc".into()),
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
        assert_eq!(plan.data_dir, Removal::Delete(i.paths.data_dir.clone()));
        assert_eq!(plan.programs.len(), 3);
        assert!(!plan.sessions_open);
        let work = i.paths.home.join(".claude-work").join("settings.json");
        assert_eq!(
            plan.observe,
            vec![ObserveMode {
                settings: work.clone(),
                restore: Restore::Remove
            }]
        );

        let out = run(&fake, &plan, &i.ctl);
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert_eq!(
            *fake.calls.borrow(),
            ["stop_app", "remove_autostart", "delete_keychain_item"]
        );
        assert!(!i.paths.profile_dir.exists());
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
            matches!(plan.data_dir, Removal::Keep(..)),
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

        // The home folder itself, as a profile or data folder.
        let paths = Paths::new(i.paths.home.clone(), i.paths.home.clone()).with_home(&i.paths.home);
        let plan = plan_of(&paths, &i.ctl);
        assert!(matches!(plan.profile_dir, Removal::Keep(..)));
        assert!(matches!(plan.data_dir, Removal::Keep(..)));
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
        assert!(p.sessions_open);
        let p = plan(
            &Fake::default(),
            &i.paths,
            Some(&store),
            &i.ctl,
            now + SESSION_OPEN_MS,
        )
        .unwrap();
        assert!(!p.sessions_open);

        let target = i.paths.home.join("src").join("target");
        std::fs::create_dir_all(target.join("release")).unwrap();
        std::fs::write(target.join("CACHEDIR.TAG"), "").unwrap();
        let built = target.join("release").join("cc-rotate-ctl");
        let p = plan(&Fake::default(), &i.paths, None, &built, now).unwrap();
        assert!(p.programs.is_empty());
        assert!(p.programs_kept.is_some());
    }

    /// A program Windows will not delete while it runs is renamed aside, and the helper deletes
    /// it once it stops.
    #[cfg(windows)]
    #[test]
    fn a_running_program_goes_once_it_stops() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::env::var_os("SystemRoot").unwrap();
        let program = dir.path().join("cc-rotate.exe");
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
        let out = run(
            &Fake::default(),
            &plan,
            &dir.path().join("cc-rotate-ctl.exe"),
        );
        assert!(out.failed.is_empty(), "{:?}", out.failed);
        assert!(!program.exists(), "renamed aside");
        assert!(out.deleted_later.is_empty());
        let [aside] = out.left.as_slice() else {
            panic!("{out:?}")
        };
        assert!(aside.exists());
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
        test_registry::delete_tree(r"Software\cc-rotate-tests");
    }

    #[cfg(windows)]
    mod test_registry {
        use windows_sys::Win32::Foundation::ERROR_SUCCESS;
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, REG_SZ, RegDeleteTreeW, RegSetKeyValueW,
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

        pub fn delete_tree(key: &str) {
            // SAFETY: a NUL-terminated string.
            unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(key).as_ptr()) };
        }
    }
}
