//! Copies accounts from the real cc-rotate store into a scratch store for an end-to-end test
//! (`tests/e2e/wait-at-limit.py`). Each copy keeps the account's access token, which must still
//! be valid, and gets a dummy refresh token, so nothing using the copy can renew the login, which
//! would retire the real one. Never prints a secret.
//!
//!   cargo run -p ccr-core --example e2e_stage -- <scratch data dir> <scratch profile> <map file> <label>...
//!
//! `<map file>` gets `{sha256(access token) hex: label}`, so a fake API can tell the accounts
//! apart without holding a token. The scratch store has usage checks off.

use anyhow::{Context, Result, bail};
use ccr_core::store::{AccountIdentity, AccountState, Store};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [data_dir, profile, map_file, labels @ ..] = args.as_slice() else {
        bail!("usage: e2e_stage <scratch data dir> <scratch profile> <map file> <label>...");
    };
    if labels.is_empty() {
        bail!("name at least one account");
    }
    if std::env::var_os("CC_ROTATE_DATA_DIR").is_some() {
        bail!("unset CC_ROTATE_DATA_DIR: the accounts come from the real store");
    }
    let (real, _) = ccr_core::open(None)?;
    let scratch = Store::open(&std::path::Path::new(data_dir).join("store.db"))?;
    scratch.set_setting("usage_checks", Some("false"))?;
    scratch.set_setting("profile_dir", Some(profile))?;
    std::fs::create_dir_all(profile)?;
    let now = ccr_core::time::now_ms();
    let mut map = Map::new();
    for label in labels {
        let account = real.find_account(label)?;
        let login = real.credential(account.id)?.context("no stored login")?;
        let minutes = (login.oauth["expiresAt"].as_i64().unwrap_or(0) - now) / 60_000;
        if minutes < 20 {
            bail!("{label}'s access token expires in {minutes} minutes; pick another account");
        }
        let mut oauth = login.oauth.clone();
        oauth["refreshToken"] = json!(format!("cc-rotate-e2e-no-refresh-{label}"));
        let token = oauth["accessToken"]
            .as_str()
            .context("the login has no access token")?;
        let digest: String = Sha256::digest(token.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        map.insert(digest, json!(label));
        let id = scratch.insert_account(
            &account.label,
            &AccountIdentity {
                email: account.email.clone(),
                account_uuid: account.account_uuid.clone(),
                org_uuid: account.org_uuid.clone(),
                org_name: account.org_name.clone(),
                subscription_type: account.subscription_type.clone(),
                rate_limit_tier: account.rate_limit_tier.clone(),
                profile_account: real.profile_account_json(account.id)?,
            },
            AccountState::Parked,
        )?;
        scratch.save_credential(id, &oauth, "login")?;
        println!("staged {label} ({minutes} minutes left on its access token)");
    }
    std::fs::write(map_file, Value::Object(map).to_string())?;
    Ok(())
}
