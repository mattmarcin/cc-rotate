//! Where Claude Code keeps a profile's credentials, and reading and writing them there.
//!
//! The credentials are one JSON document: `claudeAiOauth` (the account login) next to other keys
//! such as `mcpOAuth`. On Windows and Linux Claude Code keeps it in
//! `<CLAUDE_CONFIG_DIR>/.credentials.json`. On macOS it keeps it in a login Keychain item: it
//! reads the Keychain first and the file only when the Keychain has nothing or cannot be used
//! (locked to the session, as over SSH), and it does not write the file while the Keychain is
//! locked (`Credentials::complete`). This module mirrors that, so cc-rotate always reads what
//! Claude Code reads and writes where it reads.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::credfile::{self, OAUTH_KEY};

/// Where a profile's credentials document currently lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    File,
    Keychain,
}

/// A profile's credentials, as Claude Code stores them on this platform.
#[derive(Debug, Clone)]
pub struct Credentials {
    file: PathBuf,
    keychain: Option<Keychain>,
}

impl Credentials {
    /// The credentials of the profile Claude Code runs with `CLAUDE_CONFIG_DIR=<config_dir>`.
    pub fn for_profile(config_dir: &Path) -> Credentials {
        Self::new(config_dir, false)
    }

    /// `default` is the profile Claude Code uses when `CLAUDE_CONFIG_DIR` is unset (`~/.claude`),
    /// whose Keychain item has no per-directory suffix.
    pub fn new(config_dir: &Path, default: bool) -> Credentials {
        let keychain =
            cfg!(target_os = "macos").then(|| Keychain::for_profile(config_dir, default));
        Credentials {
            file: config_dir.join(".credentials.json"),
            keychain,
        }
    }

    pub fn file(&self) -> &Path {
        &self.file
    }

    pub fn keychain(&self) -> Option<&Keychain> {
        self.keychain.as_ref()
    }

