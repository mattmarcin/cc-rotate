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

/// Names of sign-in staging folders: `login-<uuid>`.
pub const STAGING_PREFIX: &str = "login-";
/// How long a sign-in may take; a staging folder that records no owner is abandoned after it.
pub const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A staging directory that removes itself when dropped. It records the process using it, so
/// `usage_check::sweep_logins` deletes it if that process ends without dropping it.
#[derive(Debug)]
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub fn create(paths: &Paths) -> Result<Staging> {
        // Leftovers of earlier sign-ins that ended without cleaning up go first.
        usage_check::sweep_logins(paths);
        let dir = paths
            .staging_root()
            .join(format!("{STAGING_PREFIX}{}", uuid::Uuid::new_v4().simple()));
        crate::secret::create_private_dir(&dir)?;
        let staging = Staging { dir };
        usage_check::record_owner(&staging.dir)?;
        Ok(staging)
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

/// The `claude` executable: the `claude_path` setting, else the first one in an absolute PATH
/// directory (`paths::find_program`).
pub fn claude_executable(store: &Store) -> Result<PathBuf> {
    if let Some(path) = store.settings()?.claude_path {
        let path = PathBuf::from(path);
        // Set before full paths were required; never looked up from the current folder.
        if !path.is_absolute() {
            bail!(
                "claude_path must be a full path. Set it with cc-rotate-ctl config claude_path <path>."
            );
        }
        return Ok(path);
    }
    crate::paths::find_program("claude")
        .context("Couldn't find `claude` on PATH. Install Claude Code or set claude_path.")
}

/// `claude auth login --claudeai [--email <email>]` against the staging directory.
/// Callers choose stdio: inherited for the CLI, piped for the app.
pub fn login_command(claude: &Path, staging: &Path, email: Option<&str>) -> Command {
    let mut cmd = Command::new(claude);
    cmd.args(["auth", "login", "--claudeai"]);
    if let Some(email) = email.filter(|e| !e.is_empty()) {
        cmd.args(["--email", email]);
    }
    // An inherited login or backend (`CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_*`) must not stand in
    // for the one being made.
    usage_check::scrub_env(&mut cmd);
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
        .ok_or_else(|| anyhow!("Sign-in didn't finish, so no login was saved."))?;

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
        bail!("Signed in, but Claude Code didn't say which account it was. Try again.");
    }

    if let (Some(expected), Some(got)) = (&req.expect_email, &ident.email) {
        if !expected.eq_ignore_ascii_case(got) {
            bail!(
                "Signed in as {got}, not {expected}. Switch to {expected} in your browser and try again."
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
    if let Some(account) = &existing {
        check_same_org(account, &ident)?;
    }

    let has_active = store.active_account()?.is_some();
    // A usage check may be running Claude Code with this account's previous login; it must not
    // store a renewal of that login over the new one (usage_check).
    // Long enough for a check or warm-up that is running to finish (it is killed after
    // JOB_TIMEOUT).
    let wait = Duration::from_millis(store.settings()?.lock_timeout_ms) + usage_check::JOB_TIMEOUT;
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
                        bail!("Another account is already named {l}.");
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
                Some(json!({ "message": format!("Saved {}'s new login but didn't put it in the profile: an interrupted swap left it unclear which account the profile is signed in to.", current.label) })),
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
    let mut cmd = Command::new(claude);
    cmd.args(["auth", "status", "--json"]);
    // It must describe the staging login, not one an inherited variable points at.
    usage_check::scrub_env(&mut cmd);
    let output = cmd
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

/// One person can belong to several organizations under one account id, and cc-rotate keeps one
/// login per account: a sign-in to another organization must not replace the account's login.
fn check_same_org(target: &Account, ident: &AccountIdentity) -> Result<()> {
    let (Some(have), Some(got)) = (&target.org_uuid, &ident.org_uuid) else {
        return Ok(());
    };
    if have == got {
        return Ok(());
    }
    let org = |name: &Option<String>| {
        name.clone()
            .unwrap_or_else(|| "another organization".into())
    };
    bail!(
        "This sign-in is for {got_org}, but {label} is in {have_org}. cc-rotate keeps one login per Claude account, so {label}'s login was left as it was. Pick {have_org} when you sign in, or remove {label} first to add {got_org} instead.",
        got_org = org(&ident.org_name),
        have_org = org(&target.org_name),
        label = target.label,
    );
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
            "Signed in as {}, not {expected}. Switch to {expected} in your browser and try again.",
            ident.email.as_deref().unwrap_or("a different account"),
            expected = target.email.as_deref().unwrap_or(&target.label),
        );
    }
    Ok(())
}
