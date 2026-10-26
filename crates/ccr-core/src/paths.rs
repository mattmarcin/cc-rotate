use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Default name of the rotation profile directory under the user's home.
pub const PROFILE_DIR_NAME: &str = ".claude-rotate";

/// Every on-disk location cc-rotate reads or writes.
///
/// `data_dir` holds cc-rotate's own state (store, locks, staging, backups).
/// `profile_dir` is the Claude Code config directory (`CLAUDE_CONFIG_DIR`) that the pool backs.
/// `home` is where the user's other Claude Code profiles (`~/.claude*`) are looked for.
#[derive(Debug, Clone)]
pub struct Paths {
    pub data_dir: PathBuf,
    pub profile_dir: PathBuf,
    pub home: PathBuf,
}

impl Paths {
    /// Paths for the user's home directory (empty when it cannot be determined, which finds no
    /// other profiles).
    pub fn new(data_dir: impl Into<PathBuf>, profile_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            profile_dir: profile_dir.into(),
            home: dirs::home_dir().unwrap_or_default(),
        }
    }

    /// Look for the other profiles in `home` instead (tests use a scratch directory).
    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = home.into();
        self
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

    /// Held by whoever uses a parked account's stored login outside the store: a usage check
    /// running Claude Code with it, or a swap writing it into the profile.
    pub fn account_lock(&self, account_id: i64) -> PathBuf {
        self.data_dir
            .join("locks")
            .join(format!("account-{account_id}.lock"))
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

/// `name` on PATH, else in a folder programs are installed to (`install_dirs`).
///
/// Only absolute directories are searched: a relative entry (`.`, an empty one,
/// `node_modules/.bin`) would resolve against wherever cc-rotate runs, such as a project folder,
/// and run a program from it with a login in reach.
pub fn find_program(name: &str) -> Option<PathBuf> {
    let home = home_dir().ok();
    find_program_in(
        name,
        std::env::var_os("PATH"),
        &install_dirs(home.as_deref()),
        home.as_deref(),
        &std::env::temp_dir(),
    )
}

/// `name` in the absolute folders of `path`, then in the first of `fallback` that has one the
/// user can trust (`owned_safely`). `cwd` is what `which` would resolve a relative folder
/// against, were one to slip through.
fn find_program_in(
    name: &str,
    path: Option<OsString>,
    fallback: &[PathBuf],
    home: Option<&Path>,
    cwd: &Path,
) -> Option<PathBuf> {
    let on_path: Vec<PathBuf> = path
        .iter()
        .flat_map(std::env::split_paths)
        // A quoted `~/.local/bin` reaches PATH as is; shells expand it, so this does too.
        .map(|d| match (d.strip_prefix("~"), home) {
            (Ok(rest), Some(home)) => home.join(rest),
            _ => d,
        })
        .filter(|d| d.is_absolute())
        .collect();
    search(name, on_path, cwd).or_else(|| {
        fallback
            .iter()
            .filter(|d| d.is_absolute())
            .find_map(|d| search(name, vec![d.clone()], cwd).filter(|p| owned_safely(d, p)))
    })
}

/// `name` in `dirs`, which are all absolute.
fn search(name: &str, dirs: Vec<PathBuf>, cwd: &Path) -> Option<PathBuf> {
    // `which` reads an empty list as one empty folder, the current one.
    if dirs.is_empty() {
        return None;
    }
    which::which_in(name, Some(std::env::join_paths(dirs).ok()?), cwd).ok()
}

/// Whether `program`, found in `dir`, is the user's to run: the user chose PATH's folders, but
/// not the shared ones a fallback looks in, such as a Homebrew another user of the Mac installed.
/// On Unix the program (through any link), its folder and `dir` must belong to root or the user
/// and not be writable by everyone; on Windows the folders are in the user's own profile.
#[cfg(unix)]
fn owned_safely(dir: &Path, program: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    let Ok(target) = std::fs::canonicalize(program) else {
        return false;
    };
    [Some(target.as_path()), target.parent(), Some(dir)]
        .into_iter()
        .all(|p| {
            p.and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| (m.uid() == 0 || m.uid() == me) && m.mode() & 0o002 == 0)
        })
}

#[cfg(windows)]
fn owned_safely(_dir: &Path, _program: &Path) -> bool {
    true
}