    /// Read the document the way Claude Code does: the Keychain item if it holds one, else the
    /// file. `Ok(None)` when neither has anything.
    pub fn read_raw(&self) -> Result<Option<(Location, Vec<u8>)>> {
        if let Some(keychain) = &self.keychain
            && let Some(text) = keychain.read()?
        {
            return Ok(Some((Location::Keychain, text.into_bytes())));
        }
        match std::fs::read(&self.file) {
            Ok(bytes) => Ok(Some((Location::File, bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.file.display())),
        }
    }

    pub fn location(&self) -> Result<Option<Location>> {
        Ok(self.read_raw()?.map(|(location, _)| location))
    }

    pub fn read_document(&self) -> Result<Option<(Location, Map<String, Value>)>> {
        let Some((location, bytes)) = self.read_raw()? else {
            return Ok(None);
        };
        Ok(Some((
            location,
            parse_document(&bytes, &self.describe(location))?,
        )))
    }

    /// The `claudeAiOauth` block, if there is one.
    pub fn read_oauth(&self) -> Result<Option<Value>> {
        Ok(self
            .read_document()?
            .and_then(|(_, mut doc)| doc.remove(OAUTH_KEY))
            .filter(Value::is_object))
    }

    /// Replace `claudeAiOauth` where the document lives now, keeping every other key.
    /// A profile with no credentials anywhere gets the file, which Claude Code reads when the
    /// Keychain has nothing (and moves into the Keychain itself on its next write). Callers check
    /// `complete` first: in a session the Keychain is locked to, the document's place is unknown.
    pub fn write_oauth(&self, oauth: &Value) -> Result<()> {
        if !oauth.is_object() {
            bail!("refusing to write a non-object login block");
        }
        match (self.read_document()?, &self.keychain) {
            (Some((Location::Keychain, mut doc)), Some(keychain)) => {
                doc.insert(OAUTH_KEY.to_string(), oauth.clone());
                keychain.write(&serde_json::to_string(&Value::Object(doc))?)
            }
            _ => credfile::write_oauth(&self.file, oauth),
        }
    }

    /// Changes whenever the credentials change: a fingerprint of the Keychain item or the file.
    /// Not the file's modification time: Windows stamps it from a clock that advances only every
    /// 15.6 ms or so, so a renewal right after a swap's write could keep the swap's time.
    pub fn change_marker(&self) -> Result<Option<String>> {
        let Some((location, bytes)) = self.read_raw()? else {
            return Ok(None);
        };
        let digest = Sha256::digest(&bytes);
        let kind = match location {
            Location::Keychain => "keychain",
            Location::File => "file",
        };
        Ok(Some(format!("{kind}:{}", hex::encode(&digest[..12]))))
    }

    /// Whether what `read_raw` returns is all there is: false on macOS while the Keychain cannot
    /// be used (locked, or no GUI session), when the file fallback may be stale or missing while
    /// Claude Code keeps the live login in the Keychain.
    pub fn complete(&self) -> Result<bool> {
        match &self.keychain {
            Some(keychain) => keychain.usable(),
            None => Ok(true),
        }
    }

    /// This session cannot use the macOS Keychain, asking `security` for at most `timeout`.
    /// macOS gives the login Keychain only to programs started from the desktop session that
    /// unlocked it: one started over SSH, or by a program that was (a herdr or tmux server
    /// started from an SSH connection), gets "user interaction is not allowed". Claude Code there
    /// cannot read a login in the Keychain, and cannot save a renewed one anywhere, so a login it
    /// renews is lost with the refresh token it replaced. Always false off macOS.
    pub fn keychain_locked(&self, timeout: Duration) -> Result<bool> {
        match &self.keychain {
            Some(keychain) => Ok(!keychain.usable_within(timeout)?),
            None => Ok(false),
        }
    }

    /// Delete the Keychain item, if any (for staging profiles, which are thrown away).
    pub fn delete_keychain_item(&self) -> Result<()> {
        match &self.keychain {
            Some(keychain) => keychain.delete(),
            None => Ok(()),
        }
    }

    pub fn describe(&self, location: Location) -> String {
        match (location, &self.keychain) {
            (Location::Keychain, Some(k)) => format!("the macOS Keychain item \"{}\"", k.service),
            _ => self.file.display().to_string(),
        }
    }
}

fn parse_document(bytes: &[u8], what: &str) -> Result<Map<String, Value>> {
    let text = String::from_utf8_lossy(bytes);
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text).with_context(|| format!("parsing {what}"))? {
        Value::Object(map) => Ok(map),
        _ => bail!("{what} is not a JSON object"),
    }
}

/// Claude Code's macOS Keychain item: a generic password with this service and account,
/// read and written through the `security` tool exactly as Claude Code does. Items created
/// by `security` trust `security`, so reading them never prompts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keychain {
    pub service: String,
    pub account: String,
    /// The `security` tool; tests substitute a stand-in.
    program: PathBuf,
}

const SECURITY: &str = "/usr/bin/security";
/// `security` exit codes Claude Code relies on.
const ITEM_NOT_FOUND: i32 = 44;
const INTERACTION_NOT_ALLOWED: i32 = 36;
/// Claude Code feeds `security -i` one command line on stdin when it is at most this long,
/// and passes the arguments directly otherwise.
const INTERACTIVE_LIMIT: usize = 4032;

impl Keychain {
    pub fn for_profile(config_dir: &Path, default: bool) -> Keychain {
        Keychain {
            service: service_name(&config_dir.to_string_lossy(), default),
            account: keychain_account(std::env::var("USER").ok().filter(|u| !u.is_empty())),
            program: PathBuf::from(SECURITY),
        }
    }

    /// The item's text. `Ok(None)` when there is no item, or when the Keychain cannot be used
    /// right now (locked, or no GUI session), which is when Claude Code falls back to the file.
    pub fn read(&self) -> Result<Option<String>> {
        let (code, out) = self.security(
            &[
                "find-generic-password",
                "-a",
                &self.account,
                "-w",
                "-s",
                &self.service,
            ],
            None,
        )?;
        match code {
            0 => Ok(Some(decode_password(out.trim_end_matches(['\r', '\n'])))),
            ITEM_NOT_FOUND | INTERACTION_NOT_ALLOWED => Ok(None),
            other => bail!(
                "`security find-generic-password` failed (exit {other}) for {}",
                self.service
            ),
        }
    }

