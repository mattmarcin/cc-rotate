//! Creating the rotation profile from an existing Claude Code profile (spec section 4.3).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::credfile;
use crate::paths::{self, forward_slashes};
use crate::store::Store;

const COPIED_DIRS: &[&str] = &[
    "agents",
    "commands",
    "hooks",
    "skills",
    "output-styles",
    "plugins",
];
const COPIED_JSON_FILES: &[&str] = &["settings.local.json", "keybindings.json"];
const SEEDED_CONFIG_KEYS: &[&str] = &[
    "hasCompletedOnboarding",
    "lastOnboardingVersion",
    "installMethod",
    "autoUpdates",
    "autoUpdatesProtectedForNative",
    "theme",
];
/// Per-project settings carried over: trust flags and project-scoped MCP configuration.
const PROJECT_KEYS: &[&str] = &[
    "hasTrustDialogAccepted",
    "hasCompletedProjectOnboarding",
    "mcpServers",
    "enabledMcpjsonServers",
    "disabledMcpjsonServers",
    "disabledMcpServers",
];
const ALIAS_BEGIN: &str = "# >>> cc-rotate >>>";
const ALIAS_END: &str = "# <<< cc-rotate <<<";

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub template: PathBuf,
    pub profile: PathBuf,
    /// The `cc-rotate-ctl` executable the profile's statusline and hook should run.
    pub ctl_path: PathBuf,
    pub force: bool,
    pub install_alias: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct SetupReport {
    pub profile: PathBuf,
    pub copied: Vec<String>,
    pub claude_md: Option<String>,
    pub statusline_downstream: Option<String>,
    pub alias_files: Vec<PathBuf>,
    /// User-level MCP servers carried over from the template.
    pub mcp_servers: Vec<String>,
}

