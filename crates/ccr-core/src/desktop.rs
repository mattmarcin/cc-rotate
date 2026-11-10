//! Sessions of the Claude desktop app's Code tab (spec section 24).
//!
//! The app starts Claude Code with its own login, which a swap cannot reach (F26). With its
//! policy `claudeCodeProcessWrapperEnabled` on and a launcher in Claude Code's managed settings,
//! it starts every session through that launcher instead and sends no login (F27, F33). cc-rotate's
//! launcher, `cc-rotate-ctl desktop-launch`, then puts the session on the cc-rotate profile.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What the app sets in launcher mode, or could set, that would keep a session off the
/// profile's login (F28) or switch off artifacts and fast mode (F29).
pub const REMOVED: [&str; 6] = [
    "CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST",
    "CLAUDE_CODE_HOST_AUTH_ENV_VAR",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "CLAUDE_CODE_DISABLE_FAST_MODE",
];

/// A session the app started in launcher mode: its entry point, and the mark it sets when it
/// sends no login. Anything else the app starts through the launcher is left as it is.
pub fn is_desktop_session(var: impl Fn(&str) -> Option<OsString>) -> bool {
    var("CLAUDE_CODE_ENTRYPOINT").is_some_and(|e| e == "claude-desktop")
        && var("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST").is_some_and(|v| !v.is_empty())
}

/// What the launcher does to the environment before it starts Claude Code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// The profile to set as `CLAUDE_CONFIG_DIR`; `None` leaves that variable as the app set it,
    /// or unset. On macOS Claude Code names its Keychain item after the variable's exact value, so
    /// `~/.claude` set by hand is another login than `~/.claude` by default.
    pub config_dir: Option<PathBuf>,
    pub remove: Vec<&'static str>,
    /// Variables to set; the relay's two when relay mode is on and the session runs on the pool.
    pub set: Vec<(&'static str, String)>,
}

/// The launch of `args` (Claude Code's arguments, without its path) in the environment `var`
/// reads, with the cc-rotate profile at `rotate_profile`. `relay_env` is what relay mode gives a
/// session (spec section 4); it goes only to a session that runs on the pool.
pub fn plan_launch(
    args: &[OsString],
    var: impl Fn(&str) -> Option<OsString>,
    rotate_profile: &Path,
    home: &Path,
    relay_env: Option<[(&'static str, String); 2]>,
) -> LaunchPlan {
    if !is_desktop_session(&var) {
        return LaunchPlan {
            config_dir: None,
            remove: Vec::new(),
            set: Vec::new(),
        };
    }
    // A session only the app's profile holds runs there, so the app can still resume it: one
    // `adopt_app_transcript` could not bring onto the pool (it was not started by the app, or
    // the link failed). CLAUDE_CONFIG_DIR is then left exactly as the app set it (or unset), so
    // Claude Code finds that profile's login in the same Keychain item as before.
    let app_profile = app_profile(&var, home);
    let config_dir = match resumed_id(args) {
        Some(id) if !has_transcript(rotate_profile, &id) && has_transcript(&app_profile, &id) => {
            None
        }
        _ => Some(rotate_profile.to_path_buf()),
    };
    let set = match (&config_dir, relay_env) {
        (Some(dir), Some(env)) if dir == rotate_profile => env.into(),
        _ => Vec::new(),
    };
    LaunchPlan {
        config_dir,
        remove: REMOVED.to_vec(),
        set,
    }
}

/// Whether `profile` holds the transcript of session `id` (`projects/<folder>/<id>.jsonl`).
/// An id that could name another path is never looked up.
fn has_transcript(profile: &Path, id: &str) -> bool {
    find_transcript(profile, id).is_some()
}

/// Where `profile` holds the transcript of session `id`, if it does.
fn find_transcript(profile: &Path, id: &str) -> Option<PathBuf> {
    if id.is_empty() || id.contains(['/', '\\']) || id.contains("..") {
        return None;
    }
    let file = format!("{id}.jsonl");
    std::fs::read_dir(profile.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|folder| folder.path().join(&file))
        .find(|path| path.is_file())
}

/// Bring a desktop session that only the app's profile holds onto the pool, and return the
/// pool's new transcript.
///
/// The app writes some sessions' transcripts into its own profile even after desktop sessions
/// moved to the pool: a session forked from another one, or started before the move. Resumed
/// there, such a session would run on that profile's own login, where no swap reaches it, and
/// stop at that account's limit. A hard link at the same place in the pool (the
/// `link_app_transcripts` link the other way round) makes `plan_launch` resume it on the pool,
/// while the app keeps reading the same file. Its folder (`<id>/`) is symlinked with it.
///
/// Only a session the app started (`"entrypoint":"claude-desktop"`) is brought over, and
/// nothing already in the pool is touched. `None` when there is nothing to do or the link
/// failed: the session then stays in the app's profile.
pub fn adopt_app_transcript(
    id: &str,
    rotate_profile: &Path,
    app_profile: &Path,
) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if same_profile(rotate_profile, app_profile) || has_transcript(rotate_profile, id) {
            return None;
        }
        let file = find_transcript(app_profile, id)?;
        if !started_by_the_app(&file) {
            return None;
        }
        let folder = rotate_profile
            .join("projects")
            .join(file.parent()?.file_name()?);
        let link = folder.join(file.file_name()?);
        std::fs::create_dir_all(&folder).ok()?;
        std::fs::hard_link(&file, &link).ok()?;
        let dir = file.with_extension("");
        let dir_link = link.with_extension("");
        if dir.is_dir() && std::fs::symlink_metadata(&dir_link).is_err() {
            let _ = std::os::unix::fs::symlink(&dir, &dir_link);
        }
        Some(link)
    }
    #[cfg(not(unix))]
    {
        let _ = (id, rotate_profile, app_profile);
        None
    }
}

