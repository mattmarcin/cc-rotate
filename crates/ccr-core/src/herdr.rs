//! herdr integration (spec section 19): make herdr resume cc-rotate sessions with
//! `cc-rotate --resume <id>` instead of its built-in `claude --resume <id>`, which would bring the
//! session back on the default profile.
//!
//! A `SessionStart` hook in the rotation profile reports the resume command for the pane with
//! `herdr pane report-agent-session`. herdr keeps it while the `claude` process runs, types it into
//! the pane's shell when the server restarts, and drops it when `claude` exits. Everything here is
//! pure (no processes) except [`detect`] and [`version`].

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// The `--source` cc-rotate reports under. Free-form for herdr; `herdr:` names are reserved.
pub const REPORT_SOURCE: &str = "cc-rotate";
/// The agent label herdr detects for Claude Code, which the report must match.
pub const AGENT: &str = "claude";
/// The executable herdr types on restore. It must be a bare command name on the user's `PATH`.
pub const LAUNCHER: &str = "cc-rotate";
/// Every way a main session starts: new, resumed, after `/clear`, after compaction, forked.
pub const SESSION_START_MATCHER: &str = "^(startup|resume|clear|compact|fork)$";
/// The `cc-rotate-ctl` arguments the hook runs.
pub const SESSION_START_HOOK_ARGS: &str = "hook session-start";
/// The first herdr release with `pane report-agent-session` and reported resume commands.
pub const MIN_VERSION: (u64, u64, u64) = (0, 9, 2);

/// herdr's limits on a reported command (`validate_resume_argv` in herdr's `agent_resume.rs`).
const MAX_RESUME_ARGS: usize = 64;
const MAX_RESUME_ARGV_BYTES: usize = 8 * 1024;
/// Claude Code session ids are UUIDs; anything much longer is not one.
const MAX_SESSION_ID_LEN: usize = 128;

/// The herdr pane this process runs in, from the environment herdr gives every pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub id: String,
    /// The herdr executable to call: `HERDR_BIN_PATH`, else `herdr` on `PATH`.
    pub bin: OsString,
}

/// The pane from `HERDR_ENV=1` and `HERDR_PANE_ID`, or `None` outside herdr.
pub fn pane_from_env() -> Option<Pane> {
    pane_from(|name| std::env::var_os(name))
}

fn pane_from(var: impl Fn(&str) -> Option<OsString>) -> Option<Pane> {
    if var("HERDR_ENV").as_deref() != Some(OsStr::new("1")) {
        return None;
    }
    let id = var("HERDR_PANE_ID")?.into_string().ok()?;
    let id = id.trim();
    if id.is_empty() {
        return None;
    }
    Some(Pane {
        id: id.to_string(),
        bin: bin_from(var),
    })
}

/// The herdr executable: `HERDR_BIN_PATH` when set, else `herdr` (found on `PATH`).
pub fn bin_from_env() -> OsString {
    bin_from(|name| std::env::var_os(name))
}

fn bin_from(var: impl Fn(&str) -> Option<OsString>) -> OsString {
    var("HERDR_BIN_PATH")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "herdr".into())
}

/// The session id to report from Claude Code's `SessionStart` hook input, or `None` when this
/// start needs no report (a subagent, or another event). An error means the input is unusable.
///
/// Subagents carry a non-empty `agent_id`. `agent_type` alone is not a subagent: Claude Code also
/// sets it for a main session started with `claude --agent <name>`.
pub fn session_to_report(hook_input: &str) -> Result<Option<String>> {
    let input: Value =
        serde_json::from_str(hook_input).context("the SessionStart hook input is not JSON")?;
    if let Some(event) = input.get("hook_event_name").and_then(Value::as_str) {
        if event != "SessionStart" {
            return Ok(None);
        }
    }
    let is_subagent = input
        .get("agent_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    if is_subagent {
        return Ok(None);
    }
    let Some(session) = input.get("session_id").and_then(Value::as_str) else {
        bail!("the SessionStart hook input has no session_id");
    };
    if !is_plain_token(session) {
        bail!(
            "session id {session:?} is not a plain token, so herdr could not type it back safely"
        );
    }
    Ok(Some(session.to_string()))
}

