//! Reading and writing Claude Code's `.credentials.json`.
//!
//! The file holds `claudeAiOauth` (the account login) next to other keys such as `mcpOAuth`
//! (MCP server logins). cc-rotate only ever replaces `claudeAiOauth`; every other key is
//! preserved, in order. Claude Code writes the file as compact JSON, and so do we.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const OAUTH_KEY: &str = "claudeAiOauth";

/// Read and parse the file. `Ok(None)` when it does not exist.
pub fn read_document(path: &Path) -> Result<Option<Map<String, Value>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_str::<Value>(&text)
        .with_context(|| format!("parsing {}", path.display()))?
    {
        Value::Object(map) => Ok(Some(map)),
        _ => bail!("{} is not a JSON object", path.display()),
    }
}

/// The `claudeAiOauth` block, if the file and the key exist.
pub fn read_oauth(path: &Path) -> Result<Option<Value>> {
    Ok(read_document(path)?
        .and_then(|mut doc| doc.remove(OAUTH_KEY))
        .filter(|v| v.is_object()))
}

/// Replace `claudeAiOauth` and keep every other key unchanged.
pub fn write_oauth(path: &Path, oauth: &Value) -> Result<()> {
    if !oauth.is_object() {
        bail!("refusing to write a non-object login block");
    }
    let mut doc = read_document(path)?.unwrap_or_default();
    doc.insert(OAUTH_KEY.to_string(), oauth.clone());
    let bytes = serde_json::to_vec(&Value::Object(doc))?;
    atomic_write(path, &bytes)
}

pub fn refresh_token(oauth: &Value) -> Option<&str> {
    oauth.get("refreshToken").and_then(Value::as_str)
}

/// Claude Code blanks the tokens when Anthropic rejects the refresh token (`invalid_grant`).
pub fn is_dead(oauth: &Value) -> bool {
    matches!(refresh_token(oauth), None | Some(""))
}

/// Short SHA-256 fingerprint so logins can be compared without decrypting anything.
pub fn fingerprint(token: &str) -> String {
    hex::encode(&Sha256::digest(token.as_bytes())[..12])
}

pub fn oauth_fingerprint(oauth: &Value) -> Option<String> {
    refresh_token(oauth)
        .filter(|t| !t.is_empty())
        .map(fingerprint)
}

pub fn mtime_ms(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

/// Write to a temp file in the same directory, then rename over the target.
///
/// On Unix the file is created readable only by its owner, as Claude Code creates
/// `.credentials.json` (mode 0600). On Windows a rename can briefly fail while another process
/// has the target open, so retry.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("target has no parent directory")?;
    std::fs::create_dir_all(dir)?;
    remove_stale_temps(path);
    let tmp = temp_sibling(path);
    if let Err(e) = write_new_private(&tmp, bytes) {
        // A half-written copy of a login must not stay behind.
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", tmp.display()));
    }
    let mut last = None;
    for attempt in 0..40 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == ErrorKind::PermissionDenied || e.raw_os_error() == Some(32) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(25 + attempt * 5));
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e).with_context(|| format!("replacing {}", path.display()));
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(last.unwrap()).with_context(|| format!("replacing {} (file stayed busy)", path.display()))
}

/// Create a new file only its owner can read (mode 0600 on Unix; on Windows it inherits the
/// directory's ACL, which under the user profile allows the user, SYSTEM and Administrators).
pub(crate) fn write_new_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// Temp files of earlier writes to `path` that a crash or power loss left behind, possibly
/// holding a login: removed once they are older than any write could take.
fn remove_stale_temps(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return;
    };
    let prefix = format!(".{}.cc-rotate-", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.starts_with(&prefix) || !file.ends_with(".tmp") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > Duration::from_secs(600));
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn temp_sibling(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(
        ".{name}.cc-rotate-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A temp copy a crash left behind is removed by the next write, once it is old.
    #[test]
    fn stale_temp_files_of_a_target_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(".credentials.json");
        // Named as `temp_sibling` names them: a dot, then the target's own name.
        let old = dir.path().join("..credentials.json.cc-rotate-old.tmp");
        let recent = dir.path().join("..credentials.json.cc-rotate-new.tmp");
        let other = dir.path().join(".other.json.cc-rotate-old.tmp");
        for path in [&old, &recent, &other] {
            std::fs::write(path, "{}").unwrap();
        }
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        for path in [&old, &other] {
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        atomic_write(&target, b"{}").unwrap();
        assert!(!old.exists());
        assert!(recent.exists(), "may belong to a write in progress");
        assert!(other.exists(), "another file's");
    }

    #[test]
    fn write_preserves_other_keys_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        let original = r#"{"mcpOAuth":{"srv|1":{"accessToken":"m"}},"claudeAiOauth":{"refreshToken":"old"},"zeta":1}"#;
        std::fs::write(&path, original).unwrap();

        write_oauth(&path, &json!({"refreshToken": "new", "accessToken": "a"})).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            r#"{"mcpOAuth":{"srv|1":{"accessToken":"m"}},"claudeAiOauth":{"refreshToken":"new","accessToken":"a"},"zeta":1}"#
        );
    }

    #[test]
    fn write_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        write_oauth(&path, &json!({"refreshToken": "r"})).unwrap();
        assert_eq!(read_oauth(&path).unwrap().unwrap()["refreshToken"], "r");
    }

    #[test]
    fn dead_and_fingerprint() {
        assert!(is_dead(&json!({"refreshToken": ""})));
        assert!(is_dead(&json!({})));
        assert!(!is_dead(&json!({"refreshToken": "x"})));
        assert_eq!(fingerprint("abc"), fingerprint("abc"));
        assert_ne!(fingerprint("abc"), fingerprint("abd"));
        assert_eq!(fingerprint("abc").len(), 24);
    }

    #[cfg(unix)]
    #[test]
    fn written_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".credentials.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_oauth(&path, &json!({"refreshToken": "r"})).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn missing_file_reads_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_oauth(&dir.path().join("nope.json")).unwrap().is_none());
    }
}