/// Whether the two profiles are one folder.
fn same_profile(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// The session id that `--resume <id>` or `--resume=<id>` names, if any.
pub fn resumed_id(args: &[OsString]) -> Option<String> {
    let mut args = args.iter().map(|a| a.to_string_lossy());
    while let Some(arg) = args.next() {
        let id = if arg == "--resume" {
            args.next().map(|v| v.into_owned())
        } else {
            arg.strip_prefix("--resume=").map(str::to_string)
        };
        if let Some(id) = id {
            return Some(id).filter(|id| !id.is_empty() && !id.starts_with('-'));
        }
    }
    None
}

/// The profile the app started a session in: `CLAUDE_CONFIG_DIR` as the app set it, or
/// `~/.claude`.
pub fn app_profile(var: impl Fn(&str) -> Option<OsString>, home: &Path) -> PathBuf {
    var("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".claude"))
}

/// How much of a transcript is read to find where its session was started.
const ENTRYPOINT_HEAD: u64 = 64 * 1024;

/// Link, into the app's profile, the transcripts of desktop sessions that ran on the pool, and
/// return the links made.
///
/// The app looks for a session's transcript only in its own profile
/// (`projects/<folder>/<id>.jsonl`). A session that runs on the pool writes it in the pool
/// profile, so once the app restarts (after an update, say) it finds nothing there and shows
/// the session as "not found on disk". A hard link at the same place, to the pool's file, lets
/// it read the history while the session keeps running on the pool: `plan_launch` resumes a
/// session on the pool whenever the pool holds its transcript. (The app refuses a transcript
/// that is a symlink; both profiles live under the home folder, so one volume.) The session's
/// folder of subagent transcripts and tool results (`<id>/`) is symlinked with it.
///
/// Only transcripts whose session the app started (`"entrypoint":"claude-desktop"`) are linked,
/// and nothing already at a link's place is touched. Errors leave a transcript for the next
/// sweep.
pub fn link_app_transcripts(rotate_profile: &Path, app_profile: &Path) -> Vec<PathBuf> {
    let mut linked = Vec::new();
    #[cfg(unix)]
    {
        if same_profile(rotate_profile, app_profile) {
            return linked;
        }
        let Ok(folders) = std::fs::read_dir(rotate_profile.join("projects")) else {
            return linked;
        };
        for folder in folders.flatten() {
            let Ok(files) = std::fs::read_dir(folder.path()) else {
                continue;
            };
            let app_folder = app_profile.join("projects").join(folder.file_name());
            for file in files.flatten() {
                let path = file.path();
                let is_transcript = path.extension().is_some_and(|e| e == "jsonl")
                    && file.file_type().is_ok_and(|t| t.is_file());
                if !is_transcript {
                    continue;
                }
                let link = app_folder.join(file.file_name());
                // Earlier builds made a symlink, which the app now refuses to read: such a
                // link to this transcript is replaced.
                let ours = std::fs::read_link(&link).is_ok_and(|to| to == path);
                if (!ours && std::fs::symlink_metadata(&link).is_ok()) || !started_by_the_app(&path)
                {
                    continue;
                }
                if std::fs::create_dir_all(&app_folder).is_err()
                    || (ours && std::fs::remove_file(&link).is_err())
                    || std::fs::hard_link(&path, &link).is_err()
                {
                    continue;
                }
                let dir = path.with_extension("");
                let dir_link = link.with_extension("");
                if dir.is_dir() && std::fs::symlink_metadata(&dir_link).is_err() {
                    let _ = std::os::unix::fs::symlink(&dir, &dir_link);
                }
                linked.push(link);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = (rotate_profile, app_profile, ENTRYPOINT_HEAD);
    linked
}

/// Whether the session of the transcript at `path` was started by the desktop app.
#[cfg(unix)]
fn started_by_the_app(path: &Path) -> bool {
    use std::io::Read;
    let mut head = Vec::new();
    let read =
        std::fs::File::open(path).and_then(|f| f.take(ENTRYPOINT_HEAD).read_to_end(&mut head));
    if read.is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&head);
    head.find("\"entrypoint\":\"")
        .is_some_and(|at| head[at..].starts_with("\"entrypoint\":\"claude-desktop\""))
}

// ---- Turning it on and off (spec section 24.2) ------------------------------------------------

/// Where the app's policy and Claude Code's managed settings live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Windows,
    /// No desktop app whose sessions cc-rotate can move.
    Unsupported,
}

/// The app's policy `claudeCodeProcessWrapperEnabled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Not set.
    Off,
    /// Set to false.
    Disabled,
    On,
    /// It could not be read (the reason).
    Unknown(String),
}

/// The launcher Claude Code's managed settings name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launcher {
    None,
    /// `cc-rotate-ctl desktop-launch --`, this `cc-rotate-ctl`.
    Ours,
    /// Another program, or another `cc-rotate-ctl` (the value as written).
    Other(String),
    /// The settings could not be read (the reason).
    Unreadable(String),
}

/// What `cc-rotate-ctl desktop` and `doctor` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub platform: Platform,
    pub policy: Policy,
    /// The policy's file or key holds other values: it belongs to device management.
    pub policy_foreign: bool,
    pub launcher: Launcher,
    /// The managed settings hold more than the launcher, or come from somewhere cc-rotate does
    /// not write: they belong to device management.
    pub launcher_foreign: bool,
}

impl Setup {
    /// Desktop sessions run on the pool.
    pub fn on(&self) -> bool {
        self.policy == Policy::On && self.launcher == Launcher::Ours
    }
}

/// The policy's name in the app's managed preferences and registry key.
pub const POLICY_KEY: &str = "claudeCodeProcessWrapperEnabled";

/// The `processWrapper` value that names this `cc-rotate-ctl`: a JSON array in a string, so a
/// path with spaces needs no quoting.
pub fn wrapper_value(ctl: &Path) -> String {
    serde_json::json!([ctl.to_string_lossy(), "desktop-launch", "--"]).to_string()
}

/// The managed settings file's content that names this `cc-rotate-ctl`.
pub fn settings_text(ctl: &Path) -> String {
    serde_json::json!({ "processWrapper": wrapper_value(ctl) }).to_string()
}

/// The launcher a managed settings file names, and whether the file holds anything else. The
/// app takes `env.CLAUDE_CODE_PROCESS_WRAPPER` ahead of `processWrapper`.
pub fn parse_launcher(text: &str, ctl: &Path) -> (Launcher, bool) {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim().is_empty() {
        return (Launcher::None, false);
    }
    let Ok(serde_json::Value::Object(settings)) = serde_json::from_str::<serde_json::Value>(text)
    else {
        return (Launcher::Unreadable("not a JSON object".into()), true);
    };
    let foreign = settings.keys().any(|k| k != "processWrapper");
    let from_env = settings
        .get("env")
        .and_then(|e| e.get(WRAPPER_VAR))
        .and_then(serde_json::Value::as_str);
    let value = from_env
        .or_else(|| {
            settings
                .get("processWrapper")
                .and_then(serde_json::Value::as_str)
        })
        .filter(|v| !v.trim().is_empty());
    let launcher = match value {
        None => Launcher::None,
        Some(v) if names_ctl(v, ctl) => Launcher::Ours,
        Some(v) => Launcher::Other(v.to_string()),
    };
    (launcher, foreign)
}

const WRAPPER_VAR: &str = "CLAUDE_CODE_PROCESS_WRAPPER";

