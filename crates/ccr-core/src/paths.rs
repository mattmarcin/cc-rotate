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

/// Why `find_program` has nothing to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotFound {
    /// Neither on PATH nor in a folder programs are installed to.
    Missing,
    /// In a folder programs are installed to, at `program`, but `step` on the way to it belongs
    /// to another user or can be changed by one (`owned_safely`).
    Refused { program: PathBuf, step: PathBuf },
}

/// `name` on PATH, else in a folder programs are installed to (`install_dirs`).
///
/// Only absolute directories are searched: a relative entry (`.`, an empty one,
/// `node_modules/.bin`) would resolve against wherever cc-rotate runs, such as a project folder,
/// and run a program from it with a login in reach.
pub fn find_program(name: &str) -> Result<PathBuf, NotFound> {
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
) -> Result<PathBuf, NotFound> {
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
    if let Some(found) = search(name, on_path, cwd) {
        return Ok(found);
    }
    let mut refused = None;
    for dir in fallback.iter().filter(|d| d.is_absolute()) {
        let Some(program) = search(name, vec![dir.clone()], cwd) else {
            continue;
        };
        match owned_safely(&program) {
            Ok(()) => return Ok(program),
            Err(step) => {
                refused.get_or_insert(NotFound::Refused { program, step });
            }
        }
    }
    Err(refused.unwrap_or(NotFound::Missing))
}

/// `name` in `dirs`, which are all absolute.
fn search(name: &str, dirs: Vec<PathBuf>, cwd: &Path) -> Option<PathBuf> {
    // `which` reads an empty list as one empty folder, the current one.
    if dirs.is_empty() {
        return None;
    }
    which::which_in(name, Some(std::env::join_paths(dirs).ok()?), cwd).ok()
}

/// Whether `program`, found in a fallback folder, is the user's to run: the user chose PATH's
/// folders, but not the shared ones a fallback looks in, such as a Homebrew another user of the
/// computer installed. `Err` names the first step that isn't.
///
/// On Unix the path is walked one part at a time from `/`, following each link where it leads,
/// as the system resolves it. Every link must belong to root or the user; so must every folder
/// and the program, whose permissions must also let no one else change them (`unchangeable`).
/// On Windows the folders are in the user's own profile.
#[cfg(unix)]
fn owned_safely(program: &Path) -> Result<(), PathBuf> {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    /// Links followed before giving up, as the system does (`MAXSYMLINKS`).
    const MAX_LINKS: usize = 40;
    // SAFETY: getuid has no preconditions and cannot fail.
    let me = unsafe { libc::getuid() };
    let own_group = private_group(me);
    let safe =
        |m: &std::fs::Metadata| (m.uid() == 0 || m.uid() == me) && unchangeable(m, own_group);

    /// Add `path`'s parts to what is left to walk, which holds the next part last.
    fn push(rest: &mut Vec<OsString>, path: &Path) {
        let parts: Vec<OsString> = path
            .components()
            .filter_map(|c| match c {
                Component::Normal(part) => Some(part.to_os_string()),
                Component::ParentDir => Some("..".into()),
                Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
            })
            .collect();
        rest.extend(parts.into_iter().rev());
    }

    if !program.is_absolute() {
        return Err(program.to_path_buf());
    }
    let mut rest = Vec::new();
    push(&mut rest, program);
    // Where the walk is: a real folder, never a link, so `..` leaves it as the system would.
    let mut at = PathBuf::from("/");
    if !std::fs::symlink_metadata(&at).is_ok_and(|m| safe(&m)) {
        return Err(at);
    }
    let mut links = 0;
    while let Some(part) = rest.pop() {
        if part == ".." {
            at.pop();
            continue;
        }
        let next = at.join(&part);
        let Ok(meta) = std::fs::symlink_metadata(&next) else {
            return Err(next);
        };
        if meta.file_type().is_symlink() {
            // A link's own permissions mean nothing; who made it does.
            links += 1;
            if !(meta.uid() == 0 || meta.uid() == me) || links > MAX_LINKS {
                return Err(next);
            }
            let Ok(to) = std::fs::read_link(&next) else {
                return Err(next);
            };
            if to.is_absolute() {
                at = PathBuf::from("/");
            }
            push(&mut rest, &to);
            continue;
        }
        if !safe(&meta) {
            return Err(next);
        }
        at = next;
    }
    Ok(())
}