/// A value herdr can type into any shell unquoted: ASCII letters, digits, `_`, `.` and `-`,
/// not starting with `-` (it would read as an option).
pub fn is_plain_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SESSION_ID_LEN
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// The command herdr types to resume `session_id`: `cc-rotate --resume <id>`.
pub fn resume_argv(session_id: &str) -> Result<Vec<String>> {
    if !is_plain_token(session_id) {
        bail!("session id {session_id:?} is not a plain token");
    }
    let argv = vec![
        LAUNCHER.to_string(),
        "--resume".to_string(),
        session_id.to_string(),
    ];
    validate_resume_argv(&argv).map_err(anyhow::Error::msg)?;
    Ok(argv)
}

/// herdr's own rules for a reported command, so a report herdr would reject is never sent.
/// herdr types the command into the pane's default shell, quoting POSIX-style.
pub fn validate_resume_argv(argv: &[String]) -> Result<(), String> {
    let Some(command) = argv.first() else {
        return Err("the resume command is empty".into());
    };
    if argv.len() > MAX_RESUME_ARGS {
        return Err(format!(
            "the resume command has more than {MAX_RESUME_ARGS} arguments"
        ));
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_RESUME_ARGV_BYTES {
        return Err(format!(
            "the resume command is longer than {MAX_RESUME_ARGV_BYTES} bytes"
        ));
    }
    if argv.iter().any(|a| a.chars().any(char::is_control)) {
        return Err("the resume command contains control characters".into());
    }
    if argv.iter().any(|a| a.contains('\'')) {
        return Err("the resume command contains an apostrophe".into());
    }
    let plain_command = !command.is_empty()
        && !command.starts_with('-')
        && command
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if !plain_command {
        return Err("the resume command must start with a plain command name, not a path".into());
    }
    Ok(())
}

/// The arguments for `herdr` that report `cc-rotate --resume <session_id>` for `pane_id`.
/// `seq` orders cc-rotate's reports for the pane: herdr ignores one older than the last it took.
pub fn report_args(pane_id: &str, session_id: &str, seq: u64) -> Result<Vec<String>> {
    if pane_id.is_empty() || pane_id.starts_with('-') || pane_id.chars().any(char::is_control) {
        bail!("{pane_id:?} is not a herdr pane id");
    }
    let mut args: Vec<String> = [
        "pane",
        "report-agent-session",
        pane_id,
        "--source",
        REPORT_SOURCE,
        "--agent",
        AGENT,
        "--seq",
        &seq.to_string(),
        "--",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(resume_argv(session_id)?);
    Ok(args)
}

/// Whether herdr refused the report only because it has not seen `claude` running in the pane
/// yet, which resolves itself moments after Claude Code starts.
pub fn is_not_yet_accepted(stderr: &str) -> bool {
    stderr.contains("resume_not_accepted")
}

/// Whether `settings` (a profile's `settings.json`) runs `cc-rotate-ctl hook session-start` on
/// `SessionStart`.
pub fn session_start_hook_installed(settings: &Value) -> bool {
    settings
        .pointer("/hooks/SessionStart")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|c| c.contains("cc-rotate-ctl") && c.contains(SESSION_START_HOOK_ARGS))
}

/// How herdr was found on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// This process runs inside a herdr pane.
    InPane(PathBuf),
    /// herdr is on `PATH`.
    OnPath(PathBuf),
}

impl Detected {
    pub fn bin(&self) -> &Path {
        match self {
            Detected::InPane(p) | Detected::OnPath(p) => p,
        }
    }
}