/// Whether a launcher value is `<this cc-rotate-ctl> desktop-launch --`, in either form the app
/// reads: a JSON array, or words with double quotes around those that hold spaces.
fn names_ctl(value: &str, ctl: &Path) -> bool {
    let argv = launcher_argv(value);
    match argv.as_slice() {
        [program, verb, dashes] if verb == "desktop-launch" && dashes == "--" => {
            same_file(Path::new(program), ctl)
        }
        _ => false,
    }
}

/// A launcher value as the app splits it: a JSON array, or words.
fn launcher_argv(value: &str) -> Vec<String> {
    if value.trim_start().starts_with('[') {
        serde_json::from_str(value).unwrap_or_default()
    } else {
        split_words(value)
    }
}

/// The program a launcher value starts.
pub fn launcher_program(value: &str) -> Option<PathBuf> {
    launcher_argv(value).into_iter().next().map(PathBuf::from)
}

fn split_words(value: &str) -> Vec<String> {
    let mut words = Vec::new();
    let (mut word, mut quoted, mut started) = (String::new(), false, false);
    for c in value.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            c => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

fn same_file(a: &Path, b: &Path) -> bool {
    let canonical = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (a, b) = (canonical(a), canonical(b));
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// The values of `reg query <key>` output: (name, type, data). Fields are separated by four
/// spaces; key lines and blank lines are skipped.
pub fn parse_reg_query(text: &str) -> Vec<(String, String, String)> {
    text.lines()
        .filter(|l| l.starts_with("    "))
        .filter_map(|l| {
            let mut parts = l.trim_start().trim_end_matches('\r').splitn(3, "    ");
            Some((
                parts.next()?.to_string(),
                parts.next()?.to_string(),
                parts.next().unwrap_or("").to_string(),
            ))
        })
        .collect()
}

/// `s` as one word for a POSIX shell.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `s` as a single-quoted PowerShell string.
pub fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

const MAC_POLICY: &str = "/Library/Managed Preferences/com.anthropic.claudefordesktop.plist";

/// The configuration profile that sets the app's policy on macOS.
pub const PROFILE_ID: &str = "cc-rotate.desktop-sessions";
pub const PROFILE_NAME: &str = "cc-rotate: desktop sessions on the pool";
const PROFILE_FILE: &str = r#""$HOME/Downloads/cc-rotate-desktop-sessions.mobileconfig""#;

/// The configuration profile (`.mobileconfig`) that turns on the app's policy for the whole Mac.
/// Its identifiers are fixed, so installing it again replaces it.
pub fn mobileconfig() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>PayloadContent</key>
	<array>
		<dict>
			<key>PayloadType</key>
			<string>com.anthropic.claudefordesktop</string>
			<key>PayloadIdentifier</key>
			<string>{PROFILE_ID}.policy</string>
			<key>PayloadUUID</key>
			<string>8BACDDD9-1C34-4651-B8C3-A5D3375C7BC7</string>
			<key>PayloadVersion</key>
			<integer>1</integer>
			<key>PayloadDisplayName</key>
			<string>Claude: use Claude Code's corporate launcher</string>
			<key>{POLICY_KEY}</key>
			<true/>
		</dict>
	</array>
	<key>PayloadDisplayName</key>
	<string>{PROFILE_NAME}</string>
	<key>PayloadDescription</key>
	<string>Makes the Claude app start its Code sessions through the launcher in Claude Code's managed settings, which cc-rotate uses to run them on its account pool. Remove this profile to turn that off.</string>
	<key>PayloadIdentifier</key>
	<string>{PROFILE_ID}</string>
	<key>PayloadType</key>
	<string>Configuration</string>
	<key>PayloadScope</key>
	<string>System</string>
	<key>PayloadUUID</key>
	<string>FA13B150-8F46-4494-A1E8-1B18379EA1A3</string>
	<key>PayloadVersion</key>
	<integer>1</integer>
</dict>
</plist>
"#
    )
}
const MAC_SETTINGS_DIR: &str = "/Library/Application Support/ClaudeCode";
const WIN_POLICY_KEY: &str = r"HKLM\SOFTWARE\Policies\Claude";
const WIN_CODE_POLICY_KEY: &str = r"HKLM\SOFTWARE\Policies\ClaudeCode";

/// Claude Code's managed settings folder on this platform.
pub fn settings_dir(platform: Platform) -> Option<PathBuf> {
    match platform {
        Platform::MacOs => Some(PathBuf::from(MAC_SETTINGS_DIR)),
        // Joined by hand: these are Windows paths, whatever platform builds the commands.
        Platform::Windows => Some(PathBuf::from(format!(
            r"{}\ClaudeCode",
            std::env::var("ProgramFiles")
                .unwrap_or_else(|_| r"C:\Program Files".into())
                .trim_end_matches('\\')
        ))),
        Platform::Unsupported => None,
    }
}

/// Claude Code's managed settings file on this platform.
pub fn settings_file(platform: Platform) -> Option<PathBuf> {
    let dir = settings_dir(platform)?;
    Some(match platform {
        Platform::Windows => PathBuf::from(format!(r"{}\managed-settings.json", dir.display())),
        _ => dir.join("managed-settings.json"),
    })
}

/// Where the app's policy lives, as the user would look for it.
pub fn policy_location(platform: Platform) -> &'static str {
    match platform {
        Platform::MacOs => MAC_POLICY,
        Platform::Windows => WIN_POLICY_KEY,
        Platform::Unsupported => "",
    }
}

pub fn this_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(windows) {
        Platform::Windows
    } else {
        Platform::Unsupported
    }
}

/// The commands that turn desktop sessions on, for the user to run as an administrator. A part
/// that belongs to device management gets none (see `admin_notes`).
pub fn on_commands(setup: &Setup, ctl: &Path) -> Vec<String> {
    let (Some(dir), Some(file)) = (settings_dir(setup.platform), settings_file(setup.platform))
    else {
        return Vec::new();
    };
    let mut commands = Vec::new();
    let policy_due = setup.policy != Policy::On && !setup.policy_foreign;
    let launcher_due = setup.launcher != Launcher::Ours && !setup.launcher_foreign;
    match setup.platform {
        Platform::MacOs => {
            // macOS keeps /Library/Managed Preferences in step with the installed profiles, so
            // the policy comes as one, which the user installs in System Settings.
            if policy_due {
                commands.push(format!(
                    "{} desktop --mobileconfig > {PROFILE_FILE}",
                    sh_quote(&ctl.to_string_lossy())
                ));
                commands.push(format!("open {PROFILE_FILE}"));
            }
            if launcher_due {
                commands.push(format!(
                    "sudo mkdir -p {}",
                    sh_quote(&dir.to_string_lossy())
                ));
                commands.push(format!(
                    "printf '%s\\n' {} | sudo tee {} > /dev/null",
                    sh_quote(&settings_text(ctl)),
                    sh_quote(&file.to_string_lossy())
                ));
            }
        }
        Platform::Windows => {
            if policy_due {
                commands.push(format!(
                    "reg add {WIN_POLICY_KEY} /v {POLICY_KEY} /t REG_DWORD /d 1 /f"
                ));
            }
            if launcher_due {
                commands.push(format!(
                    "New-Item -ItemType Directory -Force -Path {} | Out-Null",
                    ps_quote(&dir.to_string_lossy())
                ));
                // UTF-8 without a byte order mark.
                commands.push(format!(
                    "[IO.File]::WriteAllText({}, {})",
                    ps_quote(&file.to_string_lossy()),
                    ps_quote(&settings_text(ctl))
                ));
            }
        }
        Platform::Unsupported => {}
    }
    commands
}