    /// Replace the item's text, then read it back to confirm the Keychain holds it.
    pub fn write(&self, text: &str) -> Result<()> {
        let line = add_command(&self.account, &self.service, text);
        let (code, _) = if line.len() <= INTERACTIVE_LIMIT {
            // On stdin the token stays out of the process list.
            self.security(&["-i"], Some(&line))?
        } else {
            let hex = hex::encode(text.as_bytes());
            self.security(
                &[
                    "add-generic-password",
                    "-U",
                    "-a",
                    &self.account,
                    "-s",
                    &self.service,
                    "-X",
                    &hex,
                ],
                None,
            )?
        };
        if code != 0 {
            bail!(
                "`security add-generic-password` failed (exit {code}) for {}",
                self.service
            );
        }
        let back = self.read()?;
        let same = back.as_deref().is_some_and(|b| {
            serde_json::from_str::<Value>(b).ok() == serde_json::from_str::<Value>(text).ok()
        });
        if !same {
            bail!(
                "the Keychain item {} did not hold the new login after writing it",
                self.service
            );
        }
        Ok(())
    }

    /// Whether this session can use the Keychain now (an item or not); false while it is locked
    /// to the session, as over SSH. Asks for the default keychain's settings, which answers 36
    /// then whether or not the profile's item exists; reading a missing item answers 44 either
    /// way, and a search of an item's attributes succeeds on a locked Keychain too.
    pub fn usable(&self) -> Result<bool> {
        self.usable_within(SECURITY_TIMEOUT)
    }

    /// `usable`, asking `security` for at most `timeout` (a locked Keychain in a desktop
    /// session shows an unlock dialog and waits).
    pub fn usable_within(&self, timeout: Duration) -> Result<bool> {
        let (code, _) = run_with_timeout(&self.program, &["show-keychain-info"], None, timeout)?;
        Ok(code != INTERACTION_NOT_ALLOWED)
    }

    pub fn delete(&self) -> Result<()> {
        let (code, _) = self.security(
            &[
                "delete-generic-password",
                "-a",
                &self.account,
                "-s",
                &self.service,
            ],
            None,
        )?;
        match code {
            0 | ITEM_NOT_FOUND => Ok(()),
            other => bail!(
                "`security delete-generic-password` failed (exit {other}) for {}",
                self.service
            ),
        }
    }
}

/// `Claude Code-credentials` plus, for any profile other than the default one, `-` and the
/// first 8 hex digits of the SHA-256 of the NFC-normalized `CLAUDE_CONFIG_DIR` string. Claude
/// Code hashes the variable as given, so it must be set to exactly this path.
pub fn service_name(config_dir: &str, default: bool) -> String {
    use unicode_normalization::UnicodeNormalization;
    if default {
        return "Claude Code-credentials".to_string();
    }
    let normalized: String = config_dir.nfc().collect();
    let digest = hex::encode(Sha256::digest(normalized.as_bytes()));
    format!("Claude Code-credentials-{}", &digest[..8])
}

/// Claude Code uses `$USER` (else the OS user name) when it consists of letters, digits, `.`,
/// `_` and `-`, and `claude-code-user` otherwise.
pub fn keychain_account(user: Option<String>) -> String {
    let user = user.or_else(os_user_name).unwrap_or_default();
    let valid = !user.is_empty()
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        user
    } else {
        "claude-code-user".to_string()
    }
}

fn os_user_name() -> Option<String> {
    let out = Command::new("id")
        .arg("-un")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !name.is_empty()).then_some(name)
}

/// The `security -i` command line Claude Code writes: the JSON as hex, so no quoting is needed.
/// The account and service only contain characters that need no escaping inside quotes.
fn add_command(account: &str, service: &str, text: &str) -> String {
    format!(
        "add-generic-password -U -a \"{account}\" -s \"{service}\" -X \"{}\"\n",
        hex::encode(text.as_bytes())
    )
}