/// herdr, if it is installed or this process runs in one of its panes.
pub fn detect() -> Option<Detected> {
    if let Some(pane) = pane_from_env() {
        let bin = PathBuf::from(&pane.bin);
        let bin = if bin.components().count() > 1 {
            bin
        } else {
            crate::paths::find_program(&bin.to_string_lossy()).unwrap_or(bin)
        };
        return Some(Detected::InPane(bin));
    }
    crate::paths::find_program("herdr").map(Detected::OnPath)
}

/// `herdr --version`, for example `0.9.4-preview.2026-10-14-fbd2ae23927f`.
pub fn version(bin: &Path) -> Option<String> {
    let out = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    let v = text.strip_prefix("herdr").unwrap_or(text).trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// The numeric `major.minor.patch` at the start of a herdr version string.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+', ' ']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn pane_needs_herdr_env_and_a_pane_id() {
        assert_eq!(pane_from(env(&[])), None);
        assert_eq!(pane_from(env(&[("HERDR_PANE_ID", "w1:p2")])), None);
        assert_eq!(
            pane_from(env(&[("HERDR_ENV", "0"), ("HERDR_PANE_ID", "w1:p2")])),
            None
        );
        assert_eq!(
            pane_from(env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", " ")])),
            None
        );
        assert_eq!(
            pane_from(env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p2")])),
            Some(Pane {
                id: "w1:p2".into(),
                bin: "herdr".into()
            })
        );
        assert_eq!(
            pane_from(env(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", "w1:p2"),
                ("HERDR_BIN_PATH", "/opt/herdr/bin/herdr")
            ]))
            .map(|p| p.bin),
            Some("/opt/herdr/bin/herdr".into())
        );
    }

    #[test]
    fn hook_input_reports_main_sessions_only() {
        let main = r#"{"session_id":"0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70","transcript_path":"C:\\x\\s.jsonl","cwd":"C:\\p","hook_event_name":"SessionStart","source":"startup"}"#;
        assert_eq!(
            session_to_report(main).unwrap().as_deref(),
            Some("0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70")
        );

        // `claude --agent reviewer` is a main session with an agent_type.
        let named_agent =
            r#"{"session_id":"abc-1","hook_event_name":"SessionStart","agent_type":"reviewer"}"#;
        assert_eq!(
            session_to_report(named_agent).unwrap().as_deref(),
            Some("abc-1")
        );

        let subagent = r#"{"session_id":"abc-1","hook_event_name":"SessionStart","agent_id":"a1","agent_type":"Explore"}"#;
        assert_eq!(session_to_report(subagent).unwrap(), None);
        let blank_agent_id = r#"{"session_id":"abc-1","agent_id":""}"#;
        assert_eq!(
            session_to_report(blank_agent_id).unwrap().as_deref(),
            Some("abc-1")
        );

        let other_event = r#"{"session_id":"abc-1","hook_event_name":"SessionEnd"}"#;
        assert_eq!(session_to_report(other_event).unwrap(), None);
    }

    #[test]
    fn hook_input_refuses_what_herdr_cannot_type_back() {
        assert!(session_to_report("").is_err());
        assert!(session_to_report("not json").is_err());
        assert!(session_to_report(r#"{"hook_event_name":"SessionStart"}"#).is_err());
        for bad in [
            "", "-rf", "it's", "a b", "a;b", "a$(x)", "a\nb", "C:\\x", "a/b", "é",
        ] {
            let input = serde_json::json!({ "session_id": bad }).to_string();
            assert!(session_to_report(&input).is_err(), "accepted {bad:?}");
            assert!(!is_plain_token(bad), "plain: {bad:?}");
        }
        assert!(!is_plain_token(&"a".repeat(MAX_SESSION_ID_LEN + 1)));
        assert!(is_plain_token("0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70"));
        assert!(is_plain_token("session_1.2"));
    }

    #[test]
    fn resume_command_passes_herdr_validation() {
        let argv = resume_argv("0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70").unwrap();
        assert_eq!(
            argv,
            [
                "cc-rotate",
                "--resume",
                "0b6f2c1e-6d0e-4b7a-9a52-1c3f4d5e6f70"
            ]
        );
        assert_eq!(validate_resume_argv(&argv), Ok(()));
        assert!(resume_argv("it's").is_err());
        assert!(resume_argv("--continue").is_err());
    }

    #[test]
    fn validation_matches_herdr_rules() {
        let v = |args: &[&str]| {
            validate_resume_argv(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        };
        assert!(v(&[]).is_err());
        assert!(v(&["/usr/local/bin/cc-rotate", "--resume", "x"]).is_err());
        assert!(v(&[r"C:\bin\cc-rotate.exe"]).is_err());
        assert!(v(&["-x"]).is_err());
        assert!(v(&["cc-rotate", "it's"]).is_err());
        assert!(v(&["cc-rotate", "a\tb"]).is_err());
        assert!(v(&["cc-rotate"; MAX_RESUME_ARGS + 1]).is_err());
        assert!(v(&["cc-rotate", &"x".repeat(MAX_RESUME_ARGV_BYTES)]).is_err());
        assert!(
            v(&["cc-rotate", "--resume", "a b"]).is_ok(),
            "herdr quotes spaces"
        );
        assert!(v(&["cc-rotate.v2_beta"]).is_ok());
    }

    #[test]
    fn report_args_are_exact() {
        assert_eq!(
            report_args("w1:p2", "abc-1", 1_761_300_000_123).unwrap(),
            [
                "pane",
                "report-agent-session",
                "w1:p2",
                "--source",
                "cc-rotate",
                "--agent",
                "claude",
                "--seq",
                "1761300000123",
                "--",
                "cc-rotate",
                "--resume",
                "abc-1"
            ]
        );
        assert!(report_args("--source", "abc-1", 1).is_err());
        assert!(report_args("", "abc-1", 1).is_err());
        assert!(report_args("w1:p2", "a b", 1).is_err());
    }

    #[test]
    fn not_accepted_is_recognized_in_herdr_error_output() {
        let stderr = r#"{"id":"cli:request","error":{"code":"resume_not_accepted","message":"resume_argv requires the reporter to hold the pane; report its state with pane.report_agent first"}}"#;
        assert!(is_not_yet_accepted(stderr));
        assert!(!is_not_yet_accepted(
            r#"{"id":"cli:request","error":{"code":"pane_not_found","message":"pane w9:p9 not found"}}"#
        ));
    }

    #[test]
    fn hook_detection_in_settings() {
        let herdr_only = serde_json::json!({"hooks":{"SessionStart":[{"matcher":SESSION_START_MATCHER,"hooks":[{"type":"command","command":"powershell -File C:/x/herdr-agent-state.ps1 session"}]}]}});
        assert!(!session_start_hook_installed(&herdr_only));
        assert!(!session_start_hook_installed(&serde_json::json!({})));
        let both = serde_json::json!({"hooks":{"SessionStart":[
            {"hooks":[{"type":"command","command":"powershell -File C:/x/herdr-agent-state.ps1 session"}]},
            {"matcher":SESSION_START_MATCHER,"hooks":[{"type":"command","command":"C:/bin/cc-rotate-ctl.exe hook session-start","timeout":10}]}
        ]}});
        assert!(session_start_hook_installed(&both));
    }

    #[test]
    fn versions_parse() {
        assert_eq!(
            parse_version("0.9.4-preview.2026-10-14-fbd2ae23927f"),
            Some((0, 9, 4))
        );
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.10"), Some((0, 10, 0)));
        assert_eq!(parse_version("dev"), None);
        assert!(parse_version("0.9.1").unwrap() < MIN_VERSION);
        assert!(parse_version("0.9.2").unwrap() >= MIN_VERSION);
    }
}