/// The commands that turn desktop sessions off: only what cc-rotate's own commands wrote. The
/// policy stays while another program's launcher uses it.
pub fn off_commands(setup: &Setup) -> Vec<String> {
    let Some(file) = settings_file(setup.platform) else {
        return Vec::new();
    };
    let policy_ours = !setup.policy_foreign
        && matches!(setup.policy, Policy::On | Policy::Disabled)
        && matches!(setup.launcher, Launcher::Ours | Launcher::None);
    let launcher_ours = setup.launcher == Launcher::Ours && !setup.launcher_foreign;
    let mut commands = Vec::new();
    match setup.platform {
        Platform::MacOs => {
            if policy_ours {
                commands.push(format!("sudo profiles remove -identifier {PROFILE_ID}"));
            }
            if launcher_ours {
                commands.push(format!("sudo rm -f {}", sh_quote(&file.to_string_lossy())));
            }
        }
        Platform::Windows => {
            if policy_ours {
                commands.push(format!("reg delete {WIN_POLICY_KEY} /v {POLICY_KEY} /f"));
            }
            if launcher_ours {
                commands.push(format!(
                    "Remove-Item -Force {}",
                    ps_quote(&file.to_string_lossy())
                ));
            }
        }
        Platform::Unsupported => {}
    }
    commands
}

/// Why `uninstall` must not remove this `cc-rotate-ctl` yet: Claude Code's managed settings name
/// it, and the app could then start no Code session (spec section 24.3).
pub fn uninstall_refusal(setup: &Setup) -> Option<String> {
    if setup.launcher != Launcher::Ours {
        return None;
    }
    let off = off_commands(setup);
    let how = if off.is_empty() {
        "Ask your organization's administrator to remove processWrapper from Claude Code's managed settings first".to_string()
    } else {
        let shell = match setup.platform {
            Platform::Windows => "in PowerShell run as administrator",
            _ => "in Terminal",
        };
        format!(
            "Turn that off first by running these {shell}, then quit and reopen the Claude app:\n  {}\n",
            off.join("\n  ")
        )
    };
    Some(format!(
        "Claude Code's managed settings start the desktop app's Code sessions through this cc-rotate-ctl, so without it the app could start none. {how}Or pass --force."
    ))
}

/// What to ask an organization's administrator for, when device management owns a part.
pub fn admin_notes(setup: &Setup, ctl: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    if setup.policy_foreign && setup.policy != Policy::On {
        notes.push(format!(
            "Your organization's device management sets the Claude app's policies ({}). Ask its administrator to turn on {POLICY_KEY} (\"Use Claude Code's corporate launcher\").",
            policy_location(setup.platform)
        ));
    }
    if setup.launcher_foreign && setup.launcher != Launcher::Ours {
        notes.push(format!(
            "Your organization's device management sets Claude Code's managed settings. Ask its administrator to set processWrapper to {}.",
            wrapper_value(ctl)
        ));
    }
    notes
}

/// The setup on this computer, as the app would find it. Never fails: what can't be read is
/// reported as such.
pub fn read_setup() -> Setup {
    let platform = this_platform();
    let mut setup = Setup {
        platform,
        policy: Policy::Off,
        policy_foreign: false,
        launcher: Launcher::None,
        launcher_foreign: false,
    };
    let ctl = std::env::current_exe().unwrap_or_default();
    match platform {
        Platform::MacOs => {
            (setup.policy, setup.policy_foreign) = read_mac_policy();
        }
        Platform::Windows => {
            (setup.policy, setup.policy_foreign) = read_windows_policy();
            // Claude Code reads its registry policy ahead of the file.
            if let Some(found) = read_windows_code_policy(&ctl) {
                (setup.launcher, setup.launcher_foreign) = found;
                return setup;
            }
        }
        Platform::Unsupported => return setup,
    }
    if let Some(dir) = settings_dir(platform) {
        (setup.launcher, setup.launcher_foreign) = read_settings_dir(&dir, &ctl);
    }
    setup
}

/// The launcher in a managed settings folder: `managed-settings.json`, with drop-ins in
/// `managed-settings.d/`, which cc-rotate never writes.
fn read_settings_dir(dir: &Path, ctl: &Path) -> (Launcher, bool) {
    let drop_ins = std::fs::read_dir(dir.join("managed-settings.d"))
        .into_iter()
        .flatten()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().ends_with(".json"));
    let (launcher, foreign) = match std::fs::read_to_string(dir.join("managed-settings.json")) {
        Ok(text) => parse_launcher(&text, ctl),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Launcher::None, false),
        Err(e) => (Launcher::Unreadable(e.to_string()), true),
    };
    (launcher, foreign || drop_ins)
}

fn policy_value(value: Option<bool>) -> Policy {
    match value {
        None => Policy::Off,
        Some(false) => Policy::Disabled,
        Some(true) => Policy::On,
    }
}

fn read_mac_policy() -> (Policy, bool) {
    if !Path::new(MAC_POLICY).exists() {
        return (Policy::Off, false);
    }
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-", "--", MAC_POLICY])
        .stdin(std::process::Stdio::null())
        .output();
    let parsed = out
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| serde_json::from_slice::<serde_json::Value>(&o.stdout).ok());
    match parsed {
        Some(prefs) => parse_mac_policy(&prefs),
        None => (
            Policy::Unknown(format!("{MAC_POLICY} could not be read")),
            true,
        ),
    }
}

/// The app's managed preferences, as `plutil` gives them in JSON. A configuration profile may
/// leave its payload's `Payload…` keys there; any other key is device management's.
fn parse_mac_policy(prefs: &serde_json::Value) -> (Policy, bool) {
    let Some(prefs) = prefs.as_object() else {
        return (Policy::Unknown("not a dictionary".into()), true);
    };
    let foreign = prefs
        .keys()
        .any(|k| k != POLICY_KEY && !k.starts_with("Payload"));
    let value = prefs.get(POLICY_KEY).map(|v| {
        v.as_bool()
            .or_else(|| v.as_i64().map(|n| n != 0))
            .unwrap_or(false)
    });
    (policy_value(value), foreign)
}

