//! Adding and re-logging accounts (spec section 10).
//!
//! cc-rotate never handles the OAuth exchange itself. It runs the unmodified
//! `claude auth login --claudeai` with `CLAUDE_CONFIG_DIR` pointed at an empty staging
//! directory, then imports the login Claude Code wrote there. Each login is an independent
//! grant, so nothing is shared with the user's other profiles.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};

use crate::credfile;
use crate::credstore::Credentials;
use crate::lock::{DirLock, LockOptions};
use crate::paths::Paths;
use crate::profile_account::{self, ProfileAccount};
use crate::scheduler::Reason;
use crate::store::{Account, AccountIdentity, AccountState, Store, kind};
use crate::swap::{self, SwapRequest};
use crate::usage_check;

#[derive(Debug, Clone, Default)]
pub struct LoginRequest {
    /// Pre-fills the sign-in page.
    pub email: Option<String>,
    /// Label for a new account; derived from the email when absent.
    pub label: Option<String>,
    /// Re-login this existing account; the import is refused if a different account signs in.
    pub relogin: Option<i64>,
    /// Refuse the import unless this account signs in (used when adding an account found in
    /// another profile, so a wrong browser account never lands under that profile's label).
    pub expect_email: Option<String>,
}

/// A staging directory that removes itself when dropped.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub fn create(paths: &Paths) -> Result<Staging> {
        let dir = paths
            .staging_root()
            .join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Staging { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Staging {
    /// On macOS the login lands in a Keychain item named after the staging directory; it goes
    /// with the directory, whether the login was imported, failed or was cancelled.
    fn drop(&mut self) {
        let _ = Credentials::for_profile(&self.dir).delete_keychain_item();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The `claude` executable: the `claude_path` setting, else the first one on PATH.
pub fn claude_executable(store: &Store) -> Result<PathBuf> {
    if let Some(path) = store.settings()?.claude_path {
        return Ok(PathBuf::from(path));
    }
    which::which("claude")
        .context("could not find `claude` on PATH; install Claude Code or set claude_path")
}

/// `claude auth login --claudeai [--email <email>]` against the staging directory.
/// Callers choose stdio: inherited for the CLI, piped for the app.
pub fn login_command(claude: &Path, staging: &Path, email: Option<&str>) -> Command {
    let mut cmd = Command::new(claude);
    cmd.args(["auth", "login", "--claudeai"]);
    if let Some(email) = email.filter(|e| !e.is_empty()) {
        cmd.args(["--email", email]);
    }
    cmd.env("CLAUDE_CONFIG_DIR", staging);
    cmd
}

/// The sign-in URL Claude Code prints ("If the browser didn't open, visit: <url>").
pub fn find_sign_in_url(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"https://[^\s]+/oauth/authorize\?[^\s]+").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

#[derive(Debug, Clone, Serialize)]
pub struct Imported {
    pub account: Account,
    pub created: bool,
    pub activated: bool,
}

/// Import the login a finished `claude auth login` left in `staging`.
pub fn import(
    store: &Store,
    paths: &Paths,
    staging: &Path,
    req: &LoginRequest,
) -> Result<Imported> {
    let oauth = Credentials::for_profile(staging)
        .read_oauth()?
        .filter(|o| !credfile::is_dead(o))
        .ok_or_else(|| anyhow!("the login did not complete; no account login was written"))?;

    let mut ident = identity(
        &oauth,
        profile_account::read(&staging.join(".claude.json"))?,
    );
    // The organization decides which usage readings belong to the account (section 9.1).
    if ident.email.is_none() || ident.account_uuid.is_none() || ident.org_uuid.is_none() {
        if let Ok(claude) = claude_executable(store) {
            fill_from_auth_status(&mut ident, &claude, staging);
        }
    }
    if ident.account_uuid.is_none() && ident.email.is_none() {
        bail!("the login completed but Claude Code did not record which account signed in");
    }

    if let (Some(expected), Some(got)) = (&req.expect_email, &ident.email) {
        if !expected.eq_ignore_ascii_case(got) {
            bail!(
                "signed in as {got}, but this sign-in was for {expected}; switch to that account in your browser and try again"
            );
        }
    }

    let existing = match req.relogin {
        Some(id) => {
            let target = store
                .account(id)?
                .ok_or_else(|| anyhow!("account {id} does not exist"))?;
            check_same_account(&target, &ident)?;
            Some(target)
        }
        None => match &ident.account_uuid {
            Some(uuid) => store.account_by_uuid(uuid)?,
            None => None,
        }
        .or(match &ident.email {
            Some(email) => store.account_by_email(email)?,
            None => None,
        }),
    };

    let has_active = store.active_account()?.is_some();
    // A usage check may be running Claude Code with this account's previous login; it must not
    // store a renewal of that login over the new one (usage_check).
    // Long enough for a check that is running to finish (it is killed after RUN_TIMEOUT).
    let wait = Duration::from_millis(store.settings()?.lock_timeout_ms) + usage_check::RUN_TIMEOUT;
    let account_lock = match &existing {
        Some(account) => Some(
            DirLock::acquire(&paths.account_lock(account.id), LockOptions::claude(wait))
                .context("waiting for a usage check of the account to finish")?,
        ),
        None => None,
    };
    let (id, created) = store.immediate(|s| match &existing {
        Some(account) => {
            s.update_identity(account.id, &ident)?;
            s.save_credential(account.id, &oauth, "login")?;
            // Read again under the lock: a check may have marked it meanwhile.
            let now = s.account(account.id)?.map(|a| a.state);
            if now == Some(AccountState::NeedsLogin) {
                s.set_state(account.id, AccountState::Parked)?;
            }
            // A fresh login settles whatever earlier checks of the account left open.
            usage_check::forget(s, account.id)?;
            usage_check::discard_leftovers(paths, account.id);
            Ok((account.id, false))
        }
        None => {
            let base = req
                .label
                .clone()
                .or_else(|| {
                    ident
                        .email
                        .as_deref()
                        .and_then(|e| e.split('@').next())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "account".to_string());
            let label = match &req.label {
                Some(l) => {
                    crate::store::validate_label(l)?;
                    if s.account_by_label(l)?.is_some() {
                        bail!("an account is already labelled {l:?}");
                    }
                    l.clone()
                }
                None => s.unique_label(&base)?,
            };
            let id = s.insert_account(&label, &ident, AccountState::Parked)?;
            s.save_credential(id, &oauth, "login")?;
            Ok((id, true))
        }
    })?;

    // The swap below takes the account's lock itself.
    drop(account_lock);
    let mut activated = false;
    let current = store.account(id)?.expect("account just written");
    if current.state == AccountState::Active {
        if swap::install_active_login(store, paths, id, &oauth)? == swap::ActiveLogin::Unidentified
        {
            store.log_event(
                kind::ERROR,
                None,
                Some(id),
                Some("login"),
                Some(json!({ "message": format!("Saved the new login of {}, but did not put it into the profile yet: a swap stopped half way and cc-rotate is still finding out which account the profile is signed in to.", current.label) })),
            )?;
        }
    } else if !has_active && current.state == AccountState::Parked {
        let outcome = swap::run(
            store,
            paths,
            &SwapRequest {
                from: None,
                to: Some(id),
                reason: Reason::Login,
                wait: true,
            },
        )?;
        activated = matches!(outcome, swap::SwapOutcome::Swapped { .. });
    }

    store.log_event(
        kind::LOGIN,
        None,
        Some(id),
        Some(if created { "added" } else { "relogin" }),
        Some(json!({ "label": current.label, "activated": activated })),
    )?;
    Ok(Imported {
        account: store.account(id)?.expect("account just written"),
        created,
        activated,
    })
}

fn identity(oauth: &Value, profile: Option<ProfileAccount>) -> AccountIdentity {
    let text = |k: &str| oauth.get(k).and_then(Value::as_str).map(str::to_string);
    let profile = profile.unwrap_or_default();
    AccountIdentity {
        email: profile.email,
        account_uuid: profile.account_uuid,
        org_uuid: profile.org_uuid,
        org_name: profile.org_name,
        subscription_type: text("subscriptionType"),
        rate_limit_tier: text("rateLimitTier"),
        profile_account: Some(profile.raw).filter(|v| v.is_object()),
    }
}

fn fill_from_auth_status(ident: &mut AccountIdentity, claude: &Path, staging: &Path) {
    let output = Command::new(claude)
        .args(["auth", "status", "--json"])
        .env("CLAUDE_CONFIG_DIR", staging)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else { return };
    let Ok(status) = serde_json::from_slice::<Value>(&output.stdout) else {
        return;
    };
    let text = |k: &str| status.get(k).and_then(Value::as_str).map(str::to_string);
    ident.email = ident.email.take().or_else(|| text("email"));
    ident.org_uuid = ident.org_uuid.take().or_else(|| text("orgId"));
    ident.org_name = ident.org_name.take().or_else(|| text("orgName"));
    ident.subscription_type = ident
        .subscription_type
        .take()
        .or_else(|| text("subscriptionType"));
}

fn check_same_account(target: &Account, ident: &AccountIdentity) -> Result<()> {
    let mismatch = match (&target.account_uuid, &ident.account_uuid) {
        (Some(a), Some(b)) => a != b,
        _ => match (&target.email, &ident.email) {
            (Some(a), Some(b)) => !a.eq_ignore_ascii_case(b),
            _ => false,
        },
    };
    if mismatch {
        bail!(
            "signed in as {}, but this login was for {}; sign in to the right account in your browser and try again",
            ident.email.as_deref().unwrap_or("a different account"),
            target.email.as_deref().unwrap_or(&target.label),
        );
    }
    Ok(())
}