/// `security -w` prints the password as text, or as hex when it is not printable.
fn decode_password(out: &str) -> String {
    if serde_json::from_str::<Value>(out).is_err()
        && let Ok(bytes) = hex::decode(out)
        && let Ok(text) = String::from_utf8(bytes)
    {
        return text;
    }
    out.to_string()
}

impl Keychain {
    /// Run `security` with a time limit (a Keychain dialog would otherwise block forever).
    fn security(&self, args: &[&str], stdin: Option<&str>) -> Result<(i32, String)> {
        run_with_timeout(&self.program, args, stdin, SECURITY_TIMEOUT)
    }
}

/// How long a `security` call may take before it is given up.
const SECURITY_TIMEOUT: Duration = Duration::from_secs(10);

fn run_with_timeout(
    program: &Path,
    args: &[&str],
    stdin: Option<&str>,
    timeout: Duration,
) -> Result<(i32, String)> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("running the macOS `security` tool")?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(text.as_bytes())?;
    }
    let mut stdout = child.stdout.take().context("no stdout from `security`")?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!("`security {}` did not finish in time", args[0]));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let out = reader.join().unwrap_or_default();
    Ok((status.code().unwrap_or(-1), out))
}

/// Whether Claude Code on Windows would keep this profile's login in the Windows Credential
/// Manager instead of `.credentials.json`. Claude Code 2.1.310 does when
/// `CLAUDE_CODE_FORCE_WINDOWS_CREDMAN=1`, or when the server-side flag `tengu_windows_credman`
/// is cached as on in the profile's `.config.json` (when that file has content) or else its
/// `.claude.json`. cc-rotate's swaps would then no longer reach running sessions.
pub fn windows_credman_enabled(config_dir: &Path, env_force: Option<&str>) -> bool {
    if env_force == Some("1") {
        return true;
    }
    let legacy = std::fs::read_to_string(config_dir.join(".config.json")).unwrap_or_default();
    let text = if legacy.is_empty() {
        std::fs::read_to_string(config_dir.join(".claude.json")).unwrap_or_default()
    } else {
        legacy
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.pointer("/cachedGrowthBookFeatures/tengu_windows_credman")
                .and_then(Value::as_bool)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Expected values computed with Claude Code's own expression in Node:
    /// `"Claude Code-credentials-" + sha256(dir.normalize("NFC")).hex.substring(0, 8)`.
    #[test]
    fn service_names_match_claude_code() {
        assert_eq!(
            service_name("/Users/test/.claude-rotate", false),
            "Claude Code-credentials-a7cdcad9"
        );
        assert_eq!(
            service_name("/Users/Ren\u{e9}/.claude-rotate", false),
            "Claude Code-credentials-955e2ecc"
        );
        assert_eq!(
            service_name("/Users/Rene\u{301}/.claude-rotate", false),
            "Claude Code-credentials-955e2ecc",
            "decomposed accents hash like composed ones"
        );
        assert_eq!(
            service_name("/Users/test/.claude", true),
            "Claude Code-credentials"
        );
    }

    #[test]
    fn keychain_account_follows_claude_code() {
        assert_eq!(keychain_account(Some("jane.doe".into())), "jane.doe");
        assert_eq!(
            keychain_account(Some("jane doe".into())),
            "claude-code-user"
        );
        assert_eq!(
            keychain_account(Some("ren\u{e9}".into())),
            "claude-code-user"
        );
    }

    #[test]
    fn add_command_hex_encodes_the_document() {
        let line = add_command("jane", "Claude Code-credentials-a7cdcad9", "{\"a\":1}");
        assert_eq!(
            line,
            "add-generic-password -U -a \"jane\" -s \"Claude Code-credentials-a7cdcad9\" -X \"7b2261223a317d\"\n"
        );
    }

    #[test]
    fn password_output_as_text_or_hex() {
        assert_eq!(decode_password("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(decode_password("7b2261223a317d"), "{\"a\":1}");
    }

    #[test]
    fn file_backend_preserves_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let creds = Credentials {
            file: dir.path().join(".credentials.json"),
            keychain: None,
        };
        assert!(creds.read_oauth().unwrap().is_none());
        assert!(creds.change_marker().unwrap().is_none());
        std::fs::write(
            creds.file(),
            r#"{"mcpOAuth":{"s":1},"claudeAiOauth":{"refreshToken":"old"}}"#,
        )
        .unwrap();
        assert_eq!(creds.location().unwrap(), Some(Location::File));
        creds.write_oauth(&json!({"refreshToken": "new"})).unwrap();
        let (_, doc) = creds.read_document().unwrap().unwrap();
        assert_eq!(doc["mcpOAuth"], json!({"s": 1}));
        assert_eq!(creds.read_oauth().unwrap().unwrap()["refreshToken"], "new");
        assert!(creds.change_marker().unwrap().is_some());
    }

    /// Two logins written within one tick of the clock that stamps file times (about 15.6 ms on
    /// Windows) have the same modification time; the marker still tells them apart.
    #[test]
    fn the_marker_changes_with_the_login_not_the_file_time() {
        let dir = tempfile::tempdir().unwrap();
        let creds = Credentials {
            file: dir.path().join(".credentials.json"),
            keychain: None,
        };
        let at = std::time::SystemTime::now();
        let write = |token: &str| {
            creds.write_oauth(&json!({"refreshToken": token})).unwrap();
            std::fs::File::options()
                .write(true)
                .open(creds.file())
                .unwrap()
                .set_modified(at)
                .unwrap();
            creds.change_marker().unwrap().unwrap()
        };
        let first = write("one");
        assert_ne!(write("two"), first);
        assert_eq!(write("one"), first, "the same login, the same marker");
    }

    #[test]
    fn credman_flag_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!windows_credman_enabled(dir.path(), None));
        assert!(windows_credman_enabled(dir.path(), Some("1")));
        std::fs::write(
            dir.path().join(".claude.json"),
            r#"{"cachedGrowthBookFeatures":{"tengu_windows_credman":true}}"#,
        )
        .unwrap();
        assert!(windows_credman_enabled(dir.path(), None));
        // A non-empty legacy .config.json is read instead of .claude.json.
        std::fs::write(
            dir.path().join(".config.json"),
            r#"{"cachedGrowthBookFeatures":{"tengu_windows_credman":false}}"#,
        )
        .unwrap();
        assert!(!windows_credman_enabled(dir.path(), None));
    }

    /// A stand-in for `/usr/bin/security` that keeps each item as a file of hex, which is also
    /// how the real tool prints a password it considers binary.
    #[cfg(unix)]
    fn fake_security(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let items = dir.join("items");
        std::fs::create_dir_all(&items).unwrap();
        let script = format!(
            r#"#!/bin/sh
items='{}'
run() {{
  cmd="$1"; shift
  service=""; hex=""; password=0
  while [ $# -gt 0 ]; do
    case "$1" in
      -s) service="$2"; shift 2 ;;
      -X) hex="$2"; shift 2 ;;
      -a) shift 2 ;;
      -w) password=1; shift ;;
      *) shift ;;
    esac
  done
  case "$cmd" in
    find-generic-password)
      [ -f "$items/$service" ] || exit 44
      [ "$password" = 1 ] && [ -f "$items/../locked" ] && exit 36
      [ "$password" = 1 ] && cat "$items/$service"; echo ;;
    show-keychain-info) [ -f "$items/../locked" ] && exit 36; exit 0 ;;
    add-generic-password) printf '%s' "$hex" > "$items/$service" ;;
    delete-generic-password) [ -f "$items/$service" ] || exit 44; rm "$items/$service" ;;
    *) exit 1 ;;
  esac
}}
if [ "$1" = "-i" ]; then
  read -r line
  eval "set -- $line"