/// `reg query`'s values, `None` when the key is not there.
fn reg_query(args: &[&str]) -> Option<Vec<(String, String, String)>> {
    let reg = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into()))
        .join("System32")
        .join("reg.exe");
    let out = std::process::Command::new(reg)
        .arg("query")
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| parse_reg_query(&String::from_utf8_lossy(&out.stdout)))
}

fn read_windows_policy() -> (Policy, bool) {
    let Some(values) = reg_query(&[WIN_POLICY_KEY]) else {
        return (Policy::Off, false);
    };
    let foreign = values.iter().any(|(name, _, _)| name != POLICY_KEY);
    let value = values
        .iter()
        .find(|(name, _, _)| name == POLICY_KEY)
        .map(|(_, _, data)| {
            let data = data.trim();
            data.strip_prefix("0x")
                .map(|hex| i64::from_str_radix(hex, 16).unwrap_or(0) != 0)
                .unwrap_or(data == "1" || data.eq_ignore_ascii_case("true"))
        });
    (policy_value(value), foreign)
}

/// Claude Code's managed settings in the registry (`Settings` under its policy key), which
/// device management writes and which win over the file.
fn read_windows_code_policy(ctl: &Path) -> Option<(Launcher, bool)> {
    let values = reg_query(&[WIN_CODE_POLICY_KEY, "/v", "Settings"])?;
    let (_, _, data) = values.into_iter().find(|(name, _, _)| name == "Settings")?;
    Some((parse_launcher(&data, ctl).0, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), OsString::from(v)))
            .collect();
        move |k| map.get(k).cloned()
    }

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    const DESKTOP: [(&str, &str); 3] = [
        ("CLAUDE_CODE_ENTRYPOINT", "claude-desktop"),
        ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1"),
        ("CLAUDE_CODE_HOST_AUTH_ENV_VAR", "ANTHROPIC_AUTH_TOKEN"),
    ];

    fn transcript(profile: &Path, id: &str) {
        let dir = profile.join("projects").join("-Users-me-project");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{id}.jsonl")), "{}\n").unwrap();
    }

    #[test]
    fn not_a_desktop_session_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        for vars in [
            env(&[("CLAUDE_CODE_ENTRYPOINT", "cli")]),
            env(&[("CLAUDE_CODE_ENTRYPOINT", "claude-desktop")]),
            env(&[
                ("CLAUDE_CODE_ENTRYPOINT", "claude-desktop"),
                ("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", ""),
            ]),
            env(&[("CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST", "1")]),
        ] {
            assert!(!is_desktop_session(&vars));
            let plan = plan_launch(&args(&["-p"]), &vars, &rotate, root.path(), None);
            assert_eq!(
                plan,
                LaunchPlan {
                    config_dir: None,
                    remove: vec![],
                    set: vec![]
                }
            );
        }
    }

    #[test]
    fn a_desktop_session_moves_to_the_rotation_profile() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let vars = env(&DESKTOP);
        assert!(is_desktop_session(&vars));
        let plan = plan_launch(
            &args(&["-p", "--output-format", "stream-json"]),
            &vars,
            &rotate,
            root.path(),
            None,
        );
        assert_eq!(plan.config_dir, Some(rotate));
        assert_eq!(plan.remove, REMOVED.to_vec());
    }

    #[test]
    fn a_resumed_older_session_keeps_its_profile() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let app = root.path().join("app profile");
        transcript(&app, "abc-123");
        let mut pairs = DESKTOP.to_vec();
        let app_str = app.to_str().unwrap().to_string();
        pairs.push(("CLAUDE_CONFIG_DIR", &app_str));
        let vars = env(&pairs);
        for a in [
            args(&["-p", "--resume", "abc-123"]),
            args(&["--resume=abc-123", "-p"]),
        ] {
            let plan = plan_launch(&a, &vars, &rotate, root.path(), None);
            // The app's CLAUDE_CONFIG_DIR is left as it is.
            assert_eq!(plan.config_dir, None, "{a:?}");
            assert_eq!(plan.remove, REMOVED.to_vec());
        }

        // Without CLAUDE_CONFIG_DIR the app's profile is ~/.claude, and the variable stays unset:
        // on macOS Claude Code names its Keychain item after CLAUDE_CONFIG_DIR, so setting it to
        // ~/.claude would look for another item than the profile's login and say it is not
        // logged in.
        let home = root.path().join("home");
        transcript(&home.join(".claude"), "def-456");
        let plan = plan_launch(
            &args(&["--resume", "def-456"]),
            env(&DESKTOP),
            &rotate,
            &home,
            None,
        );
        assert_eq!(plan.config_dir, None);
        assert_eq!(plan.remove, REMOVED.to_vec());
    }

    #[test]
    fn a_resumed_session_in_the_rotation_profile_stays_there() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let home = root.path().join("home");
        transcript(&rotate, "abc-123");
        transcript(&home.join(".claude"), "abc-123");
        let plan = plan_launch(
            &args(&["--resume", "abc-123"]),
            env(&DESKTOP),
            &rotate,
            &home,
            None,
        );
        assert_eq!(plan.config_dir, Some(rotate.clone()));

        // A session in neither profile (a new one the app resumes by id) runs on the pool.
        let plan = plan_launch(
            &args(&["--resume", "zzz"]),
            env(&DESKTOP),
            &rotate,
            &home,
            None,
        );
        assert_eq!(plan.config_dir, Some(rotate));
    }

    #[test]
    fn resume_without_an_id_is_ignored() {
        assert_eq!(resumed_id(&args(&["-p", "--resume"])), None);
        assert_eq!(resumed_id(&args(&["--resume="])), None);
        assert_eq!(resumed_id(&args(&["--resume", "--verbose"])), None);
        assert_eq!(
            resumed_id(&args(&["--resume", "abc"])).as_deref(),
            Some("abc")
        );
        assert_eq!(resumed_id(&args(&["--resume=abc"])).as_deref(), Some("abc"));

        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let plan = plan_launch(
            &args(&["--resume"]),
            env(&DESKTOP),
            &rotate,
            root.path(),
            None,
        );
        assert_eq!(plan.config_dir, Some(rotate));
    }

    #[test]
    fn an_id_that_is_a_path_is_never_looked_up() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let home = root.path().join("home");
        // A file a crafted id would reach from projects/<folder>/.
        transcript(&home.join(".claude"), "x");
        std::fs::write(home.join(".claude").join("projects").join("y.jsonl"), "").unwrap();
        for id in ["../y", "a/b", "a\\b", ".."] {
            let plan = plan_launch(
                &args(&["--resume", id]),
                env(&DESKTOP),
                &rotate,
                &home,
                None,
            );
            assert_eq!(plan.config_dir, Some(rotate.clone()), "{id}");
        }
    }

    const RELAY: [(&str, &str); 2] = [("ANTHROPIC_BASE_URL", "http://x"), ("FIRST", "1")];

    fn relay_env() -> Option<[(&'static str, String); 2]> {
        Some([
            ("ANTHROPIC_BASE_URL", "http://x".to_string()),
            ("FIRST", "1".to_string()),
        ])
    }

    #[test]
    fn relay_env_set_for_pool_session() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let plan = plan_launch(
            &args(&["-p"]),
            env(&DESKTOP),
            &rotate,
            root.path(),
            relay_env(),
        );
        assert_eq!(plan.config_dir, Some(rotate));
        let want: Vec<(&str, String)> = RELAY.iter().map(|(k, v)| (*k, v.to_string())).collect();
        assert_eq!(plan.set, want);
    }

    #[test]
    fn relay_env_not_for_resumed_old_session() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let home = root.path().join("home");
        transcript(&home.join(".claude"), "def-456");
        let plan = plan_launch(
            &args(&["--resume", "def-456"]),
            env(&DESKTOP),
            &rotate,
            &home,
            relay_env(),
        );
        assert_eq!(plan.config_dir, None);
        assert!(plan.set.is_empty());
    }

    #[test]
    fn relay_env_not_for_non_desktop() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        let plan = plan_launch(
            &args(&["-p"]),
            env(&[("CLAUDE_CODE_ENTRYPOINT", "cli")]),
            &rotate,
            root.path(),
            relay_env(),
        );
        assert!(plan.set.is_empty());
    }
}

