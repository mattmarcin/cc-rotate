//! The account identity Claude Code records in a profile's `.claude.json` (`oauthAccount`).

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::credfile;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProfileAccount {
    pub account_uuid: Option<String>,
    pub email: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    /// The full `oauthAccount` object, kept so it can be restored verbatim.
    pub raw: Value,
}

impl ProfileAccount {
    pub fn from_value(raw: Value) -> Self {
        let s = |k: &str| raw.get(k).and_then(Value::as_str).map(str::to_string);
        Self {
            account_uuid: s("accountUuid"),
            email: s("emailAddress"),
            org_uuid: s("organizationUuid"),
            org_name: s("organizationName"),
            raw,
        }
    }
}

/// Read `oauthAccount` from a `.claude.json`. `Ok(None)` when the file or key is missing.
pub fn read(config_file: &Path) -> Result<Option<ProfileAccount>> {
    let Some(mut doc) = credfile::read_document(config_file)? else {
        return Ok(None);
    };
    Ok(doc
        .remove("oauthAccount")
        .filter(Value::is_object)
        .map(ProfileAccount::from_value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"numStartups":3,"oauthAccount":{"accountUuid":"u1","emailAddress":"a@example.com","organizationUuid":"o1","organizationName":"Org"}}"#,
        )
        .unwrap();
        let acct = read(&path).unwrap().unwrap();
        assert_eq!(acct.account_uuid.as_deref(), Some("u1"));
        assert_eq!(acct.email.as_deref(), Some("a@example.com"));
        assert_eq!(acct.org_name.as_deref(), Some("Org"));
    }

    #[test]
    fn missing_key_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(&path, r#"{"numStartups":3}"#).unwrap();
        assert!(read(&path).unwrap().is_none());
    }
}