fi
run "$@"
"#,
            items.display()
        );
        let path = dir.join("security");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// The Keychain path end to end against the stand-in: a profile with nothing gets the file,
    /// an item Claude Code moved into the Keychain wins over the file, writes go to the item and
    /// keep its other keys, the change marker follows the item, and staging items are deleted.
    #[cfg(unix)]
    #[test]
    fn keychain_backend_mirrors_claude_code() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain {
            service: service_name(&dir.path().to_string_lossy(), false),
            account: "jane".into(),
            program: fake_security(dir.path()),
        };
        let creds = Credentials {
            file: dir.path().join(".credentials.json"),
            keychain: Some(keychain.clone()),
        };
        assert_eq!(creds.location().unwrap(), None);
        creds.write_oauth(&json!({"refreshToken": "r1"})).unwrap();
        assert_eq!(creds.location().unwrap(), Some(Location::File));

        // Claude Code saves to the Keychain and removes the plaintext file.
        keychain
            .write(r#"{"mcpOAuth":{"s":1},"claudeAiOauth":{"refreshToken":"r2"}}"#)
            .unwrap();
        std::fs::remove_file(creds.file()).unwrap();
        assert_eq!(creds.location().unwrap(), Some(Location::Keychain));
        assert_eq!(creds.read_oauth().unwrap().unwrap()["refreshToken"], "r2");
        let before = creds.change_marker().unwrap().unwrap();
        assert!(before.starts_with("keychain:"));

        creds.write_oauth(&json!({"refreshToken": "r3"})).unwrap();
        assert!(
            !creds.file().exists(),
            "the write went to the Keychain item"
        );
        let (location, doc) = creds.read_document().unwrap().unwrap();
        assert_eq!(location, Location::Keychain);
        assert_eq!(doc["mcpOAuth"], json!({"s": 1}));
        assert_eq!(doc["claudeAiOauth"]["refreshToken"], "r3");
        assert_ne!(creds.change_marker().unwrap().unwrap(), before);

        // A document too long for one `security -i` line goes through the arguments instead.
        let big = "x".repeat(INTERACTIVE_LIMIT);
        creds
            .write_oauth(&json!({"refreshToken": "r4", "pad": big}))
            .unwrap();
        assert_eq!(creds.read_oauth().unwrap().unwrap()["refreshToken"], "r4");

        creds.delete_keychain_item().unwrap();
        assert_eq!(creds.location().unwrap(), None);
        creds.delete_keychain_item().unwrap();
    }

    /// A session the Keychain is locked to (started over SSH) cannot use it, with or without an
    /// item, and reads no login from it.
    #[cfg(unix)]
    #[test]
    fn a_locked_keychain_is_told_apart() {
        let dir = tempfile::tempdir().unwrap();
        let keychain = Keychain {
            service: service_name(&dir.path().to_string_lossy(), false),
            account: "jane".into(),
            program: fake_security(dir.path()),
        };
        let creds = Credentials {
            file: dir.path().join(".credentials.json"),
            keychain: Some(keychain.clone()),
        };
        let lock = || std::fs::write(dir.path().join("locked"), "").unwrap();
        let unlock = || std::fs::remove_file(dir.path().join("locked")).unwrap();

        let locked = || creds.keychain_locked(Duration::from_secs(5)).unwrap();
        lock();
        assert!(locked(), "no item yet");
        unlock();
        assert!(!locked());
        keychain
            .write(r#"{"claudeAiOauth":{"refreshToken":"r1"}}"#)
            .unwrap();
        assert!(!locked());
        lock();
        assert!(locked());
        // What Claude Code then sees: no login.
        assert!(creds.read_oauth().unwrap().is_none());
        assert!(!creds.complete().unwrap());
    }

    /// On a Mac, an item that does not exist reads as absent (CI runs this on macOS).
    #[cfg(target_os = "macos")]
    #[test]
    fn missing_keychain_item_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let creds = Credentials::for_profile(dir.path());
        assert!(creds.keychain().unwrap().read().unwrap().is_none());
        assert_eq!(creds.location().unwrap(), None);
        creds.delete_keychain_item().unwrap();
    }
}