#[cfg(test)]
mod setup_tests {
    use super::*;

    fn setup(platform: Platform, policy: Policy, launcher: Launcher) -> Setup {
        Setup {
            platform,
            policy,
            policy_foreign: false,
            launcher,
            launcher_foreign: false,
        }
    }

    #[test]
    fn the_wrapper_value_is_a_json_array_string() {
        let v: serde_json::Value =
            serde_json::from_str(&wrapper_value(Path::new("/a b/cc-rotate-ctl"))).unwrap();
        assert_eq!(
            v,
            serde_json::json!(["/a b/cc-rotate-ctl", "desktop-launch", "--"])
        );
    }

    #[test]
    fn launchers_are_told_apart() {
        let ctl = Path::new("/opt/cc rotate/cc-rotate-ctl");
        let ours = serde_json::json!({ "processWrapper": wrapper_value(ctl) }).to_string();
        assert_eq!(parse_launcher(&ours, ctl), (Launcher::Ours, false));
        assert_eq!(
            parse_launcher(&format!("\u{feff}{ours}"), ctl),
            (Launcher::Ours, false)
        );
        // The shell-like form the app also accepts.
        let spaced = r#"{"processWrapper": "\"/opt/cc rotate/cc-rotate-ctl\" desktop-launch --"}"#;
        assert_eq!(parse_launcher(spaced, ctl), (Launcher::Ours, false));
        assert_eq!(
            parse_launcher(r#"{"processWrapper": "/usr/local/bin/corp-launcher"}"#, ctl),
            (
                Launcher::Other("/usr/local/bin/corp-launcher".into()),
                false
            )
        );
        // Another cc-rotate-ctl is not this one.
        let other =
            serde_json::json!({ "processWrapper": wrapper_value(Path::new("/old/cc-rotate-ctl")) })
                .to_string();
        assert!(matches!(parse_launcher(&other, ctl).0, Launcher::Other(_)));
        let more = serde_json::json!({ "processWrapper": wrapper_value(ctl), "model": "opus" })
            .to_string();
        assert_eq!(parse_launcher(&more, ctl), (Launcher::Ours, true));
        // The env form wins in the app, like Claude Code.
        let env_form = serde_json::json!({ "env": { "CLAUDE_CODE_PROCESS_WRAPPER": "/x/l" }, "processWrapper": wrapper_value(ctl) })
            .to_string();
        assert_eq!(
            parse_launcher(&env_form, ctl),
            (Launcher::Other("/x/l".into()), true)
        );
        assert_eq!(parse_launcher("{}", ctl), (Launcher::None, false));
        assert_eq!(parse_launcher("", ctl), (Launcher::None, false));
        assert!(matches!(
            parse_launcher("{not json", ctl),
            (Launcher::Unreadable(_), true)
        ));
        assert!(matches!(
            parse_launcher("[1]", ctl),
            (Launcher::Unreadable(_), true)
        ));
    }

    #[test]
    fn reg_query_output_is_parsed() {
        let text = "\r\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Policies\\Claude\r\n    claudeCodeProcessWrapperEnabled    REG_DWORD    0x1\r\n    Some Name    REG_SZ    a value  with   spaces\r\n\r\n";
        assert_eq!(
            parse_reg_query(text),
            vec![
                (
                    "claudeCodeProcessWrapperEnabled".into(),
                    "REG_DWORD".into(),
                    "0x1".into()
                ),
                (
                    "Some Name".into(),
                    "REG_SZ".into(),
                    "a value  with   spaces".into()
                ),
            ]
        );
        assert!(parse_reg_query("").is_empty());
    }

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("/Users/o'k/x"), r#"'/Users/o'\''k/x'"#);
        assert_eq!(sh_quote("plain"), "'plain'");
        assert_eq!(ps_quote(r"C:\Users\O'Brien"), r"'C:\Users\O''Brien'");
    }

    /// The macOS commands, run by `sh` with `sudo` and the paths replaced, write the file the
    /// app reads.
    #[cfg(unix)]
    #[test]
    fn the_macos_commands_write_the_settings_file() {
        let ctl = Path::new("/Users/o'k/My Bin/cc-rotate-ctl");
        let commands = on_commands(&setup(Platform::MacOs, Policy::Off, Launcher::None), ctl);
        assert_eq!(commands.len(), 4, "{commands:#?}");
        // The policy comes as a configuration profile for the user to install.
        assert_eq!(
            commands[0],
            r#"'/Users/o'\''k/My Bin/cc-rotate-ctl' desktop --mobileconfig > "$HOME/Downloads/cc-rotate-desktop-sessions.mobileconfig""#
        );
        assert_eq!(
            commands[1],
            r#"open "$HOME/Downloads/cc-rotate-desktop-sessions.mobileconfig""#
        );
        assert_eq!(
            commands[2],
            "sudo mkdir -p '/Library/Application Support/ClaudeCode'"
        );
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("managed-settings.json");
        let script = commands[3].replace("sudo tee", "tee").replace(
            "'/Library/Application Support/ClaudeCode/managed-settings.json'",
            &sh_quote(target.to_str().unwrap()),
        );
        let status = std::process::Command::new("/bin/sh")
            .args(["-c", &script])
            .status()
            .unwrap();
        assert!(status.success());
        let text = std::fs::read_to_string(&target).unwrap();
        assert_eq!(parse_launcher(&text, ctl), (Launcher::Ours, false));

        // Already on and set: nothing to run.
        assert!(on_commands(&setup(Platform::MacOs, Policy::On, Launcher::Ours), ctl).is_empty());
        // A policy that is there but off gets the profile too.
        let set = on_commands(
            &setup(Platform::MacOs, Policy::Disabled, Launcher::Ours),
            ctl,
        );
        assert_eq!(set, commands[..2].to_vec());
    }