/// Where installers and package managers put programs (Claude Code's own, Homebrew, npm), for
/// a process whose PATH leaves them out: an app macOS starts at login gets only
/// `/usr/bin:/bin:/usr/sbin:/sbin`, and a desktop session on Linux may not have read the shell
/// profile that adds `~/.local/bin`.
fn install_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(home) = home {
        // Claude Code's native installer, on every platform, then its older local install.
        folders.push(home.join(".local").join("bin"));
        folders.push(home.join(".claude").join("local"));
    }
    #[cfg(unix)]
    folders.extend(
        // Homebrew on Apple silicon, on Intel Macs (and many manual installs), and on Linux.
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/home/linuxbrew/.linuxbrew/bin",
        ]
        .map(PathBuf::from),
    );
    #[cfg(windows)]
    if let Some(roaming) = dirs::data_dir() {
        // `npm install -g`.
        folders.push(roaming.join("npm"));
    }
    folders
}

/// A command running `program`, with `program`'s folder added to the end of the child's PATH
/// when it is not already on it: a program found outside PATH (`find_program`) may be a script
/// whose interpreter sits beside it, such as a `claude` installed with Homebrew's npm, which
/// runs `node`.
pub fn program_command(program: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(program);
    if let Some(path) = path_with(std::env::var_os("PATH"), program) {
        cmd.env("PATH", path);
    }
    cmd
}

/// `path` with `program`'s folder added at the end, or `None` to leave it as it is: the folder
/// is on it already, or there is no PATH, where the system's default search must stay.
fn path_with(path: Option<OsString>, program: &Path) -> Option<OsString> {
    let dir = program.parent().filter(|d| d.is_absolute())?;
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path?).collect();
    if dirs.iter().any(|d| d == dir) {
        return None;
    }
    dirs.push(dir.to_path_buf());
    std::env::join_paths(dirs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty file `name` in `dir` that `which` accepts as a program.
    fn program(dir: &Path, name: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let file = dir.join(if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        });
        std::fs::write(&file, "").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        file
    }

    #[test]
    fn programs_outside_path_are_found_in_install_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        let home = tmp.path().join("home");
        let installed = program(&home.join(".local").join("bin"), "claude");
        let elsewhere = tmp.path().join("bin");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let path = std::env::join_paths([&elsewhere]).unwrap();
        let fallback = install_dirs(Some(&home));
        let find = |path: Option<OsString>, fallback: &[PathBuf]| {
            find_program_in("claude", path, fallback, Some(&home), cwd)
        };

        // A PATH like the one macOS gives apps started at login, without the install folder.
        assert_eq!(find(Some(path.clone()), &fallback), Some(installed.clone()));
        assert_eq!(find(None, &fallback), Some(installed));

        // PATH comes first.
        let on_path = program(&elsewhere, "claude");
        assert_eq!(find(Some(path), &fallback), Some(on_path));
        assert_eq!(find(None, &[]), None);
    }

    #[test]
    fn relative_folders_are_never_searched() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path();
        // Where a relative or empty folder would lead: the folder `which` resolves them against.
        program(&cwd.join("bin"), "claude");
        program(cwd, "claude");
        let relative = PathBuf::from("bin");
        for path in [
            std::env::join_paths([&relative]).unwrap(),
            OsString::new(),
            OsString::from("."),
        ] {
            assert_eq!(
                find_program_in(
                    "claude",
                    Some(path.clone()),
                    std::slice::from_ref(&relative),
                    None,
                    cwd
                ),
                None,
                "{path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_fallback_folder_everyone_can_write_to_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let shared = tmp.path().join("shared");
        let planted = program(&shared, "claude");
        let find = || {
            find_program_in(
                "claude",
                None,
                std::slice::from_ref(&shared),
                None,
                tmp.path(),
            )
        };
        assert_eq!(find(), Some(planted));

        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(find(), None);
    }

    #[test]
    fn a_programs_folder_is_added_to_the_childs_path_once() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("node-bin");
        let claude = dir.join("claude");
        let other = tmp.path().join("other");
        let path = std::env::join_paths([&other]).unwrap();

        let added = path_with(Some(path), &claude).unwrap();
        assert_eq!(
            std::env::split_paths(&added).collect::<Vec<_>>(),
            vec![other, dir]
        );
        assert_eq!(path_with(Some(added), &claude), None);
        // Without a PATH the system's default search stays.
        assert_eq!(path_with(None, &claude), None);
        // A bare name has no folder to add.
        assert_eq!(
            path_with(Some(OsString::from("/usr/bin")), Path::new("claude")),
            None
        );
    }
}