pub fn run(store: &Store, opts: &SetupOptions) -> Result<SetupReport> {
    let template = dunce::canonicalize(&opts.template)
        .with_context(|| format!("template profile {} not found", opts.template.display()))?;
    let profile = opts.profile.clone();
    if profile.join("settings.json").exists() && !opts.force {
        bail!(
            "{} already exists; pass --force to rebuild it (its login is kept)",
            profile.display()
        );
    }
    if same_dir(&template, &profile) {
        bail!("the template and the rotation profile must be different directories");
    }
    std::fs::create_dir_all(&profile)?;
    let profile = dunce::canonicalize(&profile)?;
    let rewrite = PathRewriter::new(&template, &profile);
    let mut report = SetupReport {
        profile: profile.clone(),
        ..Default::default()
    };

    for dir in COPIED_DIRS {
        let src = template.join(dir);
        if src.is_dir() {
            copy_dir(&src, &profile.join(dir))?;
            report.copied.push(format!("{dir}/"));
        }
    }
    let plugins = profile.join("plugins");
    if plugins.is_dir() {
        for entry in std::fs::read_dir(&plugins)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json") {
                rewrite_file(&path, &path, &rewrite)?;
            }
        }
    }
    for file in COPIED_JSON_FILES {
        let src = template.join(file);
        if src.is_file() {
            rewrite_file(&src, &profile.join(file), &rewrite)?;
            report.copied.push((*file).to_string());
        }
    }

    report.claude_md = link_claude_md(&template, &profile)?;

    let (settings, downstream) = build_settings(&template, &rewrite, &opts.ctl_path)?;
    std::fs::write(
        profile.join("settings.json"),
        serde_json::to_string_pretty(&settings)? + "\n",
    )?;
    report.copied.push("settings.json".into());
    report.statusline_downstream = downstream.clone();

    report.mcp_servers = merge_config(&template, &profile)?;

    store.set_setting("profile_dir", Some(&profile.display().to_string()))?;
    store.set_setting("statusline_downstream", downstream.as_deref())?;

    if opts.install_alias {
        report.alias_files = install_powershell_alias(&profile)?;
    }
    Ok(report)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (dunce::canonicalize(a), dunce::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Rewrites occurrences of the template's path to the new profile's path inside JSON text,
/// in both the JSON-escaped backslash form and the forward-slash form. A match only counts
/// when followed by a separator or a quote, so `.claude` never rewrites `.claude-alt`.
struct PathRewriter {
    rules: Vec<(regex::Regex, String)>,
}

impl PathRewriter {
    fn new(from: &Path, to: &Path) -> Self {
        let from_s = from.display().to_string();
        let to_s = to.display().to_string();
        let variants = [
            (from_s.replace('\\', "\\\\"), to_s.replace('\\', "\\\\")),
            (from_s.replace('\\', "/"), to_s.replace('\\', "/")),
        ];
        let rules = variants
            .into_iter()
            .map(|(f, t)| {
                let re = regex::Regex::new(&format!(r#"(?i){}(\\\\|/|"|'|$)"#, regex::escape(&f)))
                    .unwrap();
                (re, format!("{}$1", t.replace('$', "$$")))
            })
            .collect();
        Self { rules }
    }

    fn apply(&self, text: &str) -> String {
        self.rules.iter().fold(text.to_string(), |acc, (re, rep)| {
            re.replace_all(&acc, rep.as_str()).into_owned()
        })
    }
}

fn rewrite_file(src: &Path, dst: &Path, rewrite: &PathRewriter) -> Result<()> {
    let text =
        std::fs::read_to_string(src).with_context(|| format!("reading {}", src.display()))?;
    std::fs::write(dst, rewrite.apply(&text)).with_context(|| format!("writing {}", dst.display()))
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            make_writable(&target);
            std::fs::copy(entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// Git stores pack files read-only, so a rebuild must clear that before copying over them.
fn make_writable(path: &Path) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perms = meta.permissions();
        if perms.readonly() {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

fn link_claude_md(template: &Path, profile: &Path) -> Result<Option<String>> {
    let src = template.join("CLAUDE.md");
    let dst = profile.join("CLAUDE.md");
    if !src.is_file() {
        return Ok(None);
    }
    if dst.exists() {
        return Ok(Some("kept".into()));
    }
    if std::fs::hard_link(&src, &dst).is_ok() {
        return Ok(Some("hard link".into()));
    }
    std::fs::copy(&src, &dst)?;
    Ok(Some("copy".into()))
}

fn build_settings(
    template: &Path,
    rewrite: &PathRewriter,
    ctl: &Path,
) -> Result<(Value, Option<String>)> {
    let src = template.join("settings.json");
    let original: Value = if src.is_file() {
        serde_json::from_str(&std::fs::read_to_string(&src)?)
            .context("parsing the template settings.json")?
    } else {
        json!({})
    };
    let downstream = original
        .pointer("/statusLine/command")
        .and_then(Value::as_str)
        .filter(|c| !c.contains("cc-rotate-ctl"))
        .map(str::to_string);

    let mut settings: Value = serde_json::from_str(&rewrite.apply(&original.to_string()))?;
    let obj = settings
        .as_object_mut()
        .context("settings.json is not an object")?;
    let ctl = command_path(ctl);

    let mut status = obj
        .get("statusLine")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    status.insert("type".into(), json!("command"));
    status.insert("command".into(), json!(format!("{ctl} statusline")));
    // Seconds. Ticks also run after every message; this covers idle sessions.
    status.insert("refreshInterval".into(), json!(30));
    obj.insert("statusLine".into(), Value::Object(status));

    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("settings.hooks is not an object")?;
    let stop_failure = hooks.entry("StopFailure").or_insert_with(|| json!([]));
    let list = stop_failure
        .as_array_mut()
        .context("settings.hooks.StopFailure is not an array")?;
    list.retain(|group| !group.to_string().contains("cc-rotate-ctl"));
    list.push(json!({
        "hooks": [{ "type": "command", "command": format!("{ctl} hook stop-failure"), "timeout": 60 }]
    }));
    Ok((settings, downstream))
}

/// The executable as it should appear at the start of a statusline or hook command.
///
/// Claude Code may run those commands through PowerShell (`CLAUDE_CODE_USE_POWERSHELL_TOOL`),
/// bash or cmd. A quoted path followed by arguments is a parse error in PowerShell, so the path
/// must be unquoted: as is when it has no spaces, else its 8.3 short form. Quoting is the last
/// resort, for volumes with short names disabled.
pub fn command_path(exe: &Path) -> String {
    let plain = forward_slashes(exe);
    if !plain.contains(char::is_whitespace) {
        return plain;
    }
    if let Some(short) = short_path(exe).map(|p| forward_slashes(&p)) {
        if !short.contains(char::is_whitespace) {
            return short;
        }
    }
    format!("\"{plain}\"")
}

#[cfg(windows)]
fn short_path(path: &Path) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut buf = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and `buf` is writable for its full length.
    let len =
        unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) } as usize;
    if len == 0 || len >= buf.len() {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(&buf[..len])))
}

#[cfg(not(windows))]
fn short_path(_path: &Path) -> Option<PathBuf> {
    None
}

/// The `.claude.json` Claude Code uses for a profile. The default profile (`~/.claude`)
/// keeps it at `~/.claude.json`; explicit `CLAUDE_CONFIG_DIR` profiles keep it inside.
fn template_config(template: &Path) -> PathBuf {
    let is_default = paths::default_template_dir()
        .ok()
        .and_then(|d| dunce::canonicalize(d).ok())
        .is_some_and(|d| d == template);
    let home_config = paths::home_dir().map(|h| h.join(".claude.json")).ok();
    match home_config {
        Some(p) if is_default && p.is_file() => p,
        _ => template.join(".claude.json"),
    }
}

/// Merge what the rotation profile needs from the template's `.claude.json` into the profile's:
/// onboarding flags, user-level MCP servers, and per-project trust flags and MCP servers.
/// Existing values in the profile always win, so re-running setup never undoes a change made in
/// the rotation profile. Account data, history and caches are never copied.
/// Returns the names of the user-level MCP servers that were added.
fn merge_config(template: &Path, profile: &Path) -> Result<Vec<String>> {
    let dst = profile.join(".claude.json");
    let src = template_config(template);
    let original: Map<String, Value> = std::fs::read_to_string(&src)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let mut merged: Map<String, Value> = match credfile::read_document(&dst)? {
        Some(doc) => doc,
        None => Map::new(),
    };

    for key in SEEDED_CONFIG_KEYS {
        if let Some(v) = original.get(*key) {
            merged.entry(*key).or_insert_with(|| v.clone());
        }
    }
    merged
        .entry("hasCompletedOnboarding")
        .or_insert(json!(true));

    let mut added = Vec::new();
    if let Some(servers) = original.get("mcpServers").and_then(Value::as_object) {
        let target = merged.entry("mcpServers").or_insert_with(|| json!({}));
        if let Some(target) = target.as_object_mut() {
            for (name, server) in servers {
                if !target.contains_key(name) {
                    target.insert(name.clone(), server.clone());
                    added.push(name.clone());
                }
            }
        }
    }

    if let Some(projects) = original.get("projects").and_then(Value::as_object) {
        let target = merged.entry("projects").or_insert_with(|| json!({}));
        if let Some(target) = target.as_object_mut() {
            for (path, project) in projects {
                let mut carried = Map::new();
                for key in PROJECT_KEYS {
                    let Some(v) = project.get(*key) else { continue };
                    let worth_copying = match v {
                        Value::Bool(b) => *b,
                        Value::Object(o) => !o.is_empty(),
                        Value::Array(a) => !a.is_empty(),
                        _ => false,
                    };
                    if worth_copying {
                        carried.insert((*key).to_string(), v.clone());
                    }
                }
                if carried.is_empty() {
                    continue;
                }
                let entry = target.entry(path.clone()).or_insert_with(|| json!({}));
                if let Some(entry) = entry.as_object_mut() {
                    for (k, v) in carried {
                        entry.entry(k).or_insert(v);
                    }
                }
            }
        }
    }

    credfile::atomic_write(
        &dst,
        serde_json::to_string_pretty(&Value::Object(merged))?.as_bytes(),
    )?;
    Ok(added)
}

/// Add (or replace) a `cc-rotate` function in the user's PowerShell profiles.
///
/// Writes to every existing profile among PowerShell 7 and Windows PowerShell 5.1; when none
/// exist, creates the PowerShell 7 one if `pwsh` is installed, else the 5.1 one.
pub fn install_powershell_alias(profile_dir: &Path) -> Result<Vec<PathBuf>> {
    let docs = dirs::document_dir().context("could not find the Documents folder")?;
    let pwsh = docs
        .join("PowerShell")
        .join("Microsoft.PowerShell_profile.ps1");
    let winps = docs
        .join("WindowsPowerShell")
        .join("Microsoft.PowerShell_profile.ps1");
    let mut targets: Vec<PathBuf> = [&pwsh, &winps]
        .into_iter()
        .filter(|p| p.is_file())
        .cloned()
        .collect();
    if targets.is_empty() {
        targets.push(if which::which("pwsh").is_ok() {
            pwsh
        } else {
            winps
        });
    }
    let quoted = profile_dir.display().to_string().replace('\'', "''");
    let block = format!(
        "{ALIAS_BEGIN}\n\
         # Runs Claude Code on the cc-rotate account pool. Managed by `cc-rotate-ctl setup`.\n\
         function cc-rotate {{\n\
         \x20   $old = $env:CLAUDE_CONFIG_DIR\n\
         \x20   $env:CLAUDE_CONFIG_DIR = '{quoted}'\n\
         \x20   try {{ claude @args }} finally {{ $env:CLAUDE_CONFIG_DIR = $old }}\n\
         }}\n\
         {ALIAS_END}\n"
    );
    for target in &targets {
        let existing = std::fs::read_to_string(target).unwrap_or_default();
        let updated = replace_block(&existing, &block);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, updated).with_context(|| format!("writing {}", target.display()))?;
    }
    Ok(targets)
}

fn replace_block(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(ALIAS_BEGIN), existing.find(ALIAS_END)) {
        if end > start {
            let after = &existing[end + ALIAS_END.len()..];
            let after = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .unwrap_or(after);
            return format!("{}{}{}", &existing[..start], block, after);
        }
    }
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewriter_respects_boundaries() {
        let r = PathRewriter::new(
            Path::new(r"C:\Users\x\.claude"),
            Path::new(r"C:\Users\x\.claude-rotate"),
        );
        let text = r#"{"a":"C:\\Users\\x\\.claude\\hooks\\h.ps1","b":"C:\\Users\\x\\.claude-alt\\x","c":"c:/users/x/.claude/plugins","d":"C:\\Users\\x\\.claude"}"#;
        let out = r.apply(text);
        assert!(out.contains(r#""a":"C:\\Users\\x\\.claude-rotate\\hooks\\h.ps1""#));
        assert!(out.contains(r#""b":"C:\\Users\\x\\.claude-alt\\x""#));
        assert!(out.contains(r#""c":"C:/Users/x/.claude-rotate/plugins""#));
        assert!(out.contains(r#""d":"C:\\Users\\x\\.claude-rotate""#));
    }

    /// Claude Code may run statusline and hook commands through any of these shells, so the
    /// generated command must start a program in every one of them, even from a path with spaces.
    #[cfg(windows)]
    #[test]
    fn command_path_runs_in_every_shell() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("Program Files Like").join("bin");
        std::fs::create_dir_all(&spaced).unwrap();
        let exe = spaced.join("probe tool.exe");
        std::fs::copy(r"C:\Windows\System32\where.exe", &exe).unwrap();

        // No `/flags`: Git Bash rewrites arguments that start with `/` into Windows paths.
        let command = format!("{} cmd", command_path(&exe));
        assert!(
            !command.starts_with('"'),
            "unquoted when 8.3 names exist: {command}"
        );

        let shells: [(&str, &[&str]); 4] = [
            ("pwsh", &["-NoProfile", "-Command"]),
            ("powershell", &["-NoProfile", "-Command"]),
            ("cmd", &["/D", "/C"]),
            ("bash", &["-c"]),
        ];
        let mut ran = 0;
        for (shell, args) in shells {
            let Ok(found) = which::which(shell) else {
                continue;
            };
            // System32\bash.exe is the WSL launcher, which runs Linux; Claude Code uses Git Bash.
            if shell == "bash" && found.to_string_lossy().to_lowercase().contains("system32") {
                continue;
            }
            let status = std::process::Command::new(found)
                .args(args)
                .arg(&command)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "{shell} could not run: {command}");
            ran += 1;
        }
        assert!(
            ran >= 2,
            "at least cmd and Windows PowerShell exist on Windows"
        );
    }

    #[test]
    fn alias_block_is_idempotent() {
        let once = replace_block(
            "existing\n",
            "# >>> cc-rotate >>>\nA\n# <<< cc-rotate <<<\n",
        );
        let twice = replace_block(&once, "# >>> cc-rotate >>>\nB\n# <<< cc-rotate <<<\n");
        assert_eq!(
            twice,
            "existing\n\n# >>> cc-rotate >>>\nB\n# <<< cc-rotate <<<\n"
        );
    }

    #[test]
    fn setup_builds_profile() {
        let root = tempfile::tempdir().unwrap();
        let template = root.path().join("tpl");
        std::fs::create_dir_all(template.join("hooks")).unwrap();
        std::fs::write(template.join("hooks").join("h.ps1"), "x").unwrap();
        std::fs::write(template.join("CLAUDE.md"), "rules").unwrap();
        std::fs::write(
            template.join(".credentials.json"),
            r#"{"claudeAiOauth":{"refreshToken":"secret"}}"#,
        )
        .unwrap();
        let tpl = dunce::canonicalize(&template).unwrap();
        let hook_path = tpl
            .join("hooks")
            .join("h.ps1")
            .display()
            .to_string()
            .replace('\\', "\\\\");
        std::fs::write(
            template.join("settings.json"),
            format!(
                r#"{{"model":"opus","statusLine":{{"type":"command","command":"node status.js"}},"hooks":{{"SessionStart":[{{"hooks":[{{"type":"command","command":"pwsh {hook_path}"}}]}}]}}}}"#
            ),
        )
        .unwrap();
        std::fs::write(
            template.join(".claude.json"),
            r#"{"hasCompletedOnboarding":true,"oauthAccount":{"accountUuid":"u"},
                "mcpServers":{"docs":{"type":"http","url":"https://example.test/mcp"},"local":{"command":"node","args":["s.js"]}},
                "projects":{"C:/p":{"hasTrustDialogAccepted":true,"history":[1],"mcpServers":{"proj":{"command":"x"}},"enabledMcpjsonServers":[]}}}"#,
        )
        .unwrap();

        let store = Store::open_in_memory().unwrap();
        let profile = root.path().join("rot");
        let report = run(
            &store,
            &SetupOptions {
                template: template.clone(),
                profile: profile.clone(),
                ctl_path: PathBuf::from(r"C:\bin\cc-rotate-ctl.exe"),
                force: false,
                install_alias: false,
            },
        )
        .unwrap();

        assert_eq!(
            report.statusline_downstream.as_deref(),
            Some("node status.js")
        );
        assert!(!profile.join(".credentials.json").exists());
        assert!(profile.join("hooks").join("h.ps1").is_file());
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(profile.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings["model"], "opus");
        assert_eq!(
            settings["statusLine"]["command"],
            "C:/bin/cc-rotate-ctl.exe statusline"
        );
        assert_eq!(settings["statusLine"]["refreshInterval"], 30);
        let hook = settings["hooks"]["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(hook.contains("rot"), "hook path rewritten: {hook}");
        assert_eq!(
            settings["hooks"]["StopFailure"][0]["hooks"][0]["command"],
            "C:/bin/cc-rotate-ctl.exe hook stop-failure"
        );
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(profile.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(config["hasCompletedOnboarding"], true);
        assert!(config.get("oauthAccount").is_none());
        assert_eq!(
            config["mcpServers"],
            json!({"docs":{"type":"http","url":"https://example.test/mcp"},"local":{"command":"node","args":["s.js"]}})
        );
        assert_eq!(report.mcp_servers, vec!["docs", "local"]);
        assert_eq!(
            config["projects"]["C:/p"],
            json!({"hasTrustDialogAccepted": true, "mcpServers": {"proj": {"command": "x"}}})
        );

        // Re-running with --force merges: values already in the profile win, new servers are added,
        // and keys Claude Code wrote meanwhile survive.
        let mut edited = config.clone();
        edited["mcpServers"]["docs"] = json!({"type":"http","url":"https://changed.test/mcp"});
        edited["numStartups"] = json!(4);
        std::fs::write(profile.join(".claude.json"), edited.to_string()).unwrap();
        let again = run(
            &store,
            &SetupOptions {
                template: template.clone(),
                profile: profile.clone(),
                ctl_path: PathBuf::from(r"C:\bin\cc-rotate-ctl.exe"),
                force: true,
                install_alias: false,
            },
        )
        .unwrap();
        assert!(again.mcp_servers.is_empty());
        let merged: Value =
            serde_json::from_str(&std::fs::read_to_string(profile.join(".claude.json")).unwrap())
                .unwrap();
        assert_eq!(
            merged["mcpServers"]["docs"]["url"],
            "https://changed.test/mcp"
        );
        assert_eq!(merged["numStartups"], 4);
        assert_eq!(
            store.settings().unwrap().statusline_downstream.as_deref(),
            Some("node status.js")
        );

        assert!(
            run(
                &store,
                &SetupOptions {
                    template,
                    profile,
                    ctl_path: PathBuf::from("x"),
                    force: false,
                    install_alias: false
                }
            )
            .is_err()
        );
    }
}