    #[test]
    fn the_configuration_profile_turns_the_policy_on() {
        let profile = mobileconfig();
        assert!(profile.contains(
            "<key>PayloadType</key>\n\t\t\t<string>com.anthropic.claudefordesktop</string>"
        ));
        assert!(profile.contains("<key>claudeCodeProcessWrapperEnabled</key>\n\t\t\t<true/>"));
        assert!(profile.contains(&format!("<string>{PROFILE_ID}</string>")));
        #[cfg(target_os = "macos")]
        {
            let root = tempfile::tempdir().unwrap();
            let file = root.path().join("p.mobileconfig");
            std::fs::write(&file, &profile).unwrap();
            let lint = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&file)
                .output()
                .unwrap();
            assert!(lint.status.success(), "{lint:?}");
        }
    }

    #[test]
    fn the_windows_commands() {
        let ctl = Path::new(r"C:\Users\O'Brien\.local\bin\cc-rotate-ctl.exe");
        let commands = on_commands(&setup(Platform::Windows, Policy::Off, Launcher::None), ctl);
        assert_eq!(
            commands[0],
            r"reg add HKLM\SOFTWARE\Policies\Claude /v claudeCodeProcessWrapperEnabled /t REG_DWORD /d 1 /f"
        );
        assert!(commands[1].starts_with("New-Item -ItemType Directory -Force -Path '"));
        assert!(commands[1].ends_with(r"\ClaudeCode' | Out-Null"));
        assert!(commands[2].starts_with("[IO.File]::WriteAllText('"));
        assert!(commands[2].contains(r"\ClaudeCode\managed-settings.json', '"));
        // The JSON inside, with PowerShell's quote doubling undone, is ours.
        let start = commands[2].find(", '").unwrap() + 3;
        let json = commands[2][start..commands[2].len() - 2].replace("''", "'");
        assert_eq!(parse_launcher(&json, ctl), (Launcher::Ours, false));

        let off = off_commands(&setup(Platform::Windows, Policy::On, Launcher::Ours));
        assert_eq!(
            off[0],
            r"reg delete HKLM\SOFTWARE\Policies\Claude /v claudeCodeProcessWrapperEnabled /f"
        );
        assert!(off[1].starts_with("Remove-Item -Force '"));
    }

    #[test]
    fn off_removes_only_what_is_ours() {
        let mac = off_commands(&setup(Platform::MacOs, Policy::On, Launcher::Ours));
        assert_eq!(
            mac,
            vec![
                "sudo profiles remove -identifier cc-rotate.desktop-sessions".to_string(),
                "sudo rm -f '/Library/Application Support/ClaudeCode/managed-settings.json'"
                    .to_string(),
            ]
        );
        // Another program's launcher, and the policy that serves it, are left alone.
        assert!(
            off_commands(&setup(
                Platform::MacOs,
                Policy::On,
                Launcher::Other("/x".into())
            ))
            .is_empty()
        );
        let foreign = Setup {
            policy_foreign: true,
            launcher_foreign: true,
            ..setup(Platform::MacOs, Policy::On, Launcher::Ours)
        };
        assert!(off_commands(&foreign).is_empty());
        assert!(
            on_commands(
                &Setup {
                    policy: Policy::Off,
                    launcher: Launcher::None,
                    ..foreign.clone()
                },
                Path::new("/x/cc-rotate-ctl")
            )
            .is_empty()
        );
        let notes = admin_notes(
            &Setup {
                policy: Policy::Off,
                launcher: Launcher::None,
                ..foreign
            },
            Path::new("/x/cc-rotate-ctl"),
        );
        assert_eq!(notes.len(), 2, "{notes:#?}");
        assert!(notes[0].contains(POLICY_KEY));
        assert!(notes[1].contains("processWrapper"));
    }

    #[test]
    fn uninstall_is_refused_while_the_app_starts_sessions_through_this_ctl() {
        let ours = setup(Platform::MacOs, Policy::On, Launcher::Ours);
        let why = uninstall_refusal(&ours).unwrap();
        assert!(
            why.contains(
                "sudo rm -f '/Library/Application Support/ClaudeCode/managed-settings.json'"
            ),
            "{why}"
        );
        assert!(why.contains("--force"));
        // Even with the policy off: the file still names the program uninstall removes.
        assert!(uninstall_refusal(&setup(Platform::MacOs, Policy::Off, Launcher::Ours)).is_some());
        assert!(
            uninstall_refusal(&setup(
                Platform::MacOs,
                Policy::On,
                Launcher::Other("/x".into())
            ))
            .is_none()
        );
        assert!(uninstall_refusal(&setup(Platform::MacOs, Policy::Off, Launcher::None)).is_none());
    }

    /// A configuration profile's managed plist may carry its payload's bookkeeping keys; only
    /// other settings mean device management owns it.
    #[test]
    fn the_macos_policy_plist() {
        let parse = |v: serde_json::Value| parse_mac_policy(&v);
        assert_eq!(
            parse(serde_json::json!({ POLICY_KEY: true })),
            (Policy::On, false)
        );
        assert_eq!(
            parse(
                serde_json::json!({ POLICY_KEY: true, "PayloadUUID": "x", "PayloadIdentifier": "y" })
            ),
            (Policy::On, false)
        );
        assert_eq!(
            parse(serde_json::json!({ POLICY_KEY: false })),
            (Policy::Disabled, false)
        );
        assert_eq!(
            parse(serde_json::json!({ POLICY_KEY: true, "disableAutoUpdates": true })),
            (Policy::On, true)
        );
        assert_eq!(parse(serde_json::json!({})), (Policy::Off, false));
        assert!(matches!(
            parse(serde_json::json!([1])),
            (Policy::Unknown(_), true)
        ));
    }

    #[test]
    fn nothing_to_run_where_there_is_no_desktop_app() {
        let s = setup(Platform::Unsupported, Policy::Off, Launcher::None);
        assert!(on_commands(&s, Path::new("/x")).is_empty());
        assert!(off_commands(&s).is_empty());
    }
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::*;

    const FOLDER: &str = "-Users-me-project";

    fn pool_transcript(rotate: &Path, id: &str, first_line: &str) -> PathBuf {
        let dir = rotate.join("projects").join(FOLDER);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{id}.jsonl"));
        std::fs::write(&file, format!("{first_line}\n")).unwrap();
        file
    }

    const DESKTOP_LINE: &str =
        r#"{"type":"user","entrypoint":"claude-desktop","sessionId":"x","message":{}}"#;

    #[test]
    fn a_desktop_transcript_on_the_pool_is_linked_into_the_app_profile() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        let file = pool_transcript(&rotate, "abc", DESKTOP_LINE);
        // Its subagents' transcripts and tool results.
        std::fs::create_dir_all(file.with_extension("").join("subagents")).unwrap();

        let linked = link_app_transcripts(&rotate, &app);

        let link = app.join("projects").join(FOLDER).join("abc.jsonl");
        assert_eq!(linked, vec![link.clone()]);
        // A hard link: the app refuses a transcript that is a symlink.
        assert_same_file(&link, &file);
        assert_eq!(
            std::fs::read_link(app.join("projects").join(FOLDER).join("abc")).unwrap(),
            file.with_extension("")
        );
        // What the session writes later shows through the link.
        std::fs::write(&file, "more\n").unwrap();
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "more\n");

        // Done once: a second sweep links nothing.
        assert!(link_app_transcripts(&rotate, &app).is_empty());
    }

    fn assert_same_file(link: &Path, file: &Path) {
        use std::os::unix::fs::MetadataExt;
        let meta = std::fs::symlink_metadata(link).unwrap();
        assert!(meta.file_type().is_file(), "{link:?} is not a regular file");
        assert_eq!(meta.ino(), std::fs::metadata(file).unwrap().ino());
    }

    #[test]
    fn a_symlink_left_by_an_earlier_build_becomes_a_hard_link() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        let file = pool_transcript(&rotate, "abc", DESKTOP_LINE);
        let link = app.join("projects").join(FOLDER).join("abc.jsonl");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();

        assert_eq!(link_app_transcripts(&rotate, &app), vec![link.clone()]);
        assert_same_file(&link, &file);
    }

    #[test]
    fn a_symlink_to_something_else_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        pool_transcript(&rotate, "abc", DESKTOP_LINE);
        let elsewhere = root.path().join("elsewhere.jsonl");
        std::fs::write(&elsewhere, "x\n").unwrap();
        let link = app.join("projects").join(FOLDER).join("abc.jsonl");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        assert!(link_app_transcripts(&rotate, &app).is_empty());
        assert_eq!(std::fs::read_link(&link).unwrap(), elsewhere);
    }

    #[test]
    fn only_desktop_sessions_are_linked() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        pool_transcript(&rotate, "cli", r#"{"type":"user","entrypoint":"cli"}"#);
        // Nothing yet tells where it was started: looked at again on the next sweep.
        pool_transcript(&rotate, "fresh", r#"{"type":"queue-operation"}"#);

        assert!(link_app_transcripts(&rotate, &app).is_empty());
        assert!(!app.join("projects").join(FOLDER).exists());
    }

    #[test]
    fn a_transcript_already_in_the_app_profile_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        pool_transcript(&rotate, "abc", DESKTOP_LINE);
        let own = app.join("projects").join(FOLDER).join("abc.jsonl");
        std::fs::create_dir_all(own.parent().unwrap()).unwrap();
        std::fs::write(&own, "the app's own copy\n").unwrap();

        assert!(link_app_transcripts(&rotate, &app).is_empty());
        assert_eq!(
            std::fs::read_to_string(&own).unwrap(),
            "the app's own copy\n"
        );
        assert!(!std::fs::symlink_metadata(&own).unwrap().is_symlink());
    }

    #[test]
    fn nothing_is_linked_when_the_app_uses_the_pool_profile() {
        let root = tempfile::tempdir().unwrap();
        let rotate = root.path().join("rotate");
        pool_transcript(&rotate, "abc", DESKTOP_LINE);
        assert!(link_app_transcripts(&rotate, &rotate).is_empty());
        assert!(
            link_app_transcripts(&root.path().join("none"), &root.path().join("app")).is_empty()
        );
    }

    fn app_transcript(app: &Path, id: &str, first_line: &str) -> PathBuf {
        pool_transcript(app, id, first_line)
    }

    #[test]
    fn a_desktop_session_only_in_the_app_profile_is_brought_onto_the_pool() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        // A session the app forked writes its transcript only in its own profile.
        let file = app_transcript(&app, "fork", DESKTOP_LINE);
        std::fs::create_dir_all(file.with_extension("").join("subagents")).unwrap();

        let adopted = adopt_app_transcript("fork", &rotate, &app);

        let pool = rotate.join("projects").join(FOLDER).join("fork.jsonl");
        assert_eq!(adopted, Some(pool.clone()));
        // One file in both profiles: what the session writes on the pool shows in the app.
        assert_same_file(&pool, &file);
        assert_eq!(
            std::fs::read_link(pool.with_extension("")).unwrap(),
            file.with_extension("")
        );
        // The launch then resumes it on the pool.
        assert!(has_transcript(&rotate, "fork"));
        // Done once.
        assert_eq!(adopt_app_transcript("fork", &rotate, &app), None);
        // And the sweep back into the app's profile leaves the app's file alone.
        assert!(link_app_transcripts(&rotate, &app).is_empty());
        assert_same_file(&file, &pool);
    }

    #[test]
    fn only_desktop_sessions_are_brought_onto_the_pool() {
        let root = tempfile::tempdir().unwrap();
        let (rotate, app) = (root.path().join("rotate"), root.path().join("app"));
        app_transcript(&app, "cli", r#"{"type":"user","entrypoint":"cli"}"#);
        app_transcript(&app, "unknown", "{}");
        for id in ["cli", "unknown", "missing", "../x", ""] {
            assert_eq!(adopt_app_transcript(id, &rotate, &app), None, "{id}");
        }
        assert!(!rotate.join("projects").exists());
        // Nothing is adopted when the app already uses the pool profile.
        app_transcript(&rotate, "own", DESKTOP_LINE);
        assert_eq!(adopt_app_transcript("own", &rotate, &rotate), None);
    }

    #[test]
    fn the_app_profile_is_its_config_dir_or_dot_claude() {
        let home = Path::new("/home/me");
        assert_eq!(app_profile(|_| None, home), home.join(".claude"));
        assert_eq!(
            app_profile(|_| Some(OsString::new()), home),
            home.join(".claude")
        );
        assert_eq!(
            app_profile(|_| Some(OsString::from("/elsewhere")), home),
            PathBuf::from("/elsewhere")
        );
    }
}