/// Whether `meta`'s permissions let no one but its owner change it: not writable by everyone,
/// nor by a group other than the user's own (`own_group`) or macOS's `admin`, whose members
/// administer the Mac. In a sticky folder such as `/tmp` others can't rename or remove what
/// isn't theirs, and what they add is theirs, which the owner check refuses.
#[cfg(unix)]
fn unchangeable(meta: &std::fs::Metadata, own_group: Option<u32>) -> bool {
    use std::os::unix::fs::MetadataExt;
    let mode = meta.mode();
    let group_ok = mode & 0o020 == 0
        || Some(meta.gid()) == own_group
        || (cfg!(target_os = "macos") && meta.gid() == 80);
    (meta.is_dir() && mode & 0o1000 != 0) || (mode & 0o002 == 0 && group_ok)
}

/// The user's primary group when it is theirs alone: named after them, with no members listed,
/// as Debian, Ubuntu and Fedora make one for every user (with a umask that lets it write).
#[cfg(unix)]
fn private_group(uid: u32) -> Option<u32> {
    use std::ffi::CStr;
    // SAFETY: getgid has no preconditions and cannot fail.
    let gid = unsafe { libc::getgid() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    // SAFETY: zeroed records are valid to fill; the user's name is copied out of `buf` before
    // `buf` is reused for the group, whose fields are read while `buf` still holds them.
    unsafe {
        let mut pw: libc::passwd = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        if libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut found) != 0
            || found.is_null()
        {
            return None;
        }
        let user = CStr::from_ptr(pw.pw_name).to_owned();
        let mut gr: libc::group = std::mem::zeroed();
        let mut found = std::ptr::null_mut();
        if libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr(), buf.len(), &mut found) != 0
            || found.is_null()
        {
            return None;
        }
        let alone = gr.gr_mem.is_null() || (*gr.gr_mem).is_null();
        (alone && CStr::from_ptr(gr.gr_name) == user.as_c_str()).then_some(gid)
    }
}

#[cfg(windows)]
fn owned_safely(_program: &Path) -> Result<(), PathBuf> {
    Ok(())
}

