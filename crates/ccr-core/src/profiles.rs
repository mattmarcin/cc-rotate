//! Finding the user's other Claude Code profiles, so the app can offer a fresh sign-in for each
//! account with its email and a label pre-filled.
//!
//! Read-only: cc-rotate never takes a login out of another profile. Copying it would leave two
//! holders of one login, and since Anthropic retires the old refresh token on every renewal,
//! whichever holder renewed second would be signed out. A fresh sign-in creates an independent
//! login instead, and the other profile keeps working.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;

use crate::paths::{self, Paths};
use crate::store::Store;
use crate::{credfile, profile_account};

#[derive(Debug, Clone, Serialize)]
pub struct FoundProfile {
    pub dir: PathBuf,
    /// Short name derived from the folder: `.claude-work` is `work`, `.claude` is `main`.
    pub name: String,
    pub email: Option<String>,
    pub account_uuid: Option<String>,
    pub subscription_type: Option<String>,
    /// The profile currently holds a usable login.
    pub signed_in: bool,
    /// Label of the pool account with the same identity, if it is already in the pool.
    pub in_pool_as: Option<String>,
}

/// The `.claude.json` that goes with a profile directory: the default profile (`~/.claude`)
/// keeps it in the home directory, every other profile keeps it inside.
pub fn config_file_for(dir: &Path) -> PathBuf {
    let default = paths::default_template_dir()
        .ok()
        .and_then(|d| dunce::canonicalize(d).ok());
    let is_default = dunce::canonicalize(dir)
        .ok()
        .is_some_and(|d| Some(d) == default);
    match paths::home_dir() {
        Ok(home) if is_default && home.join(".claude.json").is_file() => home.join(".claude.json"),
        _ => dir.join(".claude.json"),
    }
}

pub fn short_name(dir: &Path) -> String {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match folder.strip_prefix(".claude-") {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ if folder == ".claude" => "main".to_string(),
        _ => folder.trim_start_matches('.').to_string(),
    }
}

/// Every `.claude*` profile in the home directory, other than the rotation profile, that knows
/// which account it belongs to. Profiles naming the same account are listed once.
pub fn discover(store: &Store, rotation: &Paths) -> Result<Vec<FoundProfile>> {
    discover_in(store, rotation, &paths::home_dir()?)
}

pub fn discover_in(store: &Store, rotation: &Paths, home: &Path) -> Result<Vec<FoundProfile>> {
    let rotation_dir = dunce::canonicalize(&rotation.profile_dir).ok();
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(home)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".claude"))
        })
        .collect();
    dirs.sort();

    let mut found: Vec<FoundProfile> = Vec::new();
    for dir in dirs {
        if dunce::canonicalize(&dir).ok() == rotation_dir {
            continue;
        }
        let Some(ident) = profile_account::read(&config_file_for(&dir)).ok().flatten() else {
            continue;
        };
        if ident.email.is_none() && ident.account_uuid.is_none() {
            continue;
        }
        let oauth = credfile::read_oauth(&dir.join(".credentials.json"))
            .ok()
            .flatten();
        let in_pool = match (&ident.account_uuid, &ident.email) {
            (Some(uuid), _) if store.account_by_uuid(uuid)?.is_some() => {
                store.account_by_uuid(uuid)?
            }
            (_, Some(email)) => store.account_by_email(email)?,
            _ => None,
        };
        let profile = FoundProfile {
            name: short_name(&dir),
            email: ident.email,
            account_uuid: ident.account_uuid,
            subscription_type: oauth
                .as_ref()
                .and_then(|o| o.get("subscriptionType"))
                .and_then(Value::as_str)
                .map(str::to_string),
            signed_in: oauth.as_ref().is_some_and(|o| !credfile::is_dead(o)),
            in_pool_as: in_pool.map(|a| a.label),
            dir,
        };
        // Several profiles can name one account; list it once, preferring a signed-in profile.
        let same = found.iter().position(|f| {
            (profile.account_uuid.is_some() && f.account_uuid == profile.account_uuid)
                || (profile.email.is_some() && f.email == profile.email)
        });
        match same {
            Some(i) if profile.signed_in && !found[i].signed_in => found[i] = profile,
            Some(_) => {}
            None => found.push(profile),
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{AccountIdentity, AccountState};

    #[test]
    fn short_names() {
        assert_eq!(short_name(Path::new("/home/x/.claude-work")), "work");
        assert_eq!(short_name(Path::new("/home/x/.claude")), "main");
        assert_eq!(short_name(Path::new("/home/x/.claude-alt")), "alt");
    }

    #[test]
    fn discovers_profiles_without_touching_them() {
        let home = tempfile::tempdir().unwrap();
        let write = |name: &str, config: &str, creds: Option<&str>| {
            let dir = home.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".claude.json"), config).unwrap();
            if let Some(c) = creds {
                std::fs::write(dir.join(".credentials.json"), c).unwrap();
            }
        };
        write(
            ".claude-work",
            r#"{"oauthAccount":{"accountUuid":"u1","emailAddress":"one@x.test"}}"#,
            Some(r#"{"claudeAiOauth":{"refreshToken":"r","subscriptionType":"max"}}"#),
        );
        write(
            ".claude-home",
            r#"{"oauthAccount":{"accountUuid":"u2","emailAddress":"two@x.test"}}"#,
            None,
        );
        write(
            ".claude-copy",
            r#"{"oauthAccount":{"accountUuid":"u1","emailAddress":"one@x.test"}}"#,
            None,
        );
        write(".claude-empty", r#"{"numStartups":1}"#, None);
        write(
            ".claude-rotate",
            r#"{"oauthAccount":{"accountUuid":"u9","emailAddress":"pool@x.test"}}"#,
            None,
        );
        let before = std::fs::read(home.path().join(".claude-work/.credentials.json")).unwrap();

        let store = Store::open_in_memory().unwrap();
        let ident = AccountIdentity {
            account_uuid: Some("u2".into()),
            ..Default::default()
        };
        store
            .insert_account("home-acct", &ident, AccountState::Parked)
            .unwrap();
        let rotation = Paths::new(home.path().join("data"), home.path().join(".claude-rotate"));

        let found = discover_in(&store, &rotation, home.path()).unwrap();
        let names: Vec<_> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["work", "home"],
            "no duplicates, no unknown accounts, no rotation profile"
        );
        assert!(found[0].signed_in && found[0].in_pool_as.is_none());
        assert_eq!(found[0].subscription_type.as_deref(), Some("max"));
        assert!(!found[1].signed_in);
        assert_eq!(found[1].in_pool_as.as_deref(), Some("home-acct"));
        assert_eq!(
            std::fs::read(home.path().join(".claude-work/.credentials.json")).unwrap(),
            before
        );
    }
}