/// Where installers and package managers put programs (Claude Code's own, Homebrew, npm), for
/// a process whose PATH leaves them out: an app macOS starts at login gets only
/// `/usr/bin:/bin:/usr/sbin:/sbin`, and a desktop session on Linux may not have read the shell
/// profile that adds `~/.local/bin`.
fn install_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    if let Some(home) = home {
        // Claude Code's native installer, on every platform.
        folders.push(home.join(".local").join("bin"));
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

    /// `dir` and the folders above it, made the way installers make them (not as the umask says).
    fn folder(dir: &Path) {
        if dir.is_dir() {
            return;
        }
        if let Some(parent) = dir.parent() {
            folder(parent);
        }
        std::fs::create_dir(dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// An empty file `name` in `dir` that `which` accepts as a program.
    fn program(dir: &Path, name: &str) -> PathBuf {
        folder(dir);
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
        folder(&elsewhere);
        let path = std::env::join_paths([&elsewhere]).unwrap();
        let fallback = install_dirs(Some(&home));
        let find = |path: Option<OsString>, fallback: &[PathBuf]| {
            find_program_in("claude", path, fallback, Some(&home), cwd).ok()
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
                Err(NotFound::Missing),
                "{path:?}"
            );
        }
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    fn find_in(dir: &Path, cwd: &Path) -> Result<PathBuf, NotFound> {
        find_program_in("claude", None, &[dir.to_path_buf()], None, cwd)
    }

    #[cfg(unix)]
    #[test]
    fn a_fallback_folder_others_can_write_to_is_not_used() {
        let tmp = tempfile::tempdir().unwrap();
        // Where the walk reports steps: past macOS's `/var` link.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let root = root.as_path();
        let shared = root.join("shared");
        let planted = program(&shared, "claude");
        assert_eq!(find_in(&shared, root), Ok(planted.clone()));

        chmod(&shared, 0o777);
        assert_eq!(
            find_in(&shared, root),
            Err(NotFound::Refused {
                program: planted.clone(),
                step: shared.clone()
            })
        );
        // A group counts as others unless it is the user's own (`private_group`) or macOS's
        // `admin`, which a temporary folder never has.
        chmod(&shared, 0o775);
        let group = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(&shared).unwrap());
        // SAFETY: getuid has no preconditions.
        let own = private_group(unsafe { libc::getuid() }) == Some(group);
        assert_eq!(find_in(&shared, root).is_ok(), own);
    }

    /// Others can't change what is theirs in a sticky folder such as `/tmp`, and what they add
    /// there is theirs.
    #[cfg(unix)]
    #[test]
    fn a_sticky_folder_on_the_way_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        // Where the walk reports steps: past macOS's `/var` link.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let root = root.as_path();
        let sticky = root.join("sticky");
        folder(&sticky);
        chmod(&sticky, 0o1777);
        let bin = sticky.join("bin");
        let planted = program(&bin, "claude");
        assert_eq!(find_in(&bin, root), Ok(planted.clone()));
        // The program itself may not sit there, writable by all.
        chmod(&planted, 0o777);
        assert!(find_in(&bin, root).is_err());
    }

    /// Claude Code's native installer links `~/.local/bin/claude` to the current version.
    #[cfg(unix)]
    #[test]
    fn links_are_followed_and_every_step_is_checked() {
        let tmp = tempfile::tempdir().unwrap();
        // Where the walk reports steps: past macOS's `/var` link.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let root = root.as_path();
        let bin = root.join("bin");
        folder(&bin);
        let versions = root.join("versions");
        let version = program(&versions, "2.1.310");
        let claude = bin.join("claude");
        std::os::unix::fs::symlink(&version, &claude).unwrap();
        assert_eq!(find_in(&bin, root), Ok(claude.clone()));

        // A link relative to its folder, through a second one.
        std::fs::remove_file(&claude).unwrap();
        std::os::unix::fs::symlink("current", &claude).unwrap();
        std::os::unix::fs::symlink("../versions/2.1.310", bin.join("current")).unwrap();
        assert_eq!(find_in(&bin, root), Ok(claude.clone()));

        // What the link leads to sits where others can change it.
        chmod(&versions, 0o777);
        assert!(find_in(&bin, root).is_err());
        chmod(&versions, 0o755);
        assert_eq!(find_in(&bin, root), Ok(claude.clone()));

        // A link that leads to itself.
        std::fs::remove_file(bin.join("current")).unwrap();
        std::os::unix::fs::symlink("claude", bin.join("current")).unwrap();
        assert!(owned_safely(&claude).is_err());
    }

    /// A folder on the way that is a link is checked where it leads, folders above included.
    #[cfg(unix)]
    #[test]
    fn a_linked_folder_is_checked_where_it_leads() {
        let tmp = tempfile::tempdir().unwrap();
        // Where the walk reports steps: past macOS's `/var` link.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let root = root.as_path();
        let open = root.join("open");
        let planted = program(&open.join("real").join("bin"), "claude");
        let alias = root.join("alias");
        std::os::unix::fs::symlink(open.join("real"), &alias).unwrap();
        let bin = alias.join("bin");
        assert_eq!(
            find_in(&bin, root),
            Ok(bin.join(planted.file_name().unwrap()))
        );

        // Others could swap `real` for their own folder.
        chmod(&open, 0o777);
        assert_eq!(owned_safely(&bin.join("claude")), Err(open.clone()));
    }

    /// Someone else's link or program is never used, even when it leads to root's files.
    #[cfg(unix)]
    #[test]
    fn someone_elses_link_is_not_used() {
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } != 0 {
            eprintln!("skipped: only root can give a file away");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // Where the walk reports steps: past macOS's `/var` link.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let root = root.as_path();
        let bin = root.join("bin");
        let planted = program(&bin, "claude");
        let link = bin.join("other");
        std::os::unix::fs::symlink(&planted, &link).unwrap();
        assert_eq!(owned_safely(&link), Ok(()));
        // Changes a link itself, not what it leads to.
        if std::os::unix::fs::lchown(&link, Some(65534), Some(65534)).is_err() {
            eprintln!("skipped: uid 65534 isn't mapped here");
            return;
        }
        assert_eq!(owned_safely(&link), Err(link.clone()));
        assert_eq!(owned_safely(&planted), Ok(()));
        std::os::unix::fs::lchown(&planted, Some(65534), Some(65534)).unwrap();
        assert_eq!(owned_safely(&planted), Err(planted.clone()));
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
